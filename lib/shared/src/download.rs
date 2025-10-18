use serde::{Deserialize, Serialize};

use crate::musicbrainz::{Album, Track};

#[derive(Serialize, Clone, PartialEq, Deserialize, Debug)]
#[serde(untagged)]
pub enum DownloadQuery {
    Track { album: Album, tracks: Vec<Track> },
    Album { album: Album },
}
