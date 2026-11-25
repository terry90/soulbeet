//! What the app knows of each download backend without asking it: compiled into the server and
//! the browser, so the UI needs no call to show a backend's name or how it delivers.

use serde::{Deserialize, Serialize};

/// What a backend's files are, before any of them is looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Each file as its source has it, lossless or not.
    AsFound,
    /// Lossy files only, whatever the source has.
    LossyOnly,
}

/// Whether a backend can be turned off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    AlwaysOn,
    /// Off until an admin turns it on in Settings.
    AdminToggle,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackendMeta {
    /// The registry id, which `DownloadBackend::id()` returns.
    pub id: &'static str,
    /// What `DownloadBackend::name()` returns, shown in the UI.
    pub name: &'static str,
    /// The service failure texts name ("slskd").
    pub service: &'static str,
    pub delivery: Delivery,
    pub switch: Switch,
    /// Lines shown under the backend's switch.
    pub about: &'static [&'static str],
    /// A link (text, https URL) shown under the switch.
    pub link: Option<(&'static str, &'static str)>,
    /// Shown on each source row of this backend.
    pub attribution: Option<&'static str>,
    /// Why a same-source retry is refused when the backend will not give a source's files.
    pub blocked: &'static str,
}

pub const SLSKD: BackendMeta = BackendMeta {
    id: crate::slskd::BACKEND_ID,
    name: "Soulseek",
    service: "slskd",
    delivery: Delivery::AsFound,
    switch: Switch::AlwaysOn,
    about: &[],
    link: None,
    attribution: None,
    blocked: "slskd's blacklist holds this source",
};

/// Every backend this build has, the default one first.
pub const ALL: &[BackendMeta] = &[SLSKD];

/// The backend a search or a send uses when none is named.
pub const DEFAULT_ID: &str = SLSKD.id;

/// The backend with registry id `id`.
pub fn meta(id: &str) -> Option<&'static BackendMeta> {
    ALL.iter().find(|m| m.id == id)
}

/// The service texts name for the backend `id`: its meta's, else `id` itself, as for a backend
/// this build has no meta for.
pub fn service(id: &str) -> &str {
    meta(id).map_or(id, |m| m.service)
}

/// Whether an admin turned a backend on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendSwitch {
    pub id: String,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_backend_has_its_own_id_and_the_default_comes_first() {
        let ids: std::collections::BTreeSet<&str> = ALL.iter().map(|m| m.id).collect();
        assert_eq!(ids.len(), ALL.len());
        assert_eq!(ALL[0].id, DEFAULT_ID);
        assert_eq!(ALL[0].switch, Switch::AlwaysOn);
        assert_eq!(meta("slskd"), Some(&SLSKD));
        assert_eq!(meta("nicotine"), None);
    }

    #[test]
    fn texts_name_the_backends_service() {
        assert_eq!(service("slskd"), "slskd");
        assert_eq!(service("stub-3"), "stub-3");
    }

    #[test]
    fn delivery_is_written_in_snake_case() {
        assert_eq!(
            serde_json::to_value(Delivery::LossyOnly).unwrap(),
            serde_json::json!("lossy_only")
        );
    }
}
