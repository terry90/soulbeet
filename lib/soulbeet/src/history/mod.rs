pub mod listenbrainz;
pub mod navidrome;

use thiserror::Error;

use crate::navidrome::NavidromeError;
use crate::polite::RemoteError;

#[derive(Error, Debug)]
pub enum HistoryError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Navidrome(#[from] NavidromeError),
    #[error("Navidrome counts {total} songs, and its pages held {read}")]
    Incomplete { read: usize, total: u64 },
}

impl HistoryError {
    /// The source name reports use for this failure.
    pub fn service(&self) -> &'static str {
        match self {
            Self::Remote(e) => e.service(),
            Self::Navidrome(_) | Self::Incomplete { .. } => "Navidrome",
        }
    }
}
