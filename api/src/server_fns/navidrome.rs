use dioxus::prelude::*;
use shared::navidrome::{
    BlockedDeletion, DeletionReview, LibraryStats, SyncResult, UnreadableListener,
};

#[cfg(feature = "server")]
use dioxus::logger::tracing::{info, warn};

#[cfg(feature = "server")]
use super::discovery::LiveDiscovery;
#[cfg(feature = "server")]
use super::failure_error;
#[cfg(feature = "server")]
use crate::auth::UserId;
#[cfg(feature = "server")]
use crate::core::delete_guard::paths;
#[cfg(feature = "server")]
use crate::core::delete_guard::staging::{remove_staged_in, StagedOutcome};
#[cfg(feature = "server")]
use crate::core::delete_guard::{ForgetVia, Guard, NotRemoved, Origin, Verdict};
#[cfg(feature = "server")]
use crate::core::discovery::{promote, track_not_pending, DiscoveryEffects, Replace};
#[cfg(feature = "server")]
use crate::core::folders::folder_not_found;
#[cfg(feature = "server")]
use crate::core::index::ServerNavidrome;
#[cfg(feature = "server")]
use crate::core::library;
#[cfg(feature = "server")]
use crate::core::ratings::{pending_action, pending_track_for, PendingAction};
#[cfg(feature = "server")]
use crate::core::{database_failure, logged};
#[cfg(feature = "server")]
use crate::import_queue::{ImportQueue, IMPORT_QUEUE};
#[cfg(feature = "server")]
use crate::models::deletion_block::{DeletionBlockRow, KeptTrack};
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
use shared::failure::Stage;
#[cfg(feature = "server")]
use shared::navidrome::{DeletionReason, DiscoveryTrack};
#[cfg(feature = "server")]
use soulbeet::navidrome::models::SubsonicSong;
#[cfg(feature = "server")]
use std::path::Path;
#[cfg(feature = "server")]
use std::sync::Arc;

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
        DiscoveryTrackRow::get_pending_by_folder_in(&crate::db::DB, fid).await?
    } else {
        vec![]
    };

    let mut removed_tracks =
        remove_disliked_pending_in(&crate::db::DB, &songs, &pending, promote_threshold).await;
    let mut promoted_tracks = 0u32;
    let (live, actor): (Arc<dyn DiscoveryEffects>, _) =
        (Arc::new(LiveDiscovery), UserId::new(user_id));
    for song in &songs {
        if let Some(PendingAction::Promote(track)) =
            pending_action(song, &pending, promote_threshold)
        {
            let replace = Replace::Guarded(Origin::Auto);
            if let Err(e) = promote(&crate::db::DB, &live, &actor, &track.id, replace).await {
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
            Err(f) if f == track_not_pending() => {
                info!("Skipped removing track {}: {}", track.title, f.message)
            }
            Err(f) => warn!("Failed to remove track {}: {}", track.title, logged(&f)),
        }
    }
    removed
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
/// star is removed; one rated otherwise since `songs` was listed is skipped, also when the
/// guard refuses it. A song whose file an earlier deletion of `actor` removed, which Navidrome
/// lists until it notices, is skipped. Any other failure is kept with its song.
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
    let deleted = match DeletionReviewRow::deleted_files_in(guard.db, &actor.to_string()).await {
        Ok(deleted) => deleted,
        Err(e) => {
            warn!("Auto-delete could not read which files it deleted before: {e}");
            vec![]
        }
    };
    for song in songs {
        if song.user_rating != Some(1) || pending_track_for(song, pending).is_some() {
            continue;
        }
        let artist = song.artist.as_deref().unwrap_or_default();
        let skipped = |why: &str| info!("Auto-delete skipped {artist} - {}: {why}", song.title);
        if deleted_before(guard, actor, &deleted, &song.id).await {
            skipped("deleted earlier, and Navidrome has not noticed yet");
            continue;
        }
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
                    // Its only warning, an unrecorded deletion, is logged where it happens.
                    .map(|_| run.deleted += 1)
                    .map_err(NotRemoved::into_failure)
            }
            Ok(Verdict::Library(_)) => {
                skipped("no longer rated 1");
                Ok(())
            }
            Ok(Verdict::Discovery { track, rating: 1 }) => {
                match Folder::get_by_id_in(guard.db, &track.folder_id).await {
                    Ok(Some(folder)) => {
                        match remove_staged_in(guard.db, &track, &folder, StagedOutcome::Removed)
                            .await
                        {
                            Ok(()) => {
                                run.removed += 1;
                                Ok(())
                            }
                            Err(f) if f == track_not_pending() => {
                                skipped("no longer pending in discovery");
                                Ok(())
                            }
                            Err(f) => Err(f),
                        }
                    }
                    Ok(None) => Err(folder_not_found()),
                    Err(e) => Err(database_failure(Stage::Delete, e)),
                }
            }
            Ok(Verdict::Discovery { .. }) => {
                skipped("no longer rated 1");
                Ok(())
            }
            Err(f) => match still_rated_one(guard, actor, &song.id).await {
                true => Err(f),
                false => {
                    skipped("no longer rated 1");
                    Ok(())
                }
            },
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

/// Whether a deletion in `deleted` (song id, canonical path) removed the file song `song_id`
/// has now: `actor`'s native record of it names that path on this machine, found as
/// local_file_in finds it (paths::names_file), and nothing is there. Navidrome keeps a song's id
/// and ratings when its scanner matches a missing file to a new one
/// (scanner/phase_2_missing_tracks.go:275-308, moveMatched), so a song whose file is now
/// elsewhere is checked as any other. A record that cannot be read is not skipped either: the
/// guard reads it again and says why it keeps the file.
#[cfg(feature = "server")]
async fn deleted_before(
    guard: &Guard<'_>,
    actor: &UserId,
    deleted: &[(String, String)],
    song_id: &str,
) -> bool {
    let recorded: Vec<&Path> = deleted
        .iter()
        .filter(|(id, _)| id == song_id)
        .map(|(_, path)| Path::new(path))
        .collect();
    if recorded.is_empty() {
        return false;
    }
    let Ok(Some(song)) = guard.listeners.native_song(actor, song_id).await else {
        return false;
    };
    for path in recorded {
        let named = paths::names_file(guard.db, &song, guard.music_path, path)
            .await
            .unwrap_or(false);
        if named
            && tokio::fs::symlink_metadata(path)
                .await
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            return true;
        }
    }
    false
}

/// Whether `actor`'s record of `song_id` still shows a rating of 1; true when it cannot be
/// read, so the refusal stands.
#[cfg(feature = "server")]
async fn still_rated_one(guard: &Guard<'_>, actor: &UserId, song_id: &str) -> bool {
    match guard.listeners.native_song(actor, song_id).await {
        Ok(Some(song)) => song.rating == 1,
        Ok(None) | Err(_) => true,
    }
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
    let client = library::navidrome_for(&ServerNavidrome, &auth.0, Stage::LibraryCheck)
        .await
        .map_err(failure_error)?;
    library::stats(&client).await.map_err(failure_error)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::core::delete_guard::test_support::Library;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use crate::test_navidrome::subsonic_song;
    use shared::failure::FailureCode;
    use soulbeet::navidrome::models::NativeSong;
    use std::sync::Arc;

    #[tokio::test]
    async fn one_star_songs_are_deleted_through_the_guard_and_the_kept_ones_say_why() {
        let lib = Library::new().await;
        lib.song("s2", "alice/A/y.flac", b"audio");
        lib.fake.rates("alice", "s2", 1);
        lib.fake.rates("bob", "s2", 4);
        let stub = Arc::new(StubImporter::new());
        let queue = ImportQueue::new(RecordingDeps::new(stub.clone()));
        let songs = vec![
            subsonic_song("s1", "O Holy Night", Some(1)),
            subsonic_song("s2", "Silent Night", Some(1)),
            subsonic_song("s3", "Away in a Manger", Some(5)),
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
        sqlx::query("INSERT INTO user_settings (user_id, auto_delete_enabled) VALUES ('alice', 1)")
            .execute(&lib.db)
            .await
            .unwrap();
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
            &[subsonic_song("s1", "O Holy Night", Some(1))],
            &[],
            &queue,
        )
        .await;
        assert_eq!((run.deleted, run.removed, run.kept.len()), (0, 0, 0));
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn a_song_rated_again_since_the_listing_is_not_kept_when_the_guard_refuses_it() {
        let lib = Library::new().await;
        lib.fake.stars("bob", "s1");
        lib.fake.rates("alice", "s1", 4);
        let queue = ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())));
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &[subsonic_song("s1", "O Holy Night", Some(1))],
            &[],
            &queue,
        )
        .await;
        assert_eq!((run.deleted, run.removed, run.kept.len()), (0, 0, 0));
        // Still at 1 star, the same refusal keeps it.
        lib.fake.rates("alice", "s1", 1);
        let run = auto_delete_songs(
            &lib.guard(),
            &lib.alice(),
            &[subsonic_song("s1", "O Holy Night", Some(1))],
            &[],
            &queue,
        )
        .await;
        assert_eq!(
            run.kept.iter().map(|k| k.failure.code).collect::<Vec<_>>(),
            vec![FailureCode::DeleteBlockedByOtherUser]
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn a_song_deleted_earlier_that_navidrome_still_lists_is_neither_checked_nor_kept() {
        let lib = Library::new().await;
        let queue = ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())));
        let songs = [subsonic_song("s1", "O Holy Night", Some(1))];
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!((run.deleted, run.kept.len()), (1, 0));
        // The next sync lists it again: Navidrome has not scanned since. Only its record is
        // read: the guard never checks it, so no rating is sent.
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!((run.deleted, run.removed, run.kept.len()), (0, 0, 0));
        assert_eq!(lib.fake.ratings_set().len(), 1);
        // A file at that path again (re-imported) is checked as usual.
        lib.write("alice/A/x.flac", b"audio");
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!(run.deleted, 1);
    }

    #[tokio::test]
    async fn a_song_navidrome_moved_to_another_file_since_its_deletion_is_checked_again() {
        let lib = Library::new().await;
        let queue = ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())));
        let songs = [subsonic_song("s1", "O Holy Night", Some(1))];
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!(run.deleted, 1);
        // Re-imported at another path: Navidrome's scanner matched the missing file to the new
        // one and gave it the song's id and alice's rating of 1 (moveMatched).
        lib.write("alice/B/x.flac", b"audio");
        lib.fake.file(
            NativeSong {
                path: "alice/B/x.flac".into(),
                ..lib.record("s1")
            },
            "al1",
        );
        let run = auto_delete_songs(&lib.guard(), &lib.alice(), &songs, &[], &queue).await;
        assert_eq!((run.deleted, run.kept.len()), (1, 0));
        assert!(!lib.root.join("music/alice/B/x.flac").exists());
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
            &[subsonic_song("s5", "03", Some(1))],
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
            ..subsonic_song("s9", "O Holy Night", Some(1))
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
}
