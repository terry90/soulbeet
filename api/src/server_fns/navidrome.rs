use dioxus::prelude::*;
use shared::navidrome::{
    BlockedDeletion, DeletionReview, LibraryStats, SyncResult, UnreadableListener,
};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};

#[cfg(feature = "server")]
use super::failure_error;
#[cfg(feature = "server")]
use crate::auth::UserId;
#[cfg(feature = "server")]
use crate::core::delete_guard::staging::{remove_staged_in, StagedOutcome};
#[cfg(feature = "server")]
use crate::core::delete_guard::{ForgetVia, Guard, Origin, Verdict};
#[cfg(feature = "server")]
use crate::core::folders::folder_not_found;
#[cfg(feature = "server")]
use crate::core::{database_failure, logged};
#[cfg(feature = "server")]
use crate::import_queue::{ImportQueue, IMPORT_QUEUE};
#[cfg(feature = "server")]
use crate::models::deletion_block::DeletionBlockRow;
#[cfg(feature = "server")]
use crate::models::deletion_review::DeletionReviewRow;
#[cfg(feature = "server")]
use crate::models::discovery_playlist::DiscoveryTrackRow;
#[cfg(feature = "server")]
use crate::models::folder::Folder;
#[cfg(feature = "server")]
use crate::models::navidrome_read_failure::NavidromeReadFailureRow;
#[cfg(feature = "server")]
use crate::models::user_settings::UserSettings;
#[cfg(feature = "server")]
use crate::services::navidrome_client_for_user;
#[cfg(feature = "server")]
use crate::{AdminSession, AuthSession};
#[cfg(feature = "server")]
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
#[cfg(feature = "server")]
use shared::navidrome::{DeletionReason, DiscoveryStatus, DiscoveryTrack};
#[cfg(feature = "server")]
use soulbeet::navidrome::models::SubsonicSong;
#[cfg(feature = "server")]
use std::path::Path;

#[cfg(feature = "server")]
use super::server_error;

#[post("/api/navidrome/sync-ratings", auth: AuthSession)]
pub async fn sync_ratings() -> Result<SyncResult, ServerFnError> {
    sync_ratings_internal(&auth.0.user_id)
        .await
        .map_err(server_error)
}

#[cfg(feature = "server")]
pub async fn sync_ratings_internal(user_id: &str) -> Result<SyncResult, String> {
    let client = navidrome_client_for_user(user_id).await?;
    let songs = client
        .get_all_songs_with_ratings()
        .await
        .map_err(|e| e.to_string())?;

    let total_songs_scanned = songs.len() as u32;

    let user_settings = UserSettings::get(user_id).await?;
    let promote_threshold = user_settings.discovery_promote_threshold;

    let pending = if let Some(ref fid) = user_settings.discovery_folder_id {
        DiscoveryTrackRow::get_pending_by_folder(fid).await?
    } else {
        vec![]
    };

    let mut removed_tracks =
        remove_disliked_pending_in(&crate::db::DB, &songs, &pending, promote_threshold).await;
    let mut promoted_tracks = 0u32;
    for song in &songs {
        if let Some(PendingAction::Promote(track)) =
            pending_action(song, &pending, promote_threshold)
        {
            if let Err(e) = promote_discovery_track_internal(&track.id, user_id).await {
                warn!("Failed to promote track {}: {}", track.title, e.message);
            } else {
                promoted_tracks += 1;
            }
        }
    }

    let (mut deleted_tracks, mut kept_tracks) = (0u32, 0u32);
    if user_settings.auto_delete_enabled {
        let run = auto_delete_songs(
            &Guard::live(),
            &UserId::new(user_id),
            &songs,
            &pending,
            &IMPORT_QUEUE,
        )
        .await;
        let mut by_code = std::collections::BTreeMap::<&str, u32>::new();
        for kept in &run.kept {
            let code = kept.failure.code.as_str();
            *by_code.entry(code).or_default() += 1;
            let (artist, title, text) = (&kept.artist, &kept.title, logged(&kept.failure));
            if kept.failure.cause.is_some() {
                warn!("Auto-delete kept {artist} - {title}: {code}: {text}");
            } else {
                info!("Auto-delete kept {artist} - {title}: {code}: {text}");
            }
        }
        for (code, n) in by_code {
            info!("Auto-delete kept {n}: {code}");
        }
        DeletionBlockRow::replace_in(&crate::db::DB, user_id, &run.kept).await?;
        deleted_tracks = run.deleted;
        removed_tracks += run.removed;
        kept_tracks = run.kept.len() as u32;
    } else {
        info!("Auto-delete is disabled for this user");
    }
    info!(
        "Ratings sync complete: {} songs scanned, {} deleted, {} promoted, {} removed, {} kept",
        total_songs_scanned, deleted_tracks, promoted_tracks, removed_tracks, kept_tracks
    );

    Ok(SyncResult {
        deleted_tracks,
        promoted_tracks,
        removed_tracks,
        total_songs_scanned,
        kept_tracks,
    })
}

/// The pending discovery track `song` is: the track's song id when it has one, else the file
/// name of the song's Subsonic path, ignoring ASCII case.
#[cfg(feature = "server")]
pub(crate) fn pending_track_for<'a>(
    song: &SubsonicSong,
    pending: &'a [DiscoveryTrack],
) -> Option<&'a DiscoveryTrack> {
    let file_name = |path: &str| Path::new(path).file_name().map(|f| f.to_ascii_lowercase());
    pending.iter().find(|track| match &track.song_id {
        Some(song_id) => *song_id == song.id,
        None => song
            .path
            .as_deref()
            .and_then(file_name)
            .is_some_and(|name| Some(name) == file_name(&track.path)),
    })
}

/// What the rating sync does with a pending discovery track the user rated.
#[cfg(feature = "server")]
pub(crate) enum PendingAction<'a> {
    Promote(&'a DiscoveryTrack),
    Remove(&'a DiscoveryTrack),
}

/// Promote the pending track of `song` at `promote_threshold` or above, else remove it at
/// rating 1.
#[cfg(feature = "server")]
pub(crate) fn pending_action<'a>(
    song: &SubsonicSong,
    pending: &'a [DiscoveryTrack],
    promote_threshold: u8,
) -> Option<PendingAction<'a>> {
    let rating = song.user_rating?;
    let track = pending_track_for(song, pending)?;
    if rating >= promote_threshold {
        Some(PendingAction::Promote(track))
    } else if rating == 1 {
        Some(PendingAction::Remove(track))
    } else {
        None
    }
}

/// Discovery's own cleanup (D12): removes the pending track of each song of `songs` that
/// pending_action removes, through remove_staged_in with its folder, with no Navidrome call.
/// The count removed.
#[cfg(feature = "server")]
pub(crate) async fn remove_disliked_pending_in(
    db: &sqlx::SqlitePool,
    songs: &[SubsonicSong],
    pending: &[DiscoveryTrack],
    promote_threshold: u8,
) -> u32 {
    let mut removed = 0;
    for song in songs {
        let Some(PendingAction::Remove(track)) = pending_action(song, pending, promote_threshold)
        else {
            continue;
        };
        let result = match Folder::get_by_id_in(db, &track.folder_id).await {
            Ok(Some(folder)) => remove_staged_in(db, track, &folder, StagedOutcome::Removed).await,
            Ok(None) => Err(folder_not_found()),
            Err(e) => Err(database_failure(Stage::Delete, e)),
        };
        match result {
            Ok(()) => removed += 1,
            Err(f) => warn!("Failed to remove track {}: {}", track.title, logged(&f)),
        }
    }
    removed
}

/// A 1-star song auto-delete kept, and the refusal that kept it.
#[cfg(feature = "server")]
pub(crate) struct KeptTrack {
    pub song_id: String,
    pub title: String,
    pub artist: String,
    pub failure: Failure,
}

/// What one auto-delete run did.
#[cfg(feature = "server")]
pub(crate) struct AutoDeleteRun {
    /// Library files deleted.
    pub deleted: u32,
    /// Pending discovery tracks the guard found and removed.
    pub removed: u32,
    pub kept: Vec<KeptTrack>,
}

/// Deletes through the guard every song of `songs` that `actor` rated 1 and pending_track_for
/// does not match in `pending`: a library file is carried out, a pending discovery track the
/// guard finds is removed as a staged track. Only a song the fresh record still shows at 1
/// star is removed; one rated otherwise since `songs` was listed is skipped. A failure of any
/// step is kept with its song.
#[cfg(feature = "server")]
pub(crate) async fn auto_delete_songs(
    guard: &Guard<'_>,
    actor: &UserId,
    songs: &[SubsonicSong],
    pending: &[DiscoveryTrack],
    queue: &ImportQueue,
) -> AutoDeleteRun {
    let mut run = AutoDeleteRun {
        deleted: 0,
        removed: 0,
        kept: Vec::new(),
    };
    for song in songs {
        if song.user_rating != Some(1) || pending_track_for(song, pending).is_some() {
            continue;
        }
        let artist = song.artist.as_deref().unwrap_or_default();
        let outcome = match guard.check_song(actor, &song.id).await {
            Ok(Verdict::Library(checked)) if checked.song.rating == 1 => {
                let forget = ForgetVia::Queue(queue.clone());
                guard
                    .carry_out(
                        actor,
                        checked,
                        DeletionReason::Disliked,
                        Origin::Auto,
                        forget,
                    )
                    .await
                    .map(|warnings| {
                        for warning in warnings {
                            warn!("{warning}");
                        }
                        run.deleted += 1;
                    })
            }
            Ok(Verdict::Discovery { track, rating: 1 }) => {
                match Folder::get_by_id_in(guard.db, &track.folder_id).await {
                    Ok(Some(folder)) => {
                        remove_staged_in(guard.db, &track, &folder, StagedOutcome::Removed)
                            .await
                            .map(|()| run.removed += 1)
                    }
                    Ok(None) => Err(folder_not_found()),
                    Err(e) => Err(database_failure(Stage::Delete, e)),
                }
            }
            Ok(_) => {
                info!(
                    "Auto-delete skipped {artist} - {}: no longer rated 1",
                    song.title
                );
                Ok(())
            }
            Err(f) => Err(f),
        };
        if let Err(failure) = outcome {
            run.kept.push(KeptTrack {
                song_id: song.id.clone(),
                title: song.title.clone(),
                artist: artist.to_string(),
                failure,
            });
        }
    }
    run
}

#[get("/api/navidrome/deletion-history", auth: AuthSession)]
pub async fn get_deletion_history() -> Result<Vec<DeletionReview>, ServerFnError> {
    DeletionReviewRow::history(&auth.0.user_id, 50)
        .await
        .map_err(server_error)
}

/// The tracks auto-delete kept at the caller's last rating sync, and why.
#[get("/api/navidrome/deletion-blocks", auth: AuthSession)]
pub async fn get_deletion_blocks() -> Result<Vec<BlockedDeletion>, ServerFnError> {
    DeletionBlockRow::for_user_in(&crate::db::DB, &auth.0.user_id)
        .await
        .map_err(|e| failure_error(database_failure(Stage::Delete, e)))
}

/// The accounts the delete guard cannot read, which keeps every file they might have rated.
#[get("/api/navidrome/unreadable-listeners", _: AdminSession)]
pub async fn get_unreadable_listeners() -> Result<Vec<UnreadableListener>, ServerFnError> {
    NavidromeReadFailureRow::all_in(&crate::db::DB)
        .await
        .map_err(|e| failure_error(database_failure(Stage::Delete, e)))
}

/// Check if the Soulbeet player in Navidrome has ReportRealPath enabled.
/// Returns Some(true) if enabled, Some(false) if disabled, None if player not found yet.
#[cfg(feature = "server")]
pub async fn check_report_real_path(user_id: &str) -> Result<Option<bool>, String> {
    let client = navidrome_client_for_user(user_id).await?;
    let players = client.get_players().await.map_err(|e| e.to_string())?;
    let soulbeet = players.iter().find(|p| p.client == "Soulbeet");
    Ok(soulbeet.map(|p| p.report_real_path))
}

#[get("/api/navidrome/library-stats", auth: AuthSession)]
pub async fn get_library_stats() -> Result<LibraryStats, ServerFnError> {
    let client = navidrome_client_for_user(&auth.0.user_id)
        .await
        .map_err(server_error)?;
    let songs = client
        .get_all_songs_with_ratings()
        .await
        .map_err(server_error)?;

    let albums = client.get_all_albums().await.map_err(server_error)?;

    let total_tracks = songs.len() as u32;
    let mut rated_tracks = 0u32;
    let mut rating_sum = 0.0f64;
    let mut rating_distribution = [0u32; 5];
    let mut genres: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut artists: std::collections::HashSet<String> = std::collections::HashSet::new();

    for song in &songs {
        if let Some(artist) = &song.artist {
            artists.insert(artist.to_lowercase());
        }
        if let Some(genre) = &song.genre {
            if !genre.is_empty() {
                *genres.entry(genre.clone()).or_default() += 1;
            }
        }
        if let Some(rating) = song.user_rating {
            if (1..=5).contains(&rating) {
                rated_tracks += 1;
                rating_sum += rating as f64;
                rating_distribution[(rating - 1) as usize] += 1;
            }
        }
    }

    let average_rating = if rated_tracks > 0 {
        rating_sum / rated_tracks as f64
    } else {
        0.0
    };

    let mut genre_vec: Vec<(String, u32)> = genres.into_iter().collect();
    genre_vec.sort_by(|a, b| b.1.cmp(&a.1));
    genre_vec.truncate(20);

    Ok(LibraryStats {
        total_tracks,
        rated_tracks,
        unrated_tracks: total_tracks - rated_tracks,
        average_rating,
        rating_distribution,
        total_albums: albums.len() as u32,
        total_artists: artists.len() as u32,
        genres: genre_vec,
    })
}

/// Promotes in its own task, so the status and history updates always follow
/// the import even when the request that started it goes away while the
/// import waits in the library's queue.
#[cfg(feature = "server")]
pub(crate) async fn promote_discovery_track_internal(
    track_id: &str,
    user_id: &str,
) -> Result<(), Failure> {
    let (track_id, user_id) = (track_id.to_string(), user_id.to_string());
    tokio::spawn(async move { promote_detached(&track_id, &user_id).await })
        .await
        .unwrap_or_else(|e| Err(promote_stopped(&e)))
}

#[cfg(feature = "server")]
fn discovery_track_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Discovery track not found",
    )
}

#[cfg(feature = "server")]
fn not_your_track() -> Failure {
    Failure::new(
        FailureCode::NotOwner,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Not authorized to modify this track",
    )
}

#[cfg(feature = "server")]
fn promote_not_pending() -> Failure {
    Failure::new(
        FailureCode::ExpiredHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Track is no longer pending (already promoted, removed, or being promoted)",
    )
}

#[cfg(feature = "server")]
fn promote_file_gone() -> Failure {
    Failure::new(
        FailureCode::ImportFilesGone,
        Stage::Import,
        RetryAdvice::NotRetryable,
        "The downloaded file is gone",
    )
}

/// A promote task that panicked, coded like a panicked import job.
#[cfg(feature = "server")]
fn promote_stopped(e: &tokio::task::JoinError) -> Failure {
    Failure::new(
        FailureCode::ImportBeetsFailed,
        Stage::Import,
        RetryAdvice::SameSourceLater,
        format!("Promote stopped: {e}"),
    )
}

/// The track's folder when `user_id` owns it: `folder_not_found` for none,
/// `not_your_track` for another user's.
#[cfg(feature = "server")]
fn track_folder_of(folder: Option<Folder>, user_id: &str) -> Result<Folder, Failure> {
    let folder = folder.ok_or_else(folder_not_found)?;
    if folder.user_id == user_id {
        Ok(folder)
    } else {
        Err(not_your_track())
    }
}

/// The track (`discovery_track_not_found` when missing) and its folder
/// (`track_folder_of`). Database errors are `database_failure(stage, e)`.
#[cfg(feature = "server")]
pub(crate) async fn owned_discovery_track(
    track_id: &str,
    user_id: &str,
    stage: Stage,
) -> Result<(DiscoveryTrack, Folder), Failure> {
    owned_discovery_track_in(&crate::db::DB, track_id, user_id, stage).await
}

#[cfg(feature = "server")]
async fn owned_discovery_track_in(
    db: &sqlx::SqlitePool,
    track_id: &str,
    user_id: &str,
    stage: Stage,
) -> Result<(DiscoveryTrack, Folder), Failure> {
    let track = DiscoveryTrackRow::get_by_id_in(db, track_id)
        .await
        .map_err(|e| database_failure(stage, e))?
        .ok_or_else(discovery_track_not_found)?;
    let folder = Folder::get_by_id_in(db, &track.folder_id)
        .await
        .map_err(|e| database_failure(stage, e))?;
    let folder = track_folder_of(folder, user_id)?;
    Ok((track, folder))
}

/// `owned_discovery_track` for a promote, which then marks the track
/// Promoting: `promote_not_pending` when it is not Pending. The owner is
/// checked first, so another user's track is never left promoting.
#[cfg(feature = "server")]
async fn claim_for_promote_in(
    db: &sqlx::SqlitePool,
    track_id: &str,
    user_id: &str,
) -> Result<(DiscoveryTrack, Folder), Failure> {
    let owned = owned_discovery_track_in(db, track_id, user_id, Stage::Import).await?;
    // CAS guard: only one caller can promote a given track
    if !DiscoveryTrackRow::try_set_promoting_in(db, track_id)
        .await
        .map_err(|e| database_failure(Stage::Import, e))?
    {
        return Err(promote_not_pending());
    }
    Ok(owned)
}

#[cfg(feature = "server")]
async fn promote_detached(track_id: &str, user_id: &str) -> Result<(), Failure> {
    use crate::models::discovery_history::DiscoveryHistoryRow;

    let (track, folder) = claim_for_promote_in(&crate::db::DB, track_id, user_id).await?;

    let src = std::path::PathBuf::from(&track.path);
    if !src.exists() {
        // File missing -- revert to Pending so expiry can handle it
        let _ = DiscoveryTrackRow::update_status(track_id, &DiscoveryStatus::Pending).await;
        warn!("Promote of '{}': {} is gone", track.title, track.path);
        return Err(promote_file_gone());
    }

    let target = std::path::PathBuf::from(&folder.path);
    if let Err(failure) = crate::import_queue::import_to_library(&src, &target).await {
        // Import failed -- revert to Pending
        let _ = DiscoveryTrackRow::update_status(track_id, &DiscoveryStatus::Pending).await;
        return Err(failure);
    }

    if let Some(parent) = src.parent() {
        let profile_root = folder.discovery_profile_path(&track.profile);
        let _ = super::cleanup_empty_ancestors(parent, std::path::Path::new(&profile_root)).await;
    }

    DiscoveryTrackRow::update_status(track_id, &DiscoveryStatus::Promoted)
        .await
        .map_err(|e| database_failure(Stage::Import, e))?;

    if let Err(e) =
        DiscoveryHistoryRow::update_outcome(user_id, &track.artist, &track.title, "promoted").await
    {
        warn!(
            "Failed to update history for promoted track '{}': {}",
            track.title, e
        );
    }

    info!(
        "Promoted discovery track: {} -> {}",
        track.title, folder.path
    );
    Ok(())
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::core::delete_guard::test_support::Library;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use std::sync::Arc;

    #[tokio::test]
    async fn another_users_track_is_refused_before_it_is_claimed() {
        let db = crate::db::test_db::migrated().await;
        let folder = crate::core::folder_of(&db, "alice").await;
        sqlx::query(
            "INSERT INTO discovery_tracks (id, title, artist, album, path, folder_id)
             VALUES ('t1', 'Wake Up', 'Kowloon', 'Come Over', '/music/alice/x.flac', ?)",
        )
        .bind(&folder)
        .execute(&db)
        .await
        .unwrap();
        let claim = |user: &'static str| {
            let db = db.clone();
            async move {
                claim_for_promote_in(&db, "t1", user)
                    .await
                    .map(|(track, folder)| (track.id, folder.user_id))
            }
        };

        assert_eq!(claim("bob").await, Err(not_your_track()));
        // Still Pending, so its owner can promote it.
        assert_eq!(
            claim("alice").await,
            Ok(("t1".to_string(), "alice".to_string()))
        );
        assert_eq!(claim("alice").await, Err(promote_not_pending()));
    }

    #[test]
    fn promote_refusals_keep_their_text() {
        use FailureCode::*;
        use RetryAdvice::*;
        for (f, code, stage, retry, message) in [
            (
                discovery_track_not_found(),
                UnknownHandle,
                Stage::Input,
                NotRetryable,
                "Discovery track not found",
            ),
            (
                not_your_track(),
                NotOwner,
                Stage::Input,
                NotRetryable,
                "Not authorized to modify this track",
            ),
            (
                promote_not_pending(),
                ExpiredHandle,
                Stage::Input,
                NotRetryable,
                "Track is no longer pending (already promoted, removed, or being promoted)",
            ),
            (
                promote_file_gone(),
                ImportFilesGone,
                Stage::Import,
                NotRetryable,
                "The downloaded file is gone",
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (code, stage, retry, message)
            );
        }
    }

    #[test]
    fn only_the_folder_owner_may_change_a_discovery_track() {
        let folder = |user_id: &str| Folder {
            id: "f1".into(),
            user_id: user_id.into(),
            name: "Music".into(),
            path: "/music".into(),
            shared: false,
        };
        assert_eq!(
            track_folder_of(Some(folder("user-b")), "user-a"),
            Err(not_your_track())
        );
        assert_eq!(track_folder_of(None, "user-a"), Err(folder_not_found()));
        assert_eq!(
            track_folder_of(Some(folder("user-a")), "user-a"),
            Ok(folder("user-a"))
        );
    }

    #[tokio::test]
    async fn a_promote_that_panics_is_an_import_that_stopped() {
        let e = tokio::spawn(async { panic!("boom") }).await.unwrap_err();
        let f = promote_stopped(&e);
        assert_eq!(
            (f.code, f.retry),
            (FailureCode::ImportBeetsFailed, RetryAdvice::SameSourceLater)
        );
        assert_eq!(f.message, format!("Promote stopped: {e}"));
    }

    /// The song search3 lists for alice: by "Marrow Choir", no path.
    fn subsonic(id: &str, title: &str, rating: Option<u8>) -> SubsonicSong {
        SubsonicSong {
            id: id.into(),
            title: title.into(),
            artist: Some("Marrow Choir".into()),
            album: Some("Winter Songs".into()),
            album_id: Some("al1".into()),
            genre: None,
            duration: None,
            path: None,
            user_rating: rating,
            average_rating: None,
            play_count: None,
            starred: None,
        }
    }

    #[tokio::test]
    async fn one_star_songs_are_deleted_through_the_guard_and_the_kept_ones_say_why() {
        let lib = Library::new().await;
        lib.song("s2", "alice/A/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.fake.rates("bob", "s2", 4);
        let stub = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let songs = vec![
            subsonic("s1", "O Holy Night", Some(1)),
            subsonic("s2", "Silent Night", Some(1)),
            subsonic("s3", "Away in a Manger", Some(5)),
        ];
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!((run.deleted, run.removed), (1, 0));
        assert_eq!(
            run.kept
                .iter()
                .map(|k| (k.song_id.as_str(), k.failure.code))
                .collect::<Vec<_>>(),
            vec![("s2", FailureCode::DeleteBlockedByOtherUser)]
        );
        assert!(
            !lib.root.join("music/alice/A/x.flac").exists()
                && lib.root.join("music/alice/A/y.flac").exists()
        );
        DeletionBlockRow::replace_in(&lib.db, "alice", &run.kept)
            .await
            .unwrap();
        let listed = DeletionBlockRow::for_user_in(&lib.db, "alice")
            .await
            .unwrap();
        assert_eq!(
            (listed[0].title.as_str(), listed[0].message.as_str()),
            (
                "Silent Night",
                "Another listener rated or starred this track"
            )
        );
        DeletionBlockRow::replace_in(&lib.db, "alice", &[])
            .await
            .unwrap();
        assert!(DeletionBlockRow::for_user_in(&lib.db, "alice")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn a_song_no_longer_rated_one_star_is_neither_deleted_nor_kept() {
        // search3 listed s1 at 1 star; alice has rated it 4 since.
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 4);
        let queue = ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())));
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &[subsonic("s1", "O Holy Night", Some(1))],
            &[],
            &queue,
        )
        .await;
        assert_eq!((run.deleted, run.removed, run.kept.len()), (0, 0, 0));
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn a_pending_track_the_guard_finds_on_its_own_is_removed_as_a_staged_track() {
        // Not among the discovery folder's tracks the sync loaded (`pending` is empty), as for a
        // track left in an earlier discovery folder of the user.
        let lib = Library::new().await;
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        lib.song("s5", "alice/Discovery/Explore/03.flac", b"audio");
        lib.fake.rates("alice", "s5", 1);
        lib.pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let queue = ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())));
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &[subsonic("s5", "03", Some(1))],
            &[],
            &queue,
        )
        .await;
        assert_eq!((run.deleted, run.removed, run.kept.len()), (0, 1, 0));
        assert_eq!(
            (
                lib.track_status("t1").await,
                lib.history_outcome("t1").await
            ),
            ("Removed".into(), "removed".into())
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_one_star_discovery_track_is_removed_without_navidrome_placing_its_file() {
        // Navidrome has no record of s9 for anyone, and the track has no song id yet: today's file
        // name match on the Subsonic path still finds it, and the guard is never asked.
        let lib = Library::new().await;
        let staged = lib
            .root
            .join("music/alice/Discovery/Explore/Marrow Choir/03 - O Holy Night.flac");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"audio").unwrap();
        let track = lib
            .pending_track("t1", "alice-music", "Explore", &staged, None)
            .await;
        let song = SubsonicSong {
            path: Some("Marrow Choir/Explore/03 - O HOLY NIGHT.flac".into()),
            ..subsonic("s9", "O Holy Night", Some(1))
        };
        let pending = vec![track];
        assert!(
            matches!(pending_action(&song, &pending, 1), Some(PendingAction::Promote(t)) if t.id == "t1")
        );
        assert_eq!(
            remove_disliked_pending_in(&lib.db, std::slice::from_ref(&song), &pending, 1).await,
            0
        );
        assert!(staged.exists());
        let queue = ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())));
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            std::slice::from_ref(&song),
            &pending,
            &queue,
        )
        .await;
        assert_eq!((run.deleted, run.removed, run.kept.len()), (0, 0, 0));
        assert_eq!(
            remove_disliked_pending_in(&lib.db, &[song], &pending, 3).await,
            1
        );
        assert!(!staged.exists());
        assert_eq!(
            (
                lib.track_status("t1").await,
                lib.history_outcome("t1").await
            ),
            ("Removed".into(), "removed".into())
        );
        assert_eq!(
            (lib.fake.reads_by("alice"), lib.fake.reads_by("bob")),
            (0, 0)
        );
    }

    #[test]
    fn a_track_with_a_song_id_matches_only_that_song() {
        let track = DiscoveryTrack {
            id: "t1".into(),
            song_id: Some("s5".into()),
            title: "O Holy Night".into(),
            artist: "Marrow Choir".into(),
            album: "Winter Songs".into(),
            path: "/music/alice/Discovery/Explore/03.flac".into(),
            folder_id: "alice-music".into(),
            profile: "Explore".into(),
            rating: None,
            status: DiscoveryStatus::Pending,
            created_at: "2026-11-15 20:00:00".into(),
        };
        let same_name = SubsonicSong {
            path: Some("Marrow Choir/Explore/03.flac".into()),
            ..subsonic("s6", "O Holy Night", Some(1))
        };
        assert!(pending_track_for(&same_name, std::slice::from_ref(&track)).is_none());
        assert!(pending_track_for(
            &subsonic("s5", "O Holy Night", Some(1)),
            std::slice::from_ref(&track)
        )
        .is_some());
    }
}
