use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::DownloadQuery;
use shared::failure::Failure;

#[cfg(feature = "server")]
use crate::core::folders;
#[cfg(feature = "server")]
use crate::AuthSession;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoDownloadRequest {
    pub query: DownloadQuery,
    /// Folder ID used for persisting as default_download_folder_id (per D-01)
    pub folder_id: String,
    /// Resolved folder path for the actual download target directory
    pub folder_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AutoDownloadResult {
    /// Pipeline spawned, all further results delivered via WebSocket AutoDownloadEvent
    Accepted { batch_id: String },
    /// Error before pipeline could start (no backends, bad input, etc.)
    Error(Failure),
}

#[post("/api/auto-download", auth: AuthSession)]
pub async fn auto_download(req: AutoDownloadRequest) -> Result<AutoDownloadResult, ServerFnError> {
    let folder = match folders::owned(&auth.0, &req.folder_id).await {
        Ok(f) if std::path::Path::new(&req.folder_path) == std::path::Path::new(&f.path) => f,
        Ok(_) => return Ok(AutoDownloadResult::Error(folders::folder_not_found())),
        Err(failure) => return Ok(AutoDownloadResult::Error(failure)),
    };
    Ok(
        match crate::core::acquire::auto_download(&auth.0, folder, req.query).await {
            Ok(batch_id) => AutoDownloadResult::Accepted { batch_id },
            Err(failure) => AutoDownloadResult::Error(failure),
        },
    )
}
