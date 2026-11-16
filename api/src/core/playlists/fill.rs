//! The playlist an `ensure_in_library` batch fills (D17): the songs its items end with go into
//! the playlist as their jobs finish, each at its wanted place, whether or not anyone follows
//! the batch.

use std::path::{Path, PathBuf};
use std::time::Duration;

use shared::failure::Stage;
use soulbeet::navidrome::NavidromeClient;
use soulbeet::ImportedItem;
use sqlx::SqlitePool;
use tokio::sync::OwnedMutexGuard;
use tracing::warn;

use super::{
    create_held, editable, facts, failed, ids_of, lock, name_free, names_key, owned, owned_read,
    playlist_key, write_tracks, NewPlaylist, PlaylistRefusal, TrackWrite, Written,
};
use crate::auth::{UserCtx, UserId};
use crate::core::index::{library_songs, NavidromeOf, ServerNavidrome};
use crate::core::jobs::EnsureItem;
use crate::core::library::navidrome_for;
use crate::core::{database_failure, logged};
use crate::ledger::store::{self, BatchPlaylist, BatchToFill};
use crate::ledger::{JobState, Ledger, OrderEntry, SlotResult};

/// How long the runner waits after a ledger change before its round, so a burst of changes
/// makes one round.
const ROUND_PAUSE: Duration = Duration::from_secs(2);

/// The longest the runner waits between rounds.
const ROUND_EVERY: Duration = Duration::from_secs(60);

/// The playlist an `ensure_in_library` call fills: one of the caller's own, or a new one.
pub(crate) enum PlaylistTarget {
    Existing(String),
    Create(NewPlaylist),
}

/// One entry per item of `ensure`'s answer, in its order: an item the library has settled with
/// its first song (its list can hold several copies of the recording), one with a job (made, or
/// the caller's own already running) waiting for that job, a refused one settled with none.
pub(crate) fn order_of(items: &[EnsureItem]) -> Vec<OrderEntry> {
    (items.iter())
        .map(|item| match item {
            EnsureItem::InLibrary { song_ids } => OrderEntry {
                job_id: None,
                song_ids: song_ids.first().cloned().into_iter().collect(),
                settled: true,
            },
            EnsureItem::Created { job_id, .. } | EnsureItem::AlreadyRunning { job_id, .. } => {
                OrderEntry {
                    job_id: Some(job_id.clone()),
                    song_ids: vec![],
                    settled: false,
                }
            }
            EnsureItem::Refused(..) => OrderEntry {
                job_id: None,
                song_ids: vec![],
                settled: true,
            },
        })
        .collect()
}

/// Before `ensure`: refuses a playlist the call cannot fill, before any job is made, and takes the
/// locks `begin` writes under, which the caller holds until `begin` ran. A new playlist takes
/// `names_key` and needs its name free; an existing one takes `playlist_key` and must be the
/// caller's own and static.
pub(crate) async fn prepare(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    target: &PlaylistTarget,
) -> Result<Vec<OwnedMutexGuard<()>>, PlaylistRefusal> {
    match target {
        PlaylistTarget::Create(new) => {
            let names = lock(&names_key(ctx)).await;
            name_free(navi, ctx, new.name.trim(), None, new.allow_duplicate_name).await?;
            Ok(vec![names])
        }
        PlaylistTarget::Existing(id) => {
            let playlist = lock(&playlist_key(id)).await;
            let (p, tracks) = owned_read(navi, ctx, id).await?;
            editable(navi, &facts(&p, &ctx.username), &tracks, &ids_of(&tracks)).await?;
            Ok(vec![playlist])
        }
    }
}

/// After `ensure`, under the locks `prepare` took: the playlist gets the songs `order_of(items)`
/// settled, in that order, created with them or appended to the caller's playlist, with the
/// usual checks and read back. Then `batch`, when ensure made one, stores that order for the
/// fill; a batch that could not store it is Unfollowed.
pub(crate) async fn begin(
    ledger: &Ledger,
    navi: &NavidromeClient,
    ctx: &UserCtx,
    batch: Option<&str>,
    target: &PlaylistTarget,
    items: &[EnsureItem],
) -> Result<Written, PlaylistRefusal> {
    let order = order_of(items);
    let owned_songs: Vec<String> = order.iter().flat_map(|e| e.song_ids.clone()).collect();
    let written = match target {
        PlaylistTarget::Create(new) => {
            let new = NewPlaylist {
                name: new.name.clone(),
                comment: new.comment.clone(),
                public: new.public,
                song_ids: owned_songs,
                allow_duplicate_name: new.allow_duplicate_name,
            };
            create_held(navi, ctx, &new).await?
        }
        PlaylistTarget::Existing(id) if owned_songs.is_empty() => owned(navi, ctx, id).await?,
        PlaylistTarget::Existing(id) => {
            write_tracks(navi, ctx, id, |_, tracks| {
                let mut after = ids_of(tracks);
                after.extend_from_slice(&owned_songs);
                Ok((TrackWrite::Append(owned_songs.clone()), after))
            })
            .await?
        }
    };
    if let Some(batch) = batch {
        let playlist_id = written.playlist.playlist_id.clone();
        (ledger.set_playlist(batch, &playlist_id, order).await).map_err(|e| {
            PlaylistRefusal::Unfollowed {
                playlist_id,
                failure: database_failure(Stage::Playlist, e),
            }
        })?;
    }
    Ok(written)
}

/// `current` with each of `ready` (an index of `order` and its songs, in that order) inserted
/// after the last entry holding a song of the nearest earlier settled item of `order` that the
/// list holds, else before the first entry holding a song of the nearest later one, else at the
/// end. An item placed by this call counts as settled for those after it.
pub(crate) fn place(
    current: &[String],
    order: &[OrderEntry],
    ready: &[(usize, Vec<String>)],
) -> Vec<String> {
    let mut list = current.to_vec();
    let mut settled: Vec<Option<&[String]>> = (order.iter())
        .map(|entry| entry.settled.then_some(entry.song_ids.as_slice()))
        .collect();
    for (at, songs) in ready {
        let holds = |item: &Option<&[String]>, id: &String| item.is_some_and(|s| s.contains(id));
        let after = (settled[..*at].iter().rev())
            .find_map(|item| list.iter().rposition(|id| holds(item, id)))
            .map(|last| last + 1);
        let before = || {
            (settled[at + 1..].iter()).find_map(|item| list.iter().position(|id| holds(item, id)))
        };
        let index = after.or_else(before).unwrap_or(list.len());
        list.splice(index..index, songs.iter().cloned());
        settled[*at] = Some(songs);
    }
    list
}

/// The songs a job of the batch ended with, as far as the ledger knows them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JobSongs {
    /// Not ended, or ended in a way it can be tried again from (failed, cancelled).
    Waiting,
    /// Dismissed, or no longer kept: nothing to place.
    Dismissed,
    /// The songs Navidrome listed for its imports, each once, in slot order.
    Listed(Vec<String>),
    /// Files in the library folder `library` to look up in Navidrome: an import whose songs
    /// were not listed (its user had no Navidrome then), or the library's own copies of a
    /// duplicate.
    Lookup {
        library: PathBuf,
        items: Vec<ImportedItem>,
    },
}

/// What `user`'s job `job` ended with. A done job is Listed when Navidrome listed every import of
/// it, else a Lookup of every file it imported. A failed job whose failed slots are all
/// duplicates (import_already_in_library) is a Lookup of those copies and of what its other
/// slots imported; with any other failed slot it waits, as it can be tried again.
pub(crate) async fn songs_of_job(
    db: &SqlitePool,
    user: &UserId,
    job: &str,
) -> Result<JobSongs, sqlx::Error> {
    let Some(record) = store::job_record(db, job, user).await? else {
        return Ok(JobSongs::Dismissed);
    };
    let tracks = match record.state {
        JobState::Dismissed => return Ok(JobSongs::Dismissed),
        JobState::Done | JobState::Failed => store::tracks_of(db, job).await?,
        _ => return Ok(JobSongs::Waiting),
    };
    let retryable = (tracks.iter()).any(|t| {
        t.state == JobState::Failed
            && !matches!(t.result, Some(SlotResult::AlreadyInLibrary { .. }))
    });
    if retryable {
        return Ok(JobSongs::Waiting);
    }
    // An album's import is on each of its slots: its songs and files are taken once.
    let results: Vec<&SlotResult> = tracks.iter().filter_map(|t| t.result.as_ref()).collect();
    let mut ids: Vec<String> = Vec::new();
    let all_listed = results.iter().all(|result| match result {
        SlotResult::Imported {
            listed: Some(songs),
            ..
        } => {
            for song in songs {
                if !ids.contains(&song.song_id) {
                    ids.push(song.song_id.clone());
                }
            }
            true
        }
        _ => false,
    });
    if all_listed {
        return Ok(JobSongs::Listed(ids));
    }
    let mut items: Vec<ImportedItem> = Vec::new();
    for result in results {
        let files = match result {
            SlotResult::Imported { applied, .. } => applied,
            SlotResult::AlreadyInLibrary { existing } => existing,
        };
        for item in files {
            if !items.iter().any(|kept| kept.path == item.path) {
                items.push(item.clone());
            }
        }
    }
    Ok(JobSongs::Lookup {
        library: PathBuf::from(&record.folder.path),
        items,
    })
}

/// The song Navidrome lists for each of `items`, files in `library`: the first of each, each
/// once. A file Navidrome does not list puts none.
async fn library_ids(
    navi: &NavidromeClient,
    library: &Path,
    items: &[ImportedItem],
) -> Result<Vec<String>, PlaylistRefusal> {
    let mut ids: Vec<String> = Vec::new();
    for item in items {
        let found = library_songs(navi, library, item).await.map_err(failed)?;
        if let Some(song) = found.first().filter(|song| !ids.contains(&song.id)) {
            ids.push(song.id.clone());
        }
    }
    Ok(ids)
}

/// Under the playlist's lock: every item of `ctx`'s batch `batch` not settled whose job ended
/// is ready, with the songs it ended with (none for a dismissed job); with none ready, Ok(0)
/// without a Navidrome call. Otherwise the ready items' songs go into the playlist at their
/// places (`place`), appended when the new list is the old one with a tail, else written whole,
/// with the usual checks and read back; then the ready items are stored settled with their songs.
/// The number of items settled. An item whose songs the playlist already lists is not placed
/// again: an earlier fill placed them and failed after its write.
pub(crate) async fn fill_in(
    db: &SqlitePool,
    ledger: &Ledger,
    navi: &NavidromeClient,
    ctx: &UserCtx,
    batch: &str,
) -> Result<u32, PlaylistRefusal> {
    let unstored = |e: String| PlaylistRefusal::Failed(database_failure(Stage::Playlist, e));
    let stored =
        (store::playlist_of(db, batch, &ctx.id()).await).map_err(|e| unstored(e.to_string()))?;
    let Some(BatchPlaylist {
        playlist_id,
        mut order,
        ..
    }) = stored
    else {
        return Ok(0);
    };
    let _playlist = lock(&playlist_key(&playlist_id)).await;
    let mut ready: Vec<(usize, Vec<String>)> = Vec::new();
    for (at, entry) in order.iter().enumerate() {
        let (false, Some(job)) = (entry.settled, &entry.job_id) else {
            continue;
        };
        let songs = match songs_of_job(db, &ctx.id(), job).await {
            Ok(JobSongs::Waiting) => continue,
            Ok(JobSongs::Dismissed) => vec![],
            Ok(JobSongs::Listed(ids)) => ids,
            Ok(JobSongs::Lookup { library, items }) => library_ids(navi, &library, &items).await?,
            Err(e) => return Err(unstored(e.to_string())),
        };
        ready.push((at, songs));
    }
    if ready.is_empty() {
        return Ok(0);
    }
    if ready.iter().any(|(_, songs)| !songs.is_empty()) {
        write_tracks(navi, ctx, &playlist_id, |_, tracks| {
            let current = ids_of(tracks);
            let mut placed = order.clone();
            let mut fresh = Vec::new();
            for (at, songs) in &ready {
                if songs.iter().all(|id| current.contains(id)) {
                    placed[*at].settled = true;
                    placed[*at].song_ids = songs.clone();
                } else {
                    fresh.push((*at, songs.clone()));
                }
            }
            let after = place(&current, &placed, &fresh);
            let write = match after.strip_prefix(current.as_slice()) {
                // Nothing to place: the read back alone.
                Some([]) => TrackWrite::Remove(vec![]),
                Some(tail) => TrackWrite::Append(tail.to_vec()),
                None => TrackWrite::Replace(after.clone()),
            };
            Ok((write, after))
        })
        .await?;
    }
    let settled = ready.len() as u32;
    for (at, songs) in ready {
        order[at].settled = true;
        order[at].song_ids = songs;
    }
    (ledger.set_playlist_order(batch, order).await).map_err(|e| unstored(e.to_string()))?;
    Ok(settled)
}

/// One fill of every batch `batches_to_fill` lists, through its user's Navidrome, storing each
/// one's outcome: its failure, or none once a fill succeeds after one failed.
pub(crate) async fn round(db: &SqlitePool, ledger: &Ledger, navidrome: &dyn NavidromeOf) {
    let batches = match store::batches_to_fill(db).await {
        Ok(batches) => batches,
        Err(e) => {
            warn!("Could not read which batches have playlists to fill: {e}");
            return;
        }
    };
    for BatchToFill {
        batch_id,
        ctx,
        failed,
    } in batches
    {
        let filled = match navidrome_for(navidrome, &ctx, Stage::Playlist).await {
            Ok(navi) => {
                (fill_in(db, ledger, &navi, &ctx, &batch_id).await).map_err(|r| r.failure())
            }
            Err(f) => Err(f),
        };
        let failure = filled.err();
        match &failure {
            None if !failed => continue,
            Some(f) if !failed => {
                warn!(
                    "Could not fill the playlist of batch {batch_id}: {}",
                    logged(f)
                );
            }
            _ => {}
        }
        // The writer logs a write it could not make; the next round tries again.
        let _ = ledger.set_playlist_failure(&batch_id, failure).await;
    }
}

/// Starts the fill runner on the server's database, ledger and Navidrome: a round at once, then
/// ROUND_PAUSE after a ledger change, or ROUND_EVERY after the last round.
pub fn start() {
    let (db, ledger) = ((*crate::db::DB).clone(), crate::ledger::LEDGER.clone());
    tokio::spawn(async move {
        let mut changes = ledger.changes();
        loop {
            round(&db, &ledger, &ServerNavidrome).await;
            tokio::select! {
                changed = changes.changed() => match changed {
                    Ok(()) => {
                        tokio::time::sleep(ROUND_PAUSE).await;
                        changes.borrow_and_update();
                    }
                    // The writer stopped: only the timer is left.
                    Err(_) => tokio::time::sleep(ROUND_EVERY).await,
                },
                _ = tokio::time::sleep(ROUND_EVERY) => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::jobs::job_already_running;
    use crate::core::jobs::runner::Acquire;
    use crate::core::jobs::test_support::{batch_of_downloads, duplicate_track_job, settle_done};
    use crate::core::playlists::{playlist_not_found, NotEditable};
    use crate::core::test_ctx;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{
        native_playlist, native_song, native_songs, playlist_body, playlist_rows, playlists_body,
        subsonic_failed, NavidromeFor, NavidromeStub, ADVENT, AWAY, DISCOVERY_RULES, HOLY, SILENT,
    };
    use serde_json::Map;
    use shared::failure::{Failure, FailureCode, RetryAdvice};

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn entry(item: u32, songs: &[&str], settled: bool) -> OrderEntry {
        OrderEntry {
            job_id: (!settled).then(|| format!("j{item}")),
            song_ids: ids(songs),
            settled,
        }
    }

    /// The entry of an item waiting for `job`.
    fn waiting(job: &str) -> OrderEntry {
        OrderEntry {
            job_id: Some(job.to_string()),
            song_ids: vec![],
            settled: false,
        }
    }

    fn tracks_of(id: &str) -> String {
        format!("/api/playlist/{id}/tracks")
    }

    fn alice() -> UserId {
        UserId::new("alice")
    }

    #[test]
    fn a_finished_item_goes_between_its_neighbours() {
        let order = vec![
            entry(0, &["a"], true),
            entry(1, &[], false),
            entry(2, &["c"], true),
            entry(3, &[], false),
        ];
        assert_eq!(
            place(
                &ids(&["a", "c"]),
                &order,
                &[(1, ids(&["b"])), (3, ids(&["d"]))]
            ),
            ids(&["a", "b", "c", "d"])
        );
        // the user took "a" out: "b" goes before its later neighbour
        assert_eq!(
            place(&ids(&["c"]), &order, &[(1, ids(&["b"]))]),
            ids(&["b", "c"])
        );
        // no settled neighbour left in the playlist: at the end, after what the user added
        assert_eq!(
            place(&ids(&["x"]), &order, &[(1, ids(&["b"]))]),
            ids(&["x", "b"])
        );
    }

    /// `navi` answers alice's static playlist ADVENT ("Advent") with `before` as its rows, then
    /// `after`.
    fn advent(
        navi: &NavidromeStub,
        before: &[(u32, &str, &str, bool)],
        after: &[(u32, &str, &str, bool)],
    ) {
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(ADVENT, "Advent", "alice", false, "", &[])],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(before), &playlist_rows(after)],
        );
        let count = before.len() as u32;
        navi.answer(
            &format!("/api/playlist/{ADVENT}"),
            &[&native_playlist(ADVENT, "Advent", count, false, None)],
        );
    }

    #[tokio::test]
    async fn a_finished_job_is_placed_once() {
        let (acq, _effects) = Acquire::for_alice().await;
        let db = acq.db.clone();
        let (batch, jobs) = batch_of_downloads(&acq, 1).await;
        let order = vec![entry(0, &[HOLY.0], true), waiting(&jobs[0])];
        acq.ledger
            .set_playlist(&batch, ADVENT, order)
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        advent(
            &navi,
            &[(1, HOLY.0, HOLY.1, false)],
            &[(1, HOLY.0, HOLY.1, false), (2, SILENT.0, SILENT.1, false)],
        );
        navi.answer("/api/song", &[&native_songs(&[(SILENT.0, false)])]);
        let (ctx, client) = (test_ctx("alice"), navi.client());
        let fill = || fill_in(&db, &acq.ledger, &client, &ctx, &batch);
        assert_eq!(fill().await.unwrap(), 0, "the job is still downloading");
        assert!(navi.asked().is_empty());
        settle_done(&acq, &jobs[0], SILENT.0).await;
        assert_eq!(fill().await.unwrap(), 1);
        assert_eq!(
            navi.posted("/rest/updatePlaylist"),
            vec![format!("playlistId={ADVENT}&songIdToAdd={}", SILENT.0)],
            "an append keeps hidden entries"
        );
        let order = store::playlist_of(&db, &batch, &alice())
            .await
            .unwrap()
            .unwrap()
            .order;
        assert_eq!(
            (order[1].settled, order[1].song_ids.clone()),
            (true, ids(&[SILENT.0]))
        );
        let asked = navi.asked().len();
        assert_eq!(fill().await.unwrap(), 0);
        assert_eq!(navi.asked().len(), asked, "nothing is left to place");
    }

    #[tokio::test]
    async fn songs_the_playlist_already_lists_are_not_placed_again() {
        let (acq, _effects) = Acquire::for_alice().await;
        let db = acq.db.clone();
        let (batch, jobs) = batch_of_downloads(&acq, 1).await;
        let order = vec![entry(0, &[HOLY.0], true), waiting(&jobs[0])];
        acq.ledger
            .set_playlist(&batch, ADVENT, order)
            .await
            .unwrap();
        settle_done(&acq, &jobs[0], SILENT.0).await;
        // An earlier fill appended SILENT and failed before it stored the order.
        let navi = NavidromeStub::start();
        let both = [(1, HOLY.0, HOLY.1, false), (2, SILENT.0, SILENT.1, false)];
        advent(&navi, &both, &both);
        let ctx = test_ctx("alice");
        let placed = fill_in(&db, &acq.ledger, &navi.client(), &ctx, &batch).await;
        assert_eq!(placed.unwrap(), 1);
        let asked = navi.asked();
        assert!(
            !asked
                .iter()
                .any(|p| p == "/rest/updatePlaylist" || p == "/rest/createPlaylist"),
            "{asked:?}"
        );
        let order = store::playlist_of(&db, &batch, &alice())
            .await
            .unwrap()
            .unwrap()
            .order;
        assert_eq!(
            (order[1].settled, order[1].song_ids.clone()),
            (true, ids(&[SILENT.0]))
        );
    }

    #[tokio::test]
    async fn a_duplicate_places_the_librarys_own_copy() {
        let (acq, effects) = Acquire::for_alice().await;
        let db = acq.db.clone();
        // The library's copy beets found, the size Navidrome lists (sparse: nothing is written).
        let path = "Marrow Choir/Winter Songs/03 - O Holy Night.flac";
        let file = effects.library().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::File::create(&file)
            .unwrap()
            .set_len(28_734_112)
            .unwrap();
        let existing = vec![ImportedItem {
            path: path.into(),
            recording_mbid: None,
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            beets_id: Some(17),
            release_mbid: None,
            album: "Winter Songs".into(),
            match_per_mille: None,
        }];
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("03 - O Holy Night.flac");
        let job = duplicate_track_job(&acq, "O Holy Night", &source, existing).await;
        let batch = (store::job_record(&db, &job, &alice()).await.unwrap())
            .unwrap()
            .batch_id;
        acq.ledger
            .set_playlist(&batch, ADVENT, vec![waiting(&job)])
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        let holy = native_song(HOLY.0, "Marrow Choir", HOLY.1, 263.4, None);
        navi.answer_when(
            "/api/song",
            "title=Marrow+Choir+O+Holy+Night",
            200,
            &format!("[{holy}]"),
        );
        navi.answer_when(
            "/api/song",
            &format!("id={}", HOLY.0),
            200,
            &native_songs(&[(HOLY.0, false)]),
        );
        advent(&navi, &[], &[(1, HOLY.0, HOLY.1, false)]);
        let ctx = test_ctx("alice");
        let placed = fill_in(&db, &acq.ledger, &navi.client(), &ctx, &batch).await;
        assert_eq!(placed.unwrap(), 1);
        assert_eq!(
            navi.posted("/rest/updatePlaylist"),
            vec![format!("playlistId={ADVENT}&songIdToAdd={}", HOLY.0)]
        );
    }

    #[tokio::test]
    async fn a_playlist_that_went_stops_the_fill_and_says_why() {
        let (acq, _effects) = Acquire::for_alice().await;
        let db = acq.db.clone();
        let (batch, jobs) = batch_of_downloads(&acq, 1).await;
        acq.ledger
            .set_playlist(&batch, ADVENT, vec![waiting(&jobs[0])])
            .await
            .unwrap();
        settle_done(&acq, &jobs[0], SILENT.0).await;
        let navi = NavidromeStub::start();
        // server/subsonic/playlists.go:47 (0.64.2)
        navi.answer(
            "/rest/getPlaylist",
            &[&subsonic_failed(70, "playlist not found")],
        );
        round(&db, &acq.ledger, &NavidromeFor(Ok(navi.client()))).await;
        let stored = store::playlist_of(&db, &batch, &alice())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.failure, Some(playlist_not_found(ADVENT)));
        assert!(
            store::batches_to_fill(&db).await.unwrap().is_empty(),
            "not_retryable: no further rounds"
        );
    }

    #[tokio::test]
    async fn a_fill_that_can_succeed_later_is_tried_again_and_its_failure_cleared() {
        let (acq, _effects) = Acquire::for_alice().await;
        let db = acq.db.clone();
        let (batch, jobs) = batch_of_downloads(&acq, 1).await;
        acq.ledger
            .set_playlist(&batch, ADVENT, vec![waiting(&jobs[0])])
            .await
            .unwrap();
        settle_done(&acq, &jobs[0], SILENT.0).await;
        // SQLite's text for a database another connection holds (sqlite3 error code 5).
        let locked = NavidromeFor(Err(|| {
            NavidromeLookupError::Database("database is locked".into())
        }));
        round(&db, &acq.ledger, &locked).await;
        let stored = store::playlist_of(&db, &batch, &alice())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.failure.map(|f| f.code),
            Some(shared::failure::FailureCode::SettingsUnreadable)
        );
        assert_eq!(
            store::batches_to_fill(&db).await.unwrap()[0].batch_id,
            batch,
            "same_source_later: the next round tries again"
        );
        let navi = NavidromeStub::start();
        advent(&navi, &[], &[(1, SILENT.0, SILENT.1, false)]);
        navi.answer("/api/song", &[&native_songs(&[(SILENT.0, false)])]);
        round(&db, &acq.ledger, &NavidromeFor(Ok(navi.client()))).await;
        let stored = store::playlist_of(&db, &batch, &alice())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((stored.failure, stored.order[0].settled), (None, true));
        assert!(store::batches_to_fill(&db).await.unwrap().is_empty());
    }

    /// The items `ensure` answers for a call of four: one the library has twice (HOLY, then
    /// AWAY), a job made for `job`, the caller's job "j-running" that already wants the
    /// recording, and one refused.
    fn ensured(job: &str) -> Vec<EnsureItem> {
        let (failure, facts) = job_already_running("j-running", JobState::Downloading);
        // Navidrome's lookup behind nginx, which answered 502 (a refused item's failure).
        let refused = Failure::new(
            FailureCode::NavidromeUnreachable,
            Stage::LibraryCheck,
            RetryAdvice::SameSourceLater,
            "Find songs failed: HTTP 502",
        );
        vec![
            EnsureItem::InLibrary {
                song_ids: ids(&[HOLY.0, AWAY.0]),
            },
            EnsureItem::Created {
                job_id: job.to_string(),
                fuzzy: vec![],
            },
            EnsureItem::AlreadyRunning {
                job_id: "j-running".into(),
                failure,
                facts,
            },
            EnsureItem::Refused(refused, Map::new()),
        ]
    }

    #[tokio::test]
    async fn an_existing_playlist_gets_the_owned_songs_and_the_batch_its_order() {
        let (acq, _effects) = Acquire::for_alice().await;
        let db = acq.db.clone();
        let (batch, jobs) = batch_of_downloads(&acq, 1).await;
        let navi = NavidromeStub::start();
        advent(&navi, &[], &[]);
        // prepare's read, begin's, and its read back.
        let holy = playlist_rows(&[(1, HOLY.0, HOLY.1, false)]);
        navi.answer(&tracks_of(ADVENT), &["[]", "[]", &holy]);
        navi.answer("/api/song", &[&native_songs(&[(HOLY.0, false)])]);
        let (ctx, target) = (test_ctx("alice"), PlaylistTarget::Existing(ADVENT.into()));
        let held = prepare(&navi.client(), &ctx, &target).await.unwrap();
        let items = ensured(&jobs[0]);
        let written = begin(
            &acq.ledger,
            &navi.client(),
            &ctx,
            Some(&batch),
            &target,
            &items,
        )
        .await;
        drop(held);
        assert_eq!(written.unwrap().song_ids, ids(&[HOLY.0]));
        assert_eq!(
            navi.posted("/rest/updatePlaylist"),
            vec![format!("playlistId={ADVENT}&songIdToAdd={}", HOLY.0)],
            "the first copy only, appended"
        );
        let stored = store::playlist_of(&db, &batch, &alice())
            .await
            .unwrap()
            .unwrap();
        let waits = |job: &str| OrderEntry {
            job_id: Some(job.to_string()),
            song_ids: vec![],
            settled: false,
        };
        let settled = |songs: &[&str]| OrderEntry {
            job_id: None,
            song_ids: ids(songs),
            settled: true,
        };
        assert_eq!(
            (stored.playlist_id.as_str(), stored.order, stored.failure),
            (
                ADVENT,
                vec![
                    settled(&[HOLY.0]),
                    waits(&jobs[0]),
                    waits("j-running"),
                    settled(&[])
                ],
                None
            )
        );
    }

    #[tokio::test]
    async fn a_smart_playlist_is_refused_before_any_job() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_body(
                ADVENT,
                "Fresh Picks",
                "alice",
                false,
                "",
                &[],
            )],
        );
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        navi.answer(
            &format!("/api/playlist/{ADVENT}"),
            &[&native_playlist(
                ADVENT,
                "Fresh Picks",
                1,
                false,
                Some(DISCOVERY_RULES),
            )],
        );
        let target = PlaylistTarget::Existing(ADVENT.into());
        let r = prepare(&navi.client(), &test_ctx("alice"), &target)
            .await
            .unwrap_err();
        assert_eq!(
            r,
            PlaylistRefusal::NotEditable {
                playlist_id: ADVENT.into(),
                name: "Fresh Picks".into(),
                why: NotEditable::Smart,
                would_be: ids(&[HOLY.0]),
            }
        );
    }

    #[tokio::test]
    async fn a_new_playlist_the_batch_cannot_follow_says_so() {
        let db = crate::db::test_db::migrated().await;
        let ledger = Ledger::start(db.clone());
        ledger.settled().await;
        // Soulful's database went away after the playlist was written.
        db.close().await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getPlaylists", &[&playlists_body(&[])]);
        navi.answer("/api/song", &[&native_songs(&[(HOLY.0, false)])]);
        let advent_body = playlist_body(ADVENT, "Advent", "alice", false, "", &[HOLY]);
        navi.answer("/rest/createPlaylist", &[&advent_body]);
        navi.answer("/rest/getPlaylist", &[&advent_body]);
        navi.answer(
            &tracks_of(ADVENT),
            &[&playlist_rows(&[(1, HOLY.0, HOLY.1, false)])],
        );
        let target = PlaylistTarget::Create(NewPlaylist {
            name: "Advent".into(),
            comment: None,
            public: None,
            song_ids: vec![],
            allow_duplicate_name: false,
        });
        let ctx = test_ctx("alice");
        let items = ensured("j-made");
        let r = begin(&ledger, &navi.client(), &ctx, Some("b1"), &target, &items)
            .await
            .unwrap_err();
        assert!(
            matches!(&r, PlaylistRefusal::Unfollowed { playlist_id, failure }
                if playlist_id == ADVENT && failure.code == FailureCode::SettingsUnreadable),
            "{r:?}"
        );
        assert!(r.after_write());
    }
}
