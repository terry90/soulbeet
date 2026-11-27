use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use shared::download::DownloadQuery;
use shared::failure::Failure;
use shared::metadata::Album;

#[cfg(feature = "server")]
use crate::auth::UserCtx;
#[cfg(feature = "server")]
use crate::core::{database_failure, folders};
#[cfg(feature = "server")]
use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::AuthSession;
#[cfg(feature = "server")]
use shared::failure::{FailureCode, RetryAdvice, Stage};
#[cfg(feature = "server")]
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoDownloadRequest {
    pub query: DownloadQuery,
    /// Folder ID used for persisting as default_download_folder_id (per D-01)
    pub folder_id: String,
    /// Resolved folder path for the actual download target directory
    pub folder_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadCatalogAlbumRequest {
    pub album: Album,
    #[serde(default)]
    pub folder_id: Option<String>,
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

#[cfg(feature = "server")]
async fn resolve_album_download_folder_in(
    db: &SqlitePool,
    ctx: &UserCtx,
    requested_folder_id: Option<&str>,
) -> Result<Folder, Failure> {
    if let Some(folder_id) = requested_folder_id {
        return folders::owned_in(db, ctx, folder_id).await;
    }

    let settings = UserSettings::get_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))?;
    if let Some(default_id) = settings
        .default_download_folder_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    {
        if let Ok(folder) = folders::owned_in(db, ctx, default_id).await {
            return Ok(folder);
        }
    }

    let accessible = folders::list_in(db, ctx).await?;
    accessible.into_iter().next().ok_or_else(|| {
        Failure::new(
            FailureCode::InvalidInput,
            Stage::Input,
            RetryAdvice::NeedsUser,
            "No download folder configured. Add a library folder in Settings before downloading.",
        )
    })
}

#[post("/api/download/album", auth: AuthSession)]
pub async fn download_catalog_album(
    req: DownloadCatalogAlbumRequest,
) -> Result<AutoDownloadResult, ServerFnError> {
    let folder =
        match resolve_album_download_folder_in(&crate::db::DB, &auth.0, req.folder_id.as_deref())
            .await
        {
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
    fn download_catalog_album_request_deserializes_with_and_without_folder_id() {
        let without_folder: DownloadCatalogAlbumRequest = serde_json::from_value(serde_json::json!({
            "album": {
                "id": "deezer:album:1408080",
                "title": "Discovery",
                "artist": "Daft Punk",
                "release_date": "2001-03-12"
            }
        }))
        .unwrap();
        assert_eq!(without_folder.album.id, "deezer:album:1408080");
        assert_eq!(without_folder.folder_id, None);

        let with_folder: DownloadCatalogAlbumRequest = serde_json::from_value(serde_json::json!({
            "album": {
                "id": "deezer:album:1408080",
                "title": "Discovery",
                "artist": "Daft Punk",
                "release_date": null
            },
            "folder_id": "folder-1"
        }))
        .unwrap();
        assert_eq!(with_folder.folder_id.as_deref(), Some("folder-1"));
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn album_download_folder_resolution_checks_access_default_and_fallback() {
        use crate::core::test_ctx;
        use crate::models::user_settings::UpdateUserSettings;

        let db = crate::db::test_db::migrated().await;
        let alice = test_ctx("alice");
        let bob = test_ctx("bob");

        // No folders configured -> InvalidInput error
        let err = resolve_album_download_folder_in(&db, &alice, None)
            .await
            .unwrap_err();
        assert_eq!((err.code, err.stage), (FailureCode::InvalidInput, Stage::Input));

        let alice_f1 = crate::core::folder_of(&db, "alice").await;
        let alice_f2 = "alice-folder-2".to_string();
        sqlx::query("INSERT INTO folders (id, user_id, name, path) VALUES (?, ?, ?, ?)")
            .bind(&alice_f2)
            .bind("alice")
            .bind("second")
            .bind("/music/alice2")
            .execute(&db)
            .await
            .unwrap();
        let bob_f1 = crate::core::folder_of(&db, "bob").await;

        // Explicit folder_id validates ownership
        assert_eq!(
            resolve_album_download_folder_in(&db, &alice, Some(&alice_f2))
                .await
                .unwrap()
                .id,
            alice_f2
        );
        assert_eq!(
            resolve_album_download_folder_in(&db, &alice, Some(&bob_f1))
                .await
                .unwrap_err(),
            folders::folder_not_found()
        );

        // No default set -> falls back to first accessible folder
        let first = resolve_album_download_folder_in(&db, &alice, None)
            .await
            .unwrap();
        assert_eq!(first.id, alice_f1);

        // Default folder set -> uses default folder
        UserSettings::upsert_in(
            &db,
            "alice",
            UpdateUserSettings {
                default_download_folder_id: Some(alice_f2.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            resolve_album_download_folder_in(&db, &alice, None)
                .await
                .unwrap()
                .id,
            alice_f2
        );

        // Default folder inaccessible/deleted -> falls back to first accessible folder
        UserSettings::upsert_in(
            &db,
            "alice",
            UpdateUserSettings {
                default_download_folder_id: Some(bob_f1.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            resolve_album_download_folder_in(&db, &alice, None)
                .await
                .unwrap()
                .id,
            alice_f1
        );

        let _ = bob;
    }
}
