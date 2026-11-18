use serde::{Deserialize, Serialize};

// Internal structs for deserializing raw API responses
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchResponseFile {
    pub filename: String,
    pub size: i64,
    pub bit_rate: Option<i32>,
    pub length: Option<i32>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    /// What the peer's client wrote (`File.cs:40-46`), often empty.
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub is_variable_bit_rate: Option<bool>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchResponse {
    pub username: String,
    pub files: Vec<SearchResponseFile>,
    pub has_free_upload_slot: bool,
    pub upload_speed: i32,
    /// A `long` (`Search/Types/Response.cs:45`): a peer may report more than
    /// 32 bits hold, and one such peer would fail the whole response list.
    pub queue_length: i64,
    /// Files the peer shares only with its buddies, listed apart (`Response.cs:43`).
    #[serde(default)]
    pub locked_file_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DownloadRequestFile {
    pub filename: String,
    pub size: i64,
}

// Wire types for POST /api/v0/transfers/downloads/batches (slskd 0.26+).
// `id` is generated here, not by slskd, so a retry after a lost answer sends
// the same id and gets 409 instead of enqueueing a second batch.
#[derive(Debug, Serialize)]
pub(crate) struct BatchEnqueueRequest<'a> {
    pub id: &'a str,
    pub username: &'a str,
    pub files: &'a [DownloadRequestFile],
    pub options: BatchEnqueueOptions<'a>,
}

// slskd NREs into a 500 when it receives `"options": null`, and rejects an
// empty destination.
#[derive(Debug, Serialize)]
pub(crate) struct BatchEnqueueOptions<'a> {
    pub destination: &'a str,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BatchEnqueueResponse {
    pub batch: EnqueuedBatch,
    #[serde(default)]
    pub failures: Vec<BatchEnqueueFailure>,
}

// slskd omits null fields (WhenWritingNull), so `transfers` can be absent.
#[derive(Debug, Deserialize)]
pub(crate) struct EnqueuedBatch {
    #[serde(default)]
    pub transfers: Vec<EnqueuedTransfer>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EnqueuedTransfer {
    pub id: String,
    pub filename: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BatchEnqueueFailure {
    pub filename: String,
    pub message: String,
}
