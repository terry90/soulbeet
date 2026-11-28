use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::DownloadQuery;
use shared::failure::Failure;
use shared::metadata::Album;

#[cfg(feature = "server")]
use crate::core::folders;
#[cfg(feature = "server")]
use crate::AuthSession;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoDownloadRequest {
    pub query: DownloadQuery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadCatalogAlbumRequest {
    pub album: Album,
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
    let folder = match folders::for_user(&auth.0.user_id).await {
        Ok(f) => f,
        Err(failure) => return Ok(AutoDownloadResult::Error(failure)),
    };
    Ok(
        match crate::core::acquire::auto_download(&auth.0, folder, req.query).await {
            Ok(batch_id) => AutoDownloadResult::Accepted { batch_id },
            Err(failure) => AutoDownloadResult::Error(failure),
        },
    )
}

#[post("/api/download/album", auth: AuthSession)]
pub async fn download_catalog_album(
    req: DownloadCatalogAlbumRequest,
) -> Result<AutoDownloadResult, ServerFnError> {
    let folder = match folders::for_user(&auth.0.user_id).await {
        Ok(f) => f,
        Err(failure) => return Ok(AutoDownloadResult::Error(failure)),
    };
    let query = DownloadQuery::new(vec![]).album(req.album);
    Ok(
        match crate::core::acquire::auto_download(&auth.0, folder, query).await {
            Ok(batch_id) => AutoDownloadResult::Accepted { batch_id },
            Err(failure) => AutoDownloadResult::Error(failure),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_catalog_album_request_deserializes() {
        let req: DownloadCatalogAlbumRequest = serde_json::from_value(serde_json::json!({
            "album": {
                "id": "deezer:album:1408080",
                "title": "Discovery",
                "artist": "Daft Punk",
                "release_date": "2001-03-12"
            }
        }))
        .unwrap();
        assert_eq!(req.album.id, "deezer:album:1408080");
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn album_download_folder_resolves_user_single_folder() {
        use shared::failure::{FailureCode, Stage};

        let db = crate::db::test_db::migrated().await;

        // No folder configured -> FolderRequired error
        let err = folders::for_user_in(&db, "alice").await.unwrap_err();
        assert_eq!(
            (err.code, err.stage),
            (FailureCode::FolderRequired, Stage::Input)
        );

        let alice_f1 = crate::core::folder_of(&db, "alice").await;
        let resolved = folders::for_user_in(&db, "alice").await.unwrap();
        assert_eq!(resolved.id, alice_f1);
    }
}
