use serde::{Deserialize, Serialize};

// Internal structs for deserializing raw API responses
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchResponseFile {
    pub filename: String,
    pub size: i64,
    pub bit_rate: Option<i32>,
    pub length: Option<i32>,
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
