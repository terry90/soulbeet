//! The library tools: what the caller's Navidrome holds.

use std::sync::Arc;

use futures::StreamExt;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_chars, Failure, Stage, TEXT_MAX_CHARS};
use shared::navidrome::LibraryStats;
use soulbeet::navidrome::NavidromeClient;

use super::args::{repeatable, Args};
use super::contract::{
    failed, invalid_value, items, items_text, success, upstream_error, ItemResult, Items, ToolError,
};
use super::paging::{Cursor, RESULT_MAX_CHARS};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::index::navidrome_keeps;
use crate::core::library::{self, navidrome_for, Found, Reference, LOOKUPS_AT_ONCE};

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

const FIND: &str = "find_in_library";

/// What a reference must have for Navidrome to search it (D9).
const SEARCHABLE: &str = "an artist or title with a character other than spaces, quotes, brackets, parentheses, braces, commas and slashes";

/// `find_in_library`: each reference from the cursor on looked up
/// LOOKUPS_AT_ONCE at a time, answered in input order, and cut at a
/// reference when the next entry would take the result over
/// RESULT_MAX_CHARS (D4).
pub(crate) async fn find_in_library(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(FIND, args, &["references", "cursor"]);
    let (elements, refs, list, start) = match read_references(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let navi = match navidrome_of(state, caller, Stage::LibraryCheck, &args).await {
        Ok(navi) => navi,
        Err(e) => return failed(e),
    };
    let mut it = Items {
        stage: Stage::LibraryCheck,
        noun: "references",
        start,
        results: Vec::new(),
        pending: refs.len() - start,
        cursor: None,
    };
    // By index: a closure taking each `&Reference` from the stream must
    // hold for any lifetime, which the boxed handler cannot show.
    let mut lookups = futures::stream::iter(start..refs.len())
        .map(|i| library::find(&navi, &refs[i]))
        .buffered(LOOKUPS_AT_ONCE);
    while let Some(found) = lookups.next().await {
        let i = start + it.results.len();
        it.results.push(entry(found, &elements[i], &refs[i]));
        it.pending = refs.len() - i - 1;
        it.cursor = (it.pending > 0).then(|| Cursor::make(FIND, &list, i + 1));
        if it.results.len() > 1 && items_text(&it).chars().count() > RESULT_MAX_CHARS {
            it.results.pop();
            it.pending = refs.len() - i;
            it.cursor = Some(Cursor::make(FIND, &list, i));
            break;
        }
    }
    // No lookup starts once the stream is no longer polled; this drops the
    // ones in flight.
    drop(lookups);
    items(it)
}

/// The references, each as read, the listing they define, and the cursor's
/// offset: every reference is read before the cursor and `finish`. A cursor
/// past the last reference is refused; one at it answers nothing.
fn read_references(args: &Args) -> Result<(Vec<Args>, Vec<Reference>, Value, usize), ToolError> {
    let elements = args.objects(
        "references",
        1,
        100,
        &["artist", "title", "album", "recording_mbid", "duration_s"],
    )?;
    let refs = elements
        .iter()
        .enumerate()
        .map(|(i, element)| read_reference(i, element))
        .collect::<Result<Vec<_>, _>>()?;
    let list = json!({"references": refs.iter().map(reference_fields).collect::<Vec<_>>()});
    let start = Cursor::read(args, &list)?;
    if start > refs.len() {
        return Err(Cursor::refused(FIND));
    }
    args.finish()?;
    Ok((elements, refs, list, start))
}

/// Reference `i`, refused when Navidrome would drop its lookup text and
/// answer every song instead (D9).
fn read_reference(i: usize, element: &Args) -> Result<Reference, ToolError> {
    let r = Reference {
        artist: element.string("artist")?,
        title: element.string("title")?,
        album: element.opt_string("album")?,
        recording_mbid: element.opt_mbid("recording_mbid")?,
        duration_s: element.opt_u32("duration_s", 1, 36_000)?,
    };
    if !navidrome_keeps(&format!("{} {}", r.artist, r.title)) {
        return Err(invalid_value(FIND, &format!("references[{i}]"), SEARCHABLE));
    }
    Ok(r)
}

/// A reference's five fields as read, `null` for one not given: the
/// listing a cursor names and a failed entry's facts.
fn reference_fields(r: &Reference) -> Value {
    json!({"artist": r.artist, "title": r.title, "album": r.album,
        "recording_mbid": r.recording_mbid, "duration_s": r.duration_s})
}

/// One reference's entry. A failed lookup offers the call for that one
/// reference as given (D13), not the whole call.
fn entry(found: Result<Found, Failure>, element: &Args, r: &Reference) -> ItemResult {
    match found {
        Ok(found) => {
            let Value::Object(fields) = json!(found) else {
                unreachable!("Found serializes as a JSON object");
            };
            ItemResult::Done(fields)
        }
        Err(f) => {
            let Value::Object(facts) = reference_fields(r) else {
                unreachable!("an object literal is an object");
            };
            let retry = repeatable(FIND, json!({"references": [element.as_value()]}));
            ItemResult::Failed(upstream_error(f, facts, retry))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use super::*;
    use crate::connection::test_util::{call_tool, document, mcp_app, mcp_app_with};
    use crate::core::library::LOOKUPS_AT_ONCE;
    use crate::mcp::paging::Cursor;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{child, native_song, subsonic_ok, NavidromeFor, NavidromeStub};

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

    fn reference(title: &str) -> Value {
        json!({"artist": "Marrow Choir", "title": title})
    }

    fn song_requests(navi: &NavidromeStub) -> Vec<String> {
        navi.asked_with_queries()
            .into_iter()
            .filter(|t| t.starts_with("/api/song"))
            .collect()
    }

    /// nginx's 502 page (src/http/ngx_http_special_response.c), which the native client keeps
    /// as the cause (client.rs, `native_failure`).
    const NGINX_502: &str = "<html>\r\n<head><title>502 Bad Gateway</title></head>\r\n<body>\r\n<center><h1>502 Bad Gateway</h1></center>\r\n<hr><center>nginx</center>\r\n</body>\r\n</html>\r\n";

    #[tokio::test]
    async fn a_reference_navidrome_would_drop_is_refused_before_navidrome_is_asked() {
        let navi = NavidromeStub::start();
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let result = call_tool(
            app,
            &token,
            "find_in_library",
            json!({"references": [reference("O Holy Night"), {"artist": "()", "title": "\"\""}]}),
        )
        .await;
        let e = &document(&result)["error"];
        let expected = "an artist or title with a character other than spaces, quotes, brackets, parentheses, braces, commas and slashes";
        assert_eq!(
            (e["code"].as_str(), e["message"].as_str()),
            (
                Some("invalid_input"),
                Some(format!("find_in_library: references[1] must be {expected}").as_str())
            )
        );
        assert_eq!(
            e["facts"],
            json!({"field": "references[1]", "expected": expected})
        );
        assert!(navi.asked_with_queries().is_empty());
    }

    #[tokio::test]
    async fn a_name_navidrome_keeps_is_looked_up() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/api/song",
            &[&format!(
                "[{}]",
                native_song("m1", "!!!", "Myth Takes", 263.4, None)
            )],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "find_in_library",
                json!({"references": [{"artist": "!!!", "title": "Myth Takes"}]}),
            )
            .await,
        );
        assert_eq!(doc["results"][0]["matches"][0]["match"]["kind"], "exact");
        // form encoding of "!!! Myth Takes" (url crate's application/x-www-form-urlencoded serializer)
        assert!(
            song_requests(&navi)
                .iter()
                .any(|t| t.contains("title=%21%21%21+Myth+Takes")),
            "{:?}",
            navi.asked_with_queries()
        );
    }

    #[tokio::test]
    async fn a_match_carries_the_song_facts_and_how_it_matched() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/api/song",
            &[&format!(
                "[{}]",
                native_song("e1", "Marrow Choir", "O Holy Night", 263.4, None)
            )],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "find_in_library",
                json!({"references": [{"artist": "Marrow Choir", "title": "O Holy Night", "duration_s": 263}]}),
            )
            .await,
        );
        assert_eq!(
            doc,
            json!({"ok": true, "summary": {"requested": 1, "succeeded": 1, "failed": 0, "pending": 0},
                "results": [{"index": 0, "ok": true, "more_matches": 0, "candidates_truncated": false,
                    "matches": [{"song_id": "e1", "title": "O Holy Night", "artist": "Marrow Choir",
                        "album": "Winter Songs", "album_id": "7hQ2nLr5VxZ0cYbM8wTkAf", "year": 2019,
                        "track": 3, "duration_s": 263, "format": "flac", "bitrate_kbps": 912,
                        "size_bytes": 28734112, "path": "Marrow Choir/Winter Songs/03 - O Holy Night.flac",
                        "recording_mbid": null, "rating": 4, "starred": true,
                        "starred_at": "2026-11-02T18:04:51.112Z", "play_count": 3,
                        "match": {"kind": "exact", "score": null, "differs": [], "duration_delta_s": 0}}]}],
                "cursor": null})
        );
    }

    #[tokio::test]
    async fn one_failed_lookup_makes_the_result_partial() {
        let navi = NavidromeStub::start();
        navi.answer_when(
            "/api/song",
            "title=Marrow+Choir+O+Holy+Night",
            200,
            &format!(
                "[{}]",
                native_song("e1", "Marrow Choir", "O Holy Night", 263.4, None)
            ),
        );
        navi.answer_when(
            "/api/song",
            "title=Marrow+Choir+Silent+Night",
            502,
            NGINX_502,
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let result = call_tool(
            app,
            &token,
            "find_in_library",
            json!({"references": [reference("O Holy Night"), reference("Silent Night")]}),
        )
        .await;
        assert_eq!(result["isError"], true);
        let doc = document(&result);
        assert_eq!(
            (doc["ok"].as_bool(), &doc["summary"]),
            (
                Some(false),
                &json!({"requested": 2, "succeeded": 1, "failed": 1, "pending": 0})
            )
        );
        assert_eq!(
            (
                doc["results"][0]["matches"][0]["song_id"].as_str(),
                doc["results"][0]["matches"][0]["match"]["kind"].as_str()
            ),
            (Some("e1"), Some("exact"))
        );
        let e = &doc["results"][1]["error"];
        assert_eq!(
            (
                e["code"].as_str(),
                e["retry"].as_str(),
                e["message"].as_str()
            ),
            (
                Some("navidrome_unreachable"),
                Some("same_source_later"),
                Some("Find songs failed: HTTP 502")
            )
        );
        assert_eq!(
            (&e["cause"]["status"], &e["cause"]["raw"]),
            (&json!(502), &json!(NGINX_502))
        );
        assert_eq!(
            e["facts"],
            json!({"artist": "Marrow Choir", "title": "Silent Night", "album": null, "recording_mbid": null, "duration_s": null})
        );
        assert_eq!(
            e["next"],
            json!([{"tool": "find_in_library", "args": {"references": [{"artist": "Marrow Choir", "title": "Silent Night"}]}}])
        );
        assert_eq!(doc["error"]["code"], "partial");
    }

    #[tokio::test]
    async fn failed_entries_count_toward_the_cap() {
        let navi = NavidromeStub::start();
        navi.answer_when("/api/song", "", 502, NGINX_502);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        // An album name long enough that 100 failed entries, each repeating its reference in `next`, pass 60,000 characters.
        let album = "Christmas Songs and Carols from the Cathedral, Recorded Live ".repeat(6);
        let refs: Vec<Value> = (0..100)
            .map(|i| json!({"artist": "Marrow Choir", "title": format!("Carol {i}"), "album": album}))
            .collect();
        let result = call_tool(app, &token, "find_in_library", json!({"references": refs})).await;
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 60_000
        );
        let doc = document(&result);
        let kept = doc["results"].as_array().unwrap().len() as u64;
        assert!(kept < 100 && doc["ok"] == false);
        assert_eq!(
            doc["summary"],
            json!({"requested": 100, "succeeded": 0, "failed": kept, "pending": 100 - kept})
        );
        assert_eq!(
            doc["error"]["facts"]["failed_indexes"],
            json!((0..kept).collect::<Vec<_>>())
        );
        assert!(doc["cursor"].is_string());
        let asked = song_requests(&navi).len() as u64;
        assert!(
            asked <= kept + LOOKUPS_AT_ONCE as u64,
            "{asked} lookups for {kept} kept"
        );
    }

    #[tokio::test]
    async fn a_result_too_large_stops_at_a_reference_and_resumes_there() {
        let navi = NavidromeStub::start();
        let six: Vec<String> = (0..6)
            .map(|i| {
                native_song(
                    &format!("s{i}"),
                    "Marrow Choir",
                    "O Holy Night",
                    263.4,
                    None,
                )
            })
            .collect();
        navi.answer("/api/song", &[&format!("[{}]", six.join(","))]);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let refs: Vec<Value> = (0..100).map(|_| reference("O Holy Night")).collect();
        let first = call_tool(
            app.clone(),
            &token,
            "find_in_library",
            json!({"references": refs.clone()}),
        )
        .await;
        assert!(
            first["content"][0]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 60_000
        );
        let doc = document(&first);
        let kept = doc["results"].as_array().unwrap().len();
        assert!(kept < 100 && doc["ok"] == true);
        assert_eq!(
            (
                doc["summary"]["pending"].as_u64(),
                doc["results"][0]["more_matches"].as_u64()
            ),
            (Some(100 - kept as u64), Some(1))
        );
        let second = document(
            &call_tool(
                app,
                &token,
                "find_in_library",
                json!({"references": refs, "cursor": doc["cursor"]}),
            )
            .await,
        );
        assert_eq!(second["results"][0]["index"].as_u64(), Some(kept as u64));
    }

    #[tokio::test]
    async fn a_cursor_at_the_end_answers_nothing_and_one_past_it_is_refused() {
        let navi = NavidromeStub::start();
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let list = json!({"references": [{"artist": "Marrow Choir", "title": "O Holy Night",
            "album": null, "recording_mbid": null, "duration_s": null}]});
        let at = |offset: usize| {
            json!({"references": [reference("O Holy Night")],
                "cursor": Cursor::make("find_in_library", &list, offset)})
        };
        let end = document(&call_tool(app.clone(), &token, "find_in_library", at(1)).await);
        assert_eq!(
            (&end["ok"], &end["summary"], &end["results"], &end["cursor"]),
            (
                &json!(true),
                &json!({"requested": 0, "succeeded": 0, "failed": 0, "pending": 0}),
                &json!([]),
                &Value::Null
            )
        );
        let past = document(&call_tool(app, &token, "find_in_library", at(2)).await);
        assert_eq!(
            past["error"]["message"],
            "find_in_library: cursor must be a cursor find_in_library returned for the same arguments"
        );
        assert!(song_requests(&navi).is_empty());
    }

    /// 100 references of three 500-character fields: about 155,000 characters of arguments.
    fn large_references() -> Value {
        let field = "x".repeat(500);
        let refs: Vec<Value> = (0..100)
            .map(|_| json!({"artist": field, "title": field, "album": field}))
            .collect();
        json!({"references": refs})
    }

    #[tokio::test]
    async fn a_call_too_large_to_repeat_is_answered_under_the_cap() {
        let mut args = large_references();
        args["verbose"] = json!(true);
        let (_, app, token) = mcp_app().await;
        let result = call_tool(app, &token, "find_in_library", args).await;
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 60_000
        );
        let e = &document(&result)["error"];
        assert_eq!(
            (e["code"].as_str(), &e["next"]),
            (Some("invalid_input"), &json!([]))
        );
        assert!(e["message"]
            .as_str()
            .unwrap()
            .ends_with("The call is too large to offer again in next."));

        // sqlx's text for SQLITE_BUSY: sqlx-core 0.8.6 src/error.rs:44, sqlx-sqlite 0.8.6 src/error.rs:89
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Err(|| {
            NavidromeLookupError::Database(
                "error returned from database: (code: 5) database is locked".into(),
            )
        }))))
        .await;
        let result = call_tool(app, &token, "find_in_library", large_references()).await;
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 60_000
        );
        let e = &document(&result)["error"];
        assert_eq!(
            (e["code"].as_str(), e["retry"].as_str(), &e["next"]),
            (
                Some("settings_unreadable"),
                Some("same_source_later"),
                &json!([])
            )
        );
        assert_eq!(
            e["message"],
            "error returned from database: (code: 5) database is locked The call is too large to offer again in next."
        );
    }
}
