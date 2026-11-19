use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// Envelopes the client unwraps before handing results out.

#[derive(Debug, Deserialize)]
pub(crate) struct Payload<T> {
    pub payload: T,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ArtistsPayload<T> {
    pub artists: Vec<T>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RecordingsPayload {
    pub recordings: Vec<LbRecording>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListensPayload {
    pub listens: Vec<LbListen>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FeedbackPage {
    pub feedback: Vec<LbFeedback>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ArtistMbids<'a> {
    pub artist_mbids: &'a [String],
}

/// Field order is the order ListenBrainz documents the body in.
#[derive(Debug, Serialize)]
pub(crate) struct RecordingMetadataQuery<'a> {
    pub recording_mbids: &'a [String],
    pub inc: &'static str,
}

// --- User statistics, listens and feedback ---

#[derive(Debug, Clone, Deserialize)]
pub struct LbArtist {
    pub artist_name: String,
    pub artist_mbid: Option<String>,
    pub listen_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LbRecording {
    pub track_name: String,
    pub artist_name: String,
    pub recording_mbid: Option<String>,
    pub artist_mbids: Vec<String>,
    pub listen_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LbListen {
    pub listened_at: i64,
    pub track_metadata: TrackMetadata,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrackMetadata {
    pub artist_name: String,
    pub track_name: String,
    pub release_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LbFeedback {
    pub recording_mbid: Option<String>,
    pub track_metadata: Option<FeedbackTrack>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedbackTrack {
    pub artist_name: String,
    pub track_name: String,
    pub mbid_mapping: Option<MbidMapping>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MbidMapping {
    pub artist_mbids: Vec<String>,
    pub recording_mbid: Option<String>,
}

// --- LB Radio ---

/// Recordings keyed by the artist MBID they belong to.
pub type ArtistRadioResponse = HashMap<String, Vec<ArtistRadioRecording>>;

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistRadioRecording {
    pub recording_mbid: String,
    pub total_listen_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TagRadioRecording {
    pub recording_mbid: String,
    pub percent: f64,
}

// --- Popularity and sitewide statistics ---

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistPopularityEntry {
    pub artist_mbid: String,
    pub total_user_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SitewideArtist {
    #[serde(default)]
    pub artist_mbid: Option<String>,
    pub artist_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TopRecordingForArtist {
    pub artist_name: String,
    pub artist_mbids: Vec<String>,
    pub recording_mbid: String,
    pub recording_name: String,
    pub total_listen_count: u64,
}

// --- Metadata ---

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistMetadataEntry {
    pub artist_mbid: String,
    pub tag: Option<ArtistTags>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistTags {
    pub artist: Vec<ArtistTagEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtistTagEntry {
    pub tag: String,
    pub count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordingMetadata {
    pub artist: RecordingArtist,
    pub recording: RecordingFields,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordingArtist {
    pub name: String,
    pub artists: Vec<CreditArtist>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreditArtist {
    pub artist_mbid: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordingFields {
    pub name: String,
    #[serde(default)]
    pub first_release_date: Option<String>,
}

// --- ListenBrainz Labs ---

#[derive(Debug, Clone, Deserialize)]
pub struct LabsNeighbour {
    pub artist_mbid: String,
    #[serde(default)]
    pub name: Option<String>,
    pub score: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimilarTag {
    pub similar_tag: String,
    pub count: u64,
}
