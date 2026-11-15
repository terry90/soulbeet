//! The one check every deletion of a music file goes through (design section 13).

use crate::auth::UserId;
use crate::core::database_failure;
use crate::folder_policy::PolicyRows;
use crate::models::discovery_playlist::DiscoveryTrackRow;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::navidrome::DiscoveryTrack;
use soulbeet::navidrome::models::NativeSong;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

mod listeners;
pub mod paths;
mod rules;
#[cfg(test)]
pub(crate) mod test_support;
mod unlink;

pub use listeners::{Listeners, LiveListeners};

/// The guard over Soulful's database, Navidrome as each user sees it, and the local path of
/// Navidrome's default library when it is set (D8).
pub struct Guard<'a> {
    pub db: &'a SqlitePool,
    pub listeners: &'a dyn Listeners,
    pub music_path: Option<&'a Path>,
}

impl Guard<'static> {
    /// The guard on Soulful's database, each user's stored credentials and CONFIG's music path.
    pub fn live() -> Guard<'static> {
        Guard {
            db: &crate::db::DB,
            listeners: &LiveListeners,
            music_path: crate::config::CONFIG.navidrome_music_path(),
        }
    }
}

/// What may be removed for a song.
#[derive(Debug)]
pub enum Verdict {
    /// Every rule allows removing this library file.
    Library(Checked),
    /// The file is a pending discovery track of the caller, whose own rating of the song is
    /// `rating`; discovery's staging rules apply (section 13).
    Discovery { track: DiscoveryTrack, rating: u8 },
}

/// A library file every rule allows removing.
#[derive(Debug)]
pub struct Checked {
    /// The caller's native record of the song, as read after the rating was re-sent.
    pub song: NativeSong,
    /// The canonical file.
    pub path: PathBuf,
    /// The canonical deepest registered folder holding it, the prune root.
    pub folder: PathBuf,
    /// The file's identity when it was checked, compared when it is removed (rule 6).
    id: unlink::FileId,
}

impl Guard<'_> {
    /// Resolves `song_id` from `actor`'s native record, hands back a pending discovery track of
    /// `actor` by song id before any path is resolved, then by its stored path canonicalized
    /// equal to the song's file (D12), and checks a library file with check_resolved (D5).
    pub async fn check_song(&self, actor: &UserId, song_id: &str) -> Result<Verdict, Failure> {
        let song = self
            .listeners
            .native_song(actor, song_id)
            .await
            .map_err(|e| e.failure(Stage::Delete, e.to_string()))?
            .ok_or_else(song_not_found)?;
        let pending = DiscoveryTrackRow::pending_for_user_in(self.db, &actor.to_string())
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?;
        if let Some(track) = pending
            .iter()
            .find(|t| t.song_id.as_deref() == Some(song.id.as_str()))
        {
            return Ok(Verdict::Discovery {
                track: track.clone(),
                rating: song.rating,
            });
        }
        let path = paths::local_file_in(self.db, &song, self.music_path).await?;
        for track in pending {
            if tokio::fs::canonicalize(&track.path)
                .await
                .is_ok_and(|staged| staged == path)
            {
                return Ok(Verdict::Discovery {
                    track,
                    rating: song.rating,
                });
            }
        }
        self.check_resolved(actor, song, path, &[])
            .await
            .map(Verdict::Library)
    }

    /// Rules 1, 2, 5, the one-song check, 3 and 4, in that order, for `path`, the canonical
    /// file local_file_in gave for `song`. Jobs in `exempt_jobs` do not count for rule 5.
    async fn check_resolved(
        &self,
        actor: &UserId,
        song: NativeSong,
        path: PathBuf,
        exempt_jobs: &[String],
    ) -> Result<Checked, Failure> {
        let rows = PolicyRows::load_in(self.db)
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?;
        let (folders, _) = rows.inputs();
        let folder = rules::owning_folders(&path, &folders, actor)?.to_path_buf();
        let id = rules::track_identity(&path)?;
        rules::not_in_use_in(
            self.db,
            &rules::holding_folders(&path, &folders),
            &song,
            exempt_jobs,
        )
        .await?;
        self.one_song(actor, &song, &path).await?;
        let others = self.others_permit(actor, &song, &path).await?;
        let song = self.no_unknown_rater(actor, &song, &others).await?;
        Ok(Checked {
            song,
            path,
            folder,
            id,
        })
    }

    /// Navidrome lists the canonical file `path` as `song` only, in `actor`'s view: their
    /// search for its tags finds `song` and no other song that is the same file.
    async fn one_song(
        &self,
        actor: &UserId,
        song: &NativeSong,
        path: &Path,
    ) -> Result<(), Failure> {
        let unsearchable = || {
            path_unresolved(format!(
                "Navidrome cannot be searched for the tags of {}",
                path.display()
            ))
        };
        let candidates = self
            .listeners
            .songs_for_item(actor, &listeners::item_of(song))
            .await
            .map_err(|e| e.failure(Stage::Delete, e.to_string()))?
            .ok_or_else(unsearchable)?;
        if !candidates.iter().any(|c| c.id == song.id) {
            return Err(unsearchable());
        }
        if self.another_record(song, path, &candidates).await? {
            return Err(path_unresolved(format!(
                "Navidrome lists {} as more than one song",
                path.display()
            )));
        }
        Ok(())
    }

    /// Whether a song of `candidates` other than `song` is its canonical file `path`: its
    /// library path joined with its path is the song's, or its path cannot be compared (absolute
    /// or with `..`), or its path on this machine resolves to `path`.
    async fn another_record(
        &self,
        song: &NativeSong,
        path: &Path,
        candidates: &[NativeSong],
    ) -> Result<bool, Failure> {
        let listed = Path::new(&song.library_path).join(&song.path);
        for candidate in candidates.iter().filter(|c| c.id != song.id) {
            let Some(relative) = paths::inside(&candidate.path) else {
                return Ok(true);
            };
            if Path::new(&candidate.library_path).join(relative) == listed {
                return Ok(true);
            }
            let local = paths::local_path_in(self.db, candidate, self.music_path).await?;
            if tokio::fs::canonicalize(&local)
                .await
                .is_ok_and(|resolved| resolved == path)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The song's file on this machine cannot be told: `message` says why.
pub(crate) fn path_unresolved(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::DeletePathUnresolved,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        message,
    )
}

/// The caller's Navidrome has no song by that id.
pub(crate) fn song_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Song not found",
    )
}

/// The file is not in a library folder of the caller: `message` says why.
pub(crate) fn outside_library(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::DeleteOutsideLibrary,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        message,
    )
}

/// The file is not a track: `message` says why.
pub(crate) fn not_a_track(message: impl Into<String>) -> Failure {
    Failure::new(
        FailureCode::DeleteNotATrack,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        message,
    )
}

/// Another listener's mark keeps the file; it never says who or what (section 13 rule 3).
pub(crate) fn blocked_by_other_user() -> Failure {
    Failure::new(
        FailureCode::DeleteBlockedByOtherUser,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        "Another listener rated or starred this track",
    )
}

/// Another listener's marks cannot be read, so the file is kept.
pub(crate) fn veto_unknown() -> Failure {
    Failure::new(
        FailureCode::DeleteVetoUnknown,
        Stage::Delete,
        RetryAdvice::SameSourceLater,
        "Soulful could not read another listener's ratings for this track",
    )
}

/// An unfinished job wants the file (rule 5).
pub(crate) fn in_use() -> Failure {
    Failure::new(
        FailureCode::DeleteInUse,
        Stage::Delete,
        RetryAdvice::SameSourceLater,
        "A download or import that is not finished wants this track",
    )
}

#[cfg(test)]
mod tests {
    use super::test_support::{wanted, FakeNavidrome, Library};
    use super::*;
    use crate::models::navidrome_read_failure::NavidromeReadFailureRow;
    use crate::services::NavidromeLookupError;
    use shared::download::DownloadQuery;
    use shared::metadata::Album;
    use soulbeet::NavidromeError;

    fn library_verdict(v: Result<Verdict, Failure>) -> Checked {
        match v {
            Ok(Verdict::Library(c)) => c,
            Ok(Verdict::Discovery { track, .. }) => panic!("discovery {track:?}"),
            Err(f) => panic!("{f:?}"),
        }
    }

    fn refusal(v: Result<Verdict, Failure>) -> (FailureCode, String) {
        match v {
            Err(f) => (f.code, f.message),
            Ok(_) => panic!("allowed"),
        }
    }

    fn canonical(lib: &Library, rel: &str) -> PathBuf {
        std::fs::canonicalize(lib.root.join(rel)).unwrap()
    }

    const VETO: &str = "Soulful could not read another listener's ratings for this track";

    #[tokio::test]
    async fn a_file_only_its_owner_rated_one_star_is_allowed_after_the_average_is_refreshed() {
        let lib = Library::new().await;
        let checked = library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
        assert_eq!(checked.path, canonical(&lib, "music/alice/A/x.flac"));
        assert_eq!(checked.folder, canonical(&lib, "music/alice"));
        assert_eq!(
            checked.id,
            unlink::FileId::of(&std::fs::symlink_metadata(&checked.path).unwrap())
        );
        assert_eq!(
            (checked.song.rating, checked.song.average_rating),
            (1, Some(1.0))
        );
        assert_eq!(
            lib.fake.ratings_set(),
            vec![("alice".to_string(), "s1".to_string(), 1)]
        );
    }

    #[tokio::test]
    async fn a_stale_average_is_refreshed_before_it_is_compared() {
        // A deleted user's rating stays in the average until someone calls setRating.
        let lib = Library::new().await;
        lib.fake.stale_average("s1", 3.0);
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
    }

    #[tokio::test]
    async fn an_average_with_a_rater_soulful_cannot_read_blocks() {
        let lib = Library::new().await;
        lib.fake.hidden_rater("s1", 4);
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (
                FailureCode::DeleteBlockedByOtherUser,
                "Another listener rated or starred this track".into()
            )
        );
    }

    #[tokio::test]
    async fn another_users_rating_or_star_of_the_track_or_its_album_blocks() {
        type Mark = fn(&FakeNavidrome);
        let marks: [(Mark, bool); 6] = [
            (|f| f.rates("bob", "s1", 2), true),
            (|f| f.rates("bob", "s1", 1), false),
            (|f| f.stars("bob", "s1"), true),
            (|f| f.rates_album("bob", "al1", 3), true),
            (|f| f.rates_album("bob", "al1", 1), false),
            (|f| f.stars_album("bob", "al1"), true),
        ];
        for (i, (mark, blocks)) in marks.into_iter().enumerate() {
            let lib = Library::new().await;
            mark(&lib.fake);
            let verdict = lib.guard().check_song(&lib.alice(), "s1").await;
            match blocks {
                true => assert_eq!(
                    refusal(verdict).0,
                    FailureCode::DeleteBlockedByOtherUser,
                    "case {i}"
                ),
                false => {
                    library_verdict(verdict);
                }
            }
        }
    }

    #[tokio::test]
    async fn a_song_another_user_cannot_see_does_not_block() {
        let lib = Library::new().await;
        lib.fake.rates("bob", "s1", 5);
        // getSong answers code 70 for bob. bob's rating still counts in Navidrome's average, so
        // rule 4 blocks: a user who rated and lost access is a rater Soulful cannot read.
        lib.fake.unsee("bob", "s1");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteBlockedByOtherUser
        );
        let lib = Library::new().await;
        lib.fake.unsee("bob", "s1");
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
    }

    #[tokio::test]
    async fn an_unreadable_linked_user_blocks_and_is_named_for_admins() {
        let lib = Library::new().await;
        lib.fake
            .unreadable("bob", || NavidromeLookupError::PasswordUnreadable);
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (FailureCode::DeleteVetoUnknown, VETO.into())
        );
        let listed = NavidromeReadFailureRow::all_in(&lib.db).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|l| (l.username.as_str(), l.error.as_str()))
                .collect::<Vec<_>>(),
            vec![("bob", crate::services::STORED_PASSWORD_UNREADABLE)]
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
        lib.fake.readable("bob");
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
        assert!(NavidromeReadFailureRow::all_in(&lib.db)
            .await
            .unwrap()
            .is_empty());

        let lib = Library::new().await;
        lib.fake.unreadable("bob", || {
            NavidromeLookupError::Client(NavidromeError::CircuitOpen)
        });
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteVetoUnknown
        );
    }

    #[tokio::test]
    async fn an_answer_without_the_rating_fields_is_unreadable() {
        for client in [
            FakeNavidrome::minimal_client as fn(&FakeNavidrome, &str),
            FakeNavidrome::legacy_client,
        ] {
            let lib = Library::new().await;
            client(&lib.fake, "bob");
            assert_eq!(
                refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
                FailureCode::DeleteVetoUnknown
            );
            let listed = NavidromeReadFailureRow::all_in(&lib.db).await.unwrap();
            assert_eq!(
                listed
                    .iter()
                    .map(|l| l.username.as_str())
                    .collect::<Vec<_>>(),
                vec!["bob"]
            );
        }
    }

    #[tokio::test]
    async fn a_visible_song_without_its_album_is_unreadable() {
        let lib = Library::new().await;
        lib.fake.lose_album("al1");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (FailureCode::DeleteVetoUnknown, VETO.into())
        );
        let listed = NavidromeReadFailureRow::all_in(&lib.db).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|l| (l.username.as_str(), l.error.as_str()))
                .collect::<Vec<_>>(),
            vec![(
                "bob",
                "Navidrome does not show this account the album of a song it shows"
            )]
        );
    }

    #[tokio::test]
    async fn a_user_never_linked_to_navidrome_is_not_read() {
        let lib = Library::new().await;
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
        assert_eq!(
            (lib.fake.reads_by("bob") > 0, lib.fake.reads_by("carol")),
            (true, 0)
        );
    }

    #[tokio::test]
    async fn a_symlink_out_of_the_folder_is_refused_before_the_rules() {
        let lib = Library::new().await;
        std::fs::create_dir_all(lib.root.join("outside")).unwrap();
        std::fs::write(lib.root.join("outside/z.flac"), b"audio").unwrap();
        std::os::unix::fs::symlink(
            lib.root.join("outside/z.flac"),
            lib.root.join("music/alice/link.flac"),
        )
        .unwrap();
        lib.fake.file(
            NativeSong {
                id: "s2".into(),
                path: "alice/link.flac".into(),
                size: 5,
                ..lib.record("s1")
            },
            "al1",
        );
        lib.fake.sees("alice", "s2");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s2").await),
            (
                FailureCode::DeletePathUnresolved,
                "Navidrome's path for this song passes through a symbolic link: alice/link.flac"
                    .into()
            )
        );
        assert!(lib.root.join("outside/z.flac").exists());
    }

    #[tokio::test]
    async fn a_file_outside_every_folder_is_outside_the_library() {
        let lib = Library::new().await;
        lib.song("s2", "carol/z.flac", b"audio");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s2").await),
            (
                FailureCode::DeleteOutsideLibrary,
                format!(
                    "{} is not inside one of your library folders",
                    canonical(&lib, "music/carol/z.flac").display()
                )
            )
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_file_in_another_users_nested_folder_is_theirs_and_a_tie_is_each_ones() {
        // alice's folder `music` is shared over bob's `music/bob`.
        let lib = Library::new().await;
        lib.folder("alice", "alice-top", "music", true).await;
        lib.song("s3", "bob/B/y.flac", b"audio");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s3").await).0,
            FailureCode::DeleteOutsideLibrary
        );
        // bob's second folder at alice's path: the deepest folder ties, so each owns the file.
        lib.folder("bob", "bob-alice", "music/alice", false).await;
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
    }

    #[tokio::test]
    async fn a_directory_or_a_file_that_is_not_audio_is_not_a_track() {
        let lib = Library::new().await;
        lib.song("s4", "alice/A/cover.jpg", b"jpeg!");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s4").await),
            (
                FailureCode::DeleteNotATrack,
                format!(
                    "{} is not an audio file",
                    canonical(&lib, "music/alice/A/cover.jpg").display()
                )
            )
        );
        // A directory whose size Navidrome's record happens to give.
        std::fs::create_dir_all(lib.root.join("music/alice/B/dir.flac")).unwrap();
        let size = std::fs::metadata(lib.root.join("music/alice/B/dir.flac"))
            .unwrap()
            .len();
        lib.fake.file(
            NativeSong {
                id: "s5".into(),
                path: "alice/B/dir.flac".into(),
                size,
                ..lib.record("s1")
            },
            "al1",
        );
        lib.fake.sees("alice", "s5");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s5").await),
            (
                FailureCode::DeleteNotATrack,
                format!(
                    "{} is not a regular file",
                    canonical(&lib, "music/alice/B/dir.flac").display()
                )
            )
        );
    }

    #[tokio::test]
    async fn a_job_that_still_wants_the_recording_keeps_the_file() {
        let lib = Library::new().await;
        // Step 5's jobs row: same folder, same recording by artist and title as normalize_for_matching
        // reads them, still downloading.
        lib.job(
            "j1",
            "alice-music",
            "downloading",
            wanted("marrow  choir", "O Holy Night!", None),
        )
        .await;
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (
                FailureCode::DeleteInUse,
                "A download or import that is not finished wants this track".into()
            )
        );
        lib.set_job_state("j1", "done").await;
        library_verdict(lib.guard().check_song(&lib.alice(), "s1").await);
    }

    #[tokio::test]
    async fn a_job_the_check_is_made_for_does_not_keep_the_file() {
        let lib = Library::new().await;
        lib.job(
            "j1",
            "alice-music",
            "importing",
            wanted("Marrow Choir", "O Holy Night", None),
        )
        .await;
        let song = lib
            .fake
            .native_song(&lib.alice(), "s1")
            .await
            .unwrap()
            .unwrap();
        let path = paths::local_file_in(&lib.db, &song, None).await.unwrap();
        let guard = lib.guard();
        let kept = guard
            .check_resolved(&lib.alice(), song.clone(), path.clone(), &["j2".into()])
            .await
            .unwrap_err();
        assert_eq!(kept.code, FailureCode::DeleteInUse);
        let checked = guard
            .check_resolved(&lib.alice(), song, path, &["j1".into()])
            .await
            .unwrap();
        assert_eq!(checked.path, canonical(&lib, "music/alice/A/x.flac"));
    }

    #[tokio::test]
    async fn rule_five_reads_the_recording_an_album_query_and_fails_closed_on_what_it_cannot_read()
    {
        let mbid = "0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c";
        let album = Album {
            id: "rel-1".into(),
            title: "Winter Songs".into(),
            artist: "Marrow Choir".into(),
            release_date: None,
            mbid: None,
            cover_url: None,
        };
        let cases: [(&str, &str, DownloadQuery, bool); 8] = [
            (
                "alice-music",
                "queued",
                wanted("Someone Else", "Another Title", Some(mbid)),
                true,
            ),
            (
                "alice-music",
                "searching",
                DownloadQuery::new(vec![]).album(album.clone()),
                true,
            ),
            (
                "alice-music",
                "queued",
                DownloadQuery::new(vec![]).album(Album {
                    title: "Summer Songs".into(),
                    ..album.clone()
                }),
                false,
            ),
            // An album job wants every track of its album, hydrated or not.
            (
                "alice-music",
                "downloading",
                wanted("Marrow Choir", "Silent Night", None).album(album.clone()),
                true,
            ),
            (
                "alice-music",
                "needs_decision",
                wanted("Marrow Choir", "Silent Night", None),
                false,
            ),
            (
                "bob-music",
                "downloading",
                wanted("Marrow Choir", "O Holy Night", None),
                false,
            ),
            // A job in an outer folder may import into alice's.
            (
                "alice-top",
                "importing",
                wanted("Marrow Choir", "O Holy Night", None),
                true,
            ),
            // A state JobState::parse does not know.
            (
                "alice-music",
                "paused",
                wanted("Marrow Choir", "Silent Night", None),
                true,
            ),
        ];
        for (i, (folder, state, query, blocks)) in cases.into_iter().enumerate() {
            let lib = Library::new().await;
            lib.folder("alice", "alice-top", "music", true).await;
            lib.fake.file(
                NativeSong {
                    mbz_recording_id: Some(mbid.into()),
                    ..lib.record("s1")
                },
                "al1",
            );
            lib.job("j1", folder, state, query).await;
            let verdict = lib.guard().check_song(&lib.alice(), "s1").await;
            match blocks {
                true => assert_eq!(refusal(verdict).0, FailureCode::DeleteInUse, "case {i}"),
                false => {
                    library_verdict(verdict);
                }
            }
        }
        // A `wanted` that does not parse.
        let lib = Library::new().await;
        sqlx::query(
            "INSERT INTO jobs (id, user_id, folder_id, created_at, updated_at, wanted, import_mode, state) \
             VALUES ('j2', 'alice', 'alice-music', 1763200000, 1763200000, '{\"tracks\":7}', 'album', 'queued')",
        )
        .execute(&lib.db)
        .await
        .unwrap();
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteInUse
        );
    }

    #[tokio::test]
    async fn a_pending_discovery_track_is_handed_back_by_song_id_or_exact_path() {
        let lib = Library::new().await;
        lib.song(
            "s5",
            "alice/Discovery/Explore/Marrow Choir/03.flac",
            b"audio",
        );
        lib.pending_track(
            "t1",
            "alice-music",
            "Explore",
            &lib.root
                .join("music/alice/Discovery/Explore/Marrow Choir/03.flac"),
            None,
        )
        .await;
        assert!(
            matches!(lib.guard().check_song(&lib.alice(), "s5").await, Ok(Verdict::Discovery { track, rating: 0 }) if track.id == "t1")
        );
        lib.pending_track(
            "t2",
            "alice-music",
            "Explore",
            &lib.root.join("music/alice/elsewhere.flac"),
            Some("s1"),
        )
        .await;
        assert!(
            matches!(lib.guard().check_song(&lib.alice(), "s1").await, Ok(Verdict::Discovery { track, rating: 1 }) if track.id == "t2")
        );
        // By song id before any path: Navidrome's size for s6 does not match the file on disk.
        lib.song("s6", "alice/Discovery/Explore/06.flac", b"audio");
        lib.fake.file(
            NativeSong {
                size: 9,
                ..lib.record("s6")
            },
            "al1",
        );
        lib.pending_track(
            "t3",
            "alice-music",
            "Explore",
            &lib.root.join("music/alice/Discovery/Explore/06.flac"),
            Some("s6"),
        )
        .await;
        assert!(
            matches!(lib.guard().check_song(&lib.alice(), "s6").await, Ok(Verdict::Discovery { track, .. }) if track.id == "t3")
        );
        // Handing a track back neither removes nor marks it.
        for id in ["t1", "t2", "t3"] {
            assert_eq!(
                (lib.track_status(id).await, lib.history_outcome(id).await),
                ("Pending".to_string(), "suggested".to_string())
            );
        }
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_native_path_that_leaves_its_library_is_refused_even_when_a_file_is_there() {
        let lib = Library::new().await;
        std::fs::create_dir_all(lib.root.join("outside")).unwrap();
        std::fs::write(lib.root.join("outside/z.flac"), b"audio").unwrap();
        let absolute = lib
            .root
            .join("outside/z.flac")
            .to_string_lossy()
            .into_owned();
        for (id, path) in [("s6", "../outside/z.flac".to_string()), ("s7", absolute)] {
            lib.fake.file(
                NativeSong {
                    id: id.into(),
                    path: path.clone(),
                    size: 5,
                    ..lib.record("s1")
                },
                "al1",
            );
            lib.fake.sees("alice", id);
            assert_eq!(
                refusal(lib.guard().check_song(&lib.alice(), id).await),
                (
                    FailureCode::DeletePathUnresolved,
                    format!("Navidrome's path for this song is not inside its library: {path}")
                )
            );
        }
        assert!(lib.root.join("outside/z.flac").exists());
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_file_an_unresolvable_folder_of_another_user_may_hold_is_kept_without_naming_that_folder(
    ) {
        let lib = Library::new().await;
        // Stored with `..`: it neither resolves nor compares.
        lib.folder("bob", "bob-odd", "music/alice/../bob", false)
            .await;
        let f = lib
            .guard()
            .check_song(&lib.alice(), "s1")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.message, f.cause),
            (
                FailureCode::DeleteOutsideLibrary,
                format!(
                    "{} may be inside a folder of another user",
                    canonical(&lib, "music/alice/A/x.flac").display()
                ),
                None
            )
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    /// A second record of `music/alice/A/x.flac`: library 2 at `{root}/music/alice`.
    fn second_record(lib: &Library) -> NativeSong {
        NativeSong {
            id: "s1b".into(),
            library_id: 2,
            library_path: lib.root.join("music/alice").to_string_lossy().into_owned(),
            path: "A/x.flac".into(),
            ..lib.record("s1")
        }
    }

    #[tokio::test]
    async fn a_second_song_of_the_file_keeps_it_and_says_who_sees_it_only_to_its_viewer() {
        for viewer in ["bob", "alice"] {
            let lib = Library::new().await;
            lib.fake.file(second_record(&lib), "al1");
            lib.fake.sees(viewer, "s1b");
            let expected = match viewer {
                "bob" => (FailureCode::DeleteVetoUnknown, VETO.to_string()),
                _ => (
                    FailureCode::DeletePathUnresolved,
                    format!(
                        "Navidrome lists {} as more than one song",
                        canonical(&lib, "music/alice/A/x.flac").display()
                    ),
                ),
            };
            assert_eq!(
                refusal(lib.guard().check_song(&lib.alice(), "s1").await),
                expected
            );
            assert_eq!(lib.fake.ratings_set(), vec![]);
            assert!(lib.root.join("music/alice/A/x.flac").exists());
        }
    }

    #[tokio::test]
    async fn another_users_song_that_links_to_the_file_is_the_same_file() {
        // Navidrome follows links and lists bob's link as a song of its own.
        let lib = Library::new().await;
        std::os::unix::fs::symlink(
            lib.root.join("music/alice/A/x.flac"),
            lib.root.join("music/bob/x.flac"),
        )
        .unwrap();
        lib.fake.file(
            NativeSong {
                id: "s8".into(),
                path: "bob/x.flac".into(),
                ..lib.record("s1")
            },
            "al1",
        );
        lib.fake.sees("bob", "s8");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (FailureCode::DeleteVetoUnknown, VETO.into())
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_song_its_own_search_does_not_find_cannot_be_checked() {
        let lib = Library::new().await;
        lib.fake.unsearchable("s1");
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (
                FailureCode::DeletePathUnresolved,
                format!(
                    "Navidrome cannot be searched for the tags of {}",
                    canonical(&lib, "music/alice/A/x.flac").display()
                )
            )
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn navidrome_before_0_60_or_of_an_unknown_version_keeps_every_file() {
        for version in [Some("0.59.0 (fa46aefa)"), Some("dev"), None] {
            let lib = Library::new().await;
            lib.fake.reports_version(version);
            assert_eq!(
                refusal(lib.guard().check_song(&lib.alice(), "s1").await),
                (
                    FailureCode::DeleteVetoUnknown,
                    format!(
                        "Navidrome 0.60 or newer is needed to see other listeners' ratings (this server reports {})",
                        version.unwrap_or("no version")
                    )
                )
            );
            assert_eq!(lib.fake.ratings_set(), vec![]);
        }
        let lib = Library::new().await;
        lib.fake
            .version_unreadable(|| NavidromeLookupError::Client(NavidromeError::CircuitOpen));
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (FailureCode::DeleteVetoUnknown, VETO.into())
        );
        assert_eq!(lib.fake.ratings_set(), vec![]);
    }

    #[tokio::test]
    async fn a_file_nobody_rated_has_no_average_and_passes_rule_four() {
        let lib = Library::new().await;
        lib.song("s2", "alice/A/y.flac", b"audio");
        let checked = library_verdict(lib.guard().check_song(&lib.alice(), "s2").await);
        assert_eq!(
            (checked.song.rating, checked.song.average_rating),
            (0, None)
        );
        assert_eq!(
            lib.fake.ratings_set(),
            vec![("alice".to_string(), "s2".to_string(), 0)]
        );
    }

    #[tokio::test]
    async fn an_own_rating_with_no_average_means_navidrome_sends_none() {
        let lib = Library::new().await;
        lib.fake.without_averages();
        lib.fake.rates("alice", "s1", 3);
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await),
            (FailureCode::DeleteVetoUnknown, VETO.into())
        );
        assert_eq!(
            lib.fake.ratings_set(),
            vec![("alice".to_string(), "s1".to_string(), 3)]
        );
    }

    #[tokio::test]
    async fn a_rating_changed_during_the_check_is_the_one_re_sent_and_handed_back() {
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 1);
        let stale = lib
            .fake
            .native_song(&lib.alice(), "s1")
            .await
            .unwrap()
            .unwrap();
        let path = paths::local_file_in(&lib.db, &stale, None).await.unwrap();
        lib.fake.rates("alice", "s1", 5);
        let checked = lib
            .guard()
            .check_resolved(&lib.alice(), stale, path, &[])
            .await
            .unwrap();
        assert_eq!(
            lib.fake.ratings_set(),
            vec![("alice".to_string(), "s1".to_string(), 5)]
        );
        assert_eq!(
            (checked.song.rating, checked.song.average_rating),
            (5, Some(5.0))
        );
    }

    #[tokio::test]
    async fn an_average_above_the_callers_own_share_blocks() {
        let lib = Library::new().await;
        lib.fake.rates("alice", "s1", 3);
        lib.fake.hidden_rater("s1", 5);
        assert_eq!(
            refusal(lib.guard().check_song(&lib.alice(), "s1").await).0,
            FailureCode::DeleteBlockedByOtherUser
        );
    }

    #[test]
    fn guard_refusals_keep_their_text() {
        use FailureCode::*;
        use RetryAdvice::*;
        for (f, code, stage, retry, message) in [
            (
                song_not_found(),
                UnknownHandle,
                Stage::Input,
                NotRetryable,
                "Song not found",
            ),
            (
                blocked_by_other_user(),
                DeleteBlockedByOtherUser,
                Stage::Delete,
                NotRetryable,
                "Another listener rated or starred this track",
            ),
            (
                veto_unknown(),
                DeleteVetoUnknown,
                Stage::Delete,
                SameSourceLater,
                VETO,
            ),
            (
                in_use(),
                DeleteInUse,
                Stage::Delete,
                SameSourceLater,
                "A download or import that is not finished wants this track",
            ),
        ] {
            assert_eq!(
                (f.code, f.stage, f.retry, f.message.as_str()),
                (code, stage, retry, message)
            );
            assert_eq!(f.user_action, None);
        }
    }
}
