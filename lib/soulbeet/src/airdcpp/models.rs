//! The parts of AirDC++'s Web API answers the backend reads, as AirDC++ 2.14.0 writes them.
//! Sizes and counts come as JSON numbers that may carry a fraction (`3441572.0`).

use serde::Deserialize;

/// A JSON number as a whole count: negative or missing reads as 0.
pub(crate) fn whole(n: f64) -> u64 {
    if n.is_finite() && n > 0.0 {
        n as u64
    } else {
        0
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Id {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct StatusText {
    pub id: String,
    #[serde(default)]
    pub str: String,
}

/// `GET /sessions/self`.
#[derive(Debug, Deserialize)]
pub(crate) struct SessionSelf {
    pub user: WebUser,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WebUser {
    #[serde(default)]
    pub permissions: Vec<String>,
}

/// `GET /system/system_info`.
#[derive(Debug, Deserialize)]
pub(crate) struct SystemInfo {
    pub api_version: u32,
    pub client_version: String,
}

/// One of `GET /hubs`.
#[derive(Debug, Deserialize)]
pub(crate) struct Hub {
    pub connect_state: Id,
}

/// The settings `POST /settings/get` gives for the keys the backend reads.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SidecarSettings {
    pub download_directory: String,
    pub language_file: String,
    /// -1 off, 0 active, 1 active through UPnP, 2 passive.
    pub connection_mode_v4: i32,
    pub use_partial_sharing: bool,
    /// Seconds.
    pub min_search_interval: u64,
}

impl SidecarSettings {
    pub(crate) const KEYS: [&'static str; 5] = [
        "download_directory",
        "language_file",
        "connection_mode_v4",
        "use_partial_sharing",
        "min_search_interval",
    ];

    pub(crate) fn passive(&self) -> bool {
        self.connection_mode_v4 == 2
    }
}

/// `POST /search` and `GET /search/{id}`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SearchInstance {
    pub id: u64,
    /// The session type and the suffix the instance was made with: `basic_auth:<suffix>`.
    #[serde(default)]
    pub owner: String,
}

/// `POST /search/{id}/hub_search`.
#[derive(Debug, Deserialize)]
pub(crate) struct HubSearchQueued {
    /// Milliseconds until the search goes out to the last hub.
    pub queue_time: u64,
    pub queued_count: u32,
}

/// A peer on a hub, as answers name one.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct User {
    pub cid: String,
    pub hub_url: String,
    #[serde(default)]
    pub nicks: String,
    #[serde(default)]
    pub hub_names: String,
    #[serde(default)]
    pub flags: Vec<String>,
}

impl User {
    pub(crate) fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Slots {
    pub free: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Users {
    pub count: f64,
    pub user: User,
}

/// One of `GET /search/{id}/results/{start}/{count}`: a path the search found, with the first
/// of the users that have it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GroupedResult {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub tth: String,
    #[serde(rename = "type")]
    pub kind: Id,
    pub slots: Slots,
    pub users: Users,
    #[serde(default)]
    pub hits: f64,
}

/// One of `GET /search/{id}/results/{result}/children`: the path as one user has it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserResult {
    pub path: String,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub tth: String,
    #[serde(rename = "type")]
    pub kind: Id,
    pub slots: Slots,
    pub user: User,
}

/// `GET /filelists/{cid}`.
#[derive(Debug, Deserialize)]
pub(crate) struct FilelistSession {
    pub state: Id,
    #[serde(default)]
    pub location: Option<Location>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Location {
    pub path: String,
}

/// `GET /filelists/{cid}/items/{start}/{count}`.
#[derive(Debug, Deserialize)]
pub(crate) struct FilelistItems {
    pub items: Vec<ListItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ListItem {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub tth: String,
    #[serde(rename = "type")]
    pub kind: Id,
}

/// `POST /queue/bundles/file`.
#[derive(Debug, Deserialize)]
pub(crate) struct Queued {
    pub id: u64,
    #[serde(default)]
    pub merged: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SourceCount {
    pub online: f64,
}

/// One of `GET /queue/bundles/{start}/{count}`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Bundle {
    pub id: u64,
    /// The file's path for a file bundle, as AirDC++ saves it.
    pub target: String,
    #[serde(default)]
    pub downloaded_bytes: f64,
    #[serde(default)]
    pub speed: f64,
    pub status: StatusText,
    pub sources: SourceCount,
}

/// `POST /queue/check_path_queued`: the bundle that holds a path, if any.
#[derive(Debug, Deserialize)]
pub(crate) struct PathQueued {
    pub bundle: Option<QueuedBundle>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct QueuedBundle {
    pub id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TransferType {
    #[serde(default)]
    pub content_type: String,
}

/// One of `GET /transfers`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Transfer {
    #[serde(default)]
    pub download: bool,
    #[serde(default)]
    pub target: String,
    pub status: StatusText,
    pub user: User,
    /// null while the transfer has no target.
    #[serde(rename = "type", default)]
    pub kind: Option<TransferType>,
}

impl Transfer {
    /// A file list load, which uses no download slot and is no file of a bundle.
    pub(crate) fn is_filelist(&self) -> bool {
        (self.kind.as_ref()).is_some_and(|k| k.content_type == "filelist")
    }
}
