//! The Direct Connect download backend's own data: how a source names a peer on one hub, what
//! its search hands to `download`, and what a check of the AirDC++ sidecar found.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The id Direct Connect goes by as a download backend, in the registry and on every row and
/// search.
pub const BACKEND_ID: &str = "airdcpp";

/// Whether `text` is a Tiger hash as AirDC++ writes one, a CID or a TTH: 39 base32 characters.
pub fn is_tiger_hash(text: &str) -> bool {
    text.len() == 39
        && text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
}

/// One peer on one hub, as AirDC++ addresses it: written `<CID>@<hub URL>`. An NMDC hub gives
/// each peer a CID of its own, so one person on two hubs is two sources.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Source {
    pub cid: String,
    pub hub_url: String,
}

impl Source {
    /// The source `text` names; None for a text that is not `<CID>@<hub URL>`. A CID holds no
    /// `@`, so the first one splits it.
    pub fn parse(text: &str) -> Option<Source> {
        let (cid, hub_url) = text.split_once('@')?;
        (is_tiger_hash(cid) && !hub_url.is_empty()).then(|| Source {
            cid: cid.to_string(),
            hub_url: hub_url.to_string(),
        })
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.cid, self.hub_url)
    }
}

/// One file of a search result, kept in `DownloadableItem::backend_data`: what `download` queues
/// and the TTH AirDC++ checks the bytes against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemData {
    pub cid: String,
    pub hub_url: String,
    pub nick: String,
    pub hub_name: String,
    pub tth: String,
    pub size: u64,
    /// The file came from the peer's file list of its folder, not only from the search.
    pub listed: bool,
}

/// The AirDC++ settings an admin gave, as the settings page reads them: the password is never
/// sent back, only whether one is stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarSettings {
    pub url: String,
    pub username: String,
    pub password_set: bool,
}

/// What a check of the sidecar found, for the settings page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarReport {
    /// What stops Soulful from using the sidecar, in the words its status line shows; None when
    /// nothing does.
    pub problem: Option<String>,
    pub client_version: Option<String>,
    pub hubs_connected: u32,
    pub hubs: u32,
    /// The sidecar takes no incoming connections, so peers that take none either are left out.
    pub passive: Option<bool>,
    /// Files the sidecar shares with its hubs.
    pub shared_files: Option<u64>,
    /// Partial downloads are shared with other peers while they download.
    pub partial_sharing: Option<bool>,
    /// The folder the sidecar saves into, as it names it.
    pub download_directory: Option<String>,
    /// Soulful saw a folder the sidecar made there in its own downloads folder.
    pub same_folder: Option<bool>,
    /// The web user Soulful signs in as is an AirDC++ admin.
    pub admin_user: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const CID: &str = "3IDK7KWKV2TJVM5IA4EPORCPZX2I7AH6HG2NLWY";

    #[test]
    fn a_source_is_written_and_read_back_whatever_its_hub_url_holds() {
        for hub_url in [
            "nmdcs://hub.example.net:411",
            "adcs://user@hub.example.net:1511",
            "dchub://hub.example.net",
        ] {
            let source = Source {
                cid: CID.into(),
                hub_url: hub_url.into(),
            };
            let text = source.to_string();
            assert_eq!(text, format!("{CID}@{hub_url}"));
            assert_eq!(Source::parse(&text), Some(source));
        }
        for bad in [
            "",
            "nick@hub",
            CID,
            &format!("{CID}@"),
            &format!("{}@x", &CID[1..]),
        ] {
            assert_eq!(Source::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_tiger_hash_is_39_base32_characters() {
        assert!(is_tiger_hash(CID));
        assert!(!is_tiger_hash(&CID.to_lowercase()));
        assert!(!is_tiger_hash(&format!("{}1", &CID[1..])));
        assert!(!is_tiger_hash(&CID[1..]));
    }
}
