//! The playlist tools: the caller's Navidrome playlists and other users'
//! public ones.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{FailureCode, Stage};

use super::args::Args;
use super::contract::{failed, success, upstream_error, NextCall, ToolError};
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
    let read = || -> Result<_, ToolError> {
        let page = Page::read(&args, json!({}))?;
        args.finish()?;
        Ok(page)
    };
    let page = match read() {
        Ok(page) => page,
        Err(e) => return failed(e),
    };
    let navi = match navidrome_of(state, caller, Stage::Playlist, &args).await {
        Ok(navi) => navi,
        Err(e) => return failed(e),
    };
    match playlists::list(&navi, &caller.ctx).await {
        Ok(listed) => {
            let items: Vec<Value> = listed
                .get(page.offset..)
                .unwrap_or_default()
                .iter()
                .map(|p| json!(p))
                .collect();
            success(fit(Map::new(), "playlists", items, &page, false))
        }
        Err(f) => failed(upstream_error(f, Map::new(), args.same_call())),
    }
}

/// `get_playlist`: the playlist and a page of its entries. An id with no
/// playlist the caller may see offers the list to pick one from.
pub(crate) async fn get_playlist(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_playlist", args, &["playlist_id", "limit", "cursor"]);
    let read = || -> Result<_, ToolError> {
        let playlist_id = args.string("playlist_id")?;
        let page = Page::read(&args, json!({"playlist_id": playlist_id}))?;
        args.finish()?;
        Ok((playlist_id, page))
    };
    let (playlist_id, page) = match read() {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let navi = match navidrome_of(state, caller, Stage::Playlist, &args).await {
        Ok(navi) => navi,
        Err(e) => return failed(e),
    };
    match playlists::get(&navi, &caller.ctx, &playlist_id).await {
        Ok((playlist, entries)) => {
            let items: Vec<Value> = entries
                .get(page.offset..)
                .unwrap_or_default()
                .iter()
                .map(|e| json!(e))
                .collect();
            let mut fields = Map::new();
            fields.insert("playlist".into(), json!(playlist));
            success(fit(fields, "entries", items, &page, false))
        }
        Err(f) => {
            let not_found = f.code == FailureCode::PlaylistNotFound;
            let mut facts = Map::new();
            facts.insert("playlist_id".into(), json!(playlist_id));
            let mut e = upstream_error(f, facts, args.same_call());
            if not_found {
                e.next = vec![NextCall {
                    tool: "list_playlists",
                    args: json!({}),
                }];
            }
            failed(e)
        }
    }
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
}
