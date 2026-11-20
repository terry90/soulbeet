pub mod audio;
pub mod beets;
pub mod deezer;
pub mod engine;
pub mod error;
pub mod history;
pub mod http;
pub mod knowledge;
pub mod listenbrainz;
pub mod metadata_error;
pub mod musicbrainz;
pub mod navidrome;
pub mod polite;
pub mod slskd;
#[cfg(test)]
pub(crate) mod test_http;
pub mod traits;

pub use beets::{duplicate_note, DuplicateNote};
pub use metadata_error::MetadataError;
pub use navidrome::{NavidromeClient, NavidromeClientBuilder, NavidromeError};
pub use traits::{
    CandidateGenerator, DownloadBackend, DriverChoice, DuplicateRun, FailKind, ForgetOutcome,
    GeneratorOutput, ImportMode, ImportOutcome, ImportResult, ImportedItem, KnowledgeCache,
    ListeningHistory, MetadataProvider, MusicImporter, MusicKnowledge, Relocation, SkipReason,
    SourceFile, TaskCount, TimeoutKind, WantedRemoval,
};
