//! The library tools: what the caller's Navidrome holds.

use std::sync::Arc;

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_chars, Stage, TEXT_MAX_CHARS};
use shared::navidrome::LibraryStats;
use soulbeet::navidrome::NavidromeClient;

use super::args::Args;
use super::contract::{failed, success, upstream_error, ToolError};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::library::{self, navidrome_for};

/// The caller's Navidrome client, or why there is none. The same call is
/// offered again when the lookup can answer later, as its database branch
/// can (D13).
pub(crate) async fn navidrome_of(
    state: &ConnectionState,
    caller: &McpCaller,
    stage: Stage,
    args: &Args,
) -> Result<Arc<NavidromeClient>, ToolError> {
    navidrome_for(&*state.navidrome, &caller.ctx, stage)
        .await
        .map_err(|f| upstream_error(f, Map::new(), args.same_call()))
}

/// `get_library_stats`, which takes no arguments.
pub(crate) async fn get_library_stats(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_library_stats", args, &[]);
    if let Err(e) = args.finish() {
        return failed(e);
    }
    let navi = match navidrome_of(state, caller, Stage::LibraryCheck, &args).await {
        Ok(navi) => navi,
        Err(e) => return failed(e),
    };
    match library::stats(&navi).await {
        Ok(stats) => success(stats_fields(stats)),
        Err(f) => failed(upstream_error(f, Map::new(), args.same_call())),
    }
}

/// The stats document: `LibraryStats`' fields, each genre as
/// `{"genre", "tracks"}` with its name cut as any text from Navidrome is.
/// With no rated track there is no average, so `average_rating` is `null`.
fn stats_fields(stats: LibraryStats) -> Map<String, Value> {
    let average_rating = (stats.rated_tracks > 0).then_some(stats.average_rating);
    let genres: Vec<Value> = stats
        .genres
        .into_iter()
        .map(|(genre, tracks)| json!({"genre": cut_chars(genre, TEXT_MAX_CHARS), "tracks": tracks}))
        .collect();
    let Value::Object(fields) = json!({
        "total_tracks": stats.total_tracks,
        "rated_tracks": stats.rated_tracks,
        "unrated_tracks": stats.unrated_tracks,
        "average_rating": average_rating,
        "rating_distribution": stats.rating_distribution,
        "total_albums": stats.total_albums,
        "total_artists": stats.total_artists,
        "genres": genres,
    }) else {
        unreachable!("an object literal is an object");
    };
    fields
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use super::*;
    use crate::connection::test_util::{call_tool, document, mcp_app, mcp_app_with};
    use crate::test_navidrome::{child, subsonic_ok, NavidromeFor, NavidromeStub};

    #[tokio::test]
    async fn get_library_stats_without_a_navidrome_link_says_how_to_link() {
        let (_, app, token) = mcp_app().await;
        let result = call_tool(app, &token, "get_library_stats", json!({})).await;
        assert_eq!(result["isError"], true);
        let e = &document(&result)["error"];
        // NavidromeLookupError::NotLinked's text, api/src/services.rs:245
        assert_eq!(
            (
                e["code"].as_str(),
                e["stage"].as_str(),
                e["message"].as_str(),
                e["retry"].as_str()
            ),
            (
                Some("navidrome_not_linked"),
                Some("library_check"),
                Some("Navidrome not configured for user"),
                Some("needs_user")
            )
        );
        assert_eq!(e["user_action"]["code"], "relink_navidrome");
        assert_eq!(e["next"], json!([]));
    }

    #[tokio::test]
    async fn get_library_stats_through_the_router() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/search3",
            &[&subsonic_ok(&format!(
                r#""searchResult3":{{"song":[{}]}}"#,
                child("s1", "Marrow Choir", "O Holy Night")
            ))],
        );
        navi.answer("/rest/getAlbumList2", &[&subsonic_ok(r#""albumList2":{}"#)]);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(&call_tool(app, &token, "get_library_stats", json!({})).await);
        assert_eq!(
            (
                doc["ok"].as_bool(),
                &doc["total_tracks"],
                &doc["rated_tracks"],
                &doc["genres"]
            ),
            (Some(true), &json!(1), &json!(1), &json!([]))
        );
        assert_eq!(
            (
                &doc["average_rating"],
                &doc["rating_distribution"],
                &doc["total_albums"],
                &doc["total_artists"]
            ),
            (&json!(4.0), &json!([0, 0, 0, 1, 0]), &json!(0), &json!(1))
        );
    }

    #[test]
    fn a_library_without_a_rated_track_has_no_average() {
        let doc = stats_fields(LibraryStats {
            total_tracks: 2,
            unrated_tracks: 2,
            ..LibraryStats::default()
        });
        assert_eq!(
            (&doc["rated_tracks"], &doc["average_rating"]),
            (&json!(0), &Value::Null)
        );
    }

    #[tokio::test]
    async fn genres_are_named_with_their_track_counts() {
        let navi = NavidromeStub::start();
        let with_genre = |id: &str, title: &str, genre: &str| {
            child(id, "Marrow Choir", title).replace(
                r#""genres":[]"#,
                &format!(r#""genre":"{genre}","genres":[{{"name":"{genre}"}}]"#),
            )
        };
        navi.answer(
            "/rest/search3",
            &[&subsonic_ok(&format!(
                r#""searchResult3":{{"song":[{},{},{}]}}"#,
                with_genre("s1", "O Holy Night", "Choral"),
                with_genre("s2", "Silent Night", "Choral"),
                with_genre("s3", "Coventry Carol", "Folk")
            ))],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(&call_tool(app, &token, "get_library_stats", json!({})).await);
        assert_eq!(
            doc["genres"],
            json!([{"genre": "Choral", "tracks": 2}, {"genre": "Folk", "tracks": 1}])
        );
    }

    #[tokio::test]
    async fn a_navidrome_behind_a_failing_proxy_offers_the_same_call_again() {
        let navi = NavidromeStub::start();
        navi.answer_when("/rest/search3", "songOffset=0", 502, "");
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let result = call_tool(app, &token, "get_library_stats", json!({})).await;
        assert_eq!(result["isError"], true);
        let e = &document(&result)["error"];
        assert_eq!(
            (e["code"].as_str(), e["stage"].as_str(), e["retry"].as_str()),
            (
                Some("navidrome_unreachable"),
                Some("library_check"),
                Some("same_source_later")
            )
        );
        // Soulful's own text for a Subsonic answer that is not a success
        // (lib/soulbeet/src/navidrome/client.rs, `subsonic`).
        assert_eq!(
            e["cause"],
            json!({"service": "navidrome", "status": 502, "raw": "Navidrome request failed: HTTP 502"})
        );
        assert_eq!(
            e["next"],
            json!([{"tool": "get_library_stats", "args": {}}])
        );
    }
}
