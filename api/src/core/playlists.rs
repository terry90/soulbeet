//! The caller's Navidrome playlists and other users' public ones, as the
//! tools report them (D10).

use serde::Serialize;
use shared::failure::{
    cut_json, Failure, FailureCode, RetryAdvice, Stage, RAW_MAX_CHARS, TEXT_MAX_CHARS,
};
use soulbeet::navidrome::models::{PlaylistTrack, SubsonicPlaylist};
use soulbeet::navidrome::NavidromeClient;
use soulbeet::NavidromeError;

use crate::auth::UserCtx;
use crate::core::library::{navidrome_failure, SongFacts};

/// A playlist as the tools report it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistFacts {
    pub playlist_id: String,
    pub name: String,
    pub comment: Option<String>,
    /// The owner's Navidrome user name.
    pub owner: String,
    /// Whether the caller owns it; false for another user's public playlist.
    pub owned: bool,
    pub public: bool,
    pub song_count: u32,
    pub duration_s: u32,
    pub created: String,
    pub changed: String,
}

/// A playlist entry: its 0-based place among all of the playlist's tracks,
/// the index updatePlaylist's songIndexToRemove takes
/// (core/playlists/playlists.go:188-197), whether its file is gone, and the
/// song. Tracks Navidrome does not list to this account leave gaps.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlaylistEntry {
    pub position: usize,
    pub missing: bool,
    #[serde(flatten)]
    pub song: SongFacts,
}

/// `p` as the tools report it. The caller owns it when its owner is the
/// caller's user name ignoring ASCII case: Navidrome finds a user by
/// `user_name COLLATE NOCASE` (persistence/user_repository.go:203) and
/// Soulful signs in to Navidrome with the Soulful user name (D10).
pub(crate) fn facts(p: &SubsonicPlaylist, username: &str) -> PlaylistFacts {
    PlaylistFacts {
        playlist_id: p.id.clone(),
        name: cut_json(p.name.clone(), TEXT_MAX_CHARS),
        comment: (!p.comment.is_empty()).then(|| cut_json(p.comment.clone(), RAW_MAX_CHARS)),
        owner: cut_json(p.owner.clone(), TEXT_MAX_CHARS),
        owned: p.owner.eq_ignore_ascii_case(username),
        public: p.public,
        song_count: p.song_count,
        duration_s: p.duration,
        created: p.created.clone(),
        changed: p.changed.clone(),
    }
}

/// The caller's own playlists and other users' public ones, in Navidrome's
/// order, by name (server/subsonic/playlists.go:18-30). Navidrome lists an
/// admin every playlist (persistence/playlist_repository.go:84-93); another
/// user's private one is left out.
pub async fn list(navi: &NavidromeClient, ctx: &UserCtx) -> Result<Vec<PlaylistFacts>, Failure> {
    let playlists = navi
        .get_playlists()
        .await
        .map_err(|e| navidrome_failure(Stage::Playlist, &e))?;
    Ok(playlists
        .iter()
        .map(|p| facts(p, &ctx.username))
        .filter(|p| p.owned || p.public)
        .collect())
}

/// The playlist and every track of it in Navidrome's order, missing files
/// included. playlist_not_found(id) when Navidrome has none, and for another
/// user's private playlist, which is never read further: the answer is the
/// same as for an id that does not exist.
pub async fn get(
    navi: &NavidromeClient,
    ctx: &UserCtx,
    id: &str,
) -> Result<(PlaylistFacts, Vec<PlaylistEntry>), Failure> {
    let failure = |e: NavidromeError| navidrome_failure(Stage::Playlist, &e);
    let playlist = navi
        .get_playlist(id)
        .await
        .map_err(failure)?
        .map(|p| facts(&p, &ctx.username))
        .filter(|p| p.owned || p.public)
        .ok_or_else(|| playlist_not_found(id))?;
    // None here is Navidrome's own 404 for a playlist it no longer lists
    // to this account: it was deleted, or made private, since getPlaylist.
    let tracks = navi
        .playlist_tracks(id)
        .await
        .map_err(failure)?
        .ok_or_else(|| playlist_not_found(id))?;
    Ok((playlist, tracks.iter().map(entry).collect()))
}

/// A native track row as an entry: its 1-based place less one.
fn entry(track: &PlaylistTrack) -> PlaylistEntry {
    PlaylistEntry {
        position: track.position.get() as usize - 1,
        missing: track.song.missing,
        song: SongFacts::from(&track.song),
    }
}

/// No playlist with this id, or another user's private one. It carries no
/// cause, so the two read alike.
pub(crate) fn playlist_not_found(id: &str) -> Failure {
    Failure::new(
        FailureCode::PlaylistNotFound,
        Stage::Playlist,
        RetryAdvice::NotRetryable,
        format!("Playlist {id} not found"),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::auth::Role;
    use crate::core::test_ctx;
    use crate::test_navidrome::{child, playlist_row, subsonic_failed, subsonic_ok, NavidromeStub};

    /// A playlist as getPlaylists and getPlaylist send it, `public` left out
    /// when false (responses.go:311-312).
    fn pl(id: &str, name: &str, owner: &str, public: bool) -> String {
        let mut p = json!({"id": id, "name": name, "songCount": 0, "duration": 0, "public": true,
            "owner": owner, "created": "2026-11-10T18:00:00Z", "changed": "2026-11-10T18:00:00Z"});
        if !public {
            p.as_object_mut().unwrap().remove("public");
        }
        p.to_string()
    }

    fn playlists(items: &[&str]) -> String {
        subsonic_ok(&format!(
            r#""playlists":{{"playlist":[{}]}}"#,
            items.join(",")
        ))
    }

    fn playlist_answer(p: &str, entries: &[String]) -> String {
        let mut p: Value = serde_json::from_str(p).unwrap();
        p["entry"] = serde_json::from_str(&format!("[{}]", entries.join(","))).unwrap();
        subsonic_ok(&format!(r#""playlist":{p}"#))
    }

    fn alice(role: Role) -> UserCtx {
        UserCtx {
            user_id: "u1".into(),
            username: "alice".into(),
            role,
        }
    }

    #[tokio::test]
    async fn an_admin_sees_no_private_playlist_of_another_user() {
        // Navidrome gives an admin every playlist, persistence/playlist_repository.go:84-93
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylists",
            &[&playlists(&[
                &pl("p1", "Christmas", "alice", false),
                &pl("p2", "Bob's mix", "bob", false),
                &pl("p3", "Advent", "bob", true),
            ])],
        );
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_answer(&pl("p2", "Bob's mix", "bob", false), &[])],
        );
        // server/subsonic/playlists.go:46
        navi.answer_when(
            "/rest/getPlaylist",
            "id=gone",
            200,
            &subsonic_failed(70, "playlist not found"),
        );
        let admin = alice(Role::Admin);
        let listed = list(&navi.client(), &admin).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|p| (p.playlist_id.as_str(), p.owned))
                .collect::<Vec<_>>(),
            vec![("p1", true), ("p3", false)]
        );
        let private = get(&navi.client(), &admin, "p2").await.unwrap_err();
        let unknown = get(&navi.client(), &admin, "gone").await.unwrap_err();
        assert_eq!(
            (private, unknown),
            (playlist_not_found("p2"), playlist_not_found("gone"))
        );
        assert!(
            navi.asked()
                .iter()
                .all(|p| !p.starts_with("/api/playlist/")),
            "{:?}",
            navi.asked()
        );
    }

    #[test]
    fn ownership_ignores_the_case_of_the_owner_name() {
        let p: SubsonicPlaylist =
            serde_json::from_str(&pl("p1", "Christmas", "Alice", false)).unwrap();
        assert!(facts(&p, "alice").owned);
        assert!(!facts(&p, "alicia").owned);
    }

    #[tokio::test]
    async fn a_playlist_navidrome_cannot_find_is_not_found() {
        let navi = NavidromeStub::start();
        // server/subsonic/playlists.go:46
        navi.answer(
            "/rest/getPlaylist",
            &[&subsonic_failed(70, "playlist not found")],
        );
        let f = get(&navi.client(), &test_ctx("u1"), "gone")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::PlaylistNotFound,
                Stage::Playlist,
                RetryAdvice::NotRetryable,
                "Playlist gone not found"
            )
        );
        assert_eq!(f.cause, None);
    }

    #[tokio::test]
    async fn entries_keep_navidromes_order_as_positions() {
        let navi = NavidromeStub::start();
        // getPlaylist leaves out the missing s7 (core/playlists/playlists.go:97,
        // persistence/playlist_repository.go:153-166); the native list keeps it, and leaves
        // out place 3, a track of a library alice cannot see (tracksQuery, :378-400).
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_answer(
                &pl("p1", "Christmas", "alice", false),
                &[
                    child("s9", "Marrow Choir", "Silent Night"),
                    child("s1", "Marrow Choir", "O Holy Night"),
                ],
            )],
        );
        navi.answer(
            "/api/playlist/p1/tracks",
            &[&format!(
                "[{},{},{}]",
                playlist_row(1, "s9", "Marrow Choir", "Silent Night", false),
                playlist_row(2, "s1", "Marrow Choir", "O Holy Night", false),
                playlist_row(4, "s7", "Marrow Choir", "Coventry Carol", true)
            )],
        );
        let (p, entries) = get(&navi.client(), &alice(Role::User), "p1").await.unwrap();
        assert_eq!((p.owned, p.comment, p.public), (true, None, false));
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.position, e.song.song_id.as_str(), e.missing))
                .collect::<Vec<_>>(),
            vec![(0, "s9", false), (1, "s1", false), (3, "s7", true)]
        );
        assert_eq!(
            entries[0].song.path.as_deref(),
            Some("Marrow Choir/Winter Songs/03 - Silent Night.flac")
        );
    }

    #[tokio::test]
    async fn a_proxy_404_after_the_playlist_was_found_is_its_failure() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_answer(
                &pl("p1", "Christmas", "alice", false),
                &[],
            )],
        );
        // nginx's 404 page (src/http/ngx_http_special_response.c) from a proxy that forwards
        // /rest but not /api.
        let page = "<html>\r\n<head><title>404 Not Found</title></head>\r\n<body>\r\n<center><h1>404 Not Found</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n";
        navi.answer_when("/api/playlist/p1/tracks", "", 404, page);
        let f = get(&navi.client(), &alice(Role::User), "p1")
            .await
            .unwrap_err();
        assert_eq!(
            (f.code, f.stage, f.message.as_str()),
            (
                FailureCode::NavidromeUnreachable,
                Stage::Playlist,
                "Get playlist tracks failed: HTTP 404"
            )
        );
        assert_eq!(
            f.cause,
            Some(shared::failure::Cause::new(
                shared::failure::Service::Navidrome,
                Some(404),
                page
            ))
        );
    }

    #[tokio::test]
    async fn a_playlist_gone_between_the_two_reads_is_not_found() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getPlaylist",
            &[&playlist_answer(
                &pl("p1", "Christmas", "alice", false),
                &[],
            )],
        );
        // http.Error(w, "not found", 404), server/nativeapi/playlists.go:42-44
        navi.answer_when("/api/playlist/p1/tracks", "", 404, "not found\n");
        let f = get(&navi.client(), &alice(Role::User), "p1")
            .await
            .unwrap_err();
        assert_eq!(f, playlist_not_found("p1"));
    }
}
