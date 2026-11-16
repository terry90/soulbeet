pub mod audio;
pub mod beets;
pub mod engine;
pub mod error;
pub mod http;
pub mod lastfm;
pub mod listenbrainz;
pub mod metadata_error;
pub mod musicbrainz;
pub mod navidrome;
pub mod services;
pub mod slskd;
#[cfg(test)]
pub(crate) mod test_http;
pub mod traits;

pub use beets::{duplicate_note, DuplicateNote};
pub use lastfm::LastFmProvider;
pub use listenbrainz::ListenBrainzProvider;
pub use metadata_error::MetadataError;
pub use navidrome::{NavidromeClient, NavidromeClientBuilder, NavidromeError};
pub use services::{Services, ServicesBuilder};
pub use traits::{
    CandidateGenerator, DownloadBackend, DriverChoice, DuplicateRun, FailKind, ForgetOutcome,
    ImportMode, ImportOutcome, ImportResult, ImportedItem, MetadataProvider, MusicImporter,
    Relocation, ScrobbleProvider, SkipReason, SourceFile, TaskCount, TimeoutKind, WantedRemoval,
};
