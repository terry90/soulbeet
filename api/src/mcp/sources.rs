//! The source tools: `find_sources` searches the download backend for a reference and
//! `get_sources` reads that search later, a page of candidates at a time.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::download::SearchStats;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use tokio::task::JoinHandle;

use super::args::Args;
use super::time;
use super::contract::{answer, failed, upstream_error, NextCall, ToolError};
use super::facts::{candidate_document, search_facts};
use super::keys::{keyed, request_key, Keyed};
use super::paging::{fit, Listing, Page, MAX_LIMIT};
use super::refs::{
    filters, filters_schema, page_schema, reference_schema, reference_value, request_key_schema,
    wanted, REFERENCE_FIELDS,
};
use super::server::{common_codes, ToolSpec};
use super::{wait, McpCaller};
use crate::auth::UserCtx;
use crate::connection::ConnectionState;
use crate::core::database_failure;
use crate::core::jobs::runner::Acquire;
use crate::core::jobs::Wanted;
use crate::core::sources::{
    no_candidate_passed, owned_search, search_not_found, server_restarted_search, start_find, view,
    DroppedByFilter, FindRefused, SearchProgress, SearchRun, FINDS_WAITING_PER_USER, FIND_WAIT,
    RECENT_FAILURES_SECS,
};
use crate::ledger::store::{self, StoredSearch};
use crate::ledger::StoredSpec;

const FIND_SOURCES_ARGS: &[&str] = &[
    "reference",
    "query_text",
    "formats",
    "min_bitrate_kbps",
    "lossless_only",
    "max_queue_length",
    "include_below_floor",
    "limit",
    "request_key",
];

const GET_SOURCES_ARGS: &[&str] = &["search_id", "wait_seconds", "limit", "cursor"];

pub(crate) const FIND_SOURCES: ToolSpec = ToolSpec {
    name: "find_sources",
    title: "Find download sources",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Searches the download backend (slskd, the Soulseek network) for a track or an album and ranks what it finds. Takes a reference ({{artist, title, album?, recording_mbid?, duration_s?}} for a track, or {{kind: \"album\", artist, title, release_mbid}} for an album, whose tracklist is read from MusicBrainz), optional query_text to send instead of \"artist title\", quality filters (formats, min_bitrate_kbps, lossless_only, max_queue_length) and include_below_floor to keep weak matches, flagged. Returns candidates with candidate_id, source, remote file or folder, extension, size, bitrate, sample rate, bit depth, duration and its difference from the wanted one, match and quality scores, for albums the tracks found and the missing titles, folder_track_count (the audio files of its folder in the source's answer: a source answers only files whose path holds every word searched, so a track search counts about one), the source's free slot, speed, queue and its failed transfers in the last 24 hours (3 or more: flagged, never picked automatically). Waits up to 45 seconds; a longer search answers state pending with search_id for get_sources. Searches run one at a time for the whole server, every user's included: pending says how many run or wait before this one (searches_ahead), and a user with {FINDS_WAITING_PER_USER} searches waiting is answered rate_limited with their search_ids. A repeat with the same request_key answers the first call's result instead of searching again, unless that call was refused before its search started: then the repeat runs. Search ids and candidate ids stay valid 24 hours. Failure codes: {common}, backend_not_configured, backend_unreachable, metadata_not_found, metadata_unavailable (album tracklist), search_no_results (nothing on the network), search_all_filtered (found, but scored out or filtered; the facts say how many of each), search_timeout, search_query_rejected, search_already_running, search_lost, search_rate_limited, already_running (a call with this request_key still runs).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["reference"], "properties": {"reference": "#,
        reference_schema!(),
        r#", "query_text": {"type": "string", "minLength": 1}, "#,
        filters_schema!(),
        r#", "include_below_floor": {"type": "boolean"}, "limit": {"type": "integer", "minimum": 1, "maximum": 100}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: true,
    call: |state, caller, args| Box::pin(find_sources(state, caller, args)),
};

pub(crate) const GET_SOURCES: ToolSpec = ToolSpec {
    name: "get_sources",
    title: "Read a source search",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Reads a search find_sources started: its state, what it read and its candidates, a page at a time. Takes search_id, wait_seconds (0 to 180) to wait for a running search, limit and cursor. Failure codes: {common}, unknown_handle (no such search for this user), expired_handle (older than 24 hours), search_stopped (the search of a cancelled job), server_restarted (Soulful restarted while the search ran), and the search's own failure codes as find_sources lists them.")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["search_id"], "properties": {"search_id": {"type": "string", "minLength": 1}, "wait_seconds": {"type": "integer", "minimum": 0, "maximum": 180}, "#,
        page_schema!(),
        "}}"
    ),
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(get_sources(state, caller, args)),
};

/// A `find_sources` call as read.
struct Find {
    wanted: Wanted,
    spec: StoredSpec,
    limit: Option<u32>,
}

/// `find_sources`: starts a search for the reference and answers it once it ends, or as
/// pending after FIND_WAIT.
pub(crate) async fn find_sources(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("find_sources", args, FIND_SOURCES_ARGS);
    let (find, key) = match read_find(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = json!({"reference": find.spec.reference, "query_text": find.spec.text,
        "filters": find.spec.filters, "include_below_floor": find.spec.keep_below_floor,
        "limit": find.limit});
    let (acquire, ctx) = (state.acquire.clone(), caller.ctx.clone());
    let work = async move {
        match start_search(&acquire, &ctx, find).await {
            Ok(started) => Keyed::Kept(answer(searched(&acquire, &ctx, started).await)),
            // Nothing was written: the same call can run again.
            Err(refused) => Keyed::Released(failed(refused)),
        }
    };
    keyed(&state.acquire, caller, "find_sources", key, &call, work).await
}

fn read_find(args: &Args) -> Result<(Find, Option<String>), ToolError> {
    let wanted = wanted(&args.object("reference", REFERENCE_FIELDS)?)?;
    let text = args.opt_string("query_text")?;
    let filters = filters(args)?;
    let keep_below_floor = args.opt_bool("include_below_floor")?.unwrap_or(false);
    let limit = args.opt_u32("limit", 1, MAX_LIMIT)?;
    let key = request_key(args)?;
    args.finish()?;
    let spec = StoredSpec {
        text,
        keep_below_floor,
        filters,
        reference: reference_value(&wanted),
    };
    Ok((
        Find {
            wanted,
            spec,
            limit,
        },
        key,
    ))
}

/// A search find_sources recorded and started.
struct Started {
    id: String,
    task: JoinHandle<SearchProgress>,
    spec: StoredSpec,
    limit: Option<u32>,
}

/// Records and starts the search `find` asks for. Each refusal comes before the search is
/// recorded, so it wrote nothing.
async fn start_search(
    acquire: &Arc<Acquire>,
    ctx: &UserCtx,
    find: Find,
) -> Result<Started, ToolError> {
    let Find {
        wanted,
        spec,
        limit,
    } = find;
    // Spares the tracklist lookup of a call start_find would refuse; start_find counts again as
    // the search enters the line, which calls made together cannot pass together.
    let waiting = acquire.searches.waiting_finds(&ctx.id());
    if waiting.len() >= FINDS_WAITING_PER_USER {
        return Err(too_many_waiting(waiting));
    }
    let mut query = wanted.query();
    if let Err(f) = acquire.effects.hydrate(&mut query).await {
        let mut facts = Map::new();
        if let Wanted::Album(album) = &wanted {
            facts.insert("release_mbid".into(), json!(album.release_mbid));
        }
        return Err(failure_of(f, facts, &spec, &Failed::Elsewhere));
    }
    let backend = acquire
        .effects
        .backend()
        .await
        .map_err(|f| failure_of(f, Map::new(), &spec, &Failed::Elsewhere))?;
    let id = uuid::Uuid::new_v4().to_string();
    let run = SearchRun {
        id: id.clone(),
        query,
        spec: spec.clone(),
    };
    let task = start_find(&acquire.searches, &acquire.ledger, ctx, backend, run)
        .await
        .map_err(|refused| match refused {
            FindRefused::TooManyWaiting(waiting) => too_many_waiting(waiting),
            FindRefused::Failed(f) => failure_of(f, Map::new(), &spec, &Failed::Elsewhere),
        })?;
    Ok(Started {
        id,
        task,
        spec,
        limit,
    })
}

/// The answer to the search `started`: once it ends, or as pending after FIND_WAIT.
async fn searched(
    acquire: &Arc<Acquire>,
    ctx: &UserCtx,
    started: Started,
) -> Result<Map<String, Value>, ToolError> {
    let Started {
        id,
        task,
        spec,
        limit,
    } = started;
    // Past FIND_WAIT the search goes on in its task, and is read as stored either way.
    let _ = tokio::time::timeout(FIND_WAIT, task).await;
    let stored = owned_search(&acquire.db, ctx, &id)
        .await
        .map_err(|f| failure_of(f, Map::new(), &spec, &Failed::Elsewhere))?;
    let mut page_args = Map::new();
    page_args.insert("search_id".into(), json!(id));
    if let Some(limit) = limit {
        page_args.insert("limit".into(), json!(limit));
    }
    // The page get_sources reads for this search, so the cursor continues there.
    let page = Page::read(
        &Args::new("get_sources", Some(page_args), GET_SOURCES_ARGS),
        json!({"search_id": id}),
    )?;
    search_answer(acquire, stored, &page).await
}

/// `get_sources`: the caller's search, once it is not running or `wait_seconds` passed.
pub(crate) async fn get_sources(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_sources", args, GET_SOURCES_ARGS);
    let acquire = &state.acquire;
    answer(
        async {
            let search_id = args.string("search_id")?;
            let wait = args.opt_u32("wait_seconds", 0, 180)?.unwrap_or(0);
            let page = Page::read(&args, json!({"search_id": search_id}))?;
            args.finish()?;
            let stored = wait::until(
                &acquire.ledger,
                Duration::from_secs(wait.into()),
                || mcp_search(acquire, &caller.ctx, &search_id),
                |stored| !runs(acquire, stored),
            )
            .await
            .map_err(|f| {
                let facts = Map::from_iter([("search_id".to_string(), json!(search_id))]);
                upstream_error(f, facts, args.same_call())
            })?;
            search_answer(acquire, stored, &page).await
        }
        .await,
    )
}

/// `ctx`'s MCP search `id`; a web UI search reads like an unknown one, since no tool started
/// it.
async fn mcp_search(acquire: &Acquire, ctx: &UserCtx, id: &str) -> Result<StoredSearch, Failure> {
    let stored = owned_search(&acquire.db, ctx, id).await?;
    match stored.spec {
        Some(_) => Ok(stored),
        None => Err(search_not_found()),
    }
}

/// Whether `stored` has not ended and a task of this process runs it.
fn runs(acquire: &Acquire, stored: &StoredSearch) -> bool {
    stored.candidates.is_none() && stored.failure.is_none() && acquire.searches.runs(&stored.id)
}

/// A stored MCP search as find_sources and get_sources answer it: pending while it runs; its
/// failure, or no_candidate_passed when its filters keep no candidate; else what it read and
/// `page` of its candidates, `next` continuing in get_sources when more follow.
async fn search_answer(
    acquire: &Acquire,
    stored: StoredSearch,
    page: &Page,
) -> Result<Map<String, Value>, ToolError> {
    let spec = stored.spec.clone().unwrap_or_default();
    let id = stored.id.clone();
    let (groups, stats) = match SearchProgress::of(&stored) {
        SearchProgress::Running if acquire.searches.runs(&id) => return Ok(pending(acquire, &id)),
        SearchProgress::Running => {
            return Err(search_failed(
                server_restarted_search(),
                &id,
                &spec,
                None,
                &DroppedByFilter::default(),
            ))
        }
        SearchProgress::Failed(f, stats) => {
            return Err(search_failed(
                f,
                &id,
                &spec,
                stats.as_ref(),
                &DroppedByFilter::default(),
            ))
        }
        SearchProgress::Done(groups, stats) => (groups, stats),
    };
    let since = chrono::Utc::now().timestamp() - RECENT_FAILURES_SECS;
    let failures = store::recent_failures(&acquire.db, since)
        .await
        .map_err(|e| {
            failure_of(
                database_failure(Stage::SourceSearch, e),
                Map::new(),
                &spec,
                &Failed::Elsewhere,
            )
        })?;
    let view = view(&id, &groups, &spec.filters, &failures, &[]);
    if view.candidates.is_empty() {
        return Err(search_failed(
            no_candidate_passed(),
            &id,
            &spec,
            stats.as_ref(),
            &view.dropped_by_filter,
        ));
    }
    let documents: Vec<Value> = view
        .candidates
        .iter()
        .map(|c| candidate_document(c, &stored.query))
        .collect();
    let mut fields = Map::new();
    fields.insert("search_id".into(), json!(id));
    fields.insert("state".into(), json!("done"));
    fields.insert("expires_at".into(), time::unix(stored.expires_at));
    fields.extend(search_facts(stats.as_ref(), &spec, &view.dropped_by_filter));
    // Room for `next`: its cursor is never longer than the one at the page's end.
    let longest =
        Listing::new("get_sources", &json!({"search_id": id})).cursor(page.offset + page.limit);
    fields.insert("next".into(), json!([continue_call(&id, &longest)]));
    let mut fields = fit(fields, "candidates", page.of(&documents), page, false);
    match fields["cursor"].as_str() {
        Some(cursor) => {
            let next = json!([continue_call(&id, cursor)]);
            fields.insert("next".into(), next);
        }
        None => {
            fields.remove("next");
        }
    }
    Ok(fields)
}

fn continue_call(search_id: &str, cursor: &str) -> NextCall {
    NextCall::new(
        "get_sources",
        json!({"search_id": search_id, "cursor": cursor}),
    )
}

fn read_later(search_id: &str) -> NextCall {
    NextCall::new(
        "get_sources",
        json!({"search_id": search_id, "wait_seconds": 60}),
    )
}

/// A running search: how many run or wait before it, and the call that reads it later.
fn pending(acquire: &Acquire, id: &str) -> Map<String, Value> {
    let ahead = acquire.searches.ahead_of(id).unwrap_or(0);
    let Value::Object(fields) = json!({"search_id": id, "state": "pending",
        "searches_ahead": ahead, "next": [read_later(id)]})
    else {
        unreachable!("an object literal is an object");
    };
    fields
}

/// The user has FINDS_WAITING_PER_USER searches waiting for the turn, `waiting`.
fn too_many_waiting(waiting: Vec<String>) -> ToolError {
    ToolError {
        failure: Failure::new(
            FailureCode::RateLimited,
            Stage::SourceSearch,
            RetryAdvice::SameSourceLater,
            format!("This user already has {FINDS_WAITING_PER_USER} source searches waiting for their turn; read them with get_sources first"),
        ),
        tried: vec![],
        next: waiting.iter().map(|id| read_later(id)).collect(),
        facts: Map::from_iter([("search_ids".to_string(), json!(waiting))]),
    }
}

/// What failed, which decides the calls offered after it.
enum Failed<'a> {
    /// A step other than the search (the album's tracklist, the backend, the ledger), which
    /// other search words cannot mend.
    Elsewhere,
    /// The search, which the backend did not finish.
    Search,
    /// The search the backend finished: the below-floor files it dropped and what the filters
    /// dropped.
    FinishedSearch(u32, &'a DroppedByFilter),
}

/// Search `id`'s failure `f`, with what it read (`stats`) and what the filters dropped.
fn search_failed(
    f: Failure,
    id: &str,
    spec: &StoredSpec,
    stats: Option<&SearchStats>,
    dropped: &DroppedByFilter,
) -> ToolError {
    let mut facts = search_facts(stats, spec, dropped);
    facts.insert("search_id".into(), json!(id));
    let failed = match stats {
        Some(s) if !spec.keep_below_floor => Failed::FinishedSearch(s.below_floor, dropped),
        Some(_) => Failed::FinishedSearch(0, dropped),
        None => Failed::Search,
    };
    failure_of(f, facts, spec, &failed)
}

/// `f` with `facts`, and the find_sources calls that can follow a search of `spec` (each built
/// from it, never from the call's own arguments): the same search when the same source can
/// answer later, with below-floor files kept when it dropped some, without filters when they
/// dropped some, and, when the search itself failed with advice for another query, with other
/// words, which only the caller can choose. A failure elsewhere (a release MusicBrainz does not
/// have) offers no other words: they never reach that step. A search with no
/// reference (a job's) offers none: its job's calls follow it.
fn failure_of(
    f: Failure,
    facts: Map<String, Value>,
    spec: &StoredSpec,
    failed: &Failed,
) -> ToolError {
    let mut next = vec![];
    if spec.reference.is_object() {
        if f.retry == RetryAdvice::SameSourceLater {
            let mut call = find_call(spec);
            if let Some(seconds) = f.retry_after_seconds {
                call = call.with("retry_after_seconds", json!(seconds));
            }
            next.push(call);
        }
        if let Failed::FinishedSearch(below_floor, by_filter) = failed {
            if *below_floor > 0 {
                next.push(find_call(&StoredSpec {
                    keep_below_floor: true,
                    ..spec.clone()
                }));
            }
            let DroppedByFilter {
                format,
                bitrate,
                not_lossless,
                queue_length,
                excluded,
            } = by_filter;
            if format + bitrate + not_lossless + queue_length + excluded > 0 {
                next.push(find_call(&StoredSpec {
                    filters: Default::default(),
                    ..spec.clone()
                }));
            }
        }
        let searched = matches!(failed, Failed::Search | Failed::FinishedSearch(..));
        if searched && f.retry == RetryAdvice::OtherQuery {
            next.push(
                NextCall::new("find_sources", json!({"reference": spec.reference}))
                    .with("needs", json!(["query_text"])),
            );
        }
    }
    ToolError {
        failure: f,
        facts,
        tried: vec![],
        next,
    }
}

/// find_sources for `spec`: its reference, text, filters and below-floor choice, each only when
/// given.
fn find_call(spec: &StoredSpec) -> NextCall {
    let mut args = Map::new();
    args.insert("reference".into(), spec.reference.clone());
    if let Some(text) = &spec.text {
        args.insert("query_text".into(), json!(text));
    }
    let f = &spec.filters;
    if !f.formats.is_empty() {
        args.insert("formats".into(), json!(f.formats));
    }
    if let Some(kbps) = f.min_bitrate_kbps {
        args.insert("min_bitrate_kbps".into(), json!(kbps));
    }
    if f.lossless_only {
        args.insert("lossless_only".into(), json!(true));
    }
    if let Some(queue) = f.max_queue_length {
        args.insert("max_queue_length".into(), json!(queue));
    }
    if spec.keep_below_floor {
        args.insert("include_below_floor".into(), json!(true));
    }
    NextCall::new("find_sources", Value::Object(args))
}

#[cfg(test)]
mod tests {
    use shared::download::{DownloadableItem, FileFacts, SearchResult, SearchState};
    use shared::failure::Service;
    use soulbeet::MetadataError;

    use super::*;
    use crate::connection::test_util::{
        access_token, add_user, alice_caller, app_of, app_with_acquire, call_tool, count, document,
        text_of, RESOURCE,
    };
    use crate::core::jobs::runner::TestEffects;
    use crate::mcp::args::tests::obj;
    use crate::server_fns::download::test_support::{group, item, search_result, track};

    // The searches below run on the real clock: a paused one jumps ahead while SQLite answers on
    // its own thread, timing out the pool. Each search polls once, after 2 s.

    #[tokio::test]
    async fn find_sources_answers_ranked_candidates_with_their_facts() {
        let (db, acq, effects, token) = app_with_acquire().await;
        effects.stub().script_polls(vec![Ok(SearchResult {
            stats: SearchStats {
                query_text: "Marrow Choir O Holy Night".into(),
                response_count: 1,
                file_count: 1,
                ..Default::default()
            },
            ..search_result(
                "search",
                SearchState::Completed,
                vec![group(
                    "carol_shares",
                    vec![DownloadableItem {
                        facts: FileFacts {
                            extension: Some("flac".into()),
                            sample_rate_hz: Some(44100),
                            bit_depth: Some(16),
                            source_has_free_slot: Some(true),
                            ..FileFacts::default()
                        },
                        duration: Some(264),
                        match_score: 0.91,
                        ..item(
                            "carol_shares",
                            "O Holy Night",
                            "@@kqzbm\\Music\\Marrow Choir\\Winter Songs\\03 - O Holy Night.flac",
                        )
                    }],
                    0.84,
                )],
            )
        })]);
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "find_sources",
                json!({"reference": {"artist": "Marrow Choir", "title": "O Holy Night", "duration_s": 263}}),
            )
            .await,
        );
        assert_eq!(
            (
                doc["ok"].as_bool(),
                doc["state"].as_str(),
                doc["file_count"].as_u64()
            ),
            (Some(true), Some("done"), Some(1)),
            "{doc}"
        );
        let c = &doc["candidates"][0];
        assert_eq!(
            (
                c["source"].as_str(),
                c["extension"].as_str(),
                c["duration_delta_s"].as_u64(),
                c["download_scope"].as_str()
            ),
            (Some("carol_shares"), Some("flac"), Some(1), Some("file"))
        );
        assert_eq!(
            (
                c["remote_file"].as_str(),
                c["peer_has_free_slot"].as_bool(),
                c["sample_rate_hz"].as_u64()
            ),
            (
                Some("@@kqzbm\\Music\\Marrow Choir\\Winter Songs\\03 - O Holy Night.flac"),
                Some(true),
                Some(44100)
            )
        );
        assert!(c["candidate_id"].as_str().unwrap().ends_with(":0"));
        // One page holds every candidate.
        assert_eq!((&doc["cursor"], doc.get("next")), (&Value::Null, None));
        let spec = &effects.stub().specs()[0];
        assert_eq!(
            (spec.tracks[0].duration.as_deref(), spec.text.as_deref()),
            (Some("4:23"), None)
        );
    }

    #[tokio::test]
    async fn an_album_reference_searches_with_its_tracklist_and_names_the_missing_titles() {
        let (db, acq, effects, token) = app_with_acquire().await;
        effects.set_tracks(vec![
            track("Kowloon", "Wake Up"),
            track("Kowloon", "Come Over"),
            track("Kowloon", "Lights Out"),
        ]);
        effects.stub().script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![group(
                "peer_a",
                vec![
                    item("peer_a", "Wake Up", "a\\Come Over\\07.flac"),
                    item("peer_a", "Come Over", "a\\Come Over\\08.flac"),
                ],
                0.9,
            )],
        ))]);
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "find_sources",
                json!({"reference": {"kind": "album", "artist": "Kowloon", "title": "Come Over",
                    "release_mbid": "6a09041b-0f79-3278-88d9-0552dd28d37f"}}),
            )
            .await,
        );
        let c = &doc["candidates"][0];
        assert_eq!(
            (
                c["download_scope"].as_str(),
                c["tracks_found"].as_u64(),
                &c["missing_titles"]
            ),
            (Some("folder"), Some(2), &json!(["Lights Out"])),
            "{doc}"
        );
        assert_eq!(
            (
                c["remote_folder"].as_str(),
                c["files"].as_array().map(Vec::len)
            ),
            (Some("a\\Come Over"), Some(2))
        );
        assert_eq!(
            effects.stub().specs()[0]
                .tracks
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Wake Up", "Come Over", "Lights Out"]
        );
    }

    #[tokio::test]
    async fn a_backend_that_cannot_search_says_who_fixes_it_and_when_to_call_again() {
        use crate::core::backend_unavailable;
        use crate::services::BackendLookupError;
        use soulbeet::error::{MissingSetting, SoulseekError};
        let (db, acq, effects, token) = app_with_acquire().await;
        let reference = json!({"artist": "Marrow Choir", "title": "O Holy Night"});
        let find = |effects: &TestEffects, e: BackendLookupError| {
            effects.fail_backend(backend_unavailable(Stage::SourceSearch, &e));
            call_tool(
                app_of(&db, &acq),
                &token,
                "find_sources",
                json!({"reference": reference}),
            )
        };
        // No slskd URL saved: a person sets it.
        let unset = BackendLookupError::Backend(SoulseekError::NotConfigured(MissingSetting::Url));
        let error = document(&find(&effects, unset).await)["error"].clone();
        assert_eq!(
            (
                error["code"].as_str(),
                error["retry"].as_str(),
                error["user_action"]["code"].as_str()
            ),
            (
                Some("backend_not_configured"),
                Some("needs_user"),
                Some("configure_backend")
            )
        );
        assert_eq!(error["next"], json!([]));
        // slskd does not answer: the same call later.
        let down = BackendLookupError::Backend(SoulseekError::Api {
            status: 503,
            message: "Connection failed: error sending request".into(),
        });
        let error = document(&find(&effects, down).await)["error"].clone();
        assert_eq!(
            (error["code"].as_str(), error["retry"].as_str()),
            (Some("backend_unreachable"), Some("same_source_later"))
        );
        let mut again = reference.clone();
        again["kind"] = json!("track");
        assert_eq!(
            error["next"],
            json!([{"tool": "find_sources", "args": {"reference": again}}])
        );
        assert!(effects.stub().specs().is_empty(), "nothing was searched");
    }

    #[tokio::test]
    async fn a_search_that_scored_everything_out_says_so_and_offers_the_floor() {
        let (db, acq, effects, token) = app_with_acquire().await;
        effects.stub().script_polls(vec![Ok(SearchResult {
            stats: SearchStats {
                query_text: "Marrow Choir O Holy Night".into(),
                response_count: 4,
                file_count: 23,
                locked_file_count: 2,
                not_audio: 3,
                below_floor: 20,
            },
            ..search_result("search", SearchState::Completed, vec![])
        })]);
        let reference = json!({"artist": "Marrow Choir", "title": "O Holy Night"});
        let result = call_tool(
            app_of(&db, &acq),
            &token,
            "find_sources",
            json!({"reference": reference}),
        )
        .await;
        assert_eq!(result["isError"], true);
        let error = &document(&result)["error"];
        assert_eq!(
            (
                error["code"].as_str(),
                error["retry"].as_str(),
                error["message"].as_str()
            ),
            (
                Some("search_all_filtered"),
                Some("other_query"),
                Some("No results found")
            )
        );
        assert_eq!(
            (
                error["facts"]["query_text_sent"].as_str(),
                error["facts"]["dropped_below_floor"].as_u64(),
                error["facts"]["dropped_not_audio"].as_u64(),
                error["facts"]["locked_file_count"].as_u64()
            ),
            (
                Some("Marrow Choir O Holy Night"),
                Some(20),
                Some(3),
                Some(2)
            )
        );
        assert!(error["facts"]["search_id"].is_string());
        let reference = json!({"kind": "track", "artist": "Marrow Choir", "title": "O Holy Night"});
        assert_eq!(
            error["next"],
            json!([
                {"tool": "find_sources", "args": {"reference": reference, "include_below_floor": true}},
                {"tool": "find_sources", "args": {"reference": reference}, "needs": ["query_text"]}
            ])
        );
    }

    #[tokio::test]
    async fn weak_matches_kept_on_request_are_flagged_and_not_offered_again() {
        let (db, acq, effects, token) = app_with_acquire().await;
        let weak = DownloadableItem {
            below_floor: true,
            match_score: 0.31,
            ..item(
                "erin_shares",
                "Jingle Bells Remix",
                "@@pq7\\Share\\Misc\\Jingle Bells Remix.mp3",
            )
        };
        effects.stub().script_polls(vec![
            Ok(SearchResult {
                stats: SearchStats {
                    query_text: "Marrow Choir O Holy Night".into(),
                    response_count: 1,
                    file_count: 1,
                    below_floor: 1,
                    ..Default::default()
                },
                ..search_result(
                    "search",
                    SearchState::Completed,
                    vec![group("erin_shares", vec![weak], 0.31)],
                )
            }),
            Ok(SearchResult {
                stats: SearchStats {
                    query_text: "Marrow Choir O Holy Night".into(),
                    ..Default::default()
                },
                ..search_result("search", SearchState::Completed, vec![])
            }),
        ]);
        let reference = json!({"artist": "Marrow Choir", "title": "O Holy Night"});
        let doc = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "find_sources",
                json!({"reference": reference, "include_below_floor": true}),
            )
            .await,
        );
        assert_eq!(
            (
                doc["dropped_below_floor"].as_u64(),
                doc["candidates"][0]["below_floor"].as_bool()
            ),
            (Some(0), Some(true)),
            "{doc}"
        );
        assert!(effects.stub().specs()[0].keep_below_floor);
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "find_sources",
                json!({"reference": reference, "include_below_floor": true}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(error["code"], "search_no_results");
        assert!(
            !error["next"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["args"].get("include_below_floor").is_some()),
            "{error}"
        );
    }

    #[tokio::test]
    async fn candidates_that_all_fail_the_filters_say_which_filter_and_offer_the_search_without_them(
    ) {
        let (db, acq, effects, token) = app_with_acquire().await;
        effects.stub().script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![group(
                "peer_b",
                vec![DownloadableItem {
                    facts: FileFacts {
                        extension: Some("mp3".into()),
                        bitrate_kbps: Some(320),
                        ..FileFacts::default()
                    },
                    ..item("peer_b", "O Holy Night", "b/03.mp3")
                }],
                0.8,
            )],
        ))]);
        let error = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "find_sources",
                json!({"lossless_only": true, "query_text": "marrow choir holy night",
                    "reference": {"artist": "Marrow Choir", "title": "O Holy Night"}}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (
                error["code"].as_str(),
                error["message"].as_str(),
                error["facts"]["dropped_by_filter"]["not_lossless"].as_u64()
            ),
            (
                Some("search_all_filtered"),
                Some("No candidate passed the filters"),
                Some(1)
            )
        );
        assert!(error["next"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["tool"] == "find_sources"
                && n["args"].get("lossless_only").is_none()
                && n["args"]["query_text"] == "marrow choir holy night"));
        assert_eq!(
            effects.stub().specs()[0].text.as_deref(),
            Some("marrow choir holy night")
        );
    }

    #[tokio::test]
    async fn a_long_search_is_pending_and_get_sources_reads_it_later() {
        let (db, acq, effects, token) = app_with_acquire().await;
        let (reached, release) = effects.stub().hold_searches();
        let app = app_of(&db, &acq);
        // Called without the router, whose test client reads an answer within 5 s.
        let state = ConnectionState::on(db.clone(), acq.clone());
        let find = tokio::spawn(async move {
            let args = obj(json!({"reference": {"artist": "Kowloon", "title": "Wake Up"}}));
            find_sources(&state, &alice_caller(false), args).await
        });
        reached.notified().await;
        // Paused only to pass the 45 s, while no query is under way.
        tokio::time::pause();
        tokio::time::advance(FIND_WAIT + Duration::from_secs(1)).await;
        tokio::time::resume();
        let pending: Value = serde_json::from_str(text_of(&find.await.unwrap())).unwrap();
        assert_eq!(
            (
                pending["state"].as_str(),
                pending["searches_ahead"].as_u64()
            ),
            (Some("pending"), Some(0)),
            "{pending}"
        );
        let search_id = pending["search_id"].as_str().unwrap().to_string();
        assert_eq!(
            pending["next"],
            json!([{"tool": "get_sources", "args": {"search_id": search_id, "wait_seconds": 60}}])
        );
        effects.stub().script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![group(
                "peer_a",
                vec![item("peer_a", "Wake Up", "a/07.flac")],
                0.9,
            )],
        ))]);
        release.notify_one();
        let done = document(
            &call_tool(
                app.clone(),
                &token,
                "get_sources",
                json!({"search_id": search_id, "wait_seconds": 120}),
            )
            .await,
        );
        assert_eq!(
            (
                done["state"].as_str(),
                done["candidates"].as_array().map(Vec::len)
            ),
            (Some("done"), Some(1)),
            "{done}"
        );
        add_user(&db, "u2", "bob").await;
        let bob = access_token(&db, "u2", RESOURCE).await;
        let refused =
            document(&call_tool(app, &bob, "get_sources", json!({"search_id": search_id})).await);
        assert_eq!(
            (
                refused["error"]["code"].as_str(),
                refused["error"]["message"].as_str()
            ),
            (Some("unknown_handle"), Some("Search not found"))
        );
    }

    #[tokio::test]
    async fn a_page_of_candidates_continues_in_get_sources() {
        let (db, acq, effects, token) = app_with_acquire().await;
        effects.stub().script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![
                group("peer_a", vec![item("peer_a", "Wake Up", "a/07.flac")], 0.9),
                group("peer_b", vec![item("peer_b", "Wake Up", "b/07.flac")], 0.8),
            ],
        ))]);
        let app = app_of(&db, &acq);
        let first = document(
            &call_tool(
                app.clone(),
                &token,
                "find_sources",
                json!({"reference": {"artist": "Kowloon", "title": "Wake Up"}, "limit": 1}),
            )
            .await,
        );
        assert_eq!(first["candidates"][0]["source"], "peer_a");
        let cursor = first["cursor"].as_str().unwrap();
        let search_id = first["search_id"].as_str().unwrap();
        assert_eq!(
            first["next"],
            json!([{"tool": "get_sources", "args": {"search_id": search_id, "cursor": cursor}}])
        );
        let second = document(
            &call_tool(
                app,
                &token,
                "get_sources",
                json!({"search_id": search_id, "cursor": cursor}),
            )
            .await,
        );
        assert_eq!(
            (
                second["candidates"][0]["source"].as_str(),
                &second["cursor"]
            ),
            (Some("peer_b"), &Value::Null)
        );
        assert!(second["candidates"][0]["candidate_id"]
            .as_str()
            .unwrap()
            .ends_with(":1"));
    }

    #[tokio::test]
    async fn a_repeated_call_with_its_request_key_searches_once() {
        let (db, acq, effects, token) = app_with_acquire().await;
        effects.stub().script_polls(vec![Ok(search_result(
            "search",
            SearchState::Completed,
            vec![group(
                "peer_a",
                vec![item("peer_a", "Wake Up", "a/07.flac")],
                0.9,
            )],
        ))]);
        let call =
            json!({"reference": {"artist": "Kowloon", "title": "Wake Up"}, "request_key": "k1"});
        let first = call_tool(app_of(&db, &acq), &token, "find_sources", call.clone()).await;
        let again = call_tool(app_of(&db, &acq), &token, "find_sources", call).await;
        assert_eq!(first, again);
        assert_eq!(effects.stub().specs().len(), 1);
        let other = document(
            &call_tool(
                app_of(&db, &acq),
                &token,
                "find_sources",
                json!({"reference": {"artist": "Kowloon", "title": "Come Over"}, "request_key": "k1"}),
            )
            .await,
        );
        assert_eq!(
            (
                other["error"]["code"].as_str(),
                other["error"]["facts"]["field"].as_str()
            ),
            (Some("invalid_input"), Some("request_key"))
        );
    }

    #[tokio::test]
    async fn a_user_with_three_searches_waiting_is_answered_before_a_fourth_starts() {
        let (db, acq, effects, token) = app_with_acquire().await;
        let (reached, _release) = effects.stub().hold_searches();
        let app = app_of(&db, &acq);
        let find = |title: &'static str| {
            let (app, token) = (app.clone(), token.clone());
            tokio::spawn(async move {
                call_tool(
                    app,
                    &token,
                    "find_sources",
                    json!({"reference": {"artist": "Kowloon", "title": title}}),
                )
                .await
            })
        };
        // The first holds the turn; three more wait for it.
        let _running = find("Wake Up");
        reached.notified().await;
        let _waiting = [find("Come Over"), find("Lights Out"), find("Neon")];
        while acq
            .searches
            .waiting_finds(&crate::auth::UserId::new("u1"))
            .len()
            < 3
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let error = document(
            &call_tool(
                app,
                &token,
                "find_sources",
                json!({"reference": {"artist": "Kowloon", "title": "Midnight"},
                    "request_key": "k1"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (
                error["code"].as_str(),
                error["stage"].as_str(),
                error["retry"].as_str()
            ),
            (
                Some("rate_limited"),
                Some("source_search"),
                Some("same_source_later")
            )
        );
        assert_eq!(
            error["message"],
            "This user already has 3 source searches waiting for their turn; read them with get_sources first"
        );
        let ids = error["facts"]["search_ids"].as_array().unwrap();
        assert_eq!(
            (ids.len(), error["next"].as_array().map(Vec::len)),
            (3, Some(3))
        );
        assert_eq!(
            error["next"][0],
            json!({"tool": "get_sources", "args": {"search_id": ids[0], "wait_seconds": 60}})
        );
        assert_eq!(effects.stub().specs().len(), 1);
        // Refused before a search started: the key is free for the same call later.
        assert_eq!(count(&db, "request_keys").await, 0);
    }

    #[tokio::test]
    async fn finds_sent_together_never_leave_more_than_three_waiting() {
        let (db, acq, effects, token) = app_with_acquire().await;
        let (reached, _release) = effects.stub().hold_searches();
        let app = app_of(&db, &acq);
        let find = |title: &'static str| {
            let (app, token) = (app.clone(), token.clone());
            tokio::spawn(async move {
                call_tool(
                    app,
                    &token,
                    "find_sources",
                    json!({"reference": {"artist": "Kowloon", "title": title}}),
                )
                .await
            })
        };
        let _running = find("Wake Up");
        reached.notified().await;
        // Four calls all read no search waiting, then enter the line together.
        effects.hold_backend();
        let mut calls = Vec::from(["Come Over", "Lights Out", "Neon", "Midnight"].map(find));
        let alice = crate::auth::UserId::new("u1");
        tokio::time::timeout(Duration::from_secs(10), async {
            while effects.backend_calls_held() < 4 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("every call passed the first count");
        assert!(acq.searches.waiting_finds(&alice).is_empty());
        effects.release_backend();
        let refused = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(done) = calls.iter().position(|call| call.is_finished()) {
                    return done;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("one call is refused at once");
        let error = document(&calls.swap_remove(refused).await.unwrap())["error"].clone();
        assert_eq!(
            (
                error["code"].as_str(),
                error["facts"]["search_ids"].as_array().map(Vec::len)
            ),
            (Some("rate_limited"), Some(3)),
            "{error}"
        );
        assert_eq!(acq.searches.waiting_finds(&alice).len(), 3);
        assert!(
            calls.iter().all(|call| !call.is_finished()),
            "the other three wait for the turn"
        );
    }

    #[tokio::test]
    async fn an_album_whose_tracklist_cannot_be_read_offers_no_other_words() {
        let (db, acq, effects, token) = app_with_acquire().await;
        let mbid = "6a09041b-0f79-3278-88d9-0552dd28d37f";
        let reference = json!({"kind": "album", "artist": "Kowloon", "title": "Come Over",
            "release_mbid": mbid});
        let find = || {
            call_tool(
                app_of(&db, &acq),
                &token,
                "find_sources",
                json!({"reference": reference, "query_text": "kowloon come over"}),
            )
        };
        // What musicbrainz_rs says of a release MusicBrainz does not have (src/error.rs:17), on the
        // URL find_album asks (lib/soulbeet/src/musicbrainz.rs:351-362, musicbrainz_rs
        // src/api/query.rs:29-46), as From maps it (lib/soulbeet/src/musicbrainz.rs:95-99).
        effects.fail_hydrate(MetadataError::NotFound {
            service: Service::Musicbrainz,
            status: None,
            raw: format!("Musicbrainz returned \"Not found\" for query \"http://musicbrainz.org/ws/2/release/{mbid}?fmt=json&inc=recordings+artist-credits+release-groups\""),
        });
        let error = document(&find().await)["error"].clone();
        assert_eq!(
            (
                error["code"].as_str(),
                error["retry"].as_str(),
                error["facts"]["release_mbid"].as_str()
            ),
            (Some("metadata_not_found"), Some("other_query"), Some(mbid)),
            "{error}"
        );
        assert_eq!(
            error["next"],
            json!([]),
            "query_text never reaches the lookup"
        );
        // A lookup MusicBrainz can answer later offers the same call: musicbrainz_rs gave up
        // retrying its 503 answers (src/error.rs:20, lib/soulbeet/src/musicbrainz.rs:111-117).
        effects.fail_hydrate(MetadataError::RateLimited {
            service: Service::Musicbrainz,
            status: None,
            raw: "The max retry count for the request as been exeeded. You may want to check if the correct url is set, musicbrainz is online, or you aren't hitting the ratelimit.".into(),
        });
        let error = document(&find().await)["error"].clone();
        assert_eq!(
            (error["code"].as_str(), &error["next"]),
            (
                Some("metadata_rate_limited"),
                &json!([{"tool": "find_sources", "args": {"reference": reference,
                    "query_text": "kowloon come over"}}])
            ),
            "{error}"
        );
        assert!(effects.stub().specs().is_empty(), "nothing was searched");
    }

    #[tokio::test]
    async fn a_bad_reference_names_its_field() {
        let (db, acq, _, token) = app_with_acquire().await;
        let find = |reference: Value| {
            call_tool(
                app_of(&db, &acq),
                &token,
                "find_sources",
                json!({"reference": reference}),
            )
        };
        let error = document(&find(json!({"artist": "Kowloon"})).await)["error"].clone();
        assert_eq!(
            (error["code"].as_str(), error["facts"]["field"].as_str()),
            (Some("invalid_input"), Some("reference.title"))
        );
        let error = document(
            &find(json!({"kind": "album", "artist": "Kowloon", "title": "Come Over"})).await,
        )["error"]
            .clone();
        assert_eq!(error["facts"]["field"], "reference.release_mbid");
        let error = document(
            &find(json!({"artist": "Kowloon", "title": "Wake Up",
            "release_mbid": "6a09041b-0f79-3278-88d9-0552dd28d37f"}))
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (error["facts"]["field"].as_str(), error["message"].as_str()),
            (
                Some("reference.release_mbid"),
                Some("find_sources: reference.release_mbid must be absent: release_mbid belongs to album references")
            )
        );
        let error =
            document(&find(json!({"artist": "Kowloon", "title": "Wake Up", "year": 2003})).await)
                ["error"]
                .clone();
        assert_eq!(error["facts"]["field"], "reference.year");
    }
}
