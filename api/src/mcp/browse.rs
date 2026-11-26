//! MCP tools for triggering a Navidrome scan (`scan_library`), browsing the library with
//! filters and sorting (`browse_library`), and inspecting an artist with their albums
//! (`get_library_artist`).

use chrono::{DateTime, NaiveDate, Utc};
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::Stage;
use soulbeet::navidrome::ScanStatus;

use super::args::Args;
use super::contract::{answer, invalid_value, upstream_error, ToolError};
use super::library::navidrome_of;
use super::paging::{fit, Page};
use super::refs::page_schema;
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::browse::{
    self, BrowseItems, BrowseKind, BrowseQuery, BrowseSort, MAX_SCAN_WAIT_SECONDS,
};
use crate::core::time;

const SCAN_ARGS: &[&str] = &["full_scan", "wait_seconds"];

const BROWSE_ARGS: &[&str] = &[
    "kind",
    "folder_id",
    "genre",
    "min_rating",
    "max_rating",
    "unrated",
    "starred",
    "year_from",
    "year_to",
    "format",
    "max_bitrate_kbps",
    "missing",
    "sort",
    "limit",
    "cursor",
];

const ARTIST_ARGS: &[&str] = &["artist_id", "limit", "cursor"];

const BROWSE_KINDS: &[&str] = &["songs", "albums"];

const BROWSE_SORTS: &[&str] = &[
    "recently_added",
    "recently_played",
    "most_played",
    "highest_rated",
    "title",
    "artist",
    "year",
    "random",
];

pub(crate) const SCAN_LIBRARY: ToolSpec = ToolSpec {
    name: "scan_library",
    title: "Scan Navidrome library",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Starts a Navidrome library scan (quick by default, or full when full_scan is true to re-read tags and metadata of every file) using the caller's Navidrome account if it is an admin or another linked admin account. Pass wait_seconds (0 to {MAX_SCAN_WAIT_SECONDS}, default 0) to poll until the scan finishes or the wait elapses; returns started, full_scan, completed and the current scan status (scanning, songs when idle, files_scanned while running, last_scan, error). Failure codes: {common}, {NAVIDROME_CODES}, index_no_scan_possible (no linked Navidrome account is an admin), scan_failed (Navidrome reported a scan error).")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "properties": {"full_scan": {"type": "boolean"}, "wait_seconds": {"type": "integer", "minimum": 0, "maximum": 30}}}"#,
    read_only: false,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(scan_library(state, caller, args)),
};

pub(crate) const BROWSE_LIBRARY: ToolSpec = ToolSpec {
    name: "browse_library",
    title: "Browse library",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Browses the user's Navidrome library (kind: songs or albums, default songs) with structured filters and sorting without requiring a search query. Supports filtering by folder_id (one of the caller's folders from list_folders), genre (case-insensitive), min_rating (1 to 5), max_rating (0 to 5), unrated, starred, year_from, year_to, format (file extension such as flac or mp3), max_bitrate_kbps (to find low-bitrate tracks to upgrade) and missing (default false; pass true to list files Navidrome marked missing). Supports sorting by recently_added (default), recently_played, most_played, highest_rated, title, artist, year or random (stable across pages for the same arguments). Songs include the facts find_in_library gives plus folder, genre, play_date, created_at and missing when true. Returns total matched count and pages of 25 by default (limit up to 100); pass back cursor for the next page. Reads the library, so it can take several seconds on a large one. Failure codes: {common}, {NAVIDROME_CODES}, unknown_handle (folder_id does not name one of this user's folders).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "properties": {"kind": {"type": "string", "enum": ["songs", "albums"]}, "folder_id": {"type": "string", "minLength": 1}, "genre": {"type": "string", "minLength": 1}, "min_rating": {"type": "integer", "minimum": 1, "maximum": 5}, "max_rating": {"type": "integer", "minimum": 0, "maximum": 5}, "unrated": {"type": "boolean"}, "starred": {"type": "boolean"}, "year_from": {"type": "integer", "minimum": 1, "maximum": 9999}, "year_to": {"type": "integer", "minimum": 1, "maximum": 9999}, "format": {"type": "string", "minLength": 1}, "max_bitrate_kbps": {"type": "integer", "minimum": 1, "maximum": 100000}, "missing": {"type": "boolean"}, "sort": {"type": "string", "enum": ["recently_added", "recently_played", "most_played", "highest_rated", "title", "artist", "year", "random"]}, "#,
        page_schema!(),
        "}}"
    ),
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(browse_library(state, caller, args)),
};

pub(crate) const GET_LIBRARY_ARTIST: ToolSpec = ToolSpec {
    name: "get_library_artist",
    title: "Library artist",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Shows one artist of the user's Navidrome library with their albums, from an artist id that search_library or list_starred gave. Ids stay valid while the artist stays in the library. Pages of 25 albums by default (limit up to 100); pass back cursor for the next page. Failure codes: {common}, {NAVIDROME_CODES}, unknown_handle (no artist with this id that this account can see).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["artist_id"], "properties": {"artist_id": {"type": "string", "minLength": 1}, "#,
        page_schema!(),
        "}}"
    ),
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(get_library_artist(state, caller, args)),
};

pub(crate) async fn scan_library(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(SCAN_LIBRARY.name, args, SCAN_ARGS);
    answer(
        async {
            let full_scan = args.opt_bool("full_scan")?.unwrap_or(false);
            let wait_seconds = args
                .opt_u32("wait_seconds", 0, MAX_SCAN_WAIT_SECONDS)?
                .unwrap_or(0);
            args.finish()?;

            let report = browse::scan_library(
                &state.db,
                &*state.navidrome,
                &caller.ctx.id(),
                full_scan,
                wait_seconds,
            )
            .await
            .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;

            let Value::Object(fields) = json!({
                "started": report.started,
                "full_scan": report.full_scan,
                "completed": report.completed,
                "scan": scan_document(&report.scan),
            }) else {
                unreachable!("an object literal is an object");
            };
            Ok(fields)
        }
        .await,
    )
}

pub(crate) async fn browse_library(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(BROWSE_LIBRARY.name, args, BROWSE_ARGS);
    answer(
        async {
            let (kind, query, list, page) = read_browse_args(&args)?;
            args.finish()?;

            let navi = navidrome_of(state, caller, Stage::LibraryCheck, &args).await?;
            let result = browse::browse_library_with_admin(
                &state.db,
                &navi,
                Some(state.navidrome.as_ref()),
                &caller.ctx,
                state.navidrome_music_path.as_deref(),
                query,
                caller.has_admin_access(),
            )
            .await
            .map_err(|f| upstream_error(f, listing_fields(&list), args.same_call()))?;

            let mut fields = Map::new();
            fields.insert("kind".into(), json!(kind.as_str()));
            fields.insert("sort".into(), list["sort"].clone());
            fields.insert("total".into(), json!(result.total));

            let (key, items) = match result.items {
                BrowseItems::Songs(songs) => ("songs", page.of(&songs)),
                BrowseItems::Albums(albums) => ("albums", page.of(&albums)),
            };
            Ok(fit(fields, key, items, &page, false))
        }
        .await,
    )
}

pub(crate) async fn get_library_artist(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(GET_LIBRARY_ARTIST.name, args, ARTIST_ARGS);
    answer(
        async {
            let artist_id = args.string("artist_id")?;
            let list = json!({"artist_id": artist_id});
            let page = Page::read(&args, list.clone())?;
            args.finish()?;

            let navi = navidrome_of(state, caller, Stage::LibraryCheck, &args).await?;
            let (artist, albums) = browse::get_artist(&navi, &artist_id)
                .await
                .map_err(|f| upstream_error(f, listing_fields(&list), args.same_call()))?;

            let mut fields = Map::new();
            fields.insert("artist".into(), json!(artist));
            Ok(fit(fields, "albums", page.of(&albums), &page, false))
        }
        .await,
    )
}

fn read_browse_args(args: &Args) -> Result<(BrowseKind, BrowseQuery, Value, Page), ToolError> {
    let kind_name = args.opt_choice("kind", BROWSE_KINDS)?.unwrap_or("songs");
    let kind = match kind_name {
        "songs" => BrowseKind::Songs,
        "albums" => BrowseKind::Albums,
        other => unreachable!("opt_choice answers one of BROWSE_KINDS, not {other}"),
    };
    let folder_id = args.opt_string("folder_id")?;
    let genre = args.opt_string("genre")?;
    let min_rating = args.opt_u32("min_rating", 1, 5)?.map(|v| v as u8);
    let max_rating = args.opt_u32("max_rating", 0, 5)?.map(|v| v as u8);
    let unrated = args.opt_bool("unrated")?;
    let starred = args.opt_bool("starred")?;
    let year_from = args.opt_u32("year_from", 1, 9999)?;
    let year_to = args.opt_u32("year_to", 1, 9999)?;
    let format = args.opt_string("format")?;
    let max_bitrate_kbps = args.opt_u32("max_bitrate_kbps", 1, 100_000)?;
    let missing = args.opt_bool("missing")?;
    let sort_name = args
        .opt_choice("sort", BROWSE_SORTS)?
        .unwrap_or("recently_added");
    let sort = match sort_name {
        "recently_added" => BrowseSort::RecentlyAdded,
        "recently_played" => BrowseSort::RecentlyPlayed,
        "most_played" => BrowseSort::MostPlayed,
        "highest_rated" => BrowseSort::HighestRated,
        "title" => BrowseSort::Title,
        "artist" => BrowseSort::Artist,
        "year" => BrowseSort::Year,
        "random" => BrowseSort::Random,
        other => unreachable!("opt_choice answers one of BROWSE_SORTS, not {other}"),
    };

    if let (Some(min), Some(max)) = (min_rating, max_rating) {
        if min > max {
            return Err(invalid_value(
                BROWSE_LIBRARY.name,
                "min_rating",
                "less than or equal to max_rating",
            ));
        }
    }
    if unrated == Some(true) && min_rating.is_some() {
        return Err(invalid_value(
            BROWSE_LIBRARY.name,
            "unrated",
            "absent or false when min_rating is given",
        ));
    }
    if let (Some(from), Some(to)) = (year_from, year_to) {
        if from > to {
            return Err(invalid_value(
                BROWSE_LIBRARY.name,
                "year_from",
                "less than or equal to year_to",
            ));
        }
    }

    let list = json!({
        "kind": kind_name,
        "folder_id": folder_id,
        "genre": genre,
        "min_rating": min_rating,
        "max_rating": max_rating,
        "unrated": unrated,
        "starred": starred,
        "year_from": year_from,
        "year_to": year_to,
        "format": format,
        "max_bitrate_kbps": max_bitrate_kbps,
        "missing": missing,
        "sort": sort_name,
    });
    let page = Page::read(args, list.clone())?;
    let random_seed = browse::random_key(0, &list.to_string());

    let query = BrowseQuery {
        kind,
        folder_id,
        genre,
        min_rating,
        max_rating,
        unrated,
        starred,
        year_from,
        year_to,
        format,
        max_bitrate_kbps,
        missing,
        sort,
        random_seed,
    };

    Ok((kind, query, list, page))
}

fn listing_fields(list: &Value) -> Map<String, Value> {
    let Value::Object(fields) = list else {
        unreachable!("a listing is an object literal");
    };
    fields.clone()
}

/// Same scan object shape as `get_status`: `songs` when idle, `files_scanned` while running,
/// `null` for Go's zero `lastScan` and empty `error`.
fn scan_document(scan: &ScanStatus) -> Value {
    let last_scan = (scan.last_scan != go_zero_time()).then(|| time::at(scan.last_scan));
    let error = (!scan.error.is_empty()).then_some(scan.error.as_str());
    let (songs, files_scanned) = match scan.scanning {
        false => (Some(scan.count), None),
        true => (None, Some(scan.count)),
    };
    json!({
        "scanning": scan.scanning,
        "songs": songs,
        "files_scanned": files_scanned,
        "last_scan": last_scan,
        "error": error,
    })
}

fn go_zero_time() -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .expect("0001-01-01 00:00:00 is a valid time")
        .and_utc()
}
