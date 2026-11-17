//! The reference a write tool takes for a track or an album (D1), and the argument schemas
//! the acquisition tools share.

use serde_json::{json, Map, Value};

use super::args::Args;
use super::contract::ToolError;
use crate::core::jobs::{AlbumRef, Wanted};
use crate::core::library::Reference;
use crate::ledger::SourceFilters;

/// The names a reference object takes (D1).
pub(crate) const REFERENCE_FIELDS: &[&str] = &[
    "kind",
    "artist",
    "title",
    "album",
    "recording_mbid",
    "duration_s",
    "release_mbid",
];

const TRACK_FIELDS: [&str; 3] = ["album", "recording_mbid", "duration_s"];

/// The reference `r` names (an `object` or an `objects` element read with REFERENCE_FIELDS): a
/// track unless `kind` says album. A name of the other kind is refused.
pub(crate) fn wanted(r: &Args) -> Result<Wanted, ToolError> {
    let kind = r.opt_choice("kind", &["track", "album"])?;
    let artist = r.string("artist")?;
    let title = r.string("title")?;
    if kind == Some("album") {
        let release_mbid = r.mbid("release_mbid")?;
        for name in TRACK_FIELDS {
            r.absent(name, &format!("absent: {name} belongs to track references"))?;
        }
        return Ok(Wanted::Album(AlbumRef {
            artist,
            title,
            release_mbid,
        }));
    }
    let reference = Reference {
        artist,
        title,
        album: r.opt_string("album")?,
        recording_mbid: r.opt_mbid("recording_mbid")?,
        duration_s: r.opt_u32("duration_s", 1, 36_000)?,
    };
    r.absent(
        "release_mbid",
        "absent: release_mbid belongs to album references",
    )?;
    Ok(Wanted::Track(reference))
}

/// `wanted` as a reference argument: its fields as read, `kind` always named, a field that was
/// not given left out. Calls offered in `next` carry it, and a search stores it.
pub(crate) fn reference_value(wanted: &Wanted) -> Value {
    let mut fields = Map::new();
    match wanted {
        Wanted::Track(r) => {
            fields.insert("kind".into(), json!("track"));
            fields.insert("artist".into(), json!(r.artist));
            fields.insert("title".into(), json!(r.title));
            let optional = [
                ("album", r.album.as_ref().map(|album| json!(album))),
                (
                    "recording_mbid",
                    r.recording_mbid.as_ref().map(|id| json!(id)),
                ),
                ("duration_s", r.duration_s.map(|s| json!(s))),
            ];
            for (name, value) in optional {
                if let Some(value) = value {
                    fields.insert(name.into(), value);
                }
            }
        }
        Wanted::Album(a) => {
            fields.insert("kind".into(), json!("album"));
            fields.insert("artist".into(), json!(a.artist));
            fields.insert("title".into(), json!(a.title));
            fields.insert("release_mbid".into(), json!(a.release_mbid));
        }
    }
    Value::Object(fields)
}

/// The quality filters (D6) `filters_schema` declares: formats lowercase without a leading dot.
pub(crate) fn filters(args: &Args) -> Result<SourceFilters, ToolError> {
    let formats = args
        .opt_strings("formats", 20)?
        .unwrap_or_default()
        .into_iter()
        .map(|format| format.trim_start_matches('.').to_lowercase())
        .collect();
    Ok(SourceFilters {
        formats,
        min_bitrate_kbps: args.opt_u32("min_bitrate_kbps", 1, 10_000)?,
        lossless_only: args.opt_bool("lossless_only")?.unwrap_or(false),
        max_queue_length: args.opt_u32("max_queue_length", 0, 100_000)?.map(u64::from),
    })
}

/// The JSON Schema of a reference (JSON Schema 2020-12, the MCP default for `inputSchema`).
macro_rules! reference_schema {
    () => {
        r#"{"oneOf": [{"type": "object", "additionalProperties": false, "required": ["artist", "title"], "properties": {"kind": {"const": "track"}, "artist": {"type": "string", "minLength": 1}, "title": {"type": "string", "minLength": 1}, "album": {"type": "string"}, "recording_mbid": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"}, "duration_s": {"type": "integer", "minimum": 1, "maximum": 36000}}}, {"type": "object", "additionalProperties": false, "required": ["kind", "artist", "title", "release_mbid"], "properties": {"kind": {"const": "album"}, "artist": {"type": "string", "minLength": 1}, "title": {"type": "string", "minLength": 1}, "release_mbid": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"}}}]}"#
    };
}

/// The quality filters' properties (D6).
macro_rules! filters_schema {
    () => {
        r#""formats": {"type": "array", "maxItems": 20, "items": {"type": "string", "minLength": 1}}, "min_bitrate_kbps": {"type": "integer", "minimum": 1, "maximum": 10000}, "lossless_only": {"type": "boolean"}, "max_queue_length": {"type": "integer", "minimum": 0, "maximum": 100000}"#
    };
}

/// The `request_key` property of a write tool (design 10.3).
macro_rules! request_key_schema {
    () => {
        r#""request_key": {"type": "string", "minLength": 1, "maxLength": 200}"#
    };
}

/// The properties of a paged listing.
macro_rules! page_schema {
    () => {
        r#""limit": {"type": "integer", "minimum": 1, "maximum": 100}, "cursor": {"type": "string"}"#
    };
}

/// The properties of a call that waits for running work to change (design 10.4).
macro_rules! wait_schema {
    () => {
        r#""wait_seconds": {"type": "integer", "minimum": 0, "maximum": 30}, "wait_for": {"enum": ["any_change", "needs_action", "all_terminal"]}"#
    };
}

pub(crate) use {filters_schema, page_schema, reference_schema, request_key_schema, wait_schema};

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mcp::args::tests::obj;

    fn read(reference: Value) -> Result<Wanted, ToolError> {
        let args = Args::new(
            "find_sources",
            obj(json!({"reference": reference})),
            &["reference"],
        );
        wanted(&args.object("reference", REFERENCE_FIELDS)?)
    }

    #[test]
    fn a_reference_is_read_as_given_and_written_back_without_what_was_not_given() {
        let track = read(json!({"artist": " Marrow Choir ", "title": "O Holy Night",
            "recording_mbid": "4D04E9B5-ECD1-4EF3-BAE0-5C98492AF1C8"}))
        .unwrap();
        assert_eq!(
            reference_value(&track),
            json!({"kind": "track", "artist": "Marrow Choir", "title": "O Holy Night",
                "recording_mbid": "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8"})
        );
        let album = read(
            json!({"kind": "album", "artist": "Kowloon", "title": "Come Over",
            "release_mbid": "6a09041b-0f79-3278-88d9-0552dd28d37f"}),
        )
        .unwrap();
        assert_eq!(read(reference_value(&album)).unwrap(), album);
        let e = read(
            json!({"kind": "album", "artist": "Kowloon", "title": "Come Over",
            "release_mbid": "6a09041b-0f79-3278-88d9-0552dd28d37f", "duration_s": 200}),
        )
        .unwrap_err();
        assert_eq!(
            e.failure.message,
            "find_sources: reference.duration_s must be absent: duration_s belongs to track references"
        );
        let e =
            read(json!({"kind": "single", "artist": "Kowloon", "title": "Wake Up"})).unwrap_err();
        assert_eq!(
            e.failure.message,
            "find_sources: reference.kind must be one of track, album"
        );
    }

    #[test]
    fn formats_are_read_as_extensions() {
        let args = Args::new(
            "find_sources",
            obj(json!({"formats": [" .FLAC", "mp3"], "max_queue_length": 0})),
            &["formats", "max_queue_length"],
        );
        assert_eq!(
            filters(&args).unwrap(),
            SourceFilters {
                formats: vec!["flac".into(), "mp3".into()],
                max_queue_length: Some(0),
                ..SourceFilters::default()
            }
        );
        let args = Args::new(
            "find_sources",
            obj(json!({"formats": ["flac", " "]})),
            &["formats"],
        );
        assert_eq!(
            filters(&args).unwrap_err().failure.message,
            "find_sources: formats must be a list of at most 20 non-empty strings of at most 500 characters"
        );
    }

    #[test]
    fn the_schemas_are_json() {
        for text in [
            reference_schema!(),
            concat!("{", filters_schema!(), "}"),
            concat!("{", request_key_schema!(), "}"),
            concat!("{", page_schema!(), "}"),
        ] {
            assert!(serde_json::from_str::<Value>(text).is_ok(), "{text}");
        }
    }
}
