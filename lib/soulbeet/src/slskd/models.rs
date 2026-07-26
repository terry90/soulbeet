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
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchResponse {
    pub username: String,
    pub files: Vec<SearchResponseFile>,
    pub has_free_upload_slot: bool,
    pub upload_speed: i32,
    pub queue_length: i32,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DownloadRequestFile {
    pub filename: String,
    pub size: i64,
}

// Wire types for POST /api/v0/transfers/downloads/batches (slskd 0.26+).
// `id`/`searchId` are deliberately absent from the request: slskd generates
// the batch id, so a Soulful-side retry can never 409 on a duplicate.
#[derive(Debug, Serialize)]
pub(crate) struct BatchEnqueueRequest<'a> {
    pub username: &'a str,
    pub files: &'a [DownloadRequestFile],
    pub options: BatchEnqueueOptions,
}

// slskd NREs into a 500 when it receives `"options": null`, so the field is
// always an object; only `destination` may be omitted.
#[derive(Debug, Serialize)]
pub(crate) struct BatchEnqueueOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
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
    pub filename: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BatchEnqueueFailure {
    pub filename: String,
    pub message: String,
}
