//! The playlist tools: the caller's Navidrome playlists and other users'
//! public ones.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map};
use shared::failure::{Failure, FailureCode, Stage};

use super::args::Args;
use super::contract::{answer, upstream_error, NextCall, ToolError};
use super::library::navidrome_of;
use super::paging::{fit, Page};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::playlists;

/// `list_playlists`: a page of the playlists the caller may see.
pub(crate) async fn list_playlists(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("list_playlists", args, &["limit", "cursor"]);
    answer(
        async {
            let page = Page::read(&args, json!({}))?;
            args.finish()?;
            let navi = navidrome_of(state, caller, Stage::Playlist, &args).await?;
            let listed = playlists::list(&navi, &caller.ctx)
                .await
                .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            Ok(fit(Map::new(), "playlists", page.of(&listed), &page, false))
        }
        .await,
    )
}

/// `get_playlist`: the playlist and a page of its entries. An id with no
/// playlist the caller may see offers the list to pick one from.
pub(crate) async fn get_playlist(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_playlist", args, &["playlist_id", "limit", "cursor"]);
    answer(
        async {
            let playlist_id = args.string("playlist_id")?;
            let page = Page::read(&args, json!({"playlist_id": playlist_id}))?;
            args.finish()?;
            let navi = navidrome_of(state, caller, Stage::Playlist, &args).await?;
            let (playlist, entries) = playlists::get(&navi, &caller.ctx, &playlist_id)
                .await
                .map_err(|f| not_got(f, &playlist_id, &args))?;
            let mut fields = Map::new();
            fields.insert("playlist".into(), json!(playlist));
            Ok(fit(fields, "entries", page.of(&entries), &page, false))
        }
        .await,
    )
}

/// get_playlist's failure: `playlist_not_found` offers the list instead of
/// the same call.
fn not_got(f: Failure, playlist_id: &str, args: &Args) -> ToolError {
    let not_found = f.code == FailureCode::PlaylistNotFound;
    let mut facts = Map::new();
    facts.insert("playlist_id".into(), json!(playlist_id));
    let mut e = upstream_error(f, facts, args.same_call());
    if not_found {
        e.next = vec![NextCall::new("list_playlists", json!({}))];
    }
    e
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use crate::connection::test_util::{call_tool, document, mcp_app_with};
    use crate::test_navidrome::{
        playlist_row, subsonic_failed, subsonic_ok, NavidromeFor, NavidromeStub,
    };

    #[tokio::test]
    async fn get_playlist_of_an_unknown_id_offers_the_list() {
        let navi = NavidromeStub::start();
        // server/subsonic/playlists.go:46
        navi.answer(
            "/rest/getPlaylist",
            &[&subsonic_failed(70, "playlist not found")],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let e = &document(
            &call_tool(app, &token, "get_playlist", json!({"playlist_id": "gone"})).await,
        )["error"];
        assert_eq!(
            (e["code"].as_str(), &e["next"]),
            (
                Some("playlist_not_found"),
                &json!([{"tool": "list_playlists", "args": {}}])
            )
        );
        assert_eq!(e["facts"], json!({"playlist_id": "gone"}));
    }

    #[tokio::test]
    async fn a_playlist_comes_with_a_page_of_its_entries() {
        let navi = NavidromeStub::start();
        // getPlaylist of alice's playlist (server/subsonic/playlists.go:41-57), its entries not read.
        navi.answer(
            "/rest/getPlaylist",
            &[&subsonic_ok(
                r#""playlist":{"id":"9fR2kLm4NpQ7sT1vW3xY5z","name":"Christmas","comment":"traditional","songCount":2,"duration":526,"public":true,"owner":"alice","created":"2026-11-15T09:12:03.418Z","changed":"2026-11-15T09:14:47.002Z","readonly":false,"entry":[]}"#,
            )],
        );
        navi.answer(
            "/api/playlist/9fR2kLm4NpQ7sT1vW3xY5z/tracks",
            &[&format!(
                "[{},{}]",
                playlist_row(1, "s9", "Marrow Choir", "Silent Night", false),
                playlist_row(2, "s1", "Marrow Choir", "O Holy Night", true)
            )],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let args = json!({"playlist_id": "9fR2kLm4NpQ7sT1vW3xY5z", "limit": 1});
        let first = document(&call_tool(app.clone(), &token, "get_playlist", args.clone()).await);
        assert_eq!(
            first["playlist"],
            json!({"playlist_id": "9fR2kLm4NpQ7sT1vW3xY5z", "name": "Christmas", "comment": "traditional",
                "owner": "alice", "owned": true, "public": true, "song_count": 2, "duration_s": 526,
                "created": "2026-11-15T09:12:03.418Z", "changed": "2026-11-15T09:14:47.002Z"})
        );
        let e = &first["entries"][0];
        assert_eq!(
            (&e["position"], &e["missing"], &e["song_id"], &e["path"]),
            (
                &json!(0),
                &json!(false),
                &json!("s9"),
                &json!("Marrow Choir/Winter Songs/03 - Silent Night.flac")
            )
        );
        let mut next = args;
        next["cursor"] = first["cursor"].clone();
        let second = document(&call_tool(app, &token, "get_playlist", next).await);
        assert_eq!(
            (
                &second["entries"][0]["position"],
                &second["entries"][0]["missing"],
                &second["cursor"]
            ),
            (&json!(1), &json!(true), &Value::Null)
        );
    }

    #[tokio::test]
    async fn an_unreachable_navidrome_is_offered_again() {
        let navi = NavidromeStub::start();
        // nginx's 502 page (src/http/ngx_http_special_response.c)
        navi.answer_when("/rest/getPlaylists", "", 502, "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n<body>\r\n<center><h1>502 Bad Gateway</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n");
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let e = &document(&call_tool(app, &token, "list_playlists", json!({"limit": 10})).await)
            ["error"];
        // NavidromeError::Http from the Subsonic path (lib/soulbeet/src/navidrome/client.rs, `subsonic`)
        assert_eq!(
            (
                e["code"].as_str(),
                e["stage"].as_str(),
                e["message"].as_str()
            ),
            (
                Some("navidrome_unreachable"),
                Some("playlist"),
                Some("Navidrome request failed: HTTP 502")
            )
        );
        assert_eq!(
            e["next"],
            json!([{"tool": "list_playlists", "args": {"limit": 10}}])
        );
    }

    #[tokio::test]
    async fn list_playlists_pages_through_what_the_caller_may_see() {
        let navi = NavidromeStub::start();
        // getPlaylists by name (server/subsonic/playlists.go:18-30); bob's private one is left out.
        let pl = |id: &str, name: &str, owner: &str, public: bool| {
            json!({"id": id, "name": name, "songCount": 1, "duration": 263, "public": public,
                "owner": owner, "created": "2026-11-10T18:00:00Z", "changed": "2026-11-10T18:00:00Z"})
        };
        navi.answer(
            "/rest/getPlaylists",
            &[&subsonic_ok(&format!(
                r#""playlists":{{"playlist":{}}}"#,
                json!([
                    pl("p1", "Advent", "bob", true),
                    pl("p2", "Bob's mix", "bob", false),
                    pl("p3", "Christmas", "alice", false),
                    pl("p4", "Epiphany", "alice", true)
                ])
            ))],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let first =
            document(&call_tool(app.clone(), &token, "list_playlists", json!({"limit": 2})).await);
        let names = |doc: &Value| {
            doc["playlists"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    (
                        p["name"].as_str().unwrap().to_string(),
                        p["owned"].as_bool().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&first),
            [
                ("Advent".to_string(), false),
                ("Christmas".to_string(), true)
            ]
        );
        let second = document(
            &call_tool(
                app,
                &token,
                "list_playlists",
                json!({"limit": 2, "cursor": first["cursor"]}),
            )
            .await,
        );
        assert_eq!(names(&second), [("Epiphany".to_string(), true)]);
        assert_eq!(second["cursor"], Value::Null);
    }
}
