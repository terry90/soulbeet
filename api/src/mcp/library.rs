//! The library tools: what the caller's Navidrome holds.

use std::sync::Arc;

use futures::StreamExt;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, Failure, Stage, TEXT_MAX_CHARS};
use shared::navidrome::LibraryStats;
use soulbeet::navidrome::{LibraryKind, NavidromeClient};

use super::args::{repeatable, Args};
use super::contract::{
    answer, failed, invalid_value, items, items_text, upstream_error, ItemResult, Items, ToolError,
};
use super::paging::{fit, Listing, Page, RESULT_MAX_CHARS};
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
    answer(
        async {
            args.finish()?;
            let navi = navidrome_of(state, caller, Stage::LibraryCheck, &args).await?;
            let stats = library::stats(&navi)
                .await
                .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            Ok(stats_fields(stats))
        }
        .await,
    )
}

/// The stats document: `LibraryStats`' fields, each genre as
/// `{"genre", "tracks"}` with its name cut as any text from Navidrome is.
/// With no rated track there is no average, so `average_rating` is `null`.
fn stats_fields(stats: LibraryStats) -> Map<String, Value> {
    let average_rating = (stats.rated_tracks > 0).then_some(stats.average_rating);
    let genres: Vec<Value> = stats
        .genres
        .into_iter()
        .map(|(genre, tracks)| json!({"genre": cut_json(genre, TEXT_MAX_CHARS), "tracks": tracks}))
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
/// RESULT_MAX_CHARS (D4). One entry always fits: it holds at most
/// MATCHES_KEPT matches whose texts are each cut to TEXT_MAX_CHARS of JSON.
pub(crate) async fn find_in_library(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(FIND, args, &["references", "cursor"]);
    match find_items(state, caller, &args).await {
        Ok(result) => result,
        Err(e) => failed(e),
    }
}

async fn find_items(
    state: &ConnectionState,
    caller: &McpCaller,
    args: &Args,
) -> Result<CallToolResult, ToolError> {
    let (elements, refs, listing, start) = read_references(args)?;
    let navi = navidrome_of(state, caller, Stage::LibraryCheck, args).await?;
    let mut it = Items {
        stage: Stage::LibraryCheck,
        noun: "references",
        start,
        results: Vec::new(),
        pending: refs.len() - start,
        cursor: None,
        fields: Map::new(),
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
        it.cursor = (it.pending > 0).then(|| listing.cursor(i + 1));
        if it.results.len() > 1 && items_text(&it).chars().count() > RESULT_MAX_CHARS {
            it.results.pop();
            it.pending = refs.len() - i;
            it.cursor = Some(listing.cursor(i));
            break;
        }
    }
    // No lookup starts once the stream is no longer polled; this drops the
    // ones in flight.
    drop(lookups);
    Ok(items(it))
}

/// The references, each as read, the listing they define, and the cursor's
/// offset: every reference is read before the cursor and `finish`. A cursor
/// past the last reference is refused; one at it answers nothing.
fn read_references(args: &Args) -> Result<(Vec<Args>, Vec<Reference>, Listing, usize), ToolError> {
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
    let listing = Listing::new(FIND, &list);
    let start = listing.read(args)?;
    if start > refs.len() {
        return Err(Listing::refused(FIND));
    }
    args.finish()?;
    Ok((elements, refs, listing, start))
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

/// The kinds `search_library` and `list_starred` take, as the items key of
/// their answer names them.
const KINDS: &[&str] = &["songs", "albums", "artists"];

/// `kind`, songs when not given, with its name.
fn read_kind(args: &Args) -> Result<(&'static str, LibraryKind), ToolError> {
    let name = args.opt_choice("kind", KINDS)?.unwrap_or("songs");
    let kind = match name {
        "songs" => LibraryKind::Songs,
        "albums" => LibraryKind::Albums,
        "artists" => LibraryKind::Artists,
        other => unreachable!("opt_choice answers one of KINDS, not {other}"),
    };
    Ok((name, kind))
}

/// A listing's fields: echoed in the answer and the facts of a failure.
fn listing_fields(list: &Value) -> Map<String, Value> {
    let Value::Object(fields) = list else {
        unreachable!("a listing is an object literal");
    };
    fields.clone()
}

const SEARCH: &str = "search_library";

/// The query refusals of `search_library` (D11).
const SEARCH_TOO_SHORT: &str = "text of at least 2 characters, or 1 character other than an ASCII character or a letter Navidrome reads as one ASCII letter (such as é), besides surrounding spaces and a trailing *";
const SEARCH_DROPPED: &str =
    "text with a character other than spaces, quotes, brackets, parentheses, braces, commas and slashes";

/// What search3 searches of `query`: one trailing `*` removed
/// (server/subsonic/searching.go:67), then the spaces around it and one
/// more trailing `*` (persistence/sql_search.go:55-56).
fn searched(query: &str) -> &str {
    let q = query.strip_suffix('*').unwrap_or(query).trim();
    q.strip_suffix('*').unwrap_or(q)
}

/// The lowercase letters deluan/sanitize's Accents turns into one ASCII
/// letter (sanitize.go:218-303 at fdfd8fdfaa55, the commit Navidrome
/// 0.64.2's go.mod pins). Æ, Œ, Þ and ß become two letters ("ae", "oe",
/// "th", "ss"), and every letter it does not list stays as it is.
const ONE_ASCII_LETTER: &str = "àáâãäåăçèéêëìíîïðłñńòóôõōöøřśșşšțùúûūüýÿż";

/// Whether search3 answers nothing for `q` as searched. searchAll lowercases
/// the query and runs sanitize.Accents on it (server/subsonic/searching.go:
/// 67), and doSearch answers nothing for a query under 2 bytes
/// (persistence/sql_search.go:75-77). So one character is too short when it
/// is ASCII or its lowercase is a letter Accents turns into one ASCII
/// letter; any other single character is 2 bytes or more and is searched,
/// as Navidrome means a CJK one to be (persistence/sql_search_like.go:39-41).
fn too_short(q: &str) -> bool {
    let mut chars = q.chars();
    match (chars.next(), chars.next()) {
        (None, _) => true,
        (Some(c), None) => {
            // Go's unicode.ToLower maps İ to i; Rust's to_lowercase gives
            // "i" and a combining dot.
            let lower: Vec<char> = if c == 'İ' {
                vec!['i']
            } else {
                c.to_lowercase().collect()
            };
            match lower.as_slice() {
                [l] => l.is_ascii() || ONE_ASCII_LETTER.contains(*l),
                _ => false,
            }
        }
        _ => false,
    }
}

/// The query, sent as read. A text Navidrome would search as nothing is
/// refused; so is one it may drop: search3 answers `""` with the whole
/// library in natural order (persistence/sql_search.go:61-65), and Soulful
/// refuses every text whose words Navidrome's sanitizer may all remove (D9).
fn read_query(args: &Args) -> Result<String, ToolError> {
    let query = args.string("query")?;
    let q = searched(&query);
    if too_short(q) {
        return Err(invalid_value(SEARCH, "query", SEARCH_TOO_SHORT));
    }
    if !navidrome_keeps(q) {
        return Err(invalid_value(SEARCH, "query", SEARCH_DROPPED));
    }
    Ok(query)
}

/// `search_library`: a page of one kind of item matching the query.
pub(crate) async fn search_library(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(SEARCH, args, &["query", "kind", "limit", "cursor"]);
    answer(
        async {
            let query = read_query(&args)?;
            let (name, kind) = read_kind(&args)?;
            let list = json!({"query": query, "kind": name});
            let page = Page::read(&args, list.clone())?;
            args.finish()?;
            let navi = navidrome_of(state, caller, Stage::LibraryCheck, &args).await?;
            let fields = listing_fields(&list);
            let offset =
                u32::try_from(page.offset).expect("Listing::read keeps an offset within u32");
            let limit = page.limit as u32;
            let found = library::search(&navi, &query, kind, offset, limit + 1)
                .await
                .map_err(|f| upstream_error(f, fields.clone(), args.same_call()))?;
            let mut items = found.into_values();
            let more = items.len() > page.limit;
            items.truncate(page.limit);
            Ok(fit(fields, name, items, &page, more))
        }
        .await,
    )
}

/// `get_library_album`: the album and a page of its songs.
pub(crate) async fn get_library_album(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_library_album", args, &["album_id", "limit", "cursor"]);
    answer(
        async {
            let album_id = args.string("album_id")?;
            let list = json!({"album_id": album_id});
            let page = Page::read(&args, list.clone())?;
            args.finish()?;
            let navi = navidrome_of(state, caller, Stage::LibraryCheck, &args).await?;
            let (album, songs) = library::album(&navi, &album_id)
                .await
                .map_err(|f| upstream_error(f, listing_fields(&list), args.same_call()))?;
            let mut fields = Map::new();
            fields.insert("album".into(), json!(album));
            Ok(fit(fields, "songs", page.of(&songs), &page, false))
        }
        .await,
    )
}

/// `list_starred`: a page of what the caller starred of one kind.
pub(crate) async fn list_starred(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("list_starred", args, &["kind", "limit", "cursor"]);
    answer(
        async {
            let (name, kind) = read_kind(&args)?;
            let list = json!({"kind": name});
            let page = Page::read(&args, list.clone())?;
            args.finish()?;
            let navi = navidrome_of(state, caller, Stage::LibraryCheck, &args).await?;
            let fields = listing_fields(&list);
            let starred = library::starred(&navi, kind)
                .await
                .map_err(|f| upstream_error(f, fields.clone(), args.same_call()))?;
            Ok(fit(
                fields,
                name,
                page.of(&starred.into_values()),
                &page,
                false,
            ))
        }
        .await,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};

    use super::*;
    use crate::connection::test_util::{call_tool, document, mcp_app, mcp_app_with};
    use crate::core::library::LOOKUPS_AT_ONCE;
    use crate::mcp::args::tests::large_references;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{
        child, native_song, song_requests, subsonic_failed, subsonic_ok, NavidromeFor,
        NavidromeStub, BAD_GATEWAY,
    };

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
                        "starred_at": "2026-11-02T18:04:51Z", "play_count": 3,
                        "match": {"kind": "exact", "score": null, "differs": [], "duration_delta_s": 0,
                            "unchecked": []}}]}],
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
            BAD_GATEWAY,
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
            (&json!(502), &json!(BAD_GATEWAY))
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
        navi.answer_when("/api/song", "", 502, BAD_GATEWAY);
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
                "cursor": Listing::new("find_in_library", &list).cursor(offset)})
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
            "error returned from database: (code: 5) database is locked. The call is too large to offer again in next."
        );
    }

    #[tokio::test]
    async fn search_library_pages_through_one_kind() {
        let navi = NavidromeStub::start();
        let three: Vec<String> = ["O Holy Night", "Silent Night", "Night Song"]
            .iter()
            .enumerate()
            .map(|(i, t)| child(&format!("s{i}"), "Marrow Choir", t))
            .collect();
        navi.answer(
            "/rest/search3",
            &[&subsonic_ok(&format!(
                r#""searchResult3":{{"song":[{}]}}"#,
                three.join(",")
            ))],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "search_library",
                json!({"query": "night", "limit": 2}),
            )
            .await,
        );
        assert_eq!(
            (
                doc["songs"].as_array().unwrap().len(),
                doc["songs"][0]["song_id"].as_str(),
                doc["songs"][0]["path"].is_null()
            ),
            (2, Some("s0"), true)
        );
        assert!(doc["cursor"].is_string());
        assert!(navi.asked_with_queries()[0].contains("songCount=3"));
    }

    #[tokio::test]
    async fn search_library_refuses_what_navidrome_would_not_search() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/search3", &[&subsonic_ok(r#""searchResult3":{}"#)]);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let short = "text of at least 2 characters, or 1 character other than an ASCII character or a letter Navidrome reads as one ASCII letter (such as é), besides surrounding spaces and a trailing *";
        for (query, expected) in [
            (" n* ", short),
            // sanitize.Accents turns it into "e", one byte (server/subsonic/searching.go:67)
            ("é", short),
            // lowercased to ō first, then "o"
            ("Ō", short),
            ("()", "text with a character other than spaces, quotes, brackets, parentheses, braces, commas and slashes"),
        ] {
            let e = &document(
                &call_tool(
                    app.clone(),
                    &token,
                    "search_library",
                    json!({"query": query}),
                )
                .await,
            )["error"];
            assert_eq!(
                (e["code"].as_str(), &e["facts"]),
                (
                    Some("invalid_input"),
                    &json!({"field": "query", "expected": expected})
                ),
                "{query}"
            );
        }
        assert!(navi.asked_with_queries().is_empty());
        let doc = document(&call_tool(app, &token, "search_library", json!({"query": "ét"})).await);
        assert_eq!(doc["ok"], true);
        assert!(
            navi.asked_with_queries()[0].contains("query=%C3%A9t"),
            "{:?}",
            navi.asked_with_queries()
        );
    }

    #[tokio::test]
    async fn an_album_navidrome_cannot_find_is_an_unknown_handle() {
        let navi = NavidromeStub::start();
        // server/subsonic/browsing.go:197
        navi.answer("/rest/getAlbum", &[&subsonic_failed(70, "Album not found")]);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let e = &document(
            &call_tool(
                app,
                &token,
                "get_library_album",
                json!({"album_id": "7hQ2nLr5VxZ0cYbM8wTkAf"}),
            )
            .await,
        )["error"];
        assert_eq!(
            (
                e["code"].as_str(),
                e["stage"].as_str(),
                e["retry"].as_str(),
                e["message"].as_str()
            ),
            (
                Some("unknown_handle"),
                Some("input"),
                Some("not_retryable"),
                Some("Navidrome has no album 7hQ2nLr5VxZ0cYbM8wTkAf that this account can see")
            )
        );
        assert_eq!(e["next"], json!([]));
    }

    #[tokio::test]
    async fn an_album_comes_with_its_songs() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getAlbum",
            &[&subsonic_ok(&format!(
                r#""album":{{"id":"7hQ2nLr5VxZ0cYbM8wTkAf","name":"Winter Songs","artist":"Marrow Choir","songCount":1,"duration":263,"year":2019,"userRating":5,"genres":[],"song":[{}]}}"#,
                child("3sG8kPq0WmZ1xYbN4vTcDe", "Marrow Choir", "O Holy Night")
            ))],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(
            &call_tool(
                app,
                &token,
                "get_library_album",
                json!({"album_id": "7hQ2nLr5VxZ0cYbM8wTkAf"}),
            )
            .await,
        );
        assert_eq!(
            (
                doc["album"]["name"].as_str(),
                doc["album"]["year"].as_u64(),
                doc["album"]["rating"].as_u64(),
                doc["songs"][0]["title"].as_str()
            ),
            (
                Some("Winter Songs"),
                Some(2019),
                Some(5),
                Some("O Holy Night")
            )
        );
    }

    #[tokio::test]
    async fn starred_artists_are_listed_and_a_refusal_is_not_offered_again() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getStarred2",
            &[
                &subsonic_ok(
                    r#""starred2":{"artist":[{"id":"2kD9pWq4ZxC1vBnM6yTjLs","name":"Marrow Choir","albumCount":1,"starred":"2026-11-03T09:10:00.000Z","musicBrainzId":""}]}"#,
                ),
                // getStarredItems returns the database error and mapToSubsonicError writes it
                // (album_lists.go:190-194, api.go:290-291)
                &subsonic_failed(0, "Internal Server Error: database is locked"),
            ],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let doc = document(
            &call_tool(
                app.clone(),
                &token,
                "list_starred",
                json!({"kind": "artists"}),
            )
            .await,
        );
        assert_eq!(
            doc["artists"],
            json!([{"artist_id": "2kD9pWq4ZxC1vBnM6yTjLs", "name": "Marrow Choir", "album_count": 1, "rating": 0,
                "starred": true, "starred_at": "2026-11-03T09:10:00Z", "mbid": null}])
        );
        let e =
            &document(&call_tool(app, &token, "list_starred", json!({"kind": "artists"})).await)
                ["error"];
        assert_eq!(
            (e["code"].as_str(), &e["next"]),
            (Some("navidrome_refused"), &json!([]))
        );
    }

    #[test]
    fn one_letter_is_searched_when_navidrome_reads_it_as_two_bytes() {
        // sanitize.Accents (sanitize.go:218-303 at fdfd8fdfaa55): ß to "ss", æ to "ae", Œ (lowercased
        // to œ) to "oe", þ to "th"; × and ÷ and Cyrillic, Greek and CJK letters are not in the table
        // and stay 2 or more bytes.
        for q in ["ß", "æ", "Œ", "þ", "ẞ", "×", "÷", "я", "λ", "日"] {
            assert!(!too_short(q), "{q}");
        }
        for q in ["", "n", "7", "é", "Ø", "ł", "ż", "Ÿ", "İ"] {
            assert!(too_short(q), "{q}");
        }
        assert!(!too_short("no"));
    }

    #[tokio::test]
    async fn search_library_follows_its_cursor() {
        let navi = NavidromeStub::start();
        let songs = |ids: &[&str]| {
            subsonic_ok(&format!(
                r#""searchResult3":{{"song":[{}]}}"#,
                ids.iter()
                    .map(|id| child(id, "Marrow Choir", "Silent Night"))
                    .collect::<Vec<_>>()
                    .join(",")
            ))
        };
        navi.answer(
            "/rest/search3",
            &[&songs(&["s0", "s1", "s2"]), &songs(&["s2", "s3"])],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let args = json!({"query": "night", "limit": 2});
        let first = document(&call_tool(app.clone(), &token, "search_library", args.clone()).await);
        let mut next = args;
        next["cursor"] = first["cursor"].clone();
        let second = document(&call_tool(app, &token, "search_library", next).await);
        let ids = |doc: &Value| {
            doc["songs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["song_id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            (ids(&first), ids(&second)),
            (
                vec!["s0".to_string(), "s1".to_string()],
                vec!["s2".to_string(), "s3".to_string()]
            )
        );
        assert_eq!(second["cursor"], Value::Null);
        let asked: Vec<String> = navi
            .asked_with_queries()
            .into_iter()
            .filter(|t| t.starts_with("/rest/search3"))
            .collect();
        assert!(
            asked[1].contains("songCount=3") && asked[1].contains("songOffset=2"),
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn an_album_page_at_or_past_its_last_song_is_empty_and_ends() {
        let navi = NavidromeStub::start();
        let album = subsonic_ok(&format!(
            r#""album":{{"id":"7hQ2nLr5VxZ0cYbM8wTkAf","name":"Winter Songs","artist":"Marrow Choir","songCount":2,"duration":526,"year":2019,"genres":[],"song":[{},{}]}}"#,
            child("s1", "Marrow Choir", "O Holy Night"),
            child("s2", "Marrow Choir", "Silent Night")
        ));
        navi.answer("/rest/getAlbum", &[&album]);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let list = json!({"album_id": "7hQ2nLr5VxZ0cYbM8wTkAf"});
        let at = |offset: usize| {
            json!({"album_id": "7hQ2nLr5VxZ0cYbM8wTkAf", "limit": 1,
                "cursor": Listing::new("get_library_album", &list).cursor(offset)})
        };
        let last = document(&call_tool(app.clone(), &token, "get_library_album", at(1)).await);
        assert_eq!(
            (last["songs"][0]["song_id"].as_str(), &last["cursor"]),
            (Some("s2"), &Value::Null)
        );
        for offset in [2, 7] {
            let doc =
                document(&call_tool(app.clone(), &token, "get_library_album", at(offset)).await);
            assert_eq!(
                (&doc["album"]["name"], &doc["songs"], &doc["cursor"]),
                (&json!("Winter Songs"), &json!([]), &Value::Null),
                "{offset}"
            );
        }
    }

    #[tokio::test]
    async fn starred_songs_page_to_their_end() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getStarred2",
            &[&subsonic_ok(&format!(
                r#""starred2":{{"song":[{},{}]}}"#,
                child("s1", "Marrow Choir", "O Holy Night"),
                child("s2", "Marrow Choir", "Silent Night")
            ))],
        );
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let list = json!({"kind": "songs"});
        let at = |offset: usize| json!({"limit": 1, "cursor": Listing::new("list_starred", &list).cursor(offset)});
        let first =
            document(&call_tool(app.clone(), &token, "list_starred", json!({"limit": 1})).await);
        assert_eq!(
            first["cursor"],
            json!(Listing::new("list_starred", &list).cursor(1))
        );
        let last = document(&call_tool(app.clone(), &token, "list_starred", at(1)).await);
        assert_eq!(
            (last["songs"][0]["song_id"].as_str(), &last["cursor"]),
            (Some("s2"), &Value::Null)
        );
        for offset in [2, 9] {
            let doc = document(&call_tool(app.clone(), &token, "list_starred", at(offset)).await);
            assert_eq!(
                (&doc["songs"], &doc["cursor"]),
                (&json!([]), &Value::Null),
                "{offset}"
            );
        }
    }

    #[tokio::test]
    async fn an_entry_of_control_characters_stays_under_the_cap() {
        // Five files from a Soulseek peer whose tags and paths are padded with U+0001, which
        // Navidrome keeps (model/metadata/metadata.go:366-380) and serde_json writes as \u0001.
        let navi = NavidromeStub::start();
        let pad = "\u{1}".repeat(488);
        let five: Vec<String> = (0..5)
            .map(|i| {
                let mut song: Value = serde_json::from_str(&native_song(
                    &format!("s{i}"),
                    &format!("Bing Crosby{pad}"),
                    &format!("Silent Night{pad}"),
                    149.0,
                    None,
                ))
                .unwrap();
                song["album"] = json!("\u{1}".repeat(500));
                song["path"] = json!("\u{1}".repeat(500));
                song["suffix"] = json!("\u{1}".repeat(500));
                song.to_string()
            })
            .collect();
        navi.answer("/api/song", &[&format!("[{}]", five.join(","))]);
        let (_, app, token) = mcp_app_with(Arc::new(NavidromeFor(Ok(navi.client())))).await;
        let result = call_tool(
            app,
            &token,
            "find_in_library",
            json!({"references": [{"artist": "Bing Crosby", "title": "Silent Night"}]}),
        )
        .await;
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.chars().count() <= 20_000, "{}", text.chars().count());
        let doc = document(&result);
        let matches = doc["results"][0]["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 5);
        assert!(matches.iter().all(|m| m["match"]["kind"] == "exact"));
    }
}
