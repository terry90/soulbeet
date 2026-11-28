use crate::models::track_share::{ShareDirection, ShareHistoryPage};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use super::{failure_error, server_error};
#[cfg(feature = "server")]
use crate::core::delete_guard::Guard;
#[cfg(feature = "server")]
use crate::core::index::ServerNavidrome;
#[cfg(feature = "server")]
use crate::core::tracks::migrate_legacy::{
    list_legacy_groups, migrate_legacy_folder, MigrateLegacyRequest,
};
#[cfg(feature = "server")]
use crate::core::tracks::share::{
    list_recipients, share_tracks, ShareTracksRequest,
};
#[cfg(feature = "server")]
use crate::core::tracks::TrackSelection;
#[cfg(feature = "server")]
use crate::import_queue::IMPORT_QUEUE;
#[cfg(feature = "server")]
use crate::models::legacy_folder::LegacyFolder;
#[cfg(feature = "server")]
use crate::models::track_share::{LinkMode, ShareOrigin, TrackShareRow};
#[cfg(feature = "server")]
use crate::{AdminSession, AuthSession};

/// Shared client/server wire types for track sharing and legacy migration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareRecipientDto {
    pub user_id: String,
    pub username: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareTracksInput {
    #[serde(default)]
    pub song_ids: Vec<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    pub recipient_user_ids: Vec<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShareResultEntryDto {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub recipient_user_id: String,
    pub recipient_username: String,
    pub status: String,
    pub link_mode: Option<String>,
    pub target_path: Option<String>,
    pub target_song_id: Option<String>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShareTracksResponseDto {
    pub batch_id: String,
    pub dry_run: bool,
    pub shared_count: usize,
    pub already_in_library_count: usize,
    pub failed_count: usize,
    pub scan_status: Option<String>,
    pub entries: Vec<ShareResultEntryDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyGroupRecipientDto {
    pub legacy_id: String,
    pub user_id: String,
    pub username: String,
    pub target_folder_id: String,
    pub target_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyFolderGroupDto {
    pub path: String,
    pub was_shared: bool,
    pub recipients: Vec<LegacyGroupRecipientDto>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MigrateLegacyResponseDto {
    pub legacy_path: String,
    pub dry_run: bool,
    pub total_audio_files: usize,
    pub target_users: Vec<String>,
    pub linked_count: usize,
    pub already_present_count: usize,
    pub failed_count: usize,
    pub removed_source_count: usize,
    pub hardlink_count: usize,
    pub reflink_count: usize,
    pub copy_count: usize,
    pub annotations_restored: usize,
    pub warnings: Vec<String>,
}

#[get("/api/shares/recipients", auth: AuthSession)]
pub async fn get_share_recipients() -> Result<Vec<ShareRecipientDto>, ServerFnError> {
    let list = list_recipients(&auth.0).await.map_err(failure_error)?;
    Ok(list
        .into_iter()
        .map(|r| ShareRecipientDto {
            user_id: r.user_id,
            username: r.username,
        })
        .collect())
}

#[post("/api/shares", auth: AuthSession)]
pub async fn share_tracks_fn(input: ShareTracksInput) -> Result<ShareTracksResponseDto, ServerFnError> {
    let selection = if let Some(album_id) = input.album_id.filter(|s| !s.trim().is_empty()) {
        TrackSelection::Album(album_id)
    } else {
        TrackSelection::Songs(input.song_ids)
    };
    let guard = Guard::live();
    let report = share_tracks(
        &guard,
        &ServerNavidrome,
        &IMPORT_QUEUE,
        &auth.0,
        ShareTracksRequest {
            selection,
            recipient_user_ids: input.recipient_user_ids,
            dry_run: input.dry_run,
            origin: ShareOrigin::Ui,
        },
    )
    .await
    .map_err(failure_error)?;

    let is_admin = auth.0.role.is_admin();
    Ok(ShareTracksResponseDto {
        batch_id: report.batch_id,
        dry_run: report.dry_run,
        shared_count: report.shared_count,
        already_in_library_count: report.already_in_library_count,
        failed_count: report.failed_count,
        scan_status: report.scan_status,
        entries: report
            .entries
            .into_iter()
            .map(|e| ShareResultEntryDto {
                song_id: e.song_id,
                title: e.title,
                artist: e.artist,
                album: e.album,
                recipient_user_id: e.recipient_user_id,
                recipient_username: e.recipient_username,
                status: match e.status {
                    crate::core::tracks::share::ShareItemStatus::Shared => "shared",
                    crate::core::tracks::share::ShareItemStatus::WouldShare => "would_share",
                    crate::core::tracks::share::ShareItemStatus::AlreadyInLibrary => {
                        "already_in_library"
                    }
                    crate::core::tracks::share::ShareItemStatus::Failed => "failed",
                }
                .to_string(),
                link_mode: e.link_mode.map(|m: LinkMode| m.as_str().to_string()),
                target_path: if is_admin { e.target_path } else { None },
                target_song_id: e.target_song_id,
                error: e.failure.map(|f| f.message),
                warnings: e.warnings,
            })
            .collect(),
    })
}

#[get("/api/shares/history", auth: AuthSession)]
pub async fn get_share_history(
    direction: Option<ShareDirection>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<ShareHistoryPage, ServerFnError> {
    let mut page = TrackShareRow::page_for_user(
        &auth.0.user_id,
        direction.unwrap_or(ShareDirection::All),
        limit.unwrap_or(50),
        offset.unwrap_or(0),
    )
    .await
    .map_err(server_error)?;

    if !auth.0.role.is_admin() {
        for entry in &mut page.entries {
            if entry.sender_user_id != auth.0.user_id {
                entry.source_path.clear();
            }
            if entry.recipient_user_id != auth.0.user_id {
                entry.target_path.clear();
            }
        }
    }

    Ok(page)
}

#[get("/api/admin/folders/legacy", _: AdminSession)]
pub async fn get_legacy_folders() -> Result<Vec<LegacyFolderGroupDto>, ServerFnError> {
    let groups = list_legacy_groups().await.map_err(failure_error)?;
    Ok(groups
        .into_iter()
        .map(|g| LegacyFolderGroupDto {
            path: g.path,
            was_shared: g.was_shared,
            recipients: g
                .recipients
                .into_iter()
                .map(|r| LegacyGroupRecipientDto {
                    legacy_id: r.legacy_id,
                    user_id: r.user_id,
                    username: r.username,
                    target_folder_id: r.target_folder_id,
                    target_path: r.target_path,
                })
                .collect(),
        })
        .collect())
}

#[post("/api/admin/folders/legacy/migrate", auth: AdminSession)]
pub async fn migrate_legacy_folder_fn(
    legacy_path: String,
    remove_source_after: bool,
    dry_run: bool,
) -> Result<MigrateLegacyResponseDto, ServerFnError> {
    let guard = Guard::live();
    let report = migrate_legacy_folder(
        &guard,
        &ServerNavidrome,
        &IMPORT_QUEUE,
        &auth.0,
        MigrateLegacyRequest {
            legacy_path,
            remove_source_after,
            dry_run,
        },
    )
    .await
    .map_err(failure_error)?;

    Ok(MigrateLegacyResponseDto {
        legacy_path: report.legacy_path,
        dry_run: report.dry_run,
        total_audio_files: report.total_audio_files,
        target_users: report.target_users,
        linked_count: report.linked_count,
        already_present_count: report.already_present_count,
        failed_count: report.failed_count,
        removed_source_count: report.removed_source_count,
        hardlink_count: report.hardlink_count,
        reflink_count: report.reflink_count,
        copy_count: report.copy_count,
        annotations_restored: report.annotations_restored,
        warnings: report.warnings,
    })
}

#[post("/api/admin/folders/legacy/dismiss", _: AdminSession)]
pub async fn dismiss_legacy_folder(legacy_id: String) -> Result<(), ServerFnError> {
    LegacyFolder::dismiss(&legacy_id)
        .await
        .map_err(server_error)
}
