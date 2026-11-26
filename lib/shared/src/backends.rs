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
    /// Why a same-source retry is refused when the backend will not give a source's files, when
    /// the backend words it its own way; else `blocked` gives the common text.
    pub blocked: Option<&'static str>,
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
    blocked: Some("slskd's blacklist holds this source"),
};

pub const ARCHIVE: BackendMeta = BackendMeta {
    id: crate::archive::BACKEND_ID,
    name: "Internet Archive",
    service: "archive.org",
    delivery: Delivery::AsFound,
    switch: Switch::AdminToggle,
    about: &[
        "Off until an admin turns it on. Auto-download then searches archive.org next to Soulseek.",
        "Offers only the files their uploaders gave, never archive.org's own conversions, and checks each one against the checksum archive.org lists before it is imported.",
        "Each item's rights are set by its uploader.",
    ],
    link: Some((
        "Rights on archive.org",
        "https://help.archive.org/help/rights/",
    )),
    attribution: None,
    blocked: None,
};

pub const YTDLP: BackendMeta = BackendMeta {
    id: crate::ytdlp::BACKEND_ID,
    name: "YouTube Music",
    service: "YouTube",
    delivery: Delivery::LossyOnly,
    switch: Switch::AdminToggle,
    about: &[
        "Off until an admin turns it on. Auto-download then searches YouTube Music only when no other source has a match it would take, and picks from it on its own only for users who chose that in their settings.",
        "Its files are Opus at about 130 kbps, never lossless, and never replace a copy already in the library.",
        "Needs the :full image. YouTube's terms forbid downloading from it: turning this on is your choice.",
    ],
    link: Some(("YouTube's Terms of Service", "https://www.youtube.com/t/terms")),
    attribution: None,
    blocked: None,
};

pub const GDSTUDIO: BackendMeta = BackendMeta {
    id: crate::gdstudio::BACKEND_ID,
    name: "NetEase (GD Studio)",
    service: "GD Studio",
    delivery: Delivery::AsFound,
    switch: Switch::AdminToggle,
    about: &[
        "Off until an admin turns it on. Auto-download then searches NetEase's catalogue through GD Studio's public API next to Soulseek.",
        "GD Studio's API page forbids downloading: \"仅限本人学习参考，严禁下载、传播或商用\" (for personal study only; downloading, spreading and commercial use are strictly forbidden), and it says continued use counts as agreeing.",
        "The API allows 50 requests in 5 minutes. Soulbeet sends at most 45 in 5 minutes for every user together, so a search can end before it has checked every track.",
        "Each file is checked against the MD5 NetEase names it by before it is imported. Bit depth and sample rate are read from the file header, which can name an upsampled master.",
    ],
    link: Some(("GD Studio's API page", "https://music-api.gdstudio.xyz/api.php")),
    attribution: Some("NetEase via GD音乐台 (music.gdstudio.xyz)"),
    blocked: None,
};

/// Every backend this build has, the default one first.
pub const ALL: &[BackendMeta] = &[SLSKD, ARCHIVE, YTDLP, GDSTUDIO];

/// The backend a search or a send uses when none is named.
pub const DEFAULT_ID: &str = SLSKD.id;

/// The backend with registry id `id`.
pub fn meta(id: &str) -> Option<&'static BackendMeta> {
    ALL.iter().find(|m| m.id == id)
}

/// What the backend `id` delivers: its meta's, else as found, as for a backend this build has no
/// meta for.
pub fn delivery(id: &str) -> Delivery {
    meta(id).map_or(Delivery::AsFound, |m| m.delivery)
}

/// The name the UI shows for the backend `id`: its meta's, else `id` itself, as for a backend
/// this build has no meta for.
pub fn name(id: &str) -> &str {
    meta(id).map_or(id, |m| m.name)
}

/// The service texts name for the backend `id`: its meta's, else `id` itself, as for a backend
/// this build has no meta for.
pub fn service(id: &str) -> &str {
    meta(id).map_or(id, |m| m.service)
}

/// Why a same-source retry on the backend `id` is refused when it will not give a source's
/// files.
pub fn blocked(id: &str) -> String {
    match meta(id).and_then(|m| m.blocked) {
        Some(text) => text.to_string(),
        None => format!("{} will not give files from this source", service(id)),
    }
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
        assert_eq!(meta("archive"), Some(&ARCHIVE));
        assert_eq!(meta("ytdlp"), Some(&YTDLP));
        assert_eq!(meta("gdstudio"), Some(&GDSTUDIO));
        assert_eq!(meta("nicotine"), None);
    }

    #[test]
    fn gd_studio_is_off_until_an_admin_turns_it_on_and_says_its_operator_forbids_downloading() {
        assert_eq!(GDSTUDIO.switch, Switch::AdminToggle);
        assert_eq!(GDSTUDIO.delivery, Delivery::AsFound);
        assert!(GDSTUDIO
            .about
            .iter()
            .any(|l| l.contains("forbids downloading")));
        assert_eq!(
            GDSTUDIO.attribution,
            Some("NetEase via GD音乐台 (music.gdstudio.xyz)")
        );
        assert!(GDSTUDIO
            .link
            .is_some_and(|(_, url)| url.starts_with("https://")));
        assert_eq!(
            blocked("gdstudio"),
            "GD Studio will not give files from this source"
        );
    }

    #[test]
    fn the_archive_is_off_until_an_admin_turns_it_on_and_links_its_rights_page() {
        assert_eq!(ARCHIVE.switch, Switch::AdminToggle);
        assert_eq!(ARCHIVE.delivery, Delivery::AsFound);
        assert!(ARCHIVE
            .link
            .is_some_and(|(_, url)| url.starts_with("https://")));
        assert_eq!(
            blocked("archive"),
            "archive.org will not give files from this source"
        );
    }

    #[test]
    fn youtube_music_is_off_until_an_admin_turns_it_on_and_delivers_lossy_files_only() {
        assert_eq!(YTDLP.switch, Switch::AdminToggle);
        assert_eq!(delivery("ytdlp"), Delivery::LossyOnly);
        assert_eq!(delivery("slskd"), Delivery::AsFound);
        assert_eq!(delivery("stub-3"), Delivery::AsFound);
        assert!(YTDLP
            .link
            .is_some_and(|(_, url)| url.starts_with("https://")));
        assert_eq!(
            blocked("ytdlp"),
            "YouTube will not give files from this source"
        );
    }

    #[test]
    fn the_ui_names_a_backend_by_its_meta_else_by_its_id() {
        assert_eq!(name("ytdlp"), YTDLP.name);
        assert_eq!(name("stub-3"), "stub-3");
    }

    #[test]
    fn texts_name_the_backends_service() {
        assert_eq!(service("slskd"), "slskd");
        assert_eq!(service("stub-3"), "stub-3");
        assert_eq!(blocked("slskd"), "slskd's blacklist holds this source");
        assert_eq!(
            blocked("stub-3"),
            "stub-3 will not give files from this source"
        );
    }

    #[test]
    fn delivery_is_written_in_snake_case() {
        assert_eq!(
            serde_json::to_value(Delivery::LossyOnly).unwrap(),
            serde_json::json!("lossy_only")
        );
    }
}
