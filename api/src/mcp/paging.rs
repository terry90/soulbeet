//! Pages of a list and the 60,000 character cap (D3, D4).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::args::Args;
use super::contract::{invalid_value, ToolError};
use crate::oauth::secrets::hash;

/// Longest result text, in characters (design 10.1).
pub(crate) const RESULT_MAX_CHARS: usize = 60_000;
/// Items in a page when the call names no `limit` (design 10.1).
pub(crate) const DEFAULT_LIMIT: u32 = 25;
/// Most items a call can ask for in a page (design 10.1).
pub(crate) const MAX_LIMIT: u32 = 100;

/// Where the next page of a listing starts (D3): unpadded base64url of
/// `{"t": tool, "k": key, "o": offset}`, `key` naming the listing. It is not
/// signed and holds nothing else: every page is read again with the
/// caller's own rows and Navidrome account, so a forged cursor can only
/// move inside the caller's own listing.
pub(crate) struct Cursor;

#[derive(Serialize, Deserialize)]
struct CursorBody {
    t: String,
    k: String,
    o: u64,
}

impl Cursor {
    /// The cursor at `offset` of `tool`'s listing `list`: a value the tool
    /// builds from the arguments that define the listing, its field names
    /// in a fixed order.
    pub(crate) fn make(tool: &'static str, list: &Value, offset: usize) -> String {
        let body = CursorBody {
            t: tool.to_string(),
            k: key(list),
            o: offset as u64,
        };
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).expect("a cursor body serializes"))
    }

    /// The offset in `args`' "cursor", 0 without one. A cursor of another
    /// tool or listing, one that does not decode, or one past what a
    /// Navidrome offset holds is refused.
    pub(crate) fn read(args: &Args, list: &Value) -> Result<usize, ToolError> {
        let tool = args.tool();
        let refused = || Cursor::refused(tool);
        let Some(cursor) = args.opt_string("cursor").map_err(|_| refused())? else {
            return Ok(0);
        };
        let body: CursorBody = URL_SAFE_NO_PAD
            .decode(cursor)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(refused)?;
        if body.t != tool || body.k != key(list) || body.o > u64::from(u32::MAX) {
            return Err(refused());
        }
        Ok(body.o as usize)
    }

    /// The refusal of a cursor `tool` cannot read.
    pub(crate) fn refused(tool: &'static str) -> ToolError {
        invalid_value(
            tool,
            "cursor",
            &format!("a cursor {tool} returned for the same arguments"),
        )
    }
}

/// The first 16 hex digits of the SHA-256 of `list`'s JSON text.
fn key(list: &Value) -> String {
    let mut digest = hash(&list.to_string());
    digest.truncate(16);
    digest
}

/// The page a call asks for.
#[derive(Debug)]
pub(crate) struct Page {
    pub offset: usize,
    pub limit: usize,
    tool: &'static str,
    list: Value,
}

impl Page {
    /// `limit` from 1 to MAX_LIMIT, DEFAULT_LIMIT without one, and the
    /// offset of `cursor` (`Cursor::read`) in the listing `list`.
    pub(crate) fn read(args: &Args, list: Value) -> Result<Page, ToolError> {
        let limit = args
            .opt_u32("limit", 1, MAX_LIMIT)?
            .unwrap_or(DEFAULT_LIMIT);
        let offset = Cursor::read(args, &list)?;
        Ok(Page {
            offset,
            limit: limit as usize,
            tool: args.tool(),
            list,
        })
    }

    /// The cursor that follows the page's first `k` items.
    fn cursor_after(&self, k: usize) -> String {
        Cursor::make(self.tool, &self.list, self.offset + k)
    }
}

/// How many leading `items` go in the page (D4): at most `page.limit`, and
/// the most k such that `fields` with `key: items[..k]` and its cursor
/// stays within RESULT_MAX_CHARS once "ok" is added, though never fewer
/// than one so a cursor always advances. The cursor is null when k takes
/// every item and none follow upstream.
fn fitting(
    fields: &Map<String, Value>,
    key: &str,
    items: &[Value],
    page: &Page,
    more_upstream: bool,
) -> usize {
    let mut empty = fields.clone();
    empty.insert("ok".into(), true.into());
    empty.insert(key.into(), json!([]));
    empty.insert("cursor".into(), Value::Null);
    // The compact text is the empty document's with each item and a comma
    // between items spliced into the list, and "null" replaced by the
    // quoted cursor (base64url needs no escape).
    let base = chars(&Value::Object(empty)) - "null".len();
    let cursor = |k: usize| {
        if k < items.len() || more_upstream {
            page.cursor_after(k).len() + 2
        } else {
            "null".len()
        }
    };
    let candidates = items.len().min(page.limit);
    let mut listed = 0;
    let mut kept = candidates.min(1);
    for (i, item) in items[..candidates].iter().enumerate() {
        listed += chars(item) + usize::from(i > 0);
        if base + listed + cursor(i + 1) <= RESULT_MAX_CHARS {
            kept = i + 1;
        }
    }
    kept
}

fn chars(value: &Value) -> usize {
    value.to_string().chars().count()
}

/// `fields` with `key: items[..k]`, k from `fitting`, and "cursor": the
/// cursor at page.offset + k when items are left or `more_upstream`, else
/// null. `items` start at the page's offset.
pub(crate) fn fit(
    mut fields: Map<String, Value>,
    key: &str,
    mut items: Vec<Value>,
    page: &Page,
    more_upstream: bool,
) -> Map<String, Value> {
    let k = fitting(&fields, key, &items, page, more_upstream);
    let cursor = (k < items.len() || more_upstream).then(|| page.cursor_after(k));
    items.truncate(k);
    fields.insert(key.into(), Value::Array(items));
    fields.insert("cursor".into(), json!(cursor));
    fields
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Map, Value};

    use super::*;
    use crate::mcp::args::tests::{obj, SEARCH};
    use crate::mcp::args::Args;

    #[test]
    fn a_cursor_reads_back_its_offset_only_for_the_same_listing() {
        let list = json!({"query": "silent night", "kind": "songs"});
        let a = Args::new(
            "search_library",
            obj(json!({"query": "silent night", "kind": "songs", "limit": 2})),
            SEARCH,
        );
        let page = Page::read(&a, list.clone()).unwrap();
        assert_eq!((page.offset, page.limit), (0, 2));
        let cursor = Cursor::make("search_library", &list, 2);
        let a = Args::new(
            "search_library",
            obj(json!({"query": "silent night", "kind": "songs", "cursor": cursor})),
            SEARCH,
        );
        assert_eq!(
            (
                Page::read(&a, list).unwrap().offset,
                Page::read(&a, json!({"query": "silent night", "kind": "songs"}))
                    .unwrap()
                    .limit
            ),
            (2, 25)
        );
        let e = Page::read(&a, json!({"query": "o holy night", "kind": "songs"})).unwrap_err();
        assert_eq!(
            e.failure.message,
            "search_library: cursor must be a cursor search_library returned for the same arguments"
        );
        let other = Cursor::make(
            "list_starred",
            &json!({"query": "silent night", "kind": "songs"}),
            2,
        );
        for bad in [other, "not a cursor".to_string()] {
            let a = Args::new(
                "search_library",
                obj(json!({"query": "silent night", "kind": "songs", "cursor": bad})),
                SEARCH,
            );
            assert!(Page::read(&a, json!({"query": "silent night", "kind": "songs"})).is_err());
        }
    }

    #[test]
    fn a_cursor_past_what_an_offset_can_hold_is_refused() {
        let list = json!({"query": "silent night"});
        let at = |offset: usize| {
            Args::new(
                "search_library",
                obj(
                    json!({"query": "silent night", "cursor": Cursor::make("search_library", &list, offset)}),
                ),
                SEARCH,
            )
        };
        let last = u32::MAX as usize;
        assert_eq!(Cursor::read(&at(last), &list).unwrap(), last);
        assert_eq!(
            Cursor::read(&at(last + 1), &list)
                .unwrap_err()
                .failure
                .message,
            "search_library: cursor must be a cursor search_library returned for the same arguments"
        );
    }

    #[test]
    fn a_page_holds_as_many_items_as_fit_in_60000_characters() {
        let a = Args::new(
            "search_library",
            obj(json!({"query": "silent night", "limit": 100})),
            SEARCH,
        );
        let page = Page::read(&a, json!({"query": "silent night"})).unwrap();
        let items: Vec<Value> = (0..100)
            .map(|i| json!({"song_id": format!("s{i}"), "title": "x".repeat(1000)}))
            .collect();
        let doc = fit(Map::new(), "songs", items.clone(), &page, false);
        let kept = doc["songs"].as_array().unwrap().len();
        let text = |m: &Map<String, Value>| {
            let mut m = m.clone();
            m.insert("ok".into(), json!(true));
            serde_json::to_string(&m).unwrap()
        };
        assert!(text(&doc).chars().count() <= RESULT_MAX_CHARS);
        let mut one_more = doc.clone();
        one_more["songs"] = Value::Array(items[..kept + 1].to_vec());
        assert!(text(&one_more).chars().count() > RESULT_MAX_CHARS);
        let next = Args::new(
            "search_library",
            obj(json!({"query": "silent night", "cursor": doc["cursor"]})),
            SEARCH,
        );
        assert_eq!(
            Cursor::read(&next, &json!({"query": "silent night"})).unwrap(),
            kept
        );
        let small = fit(Map::new(), "songs", items[..3].to_vec(), &page, false);
        assert_eq!(
            (small["songs"].as_array().unwrap().len(), &small["cursor"]),
            (3, &Value::Null)
        );
        assert!(fit(Map::new(), "songs", items[..3].to_vec(), &page, true)["cursor"].is_string());
    }

    #[test]
    fn a_page_holds_at_most_limit_items() {
        let a = Args::new(
            "search_library",
            obj(json!({"query": "silent night", "limit": 2})),
            SEARCH,
        );
        let list = json!({"query": "silent night"});
        let page = Page::read(&a, list.clone()).unwrap();
        let items: Vec<Value> = (0..5)
            .map(|i| json!({"song_id": format!("s{i}")}))
            .collect();
        let doc = fit(Map::new(), "songs", items, &page, false);
        assert_eq!(doc["songs"], json!([{"song_id": "s0"}, {"song_id": "s1"}]));
        assert_eq!(
            doc["cursor"],
            json!(Cursor::make("search_library", &list, 2))
        );
    }

    #[test]
    fn the_last_items_fit_without_the_cursor_they_no_longer_need() {
        let a = Args::new(
            "search_library",
            obj(json!({"query": "silent night", "limit": 100})),
            SEARCH,
        );
        let page = Page::read(&a, json!({"query": "silent night"})).unwrap();
        let text = |m: &Map<String, Value>| {
            let mut m = m.clone();
            m.insert("ok".into(), json!(true));
            serde_json::to_string(&m).unwrap().chars().count()
        };
        let big = |n: usize| json!({"title": "x".repeat(n)});
        // The first item with a null cursor and a one-character second item
        // fill the cap exactly; with a cursor in place of null they would not.
        let empty = text(&fit(
            Map::new(),
            "songs",
            vec![big(0), json!(1)],
            &page,
            false,
        ));
        let items = vec![big(RESULT_MAX_CHARS - empty), json!(1)];
        let doc = fit(Map::new(), "songs", items, &page, false);
        assert_eq!(
            (doc["songs"].as_array().unwrap().len(), &doc["cursor"]),
            (2, &Value::Null)
        );
        assert_eq!(text(&doc), RESULT_MAX_CHARS);
    }
}
