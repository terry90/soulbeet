//! Whether the sidecar can take work: Soulful's web user and its permissions, the sidecar's
//! language and hubs, and whether it saves into Soulful's downloads folder, proved by a folder
//! the sidecar makes there and Soulful then finds, writes in and deletes.

use super::error::AirdcppError;
use super::http::Sidecar;
use super::models::{Hub, SessionSelf, SidecarSettings, SystemInfo};
use super::{Engine, Link};
use reqwest::Method;
use serde_json::json;
use shared::airdcpp::{SidecarReport, Source};
use shared::download::SourcePresence;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;

/// How often the folder probe runs, whether it passed or not: a failed one can leave its folder
/// where Soulbeet cannot delete it. A Check runs it at once.
const PROBE_EVERY: Duration = Duration::from_secs(600);

/// The permissions Soulful's web user needs: no admin, no share or settings edits.
pub(crate) const PERMISSIONS: [&str; 12] = [
    "search",
    "download",
    "transfers",
    "queue_view",
    "queue_edit",
    "hubs_view",
    "settings_view",
    "share_view",
    "filelists_view",
    "filelists_edit",
    "filesystem_view",
    "filesystem_edit",
];

pub(crate) async fn read_settings(sidecar: &Sidecar) -> Result<SidecarSettings, AirdcppError> {
    sidecar
        .post("settings/get", &json!({ "keys": SidecarSettings::KEYS }))
        .await
}

/// The permissions of `PERMISSIONS` the web user lacks; an AirDC++ admin has every one.
fn missing(granted: &[String]) -> Vec<String> {
    if granted.iter().any(|p| p == "admin") {
        return vec![];
    }
    (PERMISSIONS.iter())
        .filter(|p| !granted.iter().any(|g| g == *p))
        .map(|p| p.to_string())
        .collect()
}

/// How many hubs are connected, of how many.
async fn hubs(sidecar: &Sidecar) -> Result<(u32, u32), AirdcppError> {
    let hubs: Vec<Hub> = sidecar.get("hubs").await?;
    let connected = hubs
        .iter()
        .filter(|h| h.connect_state.id == "connected")
        .count();
    Ok((connected as u32, hubs.len() as u32))
}

/// Makes a folder through the sidecar in its download folder and looks for it in Soulbeet's:
/// Ok once Soulbeet found it, wrote a file in it and deleted both. A folder Soulbeet finds is
/// deleted even when the write failed.
async fn probe(sidecar: &Sidecar, root: &str, download_path: &Path) -> Result<(), AirdcppError> {
    let name = format!(".soulful-check-{}", uuid::Uuid::new_v4());
    let root = match root.ends_with('/') {
        true => root.to_string(),
        false => format!("{root}/"),
    };
    let made = sidecar
        .send(
            Method::POST,
            "filesystem/directory",
            Some(&json!({"path": format!("{root}{name}/")})),
        )
        .await?;
    made.done()?;
    let ours = download_path.display().to_string();
    let folder = download_path.join(&name);
    let found = tokio::task::spawn_blocking(move || {
        if !folder.is_dir() {
            return Err(None);
        }
        let file = folder.join("probe");
        let written = std::fs::write(&file, b"soulful").and_then(|()| std::fs::remove_file(&file));
        let removed = std::fs::remove_dir(&folder);
        written.and(removed).map_err(|e| Some(e.to_string()))
    })
    .await
    .map_err(|e| AirdcppError::Unreadable(e.to_string()))?;
    match found {
        Ok(()) => Ok(()),
        Err(None) => Err(AirdcppError::OtherFolder {
            sidecar: root,
            ours,
        }),
        Err(Some(detail)) => Err(AirdcppError::FolderNotWritable { ours, detail }),
    }
}

/// The checks of the health check, in order, each stopping at its failure.
pub(crate) async fn check(engine: &Engine, link: &Link) -> Result<(), AirdcppError> {
    let sidecar = &link.sidecar;
    if sidecar.connection().is_default_login() {
        return Err(AirdcppError::DefaultLogin);
    }
    let session: SessionSelf = sidecar.get("sessions/self").await?;
    let lacking = missing(&session.user.permissions);
    if !lacking.is_empty() {
        return Err(AirdcppError::MissingPermissions(lacking));
    }
    let settings = read_settings(sidecar).await?;
    *link.settings.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(settings.clone());
    if !settings.language_file.trim().is_empty() {
        return Err(AirdcppError::Language);
    }
    if hubs(sidecar).await?.0 == 0 {
        return Err(AirdcppError::NoHub);
    }
    let kept = (link.probed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()).filter(|(at, _)| at.elapsed() < PROBE_EVERY);
    if let Some((_, found)) = kept {
        return found;
    }
    let found = probe(sidecar, &settings.download_directory, &engine.download_path).await;
    // A probe the sidecar did not answer made no folder that counts: the next check asks again.
    if !matches!(
        found,
        Err(AirdcppError::Unreachable(_) | AirdcppError::Unauthorized)
    ) {
        *link.probed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), found.clone()));
    }
    found
}

/// Every fact the settings page shows, read even when a check fails.
pub(crate) async fn report(engine: &Engine, link: &Link) -> SidecarReport {
    let sidecar = &link.sidecar;
    let mut report = SidecarReport::default();
    // The folder probe runs again, whenever it last ran.
    *link.probed.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    let checked = check(engine, link).await;
    report.problem = checked.as_ref().err().map(|e| e.to_string());
    if let Ok(info) = sidecar.get::<SystemInfo>("system/system_info").await {
        report.client_version = Some(format!(
            "{} (API {})",
            info.client_version, info.api_version
        ));
    }
    if let Ok(session) = sidecar.get::<SessionSelf>("sessions/self").await {
        report.admin_user = Some(session.user.permissions.iter().any(|p| p == "admin"));
    }
    if let Ok((connected, all)) = hubs(sidecar).await {
        report.hubs_connected = connected;
        report.hubs = all;
    }
    if let Ok(settings) = read_settings(sidecar).await {
        report.passive = Some(settings.passive());
        report.partial_sharing = Some(settings.use_partial_sharing);
        report.download_directory = Some(settings.download_directory);
    }
    report.shared_files = shared_files(sidecar).await;
    report.same_folder = match &checked {
        Ok(()) => Some(true),
        Err(AirdcppError::OtherFolder { .. } | AirdcppError::FolderNotWritable { .. }) => {
            Some(false)
        }
        Err(_) => None,
    };
    report
}

/// How many files the sidecar shares: 0 when it answers that it shares nothing.
async fn shared_files(sidecar: &Sidecar) -> Option<u64> {
    let answer = sidecar.send(Method::GET, "share/stats", None).await.ok()?;
    if answer.status == 204 {
        return Some(0);
    }
    let stats: serde_json::Value = answer.json().ok()?;
    stats["total_file_count"].as_f64().map(super::models::whole)
}

/// Whether the peer `source` names is online on its hub now.
pub(crate) async fn presence(
    sidecar: &Sidecar,
    source: &str,
) -> Result<SourcePresence, AirdcppError> {
    let Some(source) = Source::parse(source) else {
        return Ok(SourcePresence::Offline);
    };
    let body = json!({"user": {"cid": source.cid, "hub_url": source.hub_url}});
    let answer = sidecar
        .send(Method::POST, "users/search_hinted_user", Some(&body))
        .await?;
    if answer.status == 400 {
        return Ok(SourcePresence::Offline);
    }
    let user: super::models::User = answer.json()?;
    Ok(if user.has("offline") {
        SourcePresence::Offline
    } else if user.has("away") {
        SourcePresence::Away
    } else {
        SourcePresence::Online
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_admin_lacks_no_permission_and_a_plain_user_lacks_what_it_was_not_given() {
        assert!(missing(&["admin".to_string()]).is_empty());
        let all: Vec<String> = PERMISSIONS.iter().map(|p| p.to_string()).collect();
        assert!(missing(&all).is_empty());
        let some: Vec<String> = all.iter().filter(|p| *p != "queue_edit").cloned().collect();
        assert_eq!(missing(&some), ["queue_edit"]);
    }
}
