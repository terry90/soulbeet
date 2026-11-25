//! The Internet Archive download backend's own data, as its search hands it to `download`.

use serde::{Deserialize, Serialize};

/// The id the Internet Archive goes by as a download backend, in the registry and on every row
/// and search.
pub const BACKEND_ID: &str = "archive";

/// One original file of an archive item, kept in `DownloadableItem::backend_data`: what
/// `download` fetches and the checksum it checks the bytes against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveFile {
    /// The item's identifier.
    pub identifier: String,
    /// The file's name in the item, a subfolder included.
    pub name: String,
    pub size: u64,
    /// Lowercase hex, as archive.org lists it.
    pub md5: String,
}
