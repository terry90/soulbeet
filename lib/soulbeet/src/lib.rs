pub mod beets;
pub mod engine;
pub mod error;
pub mod http;
pub mod lastfm;
pub mod listenbrainz;
pub mod musicbrainz;
pub mod navidrome;
pub mod services;
pub mod slskd;
pub mod traits;

pub use lastfm::LastFmProvider;
pub use listenbrainz::ListenBrainzProvider;
pub use navidrome::{discovery_path_tail, NavidromeClient, NavidromeClientBuilder};
pub use services::{Services, ServicesBuilder};
pub use traits::{
    CandidateGenerator, DownloadBackend, FallbackMetadataProvider, ImportResult, MetadataProvider,
    MusicImporter, ScrobbleProvider,
};
