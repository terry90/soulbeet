pub mod airdcpp;
pub mod archive;
pub mod audio;
pub mod backend_error;
pub mod beets;
pub mod catalog_rank;
pub mod deezer;
pub mod engine;
pub mod error;
pub mod gdstudio;
pub mod history;
pub mod http;
pub mod knowledge;
pub mod listenbrainz;
pub mod matching;
pub mod metadata_error;
pub mod musicbrainz;
pub mod navidrome;
pub mod partial;
pub mod playlist_import;
pub mod polite;
pub mod process;
pub mod slskd;
pub mod tags;
#[cfg(test)]
pub(crate) mod test_http;
pub mod traits;
pub mod transfer_book;
pub mod verify;
pub mod ytdlp;

pub use backend_error::{BackendError, BackendFault, BackendResult, PeerAsk};
pub use beets::{duplicate_note, DuplicateNote};
pub use metadata_error::MetadataError;
pub use navidrome::{NavidromeClient, NavidromeClientBuilder, NavidromeError};
pub use traits::{
    BeetsTrackInfo, CandidateGenerator, DownloadBackend, DriverChoice, DuplicateRun, FailKind,
    ForgetOutcome, GeneratorOutput, ImportMode, ImportOutcome, ImportPlacement, ImportResult,
    ImportedItem, KnowledgeCache, ListeningHistory, MetadataProvider, MusicImporter,
    MusicKnowledge, PruneOutcome, RegisterOutcome, Relocation, SkipReason, SourceFile, TagEdits,
    TaskCount, TimeoutKind, WantedRemoval,
};
pub use transfer_book::{BookEntry, BookError, TransferBook};
pub use verify::{audio_duration_secs, verify_downloaded_audio, VerifiedAudio, VerifyPolicy};
