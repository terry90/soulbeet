//! Reading a tool's arguments (D1).

use rmcp::model::JsonObject;
use serde_json::{Map, Value};
use shared::failure::{cut_chars, TEXT_MAX_CHARS};

use super::contract::{invalid_input, invalid_value, NextCall, ToolError, TOO_LARGE_TO_REPEAT};

/// Longest text argument, in characters after trimming: a reference or
/// query is echoed in `facts` and `next`, so it stays far below the result
/// cap (design 10.1).
const STRING_MAX_CHARS: usize = 500;

/// Longest call a result offers again in `next`, in characters of its JSON
/// text. The rest of an error result is bounded by its cuts: a name in the
/// message and in `facts.field` at TEXT_MAX_CHARS, `cause.raw` at
/// RAW_MAX_CHARS and a few facts of at most STRING_MAX_CHARS, each character
/// at most six once escaped, so under 35,000 characters, and with this the
/// whole result stays under RESULT_MAX_CHARS (design 10.1).
const ECHO_MAX_CHARS: usize = 25_000;

const A_STRING: &str = "a non-empty string of at most 500 characters";
const AN_MBID: &str = "a MusicBrainz id: 36 characters, 8-4-4-4-12 hexadecimal digits";

/// A tool's arguments, or one element of a list argument. Each value is
/// read by the tool, and a value of the wrong shape is refused as it is
/// read; `finish` then refuses a name the tool does not take.
#[derive(Debug)]
pub(crate) struct Args {
    tool: &'static str,
    /// What goes before a field's name in a refusal: `references[2].` for
    /// an element, empty for the call itself.
    prefix: String,
    map: Map<String, Value>,
    known: &'static [&'static str],
}

impl Args {
    /// Holds the arguments; refuses nothing (D1).
    pub(crate) fn new(
        tool: &'static str,
        args: Option<JsonObject>,
        known: &'static [&'static str],
    ) -> Args {
        Args {
            tool,
            prefix: String::new(),
            map: args.unwrap_or_default(),
            known,
        }
    }

    /// The tool these arguments were given to.
    pub(crate) fn tool(&self) -> &'static str {
        self.tool
    }

    /// D1, called after the tool read every value: the alphabetically first
    /// name it does not take is refused, offering the call without the
    /// names it does not take, unless that call is too large to repeat.
    pub(crate) fn finish(&self) -> Result<(), ToolError> {
        let Some(name) = self.unknown() else {
            return Ok(());
        };
        let taken: Map<String, Value> = self
            .map
            .iter()
            .filter(|(name, _)| self.known.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let next = repeatable(self.tool, Value::Object(taken));
        let mut e = self.unknown_name(name, Vec::from_iter(next.clone()));
        if next.is_none() {
            e.failure.message = format!("{} {TOO_LARGE_TO_REPEAT}", e.failure.message);
        }
        Err(e)
    }

    /// A required text, trimmed.
    pub(crate) fn string(&self, name: &str) -> Result<String, ToolError> {
        self.opt_string(name)?
            .ok_or_else(|| self.refuse(name, A_STRING))
    }

    /// An optional text, trimmed; an empty one or `null` counts as missing.
    pub(crate) fn opt_string(&self, name: &str) -> Result<Option<String>, ToolError> {
        let Some(value) = self.given(name) else {
            return Ok(None);
        };
        let text = value
            .as_str()
            .map(str::trim)
            .ok_or_else(|| self.refuse(name, A_STRING))?;
        if text.chars().count() > STRING_MAX_CHARS {
            return Err(self.refuse(name, A_STRING));
        }
        Ok((!text.is_empty()).then(|| text.to_string()))
    }

    /// A MusicBrainz id in its 36-character hyphenated form, lowercase.
    pub(crate) fn mbid(&self, name: &str) -> Result<String, ToolError> {
        self.opt_mbid(name)?
            .ok_or_else(|| self.refuse(name, AN_MBID))
    }

    pub(crate) fn opt_mbid(&self, name: &str) -> Result<Option<String>, ToolError> {
        let refused = || self.refuse(name, AN_MBID);
        let Some(text) = self.opt_string(name).map_err(|_| refused())? else {
            return Ok(None);
        };
        // `try_parse` also takes the simple, braced and urn forms, which
        // differ in length.
        if text.len() != 36 || uuid::Uuid::try_parse(&text).is_err() {
            return Err(refused());
        }
        Ok(Some(text.to_ascii_lowercase()))
    }

    /// A JSON integer from `min` to `max`.
    pub(crate) fn opt_u32(&self, name: &str, min: u32, max: u32) -> Result<Option<u32>, ToolError> {
        let Some(value) = self.given(name) else {
            return Ok(None);
        };
        value
            .as_u64()
            .filter(|n| (u64::from(min)..=u64::from(max)).contains(n))
            .map(|n| Some(n as u32))
            .ok_or_else(|| self.refuse(name, &format!("an integer from {min} to {max}")))
    }

    /// One of `choices`, as written there.
    pub(crate) fn opt_choice(
        &self,
        name: &str,
        choices: &'static [&'static str],
    ) -> Result<Option<&'static str>, ToolError> {
        let refused = || self.refuse(name, &format!("one of {}", choices.join(", ")));
        let Some(text) = self.opt_string(name).map_err(|_| refused())? else {
            return Ok(None);
        };
        choices
            .iter()
            .find(|choice| **choice == text)
            .map(|choice| Some(*choice))
            .ok_or_else(refused)
    }

    /// A list of `min` to `max` objects, each read as its own `Args` taking
    /// `known`. A name an element does not take is refused here, with no
    /// call to offer: the element is the caller's to fix.
    pub(crate) fn objects(
        &self,
        name: &str,
        min: usize,
        max: usize,
        known: &'static [&'static str],
    ) -> Result<Vec<Args>, ToolError> {
        let refused = || self.refuse(name, &format!("a list of {min} to {max} objects"));
        let list = self
            .map
            .get(name)
            .and_then(Value::as_array)
            .filter(|list| (min..=max).contains(&list.len()))
            .ok_or_else(refused)?;
        let mut elements = Vec::with_capacity(list.len());
        for (i, element) in list.iter().enumerate() {
            let map = element.as_object().ok_or_else(refused)?;
            let element = Args {
                tool: self.tool,
                prefix: format!("{}[{i}].", self.field(name)),
                map: map.clone(),
                known,
            };
            if let Some(unknown) = element.unknown() {
                return Err(element.unknown_name(unknown, vec![]));
            }
            elements.push(element);
        }
        Ok(elements)
    }

    /// The arguments as given (an element's own object for an element).
    pub(crate) fn as_value(&self) -> Value {
        Value::Object(self.map.clone())
    }

    /// This call again (D13), unless it is too large to repeat.
    pub(crate) fn same_call(&self) -> Option<NextCall> {
        repeatable(self.tool, self.as_value())
    }

    /// `name`'s value, unless it is missing or `null`.
    fn given(&self, name: &str) -> Option<&Value> {
        self.map.get(name).filter(|value| !value.is_null())
    }

    /// The alphabetically first name not in `known`.
    fn unknown(&self) -> Option<&String> {
        self.map
            .keys()
            .filter(|name| !self.known.contains(&name.as_str()))
            .min()
    }

    fn field(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    fn takes(&self) -> String {
        if self.known.is_empty() {
            "it takes no arguments".to_string()
        } else {
            format!("it takes only {}", self.known.join(", "))
        }
    }

    fn refuse(&self, name: &str, expected: &str) -> ToolError {
        invalid_value(self.tool, &self.field(name), expected)
    }

    /// `name` is not taken here; the caller chose the name, so it is cut
    /// wherever it is echoed.
    fn unknown_name(&self, name: &str, next: Vec<NextCall>) -> ToolError {
        let field = cut_chars(self.field(name), TEXT_MAX_CHARS);
        invalid_input(self.tool, &field, &self.takes(), next)
    }
}

/// A call of `tool` with `args`, unless its JSON text is longer than
/// ECHO_MAX_CHARS characters.
pub(crate) fn repeatable(tool: &'static str, args: Value) -> Option<NextCall> {
    (args.to_string().chars().count() <= ECHO_MAX_CHARS).then_some(NextCall { tool, args })
}

#[cfg(test)]
pub(crate) mod tests {
    use rmcp::model::JsonObject;
    use serde_json::{json, Value};
    use shared::failure::{FailureCode, RetryAdvice, Stage};

    use super::*;
    use crate::connection::test_util::text_of;
    use crate::mcp::contract::{failed, upstream_error, NextCall};
    use crate::mcp::paging::RESULT_MAX_CHARS;
    use crate::services::NavidromeLookupError;

    pub(crate) fn obj(v: Value) -> Option<JsonObject> {
        v.as_object().cloned()
    }

    pub(crate) const SEARCH: &[&str] = &["query", "kind", "limit", "cursor"];

    #[test]
    fn an_unknown_argument_is_named_after_the_values_are_read_and_the_call_without_it_is_offered() {
        let a = Args::new(
            "search_library",
            obj(json!({"query": "silent night", "mood": "calm", "kind": "songs", "zest": 1})),
            SEARCH,
        );
        assert_eq!(
            (
                a.string("query").unwrap(),
                a.opt_choice("kind", &["songs", "albums", "artists"])
                    .unwrap()
            ),
            ("silent night".to_string(), Some("songs"))
        );
        let e = a.finish().unwrap_err();
        assert_eq!(
            (e.failure.code, e.failure.stage),
            (FailureCode::InvalidInput, Stage::Input)
        );
        assert_eq!(
            e.failure.message,
            "search_library takes no argument named 'mood': it takes only query, kind, limit, cursor"
        );
        assert_eq!(
            e.next,
            vec![NextCall {
                tool: "search_library",
                args: json!({"query": "silent night", "kind": "songs"})
            }]
        );
        let e = Args::new("get_status", obj(json!({"verbose": true})), &[])
            .finish()
            .unwrap_err();
        assert_eq!(
            e.failure.message,
            "get_status takes no argument named 'verbose': it takes no arguments"
        );
        assert_eq!(
            e.next,
            vec![NextCall {
                tool: "get_status",
                args: json!({})
            }]
        );
        assert!(Args::new(
            "search_library",
            obj(json!({"query": "silent night"})),
            SEARCH
        )
        .finish()
        .is_ok());
    }

    #[test]
    fn a_value_of_the_wrong_shape_names_what_was_expected() {
        let a = Args::new(
            "search_library",
            obj(json!({"query": "  ", "limit": 0, "kind": "tracks", "mood": "calm"})),
            SEARCH,
        );
        let e = a.string("query").unwrap_err();
        assert_eq!(
            (e.failure.code, e.failure.retry),
            (FailureCode::InvalidInput, RetryAdvice::NotRetryable)
        );
        assert_eq!(
            e.failure.message,
            "search_library: query must be a non-empty string of at most 500 characters"
        );
        assert_eq!(
            Value::Object(e.facts),
            json!({"field": "query", "expected": "a non-empty string of at most 500 characters"})
        );
        assert!(e.next.is_empty());
        assert_eq!(
            a.opt_u32("limit", 1, 100).unwrap_err().failure.message,
            "search_library: limit must be an integer from 1 to 100"
        );
        assert_eq!(
            a.opt_choice("kind", &["songs", "albums", "artists"])
                .unwrap_err()
                .failure
                .message,
            "search_library: kind must be one of songs, albums, artists"
        );
        let none = Args::new("search_library", None, SEARCH);
        assert_eq!(
            none.string("query").unwrap_err().failure.message,
            "search_library: query must be a non-empty string of at most 500 characters"
        );
        assert_eq!(
            (
                none.opt_u32("limit", 1, 100).unwrap(),
                none.opt_choice("kind", &["songs"]).unwrap()
            ),
            (None, None)
        );
        let typed = Args::new(
            "search_library",
            obj(json!({"query": 7, "limit": "10"})),
            SEARCH,
        );
        assert!(typed.string("query").is_err() && typed.opt_u32("limit", 1, 100).is_err());
    }

    #[test]
    fn a_string_longer_than_500_characters_is_refused() {
        let a = Args::new(
            "search_library",
            obj(json!({"query": format!(" {} ", "é".repeat(500)), "kind": "x".repeat(501)})),
            &["query", "kind"],
        );
        assert_eq!(a.string("query").unwrap(), "é".repeat(500));
        let a = Args::new(
            "search_library",
            obj(json!({"query": "é".repeat(501)})),
            SEARCH,
        );
        assert_eq!(
            a.string("query").unwrap_err().failure.message,
            "search_library: query must be a non-empty string of at most 500 characters"
        );
        assert!(a.opt_string("query").is_err());
    }

    #[test]
    fn ids_are_checked_and_strings_trimmed() {
        let a = Args::new(
            "find_in_library",
            obj(json!({"good": "4D04E9B5-ECD1-4EF3-BAE0-5C98492AF1C8",
                "simple": "4d04e9b5ecd14ef3bae05c98492af1c8", "artist": " Bing Crosby "})),
            &["good", "simple", "artist"],
        );
        assert_eq!(
            a.mbid("good").unwrap(),
            "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8"
        );
        assert_eq!(
            a.mbid("simple").unwrap_err().failure.message,
            "find_in_library: simple must be a MusicBrainz id: 36 characters, 8-4-4-4-12 hexadecimal digits"
        );
        assert_eq!(a.string("artist").unwrap(), "Bing Crosby");
        assert_eq!(
            a.same_call(),
            Some(NextCall {
                tool: "find_in_library",
                args: json!({"good": "4D04E9B5-ECD1-4EF3-BAE0-5C98492AF1C8",
                "simple": "4d04e9b5ecd14ef3bae05c98492af1c8", "artist": " Bing Crosby "})
            })
        );
    }

    #[test]
    fn list_elements_are_read_with_their_index_in_the_field_name() {
        let known = &["artist", "title", "album", "recording_mbid", "duration_s"];
        let a = Args::new(
            "find_in_library",
            obj(json!({"references": [
            {"artist": "Bing Crosby", "title": "Silent Night"}, {"artist": "Bing Crosby"}]})),
            &["references", "cursor"],
        );
        let refs = a.objects("references", 1, 100, known).unwrap();
        assert_eq!(refs[0].string("title").unwrap(), "Silent Night");
        assert_eq!(
            refs[0].as_value(),
            json!({"artist": "Bing Crosby", "title": "Silent Night"})
        );
        assert_eq!(
            refs[1].string("title").unwrap_err().failure.message,
            "find_in_library: references[1].title must be a non-empty string of at most 500 characters"
        );
        let a = Args::new(
            "find_in_library",
            obj(
                json!({"references": [{"artist": "Bing Crosby", "title": "Silent Night", "year": 1935}]}),
            ),
            &["references", "cursor"],
        );
        let e = a.objects("references", 1, 100, known).unwrap_err();
        assert_eq!(
            e.failure.message,
            "find_in_library takes no argument named 'references[0].year': it takes only artist, title, album, recording_mbid, duration_s"
        );
        assert!(e.next.is_empty());
        let many: Vec<Value> = (0..101)
            .map(|_| json!({"artist": "Bing Crosby", "title": "Silent Night"}))
            .collect();
        let a = Args::new(
            "find_in_library",
            obj(json!({"references": many})),
            &["references"],
        );
        assert_eq!(
            a.objects("references", 1, 100, known)
                .unwrap_err()
                .failure
                .message,
            "find_in_library: references must be a list of 1 to 100 objects"
        );
    }

    /// 100 references of three 500-character fields: about 155,000
    /// characters of arguments.
    fn large_call(extra: Value) -> Args {
        let field = "x".repeat(500);
        let refs: Vec<Value> = (0..100)
            .map(|_| json!({"artist": field, "title": field, "album": field}))
            .collect();
        let mut args = json!({"references": refs});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        Args::new("find_in_library", obj(args), &["references", "cursor"])
    }

    fn result_chars(e: ToolError) -> usize {
        text_of(&failed(e)).chars().count()
    }

    #[test]
    fn a_call_too_large_to_repeat_is_not_offered_and_its_unknown_name_is_cut() {
        let long = "z".repeat(10_000);
        let a = large_call(json!({long.clone(): 1}));
        let e = a.finish().unwrap_err();
        let field = e.facts["field"].as_str().unwrap().to_string();
        assert_eq!(field, cut_chars(long, TEXT_MAX_CHARS));
        assert_eq!(
            e.failure.message,
            format!("find_in_library takes no argument named '{field}': it takes only references, cursor The call is too large to offer again in next.")
        );
        assert!(e.next.is_empty());
        assert!(result_chars(e) <= RESULT_MAX_CHARS);
    }

    #[test]
    fn a_failure_that_could_be_retried_on_a_call_too_large_to_repeat_says_so() {
        let a = large_call(json!({}));
        assert_eq!(a.same_call(), None);
        let lookup = NavidromeLookupError::Database(
            "error returned from database: (code: 5) database is locked".into(),
        );
        let e = upstream_error(
            lookup.failure(Stage::LibraryCheck, lookup.to_string()),
            Map::new(),
            a.same_call(),
        );
        assert_eq!(
            e.failure.message,
            "error returned from database: (code: 5) database is locked The call is too large to offer again in next."
        );
        assert!(e.next.is_empty());
        assert!(result_chars(e) <= RESULT_MAX_CHARS);
    }
}
