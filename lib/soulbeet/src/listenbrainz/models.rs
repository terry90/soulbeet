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

#[derive(Debug, Serialize)]
pub(crate) struct RecordingMbids<'a> {
    pub recording_mbids: &'a [String],
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecordingPopularityEntry {
    pub recording_mbid: String,
    #[serde(default)]
    pub total_listen_count: Option<u64>,
    #[serde(default)]
    pub total_user_count: Option<u64>,
}

/// Field order is the order ListenBrainz documents the body in.
#[derive(Debug, Serialize)]
pub(crate) struct RecordingMetadataQuery<'a> {
    pub recording_mbids: &'a [String],
    pub inc: &'static str,
}

/// `/1/validate-token`'s answer; `code`, `message` and `user_name` are not read.
#[derive(Debug, Deserialize)]
pub(crate) struct TokenValidation {
    pub valid: bool,
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

// --- Similar users and recommendation playlists ---

#[derive(Debug, Clone, Deserialize)]
pub struct SimilarUser {
    pub user_name: String,
    pub similarity: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistSummary {
    pub mbid: String,
    /// The Troi patch that made it, such as `weekly-exploration`.
    pub source_patch: Option<String>,
    /// RFC 3339, as ListenBrainz sends it.
    pub date: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistTrack {
    pub title: String,
    pub creator: String,
    pub recording_mbid: Option<String>,
    pub artist_mbid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSearchHit {
    pub mbid: String,
    pub title: String,
    pub creator: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PlaylistListing {
    pub playlists: Vec<PlaylistEnvelope<PlaylistHeader>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PlaylistEnvelope<T> {
    pub playlist: T,
}

fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// `identifier` is the playlist's URL, its MBID the last segment.
#[derive(Debug, Deserialize)]
pub(crate) struct PlaylistHeader {
    pub identifier: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub title: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub creator: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub date: String,
    #[serde(default)]
    extension: Option<PlaylistHeaderExtension>,
}

impl PlaylistHeader {
    pub fn source_patch(&self) -> Option<&str> {
        self.extension
            .as_ref()?
            .metadata
            .as_ref()?
            .additional_metadata
            .as_ref()?
            .algorithm_metadata
            .as_ref()?
            .source_patch
            .as_deref()
    }
}

#[derive(Debug, Deserialize)]
struct PlaylistHeaderExtension {
    #[serde(rename = "https://musicbrainz.org/doc/jspf#playlist")]
    metadata: Option<PlaylistMetadata>,
}

#[derive(Debug, Deserialize)]
struct PlaylistMetadata {
    additional_metadata: Option<AdditionalMetadata>,
}

#[derive(Debug, Deserialize)]
struct AdditionalMetadata {
    algorithm_metadata: Option<AlgorithmMetadata>,
}

#[derive(Debug, Deserialize)]
struct AlgorithmMetadata {
    source_patch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JspfPlaylist {
    #[serde(default, deserialize_with = "null_as_default")]
    pub track: Vec<JspfTrack>,
}

/// A JSPF track: `identifier` lists recording URLs, the extension artist URLs.
/// ListenBrainz leaves out the names of a recording it has no metadata for, and arbitrary
/// community playlists may omit `identifier`, `extension`, or the MusicBrainz track block.
#[derive(Debug, Deserialize)]
pub(crate) struct JspfTrack {
    pub title: Option<String>,
    pub creator: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub identifier: Vec<String>,
    #[serde(default)]
    pub extension: Option<JspfTrackExtension>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JspfTrackExtension {
    #[serde(default, rename = "https://musicbrainz.org/doc/jspf#track")]
    pub musicbrainz: Option<MusicBrainzTrackExtension>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MusicBrainzTrackExtension {
    #[serde(default, deserialize_with = "null_as_default")]
    pub artist_identifiers: Vec<String>,
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
    #[serde(default)]
    pub area: Option<String>,
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
