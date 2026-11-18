//! Deleting library files for a connected app (design 13): a preview checks each song and
//! issues a confirmation token for the ones every rule allows; a confirm takes the token and
//! deletes each previewed file that every rule still allows and that is still the file the
//! preview checked. Nothing else here removes a file: the guard's carry_out does.

pub(crate) mod tokens;

use std::path::PathBuf;

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::navidrome::{DeletionReason, DiscoveryTrack};
use soulbeet::navidrome::models::NativeSong;

use crate::auth::UserId;
use crate::core::delete_guard::quota::room_in;
use crate::core::delete_guard::{
    changed_since_check, song_not_found, Checked, ForgetVia, Guard, Origin, Verdict,
};
use crate::core::{database_failure, utc_text};
use crate::import_queue::ImportQueue;
use tokens::{take_in, DeleteTokenRefusal};

/// Most songs one preview checks, and one token holds.
pub(crate) const SONGS_PER_TOKEN: usize = 25;

/// How long a token is valid after its preview, in seconds.
pub(crate) const TOKEN_SECS: i64 = 600;

/// How long the yes a replace_existing token carried holds for its job's import, from the take
/// (D9).
pub(crate) const REPLACE_CONSENT_SECS: i64 = 24 * 60 * 60;

/// The yes to replace a copy, given by a replace_existing token, ran out at `at` (D9).
pub(crate) fn replace_consent_expired(at: chrono::DateTime<chrono::Utc>) -> Failure {
    Failure::new(
        FailureCode::ExpiredHandle,
        Stage::Delete,
        RetryAdvice::NotRetryable,
        format!(
            "The confirmation to replace this copy ran out at {}; preview it again with reason replaced",
            utc_text(at)
        ),
    )
}

/// The caller as deletion needs it: the user and the app (OAuth grant) it acts through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct App {
    pub user: UserId,
    pub grant_id: String,
}

/// What a preview found for one song.
#[derive(Debug)]
pub(crate) enum PreviewOutcome {
    /// Every rule allows deleting its file; the token holds it.
    Allowed(Checked),
    /// A pending discovery track of the caller, which remove_discovery_track removes.
    Discovery(DiscoveryTrack),
    /// The rule that keeps it, or why it could not be checked.
    Blocked(Failure),
}

/// One song of a preview, with the caller's record of it when Navidrome gave one.
#[derive(Debug)]
pub(crate) struct PreviewEntry {
    pub song_id: String,
    pub record: Option<NativeSong>,
    pub outcome: PreviewOutcome,
}

/// A stored token: its secret, which only the preview's answer carries, when it expires and the
/// songs it allows deleting.
#[derive(Debug)]
pub(crate) struct IssuedToken {
    pub secret: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub song_ids: Vec<String>,
}

/// A preview: each song in input order, the token for the allowed ones (None when none is), and
/// the app deletions of the last 24 hours.
#[derive(Debug)]
pub(crate) struct Preview {
    pub entries: Vec<PreviewEntry>,
    pub token: Option<IssuedToken>,
    pub used_today: u32,
}

/// Design 13's preview of deleting `song_ids` for `reason`, reading the time from `clock`. The
/// day's room for every song is read first, on a connection given back before any Navidrome
/// read (D4), so a request that cannot fit reads nothing. Then per song, in order: the caller's
/// record (read_song; none is Blocked with song_not_found, a read that failed Blocked with its
/// failure) and its check (check_record); a token for the Allowed ones (tokens::issue_in), none
/// when no song is allowed, valid TOKEN_SECS from when it is stored, after the checks. Nothing is
/// deleted.
pub(crate) async fn preview(
    guard: &Guard<'_>,
    app: &App,
    song_ids: &[String],
    reason: DeletionReason,
    clock: impl Fn() -> i64,
) -> Result<Preview, Failure> {
    let used_today = {
        let mut conn = guard
            .db
            .acquire()
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?;
        room_in(&mut conn, &app.user, song_ids.len() as u32, clock()).await?
    };
    let mut entries = Vec::with_capacity(song_ids.len());
    for song_id in song_ids {
        let (record, outcome) = match guard.read_song(&app.user, song_id).await {
            Err(f) => (None, PreviewOutcome::Blocked(f)),
            Ok(None) => (None, PreviewOutcome::Blocked(song_not_found())),
            Ok(Some(song)) => {
                let outcome = match guard.check_record(&app.user, song.clone()).await {
                    Ok(Verdict::Library(checked)) => PreviewOutcome::Allowed(checked),
                    Ok(Verdict::Discovery { track, .. }) => PreviewOutcome::Discovery(track),
                    Err(f) => PreviewOutcome::Blocked(f),
                };
                (Some(song), outcome)
            }
        };
        entries.push(PreviewEntry {
            song_id: song_id.clone(),
            record,
            outcome,
        });
    }
    let files: Vec<_> = (entries.iter())
        .filter_map(|e| match &e.outcome {
            PreviewOutcome::Allowed(checked) => Some(checked.previewed()),
            _ => None,
        })
        .collect();
    let token = match files.is_empty() {
        true => None,
        false => Some(tokens::issue_in(guard.db, app, reason, &files, clock()).await?),
    };
    Ok(Preview {
        entries,
        token,
        used_today,
    })
}

/// What came of one previewed file.
#[derive(Debug)]
pub(crate) enum ConfirmOutcome {
    /// Deleted: the caller's record of the song as the check read it, and the deletion's
    /// warnings.
    Deleted {
        song: Box<NativeSong>,
        warnings: Vec<String>,
    },
    /// Kept: the rule that keeps it, or why it was not removed.
    Refused(Failure),
}

/// One previewed file of a confirm: its song, its path as the preview checked it, and what came
/// of it.
#[derive(Debug)]
pub(crate) struct ConfirmEntry {
    pub song_id: String,
    pub file: PathBuf,
    pub outcome: ConfirmOutcome,
}

/// A confirm: the token's reason and each of its files in preview order.
#[derive(Debug)]
pub(crate) struct Confirmed {
    pub reason: DeletionReason,
    pub entries: Vec<ConfirmEntry>,
}

/// Takes the token `secret` (tokens::take_in), then per previewed file in order: every rule
/// again (check_song); a song that became a pending discovery track is Refused with
/// changed_since_check; a library file must be the previewed one (Checked::is_in, D8); then
/// carry_out as the app, forgetting through `queue`. A refused take deletes nothing and leaves
/// the token unused.
pub(crate) async fn confirm(
    guard: &Guard<'_>,
    queue: &ImportQueue,
    app: &App,
    secret: &str,
    now: i64,
) -> Result<Confirmed, DeleteTokenRefusal> {
    let taken = take_in(guard.db, app, secret, None, now).await?;
    let origin = Origin::Mcp {
        grant_id: app.grant_id.clone(),
    };
    let mut entries = Vec::with_capacity(taken.files.len());
    for file in taken.files {
        let outcome = match guard.check_song(&app.user, &file.song_id).await {
            Err(f) => ConfirmOutcome::Refused(f),
            Ok(Verdict::Discovery { .. }) => ConfirmOutcome::Refused(changed_since_check()),
            Ok(Verdict::Library(checked)) => match checked.is_in(std::slice::from_ref(&file)) {
                Err(f) => ConfirmOutcome::Refused(f),
                Ok(()) => {
                    let song = checked.song.clone();
                    let forget = ForgetVia::Queue(queue.clone());
                    let removed =
                        guard.carry_out(&app.user, checked, taken.reason, &origin, forget);
                    match removed.await {
                        Ok(warnings) => ConfirmOutcome::Deleted { song, warnings },
                        Err(not_removed) => ConfirmOutcome::Refused(not_removed.into_failure()),
                    }
                }
            },
        };
        entries.push(ConfirmEntry {
            song_id: file.song_id,
            file: file.path,
            outcome,
        });
    }
    Ok(Confirmed {
        reason: taken.reason,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use shared::failure::FailureCode;
    use soulbeet::NavidromeError;

    use super::tokens::{token_not_found, token_used, TokenRefusalKind};
    use super::*;
    use crate::core::delete_guard::quota::permission_not_granted;
    use crate::core::delete_guard::test_support::Library;
    use crate::db::test_db;
    use crate::import_queue::test_support::{RecordingDeps, StubImporter};
    use crate::services::NavidromeLookupError;

    fn queue() -> ImportQueue {
        ImportQueue::new(RecordingDeps::new(Arc::new(StubImporter::new())))
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn alice_app() -> App {
        App {
            user: UserId::new("alice"),
            grant_id: "g1".into(),
        }
    }

    fn deleted(c: &Confirmed) -> Vec<&str> {
        (c.entries.iter())
            .filter(|e| matches!(e.outcome, ConfirmOutcome::Deleted { .. }))
            .map(|e| e.song_id.as_str())
            .collect()
    }

    fn refused(c: &Confirmed) -> Vec<(&str, FailureCode)> {
        (c.entries.iter())
            .filter_map(|e| match &e.outcome {
                ConfirmOutcome::Refused(f) => Some((e.song_id.as_str(), f.code)),
                _ => None,
            })
            .collect()
    }

    async fn audit_rows(lib: &Library) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM deletion_reviews")
            .fetch_one(&lib.db)
            .await
            .unwrap()
    }

    /// The token of a preview of s1 alone for `reason` at `now`.
    async fn s1_token(lib: &Library, reason: DeletionReason, now: i64) -> IssuedToken {
        preview(&lib.guard(), &alice_app(), &ids(&["s1"]), reason, || now)
            .await
            .unwrap()
            .token
            .unwrap()
    }

    #[tokio::test]
    async fn a_preview_deletes_nothing_and_tokens_only_the_allowed_songs() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        lib.fake.rates("bob", "s2", 4);
        lib.song("s5", "alice/Discovery/Explore/03.flac", b"audio");
        let staged = lib
            .pending_track(
                "t1",
                "alice-music",
                "Explore",
                &lib.root.join("music/alice/Discovery/Explore/03.flac"),
                Some("s5"),
            )
            .await;
        let now = chrono::Utc::now().timestamp();
        let p = preview(
            &lib.guard(),
            &alice_app(),
            &ids(&["s1", "s2", "s5", "gone"]),
            DeletionReason::Cleanup,
            || now,
        )
        .await
        .unwrap();
        assert!(matches!(&p.entries[0].outcome, PreviewOutcome::Allowed(_)));
        assert!(
            matches!(&p.entries[1].outcome, PreviewOutcome::Blocked(f) if f.code == FailureCode::DeleteBlockedByOtherUser)
        );
        assert!(matches!(&p.entries[2].outcome, PreviewOutcome::Discovery(t) if t.id == staged.id));
        assert!(
            matches!(&p.entries[3].outcome, PreviewOutcome::Blocked(f) if *f == song_not_found())
        );
        assert_eq!(
            p.entries[1].record.as_ref().map(|r| r.title.as_str()),
            Some("O Holy Night"),
            "a blocked song keeps its facts"
        );
        assert_eq!(p.used_today, 0);
        let token = p.token.unwrap();
        assert_eq!(
            (token.song_ids, token.expires_at.timestamp()),
            (ids(&["s1"]), now + TOKEN_SECS)
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        assert_eq!(audit_rows(&lib).await, 0);
        let stored: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM delete_tokens")
            .fetch_all(&lib.db)
            .await
            .unwrap();
        assert_eq!(
            stored,
            vec![crate::oauth::secrets::hash(&token.secret)],
            "only the hash is stored"
        );
    }

    #[tokio::test]
    async fn nothing_allowed_gives_no_token() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        lib.fake.rates("bob", "s1", 5);
        let now = chrono::Utc::now().timestamp();
        let p = preview(
            &lib.guard(),
            &alice_app(),
            &ids(&["s1"]),
            DeletionReason::Disliked,
            || now,
        )
        .await
        .unwrap();
        assert!(p.token.is_none());
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM delete_tokens")
            .fetch_one(&lib.db)
            .await
            .unwrap();
        assert_eq!(stored, 0);
    }

    #[tokio::test]
    async fn a_preview_over_the_days_room_is_refused_before_anything_is_read() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        lib.app_deletions(90, 3_600).await;
        let many: Vec<String> = (0..11).map(|i| format!("s{i}")).collect();
        let now = chrono::Utc::now().timestamp();
        let f = preview(
            &lib.guard(),
            &alice_app(),
            &many,
            DeletionReason::Cleanup,
            || now,
        )
        .await
        .unwrap_err();
        assert_eq!(f.code, FailureCode::DeleteDailyLimit);
        assert_eq!(lib.fake.reads_by("alice"), 0);
        // Ten fit, and the preview says how many were used.
        let p = preview(
            &lib.guard(),
            &alice_app(),
            &many[..10],
            DeletionReason::Cleanup,
            || now,
        )
        .await
        .unwrap();
        assert_eq!(p.used_today, 90);
    }

    #[tokio::test]
    async fn a_previewed_song_is_deleted_once_confirmed() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let now = chrono::Utc::now().timestamp();
        let token = s1_token(&lib, DeletionReason::Cleanup, now).await;
        let done = confirm(
            &lib.guard(),
            &queue(),
            &alice_app(),
            &token.secret,
            now + 60,
        )
        .await
        .unwrap();
        assert_eq!(
            (done.reason, deleted(&done)),
            (DeletionReason::Cleanup, vec!["s1"])
        );
        assert!(!lib.root.join("music/alice/A/x.flac").exists());
        let row: (String, String, Option<String>) =
            sqlx::query_as("SELECT origin, reason, client FROM deletion_reviews")
                .fetch_one(&lib.db)
                .await
                .unwrap();
        assert_eq!(
            row,
            ("mcp".into(), "cleanup".into(), Some("claude.ai".into()))
        );
    }

    #[tokio::test]
    async fn a_token_of_another_user_or_another_app_is_not_found() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        lib.app("g2", "bob", true).await;
        lib.app("g3", "alice", true).await;
        let now = chrono::Utc::now().timestamp();
        let token = s1_token(&lib, DeletionReason::Cleanup, now).await;
        for app in [
            App {
                user: UserId::new("bob"),
                grant_id: "g2".into(),
            },
            App {
                user: UserId::new("alice"),
                grant_id: "g3".into(),
            },
            App {
                user: UserId::new("bob"),
                grant_id: "g1".into(),
            },
        ] {
            let r = confirm(&lib.guard(), &queue(), &app, &token.secret, now)
                .await
                .unwrap_err();
            assert_eq!(
                r,
                DeleteTokenRefusal {
                    kind: TokenRefusalKind::NotFound,
                    failure: token_not_found(),
                    song_ids: vec![],
                    reason: None
                },
                "{app:?}"
            );
        }
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        let done = confirm(&lib.guard(), &queue(), &alice_app(), &token.secret, now)
            .await
            .unwrap();
        assert_eq!(deleted(&done), vec!["s1"], "still unused");
    }

    #[tokio::test]
    async fn a_token_works_once_even_when_two_confirms_race() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new_on(test_db::migrated_file(dir.path()).await).await;
        lib.app("g1", "alice", true).await;
        let now = chrono::Utc::now().timestamp();
        let token = s1_token(&lib, DeletionReason::Cleanup, now).await;
        let (guard, q, alice) = (lib.guard(), queue(), alice_app());
        let (a, b) = tokio::join!(
            confirm(&guard, &q, &alice, &token.secret, now),
            confirm(&guard, &q, &alice, &token.secret, now)
        );
        let refusals: Vec<(TokenRefusalKind, Failure)> = [a, b]
            .into_iter()
            .filter_map(|r| r.err())
            .map(|r| (r.kind, r.failure))
            .collect();
        assert_eq!(refusals, vec![(TokenRefusalKind::Used, token_used())]);
        assert_eq!(audit_rows(&lib).await, 1);
        let again = confirm(&guard, &q, &alice, &token.secret, now)
            .await
            .unwrap_err();
        assert_eq!(
            (again.kind, again.song_ids, again.reason),
            (
                TokenRefusalKind::Used,
                ids(&["s1"]),
                Some(DeletionReason::Cleanup)
            )
        );
    }

    #[tokio::test]
    async fn an_expired_token_is_refused_and_names_its_songs() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let now = 1_800_000_000;
        let token = s1_token(&lib, DeletionReason::Disliked, now).await;
        let r = confirm(
            &lib.guard(),
            &queue(),
            &alice_app(),
            &token.secret,
            now + TOKEN_SECS,
        )
        .await
        .unwrap_err();
        assert_eq!(r.kind, TokenRefusalKind::Expired);
        assert_eq!(
            r.failure.message,
            "This confirmation token expired at 2027-01-15 08:10:00 UTC; preview the songs again"
        );
        assert_eq!(
            (r.song_ids, r.reason),
            (ids(&["s1"]), Some(DeletionReason::Disliked))
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn the_switch_turned_off_after_the_preview_stops_the_confirm() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let now = chrono::Utc::now().timestamp();
        let token = s1_token(&lib, DeletionReason::Cleanup, now).await;
        sqlx::query("UPDATE oauth_grants SET allow_delete = 0 WHERE id = 'g1'")
            .execute(&lib.db)
            .await
            .unwrap();
        let r = confirm(&lib.guard(), &queue(), &alice_app(), &token.secret, now)
            .await
            .unwrap_err();
        assert_eq!(
            (r.kind, r.failure),
            (TokenRefusalKind::Permission, permission_not_granted())
        );
        assert!(lib.root.join("music/alice/A/x.flac").exists());
        sqlx::query("UPDATE oauth_grants SET allow_delete = 1 WHERE id = 'g1'")
            .execute(&lib.db)
            .await
            .unwrap();
        let done = confirm(&lib.guard(), &queue(), &alice_app(), &token.secret, now)
            .await
            .unwrap();
        assert_eq!(
            deleted(&done),
            vec!["s1"],
            "a refused take leaves it unused"
        );
    }

    #[tokio::test]
    async fn a_token_past_the_days_room_is_refused_and_left_unused() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        let now = chrono::Utc::now().timestamp();
        let token = s1_token(&lib, DeletionReason::Cleanup, now).await;
        lib.app_deletions(100, 60).await;
        let r = confirm(&lib.guard(), &queue(), &alice_app(), &token.secret, now)
            .await
            .unwrap_err();
        assert_eq!(
            (r.kind, r.failure.code, r.song_ids),
            (
                TokenRefusalKind::Room,
                FailureCode::DeleteDailyLimit,
                ids(&["s1"])
            )
        );
        let used: Option<i64> = sqlx::query_scalar("SELECT used_at FROM delete_tokens")
            .fetch_one(&lib.db)
            .await
            .unwrap();
        assert_eq!(used, None);
        assert!(lib.root.join("music/alice/A/x.flac").exists());
    }

    #[tokio::test]
    async fn a_file_rewritten_or_rated_after_the_preview_is_kept() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        let now = chrono::Utc::now().timestamp();
        let token = preview(
            &lib.guard(),
            &alice_app(),
            &ids(&["s1", "s2"]),
            DeletionReason::Cleanup,
            || now,
        )
        .await
        .unwrap()
        .token
        .unwrap();
        let x = lib.root.join("music/alice/A/x.flac");
        std::fs::remove_file(&x).unwrap();
        // Same size, another file.
        std::fs::write(&x, b"other").unwrap();
        lib.fake.rates("bob", "s2", 4);
        let done = confirm(&lib.guard(), &queue(), &alice_app(), &token.secret, now)
            .await
            .unwrap();
        assert_eq!(
            refused(&done),
            vec![
                ("s1", FailureCode::DeleteChangedSincePreview),
                ("s2", FailureCode::DeleteBlockedByOtherUser)
            ]
        );
        assert_eq!(std::fs::read(&x).unwrap(), b"other");
        assert!(lib.root.join("music/alice/B/y.flac").exists());
        assert_eq!(audit_rows(&lib).await, 0);
    }

    #[tokio::test]
    async fn a_directory_swapped_for_a_symlink_after_the_preview_deletes_nothing() {
        let lib = Library::new().await;
        lib.app("g1", "alice", true).await;
        lib.song("s2", "alice/B/y.flac", b"audio");
        let now = chrono::Utc::now().timestamp();
        let token = preview(
            &lib.guard(),
            &alice_app(),
            &ids(&["s1", "s2"]),
            DeletionReason::Cleanup,
            || now,
        )
        .await
        .unwrap()
        .token
        .unwrap();
        // A leaves the library: the same inode reached through a link to a folder no user
        // registered.
        std::fs::create_dir_all(lib.root.join("elsewhere")).unwrap();
        std::fs::rename(
            lib.root.join("music/alice/A"),
            lib.root.join("music/alice/A.moved"),
        )
        .unwrap();
        std::fs::hard_link(
            lib.root.join("music/alice/A.moved/x.flac"),
            lib.root.join("elsewhere/x.flac"),
        )
        .unwrap();
        std::os::unix::fs::symlink(lib.root.join("elsewhere"), lib.root.join("music/alice/A"))
            .unwrap();
        // B stays inside the folder but its canonical path moves.
        std::fs::rename(
            lib.root.join("music/alice/B"),
            lib.root.join("music/alice/B.moved"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            lib.root.join("music/alice/B.moved"),
            lib.root.join("music/alice/B"),
        )
        .unwrap();
        let done = confirm(&lib.guard(), &queue(), &alice_app(), &token.secret, now)
            .await
            .unwrap();
        // The path step refuses a song whose path passes through a link, before rule 1.
        assert_eq!(
            refused(&done),
            vec![
                ("s1", FailureCode::DeletePathUnresolved),
                ("s2", FailureCode::DeletePathUnresolved)
            ]
        );
        for kept in [
            "elsewhere/x.flac",
            "music/alice/A.moved/x.flac",
            "music/alice/B.moved/y.flac",
        ] {
            assert!(lib.root.join(kept).exists(), "{kept}");
        }
        assert_eq!(audit_rows(&lib).await, 0);
    }

    #[tokio::test]
    async fn navidrome_failing_during_the_confirm_deletes_nothing() {
        for who in ["alice", "bob", "alice-midway"] {
            let lib = Library::new().await;
            lib.app("g1", "alice", true).await;
            let now = chrono::Utc::now().timestamp();
            let token = s1_token(&lib, DeletionReason::Cleanup, now).await;
            let circuit = || NavidromeLookupError::Client(NavidromeError::CircuitOpen);
            let code = match who {
                "alice" => {
                    lib.fake.unreadable("alice", circuit);
                    FailureCode::NavidromeCircuitOpen
                }
                "bob" => {
                    lib.fake.unreadable("bob", circuit);
                    FailureCode::DeleteVetoUnknown
                }
                // The record read answers, the caller's own playlist read fails.
                _ => {
                    lib.fake.playlists_unreadable("alice", circuit);
                    FailureCode::NavidromeCircuitOpen
                }
            };
            let done = confirm(&lib.guard(), &queue(), &alice_app(), &token.secret, now)
                .await
                .unwrap();
            assert_eq!(refused(&done), vec![("s1", code)], "{who}");
            assert!(lib.root.join("music/alice/A/x.flac").exists());
            assert_eq!(audit_rows(&lib).await, 0);
        }
    }

    #[tokio::test]
    async fn two_tokens_for_one_file_delete_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::new_on(test_db::migrated_file(dir.path()).await).await;
        lib.app("g1", "alice", true).await;
        let now = chrono::Utc::now().timestamp();
        let a = s1_token(&lib, DeletionReason::Cleanup, now).await;
        let b = s1_token(&lib, DeletionReason::Cleanup, now).await;
        let (guard, q, alice) = (lib.guard(), queue(), alice_app());
        let (da, db_) = tokio::join!(
            confirm(&guard, &q, &alice, &a.secret, now),
            confirm(&guard, &q, &alice, &b.secret, now)
        );
        let (da, db_) = (da.unwrap(), db_.unwrap());
        assert_eq!(deleted(&da).len() + deleted(&db_).len(), 1);
        let lost: Vec<FailureCode> = (refused(&da).into_iter())
            .chain(refused(&db_))
            .map(|(_, c)| c)
            .collect();
        assert!(
            matches!(
                lost.as_slice(),
                [FailureCode::DeleteChangedSincePreview] | [FailureCode::DeletePathUnresolved]
            ),
            "{lost:?}"
        );
        assert_eq!(audit_rows(&lib).await, 1);
    }
}
