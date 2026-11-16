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

/// A listing a tool pages through, named once by `key`, the first 16 hex
/// digits of the SHA-256 of the JSON text of the value that defines it
/// (D3). Its cursors are unpadded base64url of `{"t": tool, "k": key, "o":
/// offset}`. A cursor is not signed and holds nothing else: every page is
/// read again with the caller's own rows and Navidrome account, so a forged
/// cursor can only move inside the caller's own listing.
#[derive(Debug)]
pub(crate) struct Listing {
    tool: &'static str,
    key: String,
}

#[derive(Serialize, Deserialize)]
struct CursorBody {
    t: String,
    k: String,
    o: u64,
}

impl Listing {
    /// `tool`'s listing `list`: a value the tool builds from the arguments
    /// that define the listing, its field names in a fixed order.
    pub(crate) fn new(tool: &'static str, list: &Value) -> Listing {
        let mut key = hash(&list.to_string());
        key.truncate(16);
        Listing { tool, key }
    }

    /// The cursor at `offset` of the listing.
    pub(crate) fn cursor(&self, offset: usize) -> String {
        let body = CursorBody {
            t: self.tool.to_string(),
            k: self.key.clone(),
            o: offset as u64,
        };
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).expect("a cursor body serializes"))
    }

    /// The offset in `args`' "cursor", 0 without one. A cursor of another
    /// tool or listing, one that does not decode, or one past what a
    /// Navidrome offset holds is refused.
    pub(crate) fn read(&self, args: &Args) -> Result<usize, ToolError> {
        let refused = || Listing::refused(self.tool);
        let Some(cursor) = args.opt_string("cursor").map_err(|_| refused())? else {
            return Ok(0);
        };
        let body: CursorBody = URL_SAFE_NO_PAD
            .decode(cursor)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(refused)?;
        if body.t != self.tool || body.k != self.key || body.o > u64::from(u32::MAX) {
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

/// The page a call asks for.
#[derive(Debug)]
pub(crate) struct Page {
    pub offset: usize,
    pub limit: usize,
    listing: Listing,
}

impl Page {
    /// `limit` from 1 to MAX_LIMIT, DEFAULT_LIMIT without one, and the
    /// offset of `cursor` (`Listing::read`) in the listing `list`.
    pub(crate) fn read(args: &Args, list: Value) -> Result<Page, ToolError> {
        let limit = args
            .opt_u32("limit", 1, MAX_LIMIT)?
            .unwrap_or(DEFAULT_LIMIT);
        let listing = Listing::new(args.tool(), &list);
        let offset = listing.read(args)?;
        Ok(Page {
            offset,
            limit: limit as usize,
            listing,
        })
    }

    /// The page's items of a listing read whole: from the page's offset on,
    /// none when the offset is past the end.
    pub(crate) fn of<T: Serialize>(&self, all: &[T]) -> Vec<Value> {
        all.get(self.offset..)
            .unwrap_or_default()
            .iter()
            .map(|item| json!(item))
            .collect()
    }

    /// The cursor that follows the page's first `k` items.
    fn cursor_after(&self, k: usize) -> String {
        self.listing.cursor(self.offset + k)
    }
}

/// `fields` as a document: with "ok" and `key: []`, every other field as given.
fn empty_document(fields: &Map<String, Value>, key: &str) -> Map<String, Value> {
    let mut empty = fields.clone();
    empty.insert("ok".into(), true.into());
    empty.insert(key.into(), json!([]));
    empty
}

/// The most k up to `candidates` such that `base` characters, `items[..k]`
/// with a comma between items, and `tail(k)` characters stay within
/// RESULT_MAX_CHARS.
pub(crate) fn longest_fitting(
    base: usize,
    items: &[Value],
    candidates: usize,
    tail: impl Fn(usize) -> usize,
) -> usize {
    let mut listed = 0;
    let mut kept = 0;
    for (i, item) in items[..candidates].iter().enumerate() {
        listed += chars(item) + usize::from(i > 0);
        // Not a stop: the last items can fit with a shorter tail.
        if base + listed + tail(i + 1) <= RESULT_MAX_CHARS {
            kept = i + 1;
        }
    }
    kept
}

/// How many leading `items` go in the page (D4): at most `page.limit`, and
/// the most k such that `fields` with `key: items[..k]` and its cursor
/// stays within RESULT_MAX_CHARS once "ok" is added, though never fewer
/// than one so a cursor always advances. One item always fits: every text
/// an item holds from outside Soulful or from the user is cut to
/// TEXT_MAX_CHARS of JSON (`cut_json`), and a folder path is a canonical
/// path the kernel bounds at PATH_MAX (4,096 bytes), so the largest item,
/// a folder whose path is all control characters, is under 30,000
/// characters. The cursor is null when k takes every item and none follow
/// upstream.
fn fitting(
    fields: &Map<String, Value>,
    key: &str,
    items: &[Value],
    page: &Page,
    more_upstream: bool,
) -> usize {
    let mut empty = empty_document(fields, key);
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
    longest_fitting(base, items, candidates, cursor).max(candidates.min(1))
}

/// The characters of `value`'s compact JSON text.
pub(crate) fn chars(value: &Value) -> usize {
    value.to_string().chars().count()
}

/// `fields` with `key: items[..k]`, k from `fitting`, and "cursor": the
/// cursor at page.offset + k when items are left or `more_upstream`, else
/// null. An empty page ends the listing, so a cursor always advances.
/// `items` start at the page's offset.
pub(crate) fn fit(
    mut fields: Map<String, Value>,
    key: &str,
    mut items: Vec<Value>,
    page: &Page,
    more_upstream: bool,
) -> Map<String, Value> {
    let k = fitting(&fields, key, &items, page, more_upstream);
    let cursor = (k > 0 && (k < items.len() || more_upstream)).then(|| page.cursor_after(k));
    items.truncate(k);
    fields.insert(key.into(), Value::Array(items));
    fields.insert("cursor".into(), json!(cursor));
    fields
}

/// The characters of `doc`'s compact JSON text.
pub(crate) fn length(doc: &Map<String, Value>) -> usize {
    serde_json::to_string(doc).map_or(0, |text| text.chars().count())
}

/// Which items of a list stay when some are left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    /// The first ones, a list in its own order.
    First,
    /// The last ones: the newest of a list kept oldest first.
    Newest,
}

/// Leaves items of the list `list` of `object` out, whole (an id is never cut), from the end
/// `keep` does not keep, until `over` characters are gone, counting them in `{list}_omitted` next
/// to it (added to a count already there). Nothing when `over` is 0 or `object` has no such list.
pub(crate) fn leave_out(object: &mut Map<String, Value>, list: &str, keep: Keep, over: usize) {
    if over == 0 {
        return;
    }
    let left_out = format!("{list}_omitted");
    let Some(Value::Array(items)) = object.get_mut(list) else {
        return;
    };
    // The count costs its name and digits.
    let needed = over + left_out.len() + 24;
    let (mut gone, mut count) = (0, 0);
    while gone < needed && !items.is_empty() {
        let item = match keep {
            Keep::First => items.pop(),
            Keep::Newest => Some(items.remove(0)),
        };
        gone += item.map_or(0, |item| chars(&item) + 1);
        count += 1;
    }
    if count > 0 {
        let before = object.get(&left_out).and_then(Value::as_u64).unwrap_or(0);
        object.insert(left_out, json!(before + count));
    }
}

/// `fields` with `key: items[..k]` and `left_out_key: items.len() - k`, k
/// the most leading items that keep the document within RESULT_MAX_CHARS
/// once "ok" is added: for a list sent whole, with no cursor to page it.
pub(crate) fn fit_whole(
    fields: Map<String, Value>,
    key: &str,
    items: Vec<Value>,
    left_out_key: &str,
) -> Map<String, Value> {
    fit_whole_after(fields, key, items, left_out_key, 0)
}

/// `fit_whole` of a list `already_left_out` items were left out of before it came, counted in
/// `left_out_key` with those it leaves out.
pub(crate) fn fit_whole_after(
    mut fields: Map<String, Value>,
    key: &str,
    mut items: Vec<Value>,
    left_out_key: &str,
    already_left_out: usize,
) -> Map<String, Value> {
    let mut empty = empty_document(&fields, key);
    // The count at its longest.
    empty.insert(left_out_key.into(), json!(already_left_out + items.len()));
    let base = chars(&Value::Object(empty));
    let k = longest_fitting(base, &items, items.len(), |_| 0);
    fields.insert(
        left_out_key.into(),
        json!(already_left_out + items.len() - k),
    );
    items.truncate(k);
    fields.insert(key.into(), Value::Array(items));
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
        let cursor = Listing::new("search_library", &list).cursor(2);
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
        let other = Listing::new(
            "list_starred",
            &json!({"query": "silent night", "kind": "songs"}),
        )
        .cursor(2);
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
                    json!({"query": "silent night", "cursor": Listing::new("search_library", &list).cursor(offset)}),
                ),
                SEARCH,
            )
        };
        let last = u32::MAX as usize;
        let listing = Listing::new("search_library", &list);
        assert_eq!(listing.read(&at(last)).unwrap(), last);
        assert_eq!(
            listing
                .read(&at(last + 1))
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
            Listing::new("search_library", &json!({"query": "silent night"}))
                .read(&next)
                .unwrap(),
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
            json!(Listing::new("search_library", &list).cursor(2))
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
        let doc = fit(Map::new(), "songs", items.clone(), &page, false);
        assert_eq!(
            (doc["songs"].as_array().unwrap().len(), &doc["cursor"]),
            (2, &Value::Null)
        );
        assert_eq!(text(&doc), RESULT_MAX_CHARS);
        // The premise: the same two items with a cursor in place of null are
        // over the cap, so with more upstream only the first is sent.
        let mut with_cursor = doc.clone();
        with_cursor["cursor"] = json!(page.cursor_after(2));
        assert!(text(&with_cursor) > RESULT_MAX_CHARS);
        let more = fit(Map::new(), "songs", items, &page, true);
        assert_eq!(
            (more["songs"].as_array().unwrap().len(), &more["cursor"]),
            (1, &json!(page.cursor_after(1)))
        );
    }

    #[test]
    fn an_empty_page_ends_the_listing_even_when_more_was_announced() {
        let a = Args::new(
            "search_library",
            obj(json!({"query": "silent night"})),
            SEARCH,
        );
        let page = Page::read(&a, json!({"query": "silent night"})).unwrap();
        // A count above what the service then lists (MusicBrainz's
        // release-count against an empty page) must not give a cursor that
        // reads the same page again.
        let doc = fit(Map::new(), "releases", vec![], &page, true);
        assert_eq!(
            (&doc["releases"], &doc["cursor"]),
            (&json!([]), &Value::Null)
        );
    }

    #[test]
    fn a_list_sent_whole_keeps_what_fits_and_counts_the_rest() {
        let items: Vec<Value> = (0..40)
            .map(|i| json!({"title": format!("{i}{}", "\u{1}".repeat(400))}))
            .collect();
        let doc = fit_whole(Map::new(), "recordings", items.clone(), "left_out");
        let kept = doc["recordings"].as_array().unwrap().len();
        let mut sent = doc.clone();
        sent.insert("ok".into(), json!(true));
        assert!(Value::Object(sent).to_string().chars().count() <= RESULT_MAX_CHARS);
        assert!(kept > 0 && kept < 40, "{kept}");
        assert_eq!(doc["left_out"], json!(40 - kept));
        assert_eq!(doc["recordings"], json!(items[..kept]));
        let small = fit_whole(Map::new(), "recordings", items[..2].to_vec(), "left_out");
        assert_eq!(
            (
                small["recordings"].as_array().unwrap().len(),
                &small["left_out"]
            ),
            (2, &json!(0))
        );
    }
}
