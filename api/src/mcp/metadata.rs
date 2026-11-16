//! The metadata tools: MusicBrainz and Last.fm searches and lookups.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::failure::{cut_json, json_chars, TEXT_MAX_CHARS};
use shared::metadata::{Album, AlbumWithTracks, Provider, SearchResult, SearchResults, Track};
use soulbeet::musicbrainz::{ArtistMatch, RecordingReleases};

use super::args::Args;
use super::contract::{answer, invalid_value, upstream_error, ToolError};
use super::paging::{fit, fit_whole, Listing, Page};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::metadata::{self, SearchKind};
use crate::services::providers;

const SEARCH: &str = "search_music";
const RELEASE: &str = "get_release";
const RECORDING: &str = "get_recording_releases";
const RECORDING_ARGS: &[&str] = &["recording_mbid", "limit", "cursor"];

/// The kinds `search_music` takes; each names its items key with an "s".
const KINDS: &[&str] = &["recording", "release", "artist"];
const PROVIDERS: &[&str] = &[providers::MUSICBRAINZ, providers::LASTFM];

const RELEASE_ID: &str =
    "a MusicBrainz release id or the lastfm: id of a release from search_music";

/// A text from MusicBrainz or Last.fm, cut as any outside text is. Ids are
/// never cut: see `whole_ids`.
fn cut(text: String) -> String {
    cut_json(text, TEXT_MAX_CHARS)
}

/// `entry` with each of its `fields` that holds an id longer than
/// TEXT_MAX_CHARS of JSON set to null, and `ids_too_long` naming those
/// fields; an entry whose ids all fit is unchanged. A cut id would still
/// look like an id and name nothing, so an id is sent whole or not at all.
/// An MBID is 36 characters; a lastfm: id holds Last.fm's artist and name,
/// which can be as long as any text.
fn whole_ids(mut entry: Value, fields: &[&str]) -> Value {
    let Value::Object(map) = &mut entry else {
        unreachable!("an entry is an object");
    };
    let too_long: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|field| {
            map.get(*field)
                .and_then(Value::as_str)
                .is_some_and(|id| json_chars(id) > TEXT_MAX_CHARS)
        })
        .collect();
    if !too_long.is_empty() {
        for field in &too_long {
            map.insert(field.to_string(), Value::Null);
        }
        map.insert("ids_too_long".into(), json!(too_long));
    }
    entry
}

fn cut_opt(text: Option<String>) -> Option<String> {
    text.map(cut)
}

fn object(value: Value) -> Map<String, Value> {
    let Value::Object(fields) = value else {
        unreachable!("an object literal is an object");
    };
    fields
}

/// D6: the search echoed, the provider asked for and the one that
/// answered, and the entries under the kind's key, as many as keep the
/// result within RESULT_MAX_CHARS, `left_out` counting the rest.
fn search_fields(
    kind: &str,
    query: &str,
    requested: Provider,
    used: Provider,
    entries: Vec<Value>,
) -> Map<String, Value> {
    let fields = object(json!({"kind": kind, "query": query,
        "provider_requested": requested, "provider_used": used}));
    fit_whole(fields, &format!("{kind}s"), entries, "left_out")
}

/// D6: `{"kind", "query", "provider_requested", "provider_used", kind's key: [..]}`
/// for a recording or release search.
pub(crate) fn music_fields(
    kind: &str,
    query: &str,
    requested: Provider,
    results: &SearchResults,
) -> Map<String, Value> {
    let entries = results
        .results
        .iter()
        .map(|result| match result {
            SearchResult::Track(track) => recording_entry(track),
            SearchResult::Album(album) => release_entry(album),
        })
        .collect();
    search_fields(kind, query, requested, results.provider, entries)
}

/// A found recording; `release` is the one the provider put it on, null
/// when it named none.
fn recording_entry(t: &Track) -> Value {
    let release = t.album_id.as_ref().map(|id| {
        let release = json!({"id": id, "mbid": t.release_mbid,
            "title": cut_opt(t.album_title.clone()), "date": cut_opt(t.release_date.clone())});
        whole_ids(release, &["id", "mbid"])
    });
    let entry = json!({"id": t.id, "recording_mbid": t.mbid, "title": cut(t.title.clone()),
        "artist": cut(t.artist.clone()), "duration_s": t.duration_secs(), "release": release});
    whole_ids(entry, &["id", "recording_mbid"])
}

/// A found release; `release_types` is null when the provider gave none (D15).
fn release_entry(a: &Album) -> Value {
    let entry = json!({"id": a.id, "release_mbid": a.mbid, "title": cut(a.title.clone()),
        "artist": cut(a.artist.clone()), "date": cut_opt(a.release_date.clone()),
        "release_types": a.release_types});
    whole_ids(entry, &["id", "release_mbid"])
}

/// D6 for an artist search, which reads MusicBrainz only.
pub(crate) fn artist_fields(query: &str, found: &[ArtistMatch]) -> Map<String, Value> {
    let entries = found
        .iter()
        .map(|a| {
            let mut a = a.clone();
            a.name = cut(a.name);
            a.sort_name = cut(a.sort_name);
            a.disambiguation = cut_opt(a.disambiguation);
            a.country = cut_opt(a.country);
            whole_ids(json!(a), &["mbid"])
        })
        .collect();
    search_fields(
        "artist",
        query,
        Provider::MusicBrainz,
        Provider::MusicBrainz,
        entries,
    )
}

/// `({"provider_used", "release": {"id", "mbid", "title", "artist", "date",
/// "release_types"}}, the tracks)`; `release_types` is null for None (D15).
/// A track's `position` is its 1-based place in the release's running order
/// across media.
pub(crate) fn release_fields(
    release: &AlbumWithTracks,
    used: Provider,
) -> (Map<String, Value>, Vec<Value>) {
    let a = &release.album;
    let album = json!({"id": a.id, "mbid": a.mbid, "title": cut(a.title.clone()),
        "artist": cut(a.artist.clone()), "date": cut_opt(a.release_date.clone()),
        "release_types": a.release_types});
    let fields =
        object(json!({"provider_used": used, "release": whole_ids(album, &["id", "mbid"])}));
    let tracks = release
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let track = json!({"position": i + 1, "recording_mbid": t.mbid, "title": cut(t.title.clone()),
                "artist": cut(t.artist.clone()), "duration_s": t.duration_secs()});
            whole_ids(track, &["recording_mbid"])
        })
        .collect();
    (fields, tracks)
}

/// The recording, its release count and the page of releases MusicBrainz
/// gave from `page.offset`, with a cursor while releases remain.
fn recording_releases_fields(found: &RecordingReleases, page: &Page) -> Map<String, Value> {
    let mut recording = found.recording.clone();
    recording.title = cut(recording.title);
    recording.artist = cut_opt(recording.artist);
    recording.disambiguation = cut_opt(recording.disambiguation);
    let releases = found
        .releases
        .iter()
        .map(|r| {
            let mut r = r.clone();
            r.title = cut(r.title);
            r.country = cut_opt(r.country);
            if let Some(group) = &mut r.release_group {
                group.title = cut(std::mem::take(&mut group.title));
            }
            let mut release = whole_ids(json!(r), &["mbid"]);
            if let Some(group) = release.get_mut("release_group").filter(|g| g.is_object()) {
                *group = whole_ids(group.take(), &["mbid"]);
            }
            release
        })
        .collect();
    let more = page.offset + found.releases.len() < found.release_count as usize;
    let recording = whole_ids(json!(recording), &["mbid"]);
    let fields = object(json!({"recording": recording, "release_count": found.release_count}));
    fit(fields, "releases", releases, page, more)
}

/// `search_music`: recordings or releases from the provider asked for, or
/// artists from MusicBrainz.
pub(crate) async fn search_music(
    _state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(
        SEARCH,
        args,
        &["kind", "query", "artist", "provider", "limit"],
    );
    answer(
        async {
            let kind = args.opt_choice("kind", KINDS)?.ok_or_else(|| {
                invalid_value(SEARCH, "kind", &format!("one of {}", KINDS.join(", ")))
            })?;
            let query = args.string("query")?;
            let artist = args.opt_string("artist")?;
            let provider = args
                .opt_choice("provider", PROVIDERS)?
                .unwrap_or(providers::MUSICBRAINZ);
            let limit = args.opt_u32("limit", 1, 25)?.unwrap_or(10);
            if kind == "artist" {
                if artist.is_some() {
                    return Err(invalid_value(
                        SEARCH,
                        "artist",
                        "absent for kind artist: put the name in query",
                    ));
                }
                if provider != providers::MUSICBRAINZ {
                    return Err(invalid_value(
                        SEARCH,
                        "provider",
                        "musicbrainz for kind artist: artist search reads MusicBrainz only",
                    ));
                }
            }
            args.finish()?;
            let requested: Provider = provider.parse().expect("PROVIDERS are provider names");
            let search_kind = match kind {
                "recording" => Some(SearchKind::Tracks),
                "release" => Some(SearchKind::Albums),
                "artist" => None,
                other => unreachable!("opt_choice answers one of KINDS, not {other}"),
            };
            let found = match search_kind {
                Some(search_kind) => metadata::search(
                    &caller.ctx,
                    search_kind,
                    artist.as_deref(),
                    &query,
                    Some(provider),
                    limit as usize,
                )
                .await
                .map(|results| music_fields(kind, &query, requested, &results)),
                None => {
                    let limit = u8::try_from(limit).expect("limit is at most 25");
                    metadata::artists(&query, limit)
                        .await
                        .map(|found| artist_fields(&query, &found))
                }
            };
            found.map_err(|f| {
                let facts = object(json!({"kind": kind, "query": query, "artist": artist,
                    "provider_requested": requested}));
                upstream_error(f, facts, args.same_call())
            })
        }
        .await,
    )
}

/// An MBID by `Args::mbid`'s rule, lowercase, or a Last.fm album id as
/// given. A Last.fm track id, or an id of the form Soulful wrote before ids
/// named their kind, is refused: a tool call never holds one, since no tool
/// gave out that form, and a track id read as an album names another one.
fn read_release_id(args: &Args) -> Result<String, ToolError> {
    let refused = || invalid_value(RELEASE, "release_id", RELEASE_ID);
    let id = args.string("release_id").map_err(|_| refused())?;
    if soulbeet::lastfm::is_album_id(&id) {
        return Ok(id);
    }
    args.mbid("release_id").map_err(|_| refused())
}

/// `get_release`: the release and a page of its tracks.
pub(crate) async fn get_release(
    _state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(RELEASE, args, &["release_id", "limit", "cursor"]);
    answer(
        async {
            let release_id = read_release_id(&args)?;
            let page = Page::read(&args, json!({"release_id": release_id}))?;
            args.finish()?;
            let (release, used) =
                metadata::release(&caller.ctx, &release_id)
                    .await
                    .map_err(|f| {
                        upstream_error(
                            f,
                            object(json!({"release_id": release_id})),
                            args.same_call(),
                        )
                    })?;
            let (fields, tracks) = release_fields(&release, used);
            Ok(fit(fields, "tracks", page.of(&tracks), &page, false))
        }
        .await,
    )
}

/// `get_recording_releases`: the recording and a page of the releases it
/// is on. MusicBrainz takes an offset of at most u16::MAX, so a cursor past
/// it is refused.
pub(crate) async fn get_recording_releases(
    _state: &ConnectionState,
    _caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new(RECORDING, args, RECORDING_ARGS);
    answer(
        async {
            let mbid = args.mbid("recording_mbid")?;
            let page = Page::read(&args, json!({"recording_mbid": mbid}))?;
            let offset = u16::try_from(page.offset).map_err(|_| Listing::refused(RECORDING))?;
            args.finish()?;
            let limit =
                u8::try_from(page.limit).expect("Page::read keeps a limit within MAX_LIMIT");
            let found = metadata::recording_releases(&mbid, offset, limit)
                .await
                .map_err(|f| {
                    upstream_error(f, object(json!({"recording_mbid": mbid})), args.same_call())
                })?;
            Ok(recording_releases_fields(&found, &page))
        }
        .await,
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Map, Value};
    use shared::failure::{FailureCode, RetryAdvice, Service, Stage};
    use shared::metadata::{Album, AlbumWithTracks, Provider, SearchResult, SearchResults, Track};
    use soulbeet::musicbrainz::{RecordingInfo, RecordingRelease, ReleaseGroupInfo};
    use soulbeet::MetadataError;

    use super::*;
    use crate::connection::test_util::{call_tool, document, mcp_app};
    use crate::core::metadata::provider_failure;
    use crate::mcp::args::Args;
    use crate::mcp::contract::{upstream_error, NextCall};

    fn silent_night() -> Track {
        // the recording of Task 8's RECORDING, as musicbrainz::search builds a Track (musicbrainz.rs:228-238)
        Track {
            id: "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into(),
            title: "Silent Night".into(),
            artist: "Bing Crosby".into(),
            album_id: Some("317d1e85-3f9e-463e-939b-bebf47aa2178".into()),
            album_title: Some("White Christmas".into()),
            release_date: Some("1995".into()),
            duration: Some("02:29".into()),
            mbid: Some("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into()),
            release_mbid: Some("317d1e85-3f9e-463e-939b-bebf47aa2178".into()),
        }
    }

    #[test]
    fn a_recording_search_names_the_provider_asked_for_and_the_one_used() {
        let results = SearchResults {
            provider: Provider::MusicBrainz,
            results: vec![SearchResult::Track(silent_night())],
        };
        assert_eq!(
            Value::Object(music_fields(
                "recording",
                "Silent Night",
                Provider::LastFm,
                &results
            )),
            json!({
            "kind": "recording", "query": "Silent Night", "provider_requested": "lastfm", "provider_used": "musicbrainz",
            "recordings": [{"id": "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8", "recording_mbid": "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8",
                "title": "Silent Night", "artist": "Bing Crosby", "duration_s": 149,
                "release": {"id": "317d1e85-3f9e-463e-939b-bebf47aa2178", "mbid": "317d1e85-3f9e-463e-939b-bebf47aa2178",
                            "title": "White Christmas", "date": "1995"}}],
            "left_out": 0})
        );
    }

    #[test]
    fn a_release_search_carries_its_types() {
        let album = Album {
            id: "317d1e85-3f9e-463e-939b-bebf47aa2178".into(),
            title: "Merry Christmas".into(),
            artist: "Bing Crosby".into(),
            release_date: Some("1995".into()),
            mbid: Some("317d1e85-3f9e-463e-939b-bebf47aa2178".into()),
            cover_url: None,
            release_types: Some(vec!["Album".into(), "Compilation".into()]),
        };
        let doc = music_fields(
            "release",
            "Merry Christmas",
            Provider::MusicBrainz,
            &SearchResults {
                provider: Provider::MusicBrainz,
                results: vec![SearchResult::Album(album)],
            },
        );
        assert_eq!(
            doc["releases"][0]["release_types"],
            json!(["Album", "Compilation"])
        );
    }

    #[test]
    fn a_release_lists_its_tracks_by_position() {
        let mut release = AlbumWithTracks {
            album: Album {
                id: "317d1e85-3f9e-463e-939b-bebf47aa2178".into(),
                title: "White Christmas".into(),
                artist: "Bing Crosby".into(),
                release_date: Some("1995".into()),
                mbid: Some("317d1e85-3f9e-463e-939b-bebf47aa2178".into()),
                cover_url: None,
                release_types: Some(vec!["Album".into(), "Compilation".into()]),
            },
            tracks: vec![silent_night()],
        };
        let (fields, tracks) = release_fields(&release, Provider::MusicBrainz);
        assert_eq!(
            (
                &fields["provider_used"],
                &fields["release"]["release_types"],
                &tracks[0]
            ),
            (
                &json!("musicbrainz"),
                &json!(["Album", "Compilation"]),
                &json!({"position": 1,
            "recording_mbid": "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8", "title": "Silent Night", "artist": "Bing Crosby", "duration_s": 149})
            )
        );
        release.album.release_types = None; // a Last.fm album
        assert_eq!(
            release_fields(&release, Provider::LastFm).0["release"]["release_types"],
            Value::Null
        );
    }

    #[tokio::test]
    async fn metadata_arguments_are_refused_with_what_was_expected() {
        let (_, app, token) = mcp_app().await;
        for (name, args, message) in [
            ("search_music", json!({"kind": "artist", "query": "Bing Crosby", "provider": "lastfm"}),
             "search_music: provider must be musicbrainz for kind artist: artist search reads MusicBrainz only"),
            ("search_music", json!({"kind": "song", "query": "Silent Night"}), "search_music: kind must be one of recording, release, artist"),
            ("get_recording_releases", json!({"recording_mbid": "4d04e9b5"}),
             "get_recording_releases: recording_mbid must be a MusicBrainz id: 36 characters, 8-4-4-4-12 hexadecimal digits"),
            ("get_release", json!({"release_id": "White Christmas"}),
             "get_release: release_id must be a MusicBrainz release id or the lastfm: id of a release from search_music"),
            // A recording's id from a Last.fm search names no release.
            ("get_release", json!({"release_id": "lastfm:track:bing crosby:white christmas"}),
             "get_release: release_id must be a MusicBrainz release id or the lastfm: id of a release from search_music"),
            ("get_release", json!({"release_id": "lastfm:bing crosby:white christmas"}),
             "get_release: release_id must be a MusicBrainz release id or the lastfm: id of a release from search_music"),
        ] {
            let e = &document(&call_tool(app.clone(), &token, name, args).await)["error"];
            assert_eq!(
                (e["code"].as_str(), e["message"].as_str()),
                (Some("invalid_input"), Some(message)),
                "{name}"
            );
        }
    }

    /// What `upstream_error` makes of `provider_failure` for the two
    /// MusicBrainz failures get_recording_releases meets most: nothing to
    /// offer for a recording it does not have, the same call for one that
    /// did not answer.
    #[test]
    fn a_provider_that_has_nothing_offers_no_call_and_a_slow_one_the_same_call() {
        let url = "http://musicbrainz.org/ws/2/recording/0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c?fmt=json&inc=artists";
        // MetadataError::from(musicbrainz_rs::Error::NotFound(url)) (lib/soulbeet/src/musicbrainz.rs:76-83, musicbrainz_rs src/error.rs:17)
        let e = MetadataError::NotFound {
            service: Service::Musicbrainz,
            status: None,
            raw: format!("Musicbrainz returned \"Not found\" for query \"{url}\""),
        };
        let args = Args::new(
            "get_recording_releases",
            json!({"recording_mbid": "0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c"})
                .as_object()
                .cloned(),
            &["recording_mbid", "limit", "cursor"],
        );
        let t = upstream_error(
            provider_failure(&e),
            json!({"recording_mbid": "0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c"})
                .as_object()
                .unwrap()
                .clone(),
            args.same_call(),
        );
        assert_eq!(
            (t.failure.code, t.failure.stage, t.failure.retry),
            (
                FailureCode::MetadataNotFound,
                Stage::Metadata,
                RetryAdvice::OtherQuery
            )
        );
        assert_eq!(
            t.failure.cause.as_ref().unwrap().raw,
            format!("Musicbrainz returned \"Not found\" for query \"{url}\"")
        );
        assert!(
            t.next.is_empty(),
            "other_query: Claude picks another recording"
        );
        // with_retry, musicbrainz.rs:140-144
        let busy = MetadataError::Unavailable {
            service: Service::Musicbrainz,
            status: None,
            raw: "no answer within 15 s".into(),
        };
        assert_eq!(
            upstream_error(provider_failure(&busy), Map::new(), args.same_call()).next,
            vec![NextCall::new(
                "get_recording_releases",
                json!({"recording_mbid": "0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c"})
            )]
        );
    }

    /// Task 8's recording and its release, as recording_releases_from
    /// builds them from the MusicBrainz answers fetched on 2026-11-15
    /// (lib/soulbeet/src/musicbrainz.rs, RECORDING and RELEASES), with
    /// `release_count` releases in all.
    fn silent_night_releases(release_count: u32) -> RecordingReleases {
        RecordingReleases {
            recording: RecordingInfo {
                mbid: "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into(),
                title: "Silent Night".into(),
                artist: Some("Bing Crosby".into()),
                duration_s: Some(149),
                disambiguation: None,
                first_release_date: Some("1995".into()),
            },
            release_count,
            releases: vec![RecordingRelease {
                mbid: "317d1e85-3f9e-463e-939b-bebf47aa2178".into(),
                title: "White Christmas".into(),
                date: Some("1995".into()),
                country: Some("GB".into()),
                status: Some("Official".into()),
                release_group: Some(ReleaseGroupInfo {
                    mbid: "a82c4b78-9e9a-390c-af1f-3c8a8a44c555".into(),
                    title: "Merry Christmas".into(),
                    release_types: Some(vec!["Album".into(), "Compilation".into()]),
                    first_release_date: Some("1945".into()),
                }),
            }],
        }
    }

    fn recording_page(args: Value) -> Page {
        let args = Args::new(RECORDING, args.as_object().cloned(), RECORDING_ARGS);
        Page::read(
            &args,
            json!({"recording_mbid": "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8"}),
        )
        .unwrap()
    }

    #[test]
    fn a_page_of_releases_offers_a_cursor_while_musicbrainz_has_more() {
        let page = recording_page(json!({"limit": 1}));
        let last = recording_releases_fields(&silent_night_releases(1), &page);
        assert_eq!(
            Value::Object(last),
            json!({"recording": {"mbid": "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8", "title": "Silent Night",
                "artist": "Bing Crosby", "duration_s": 149, "disambiguation": null, "first_release_date": "1995"},
                "release_count": 1,
                "releases": [{"mbid": "317d1e85-3f9e-463e-939b-bebf47aa2178", "title": "White Christmas",
                    "date": "1995", "country": "GB", "status": "Official",
                    "release_group": {"mbid": "a82c4b78-9e9a-390c-af1f-3c8a8a44c555", "title": "Merry Christmas",
                        "release_types": ["Album", "Compilation"], "first_release_date": "1945"}}],
                "cursor": null})
        );
        let more = recording_releases_fields(&silent_night_releases(3), &page);
        let next = recording_page(json!({"cursor": more["cursor"]}));
        assert_eq!(next.offset, 1);
    }

    #[tokio::test]
    async fn a_cursor_past_what_musicbrainz_can_offset_is_refused() {
        let (_, app, token) = mcp_app().await;
        let mbid = "4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8";
        let cursor = Listing::new(RECORDING, &json!({"recording_mbid": mbid}))
            .cursor(usize::from(u16::MAX) + 1);
        let e = &document(
            &call_tool(
                app,
                &token,
                RECORDING,
                json!({"recording_mbid": mbid, "cursor": cursor}),
            )
            .await,
        )["error"];
        assert_eq!(
            (e["code"].as_str(), e["message"].as_str()),
            (
                Some("invalid_input"),
                Some("get_recording_releases: cursor must be a cursor get_recording_releases returned for the same arguments")
            )
        );
    }

    #[tokio::test]
    async fn an_artist_search_takes_the_name_in_query_only() {
        let (_, app, token) = mcp_app().await;
        let e = &document(
            &call_tool(
                app,
                &token,
                SEARCH,
                json!({"kind": "artist", "query": "Bing Crosby", "artist": "Bing Crosby"}),
            )
            .await,
        )["error"];
        assert_eq!(
            e["message"],
            "search_music: artist must be absent for kind artist: put the name in query"
        );
    }

    #[test]
    fn an_artist_search_reads_musicbrainz() {
        // Task 8's ARTISTS answer as artist_match builds it (lib/soulbeet/src/musicbrainz.rs)
        let crosby = ArtistMatch {
            mbid: "2437980f-513a-44fc-80f1-b90d9d7fcf8f".into(),
            name: "Bing Crosby".into(),
            sort_name: "Crosby, Bing".into(),
            disambiguation: None,
            artist_type: Some("Person".into()),
            country: Some("US".into()),
            begin: Some("1903-05-03".into()),
            end: Some("1977-10-14".into()),
        };
        assert_eq!(
            Value::Object(artist_fields("Bing Crosby", &[crosby])),
            json!({"kind": "artist", "query": "Bing Crosby", "provider_requested": "musicbrainz",
                "provider_used": "musicbrainz", "artists": [{"mbid": "2437980f-513a-44fc-80f1-b90d9d7fcf8f",
                "name": "Bing Crosby", "sort_name": "Crosby, Bing", "disambiguation": null, "artist_type": "Person",
                "country": "US", "begin": "1903-05-03", "end": "1977-10-14"}], "left_out": 0})
        );
    }

    #[test]
    fn a_search_keeps_the_entries_that_fit_and_counts_the_rest() {
        // 25 recordings whose texts are long and full of quotes, each escaped to 2 characters,
        // and whose ids are just short enough to be sent whole: every text is cut to 500
        // characters as sent, and 25 entries of 4 such texts and 4 such ids still pass 60,000.
        let text = "\"".repeat(2_000);
        let id = |i: usize| format!("{i:>width$}", width = TEXT_MAX_CHARS - 2);
        let tracks: Vec<SearchResult> = (0..25)
            .map(|i| {
                SearchResult::Track(Track {
                    id: id(i),
                    title: format!("{text}{i}"),
                    artist: text.clone(),
                    album_id: Some(id(i)),
                    album_title: Some(text.clone()),
                    release_date: Some(text.clone()),
                    duration: None,
                    mbid: Some(id(i)),
                    release_mbid: Some(id(i)),
                })
            })
            .collect();
        let mut doc = music_fields(
            "recording",
            "Silent Night",
            Provider::LastFm,
            &SearchResults {
                provider: Provider::LastFm,
                results: tracks,
            },
        );
        let kept = doc["recordings"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 25, "{kept}");
        assert_eq!(doc["left_out"], json!(25 - kept));
        let first = &doc["recordings"][0];
        assert_eq!(
            (
                &first["id"],
                &first["release"]["id"],
                first.get("ids_too_long")
            ),
            (&json!(id(0)), &json!(id(0)), None)
        );
        doc.insert("ok".into(), json!(true));
        assert!(
            Value::Object(doc).to_string().chars().count() <= crate::mcp::paging::RESULT_MAX_CHARS
        );
    }

    #[test]
    fn an_id_too_long_to_return_is_null_and_named_never_cut() {
        // A lastfm: id of 501 characters, one past what a text field holds.
        let long_id = format!("lastfm:album:{}", "x".repeat(TEXT_MAX_CHARS + 1 - 13));
        let short = silent_night();
        let track = Track {
            id: long_id.clone(),
            album_id: Some(long_id.clone()),
            ..short.clone()
        };
        let found = music_fields(
            "recording",
            "Silent Night",
            Provider::LastFm,
            &SearchResults {
                provider: Provider::LastFm,
                results: vec![
                    SearchResult::Track(track.clone()),
                    SearchResult::Track(short.clone()),
                ],
            },
        );
        let (long, kept) = (&found["recordings"][0], &found["recordings"][1]);
        assert_eq!(
            (&long["id"], &long["ids_too_long"], &long["title"]),
            (&Value::Null, &json!(["id"]), &json!("Silent Night"))
        );
        assert_eq!(
            (
                &long["release"]["id"],
                &long["release"]["mbid"],
                &long["release"]["ids_too_long"]
            ),
            (&Value::Null, &json!(short.release_mbid), &json!(["id"]))
        );
        assert_eq!(
            (&kept["id"], kept.get("ids_too_long")),
            (&json!(short.id), None)
        );
        let album = Album {
            id: long_id.clone(),
            title: "Winter Songs".into(),
            artist: "Marrow Choir".into(),
            release_date: None,
            mbid: None,
            cover_url: None,
            release_types: None,
        };
        let release = &music_fields(
            "release",
            "Winter Songs",
            Provider::LastFm,
            &SearchResults {
                provider: Provider::LastFm,
                results: vec![SearchResult::Album(album.clone())],
            },
        )["releases"][0];
        assert_eq!(
            (&release["id"], &release["ids_too_long"]),
            (&Value::Null, &json!(["id"]))
        );
        let (fields, tracks) = release_fields(
            &AlbumWithTracks {
                album,
                tracks: vec![Track {
                    mbid: Some(long_id.clone()),
                    ..short
                }],
            },
            Provider::LastFm,
        );
        assert_eq!(
            (
                &fields["release"]["id"],
                &fields["release"]["ids_too_long"],
                &tracks[0]["recording_mbid"],
                &tracks[0]["ids_too_long"]
            ),
            (
                &Value::Null,
                &json!(["id"]),
                &Value::Null,
                &json!(["recording_mbid"])
            )
        );
        let mut found = silent_night_releases(1);
        found.recording.mbid = long_id.clone();
        found.releases[0].mbid = long_id.clone();
        found.releases[0].release_group.as_mut().unwrap().mbid = long_id.clone();
        let doc = recording_releases_fields(&found, &recording_page(json!({})));
        for entry in [
            &doc["recording"],
            &doc["releases"][0],
            &doc["releases"][0]["release_group"],
        ] {
            assert_eq!(
                (&entry["mbid"], &entry["ids_too_long"]),
                (&Value::Null, &json!(["mbid"]))
            );
        }
        let artist = ArtistMatch {
            mbid: long_id,
            name: "Bing Crosby".into(),
            sort_name: "Crosby, Bing".into(),
            disambiguation: None,
            artist_type: None,
            country: None,
            begin: None,
            end: None,
        };
        let a = &artist_fields("Bing Crosby", &[artist])["artists"][0];
        assert_eq!(
            (&a["mbid"], &a["ids_too_long"]),
            (&Value::Null, &json!(["mbid"]))
        );
    }

    #[test]
    fn provider_text_is_cut_and_ids_that_fit_are_sent_whole() {
        let long = "x".repeat(TEXT_MAX_CHARS + 1);
        let cut_long = cut_json(long.clone(), TEXT_MAX_CHARS);
        let track = Track {
            title: long.clone(),
            artist: long.clone(),
            album_title: Some(long.clone()),
            ..silent_night()
        };
        let found = music_fields(
            "recording",
            "Silent Night",
            Provider::MusicBrainz,
            &SearchResults {
                provider: Provider::MusicBrainz,
                results: vec![SearchResult::Track(track.clone())],
            },
        );
        let r = &found["recordings"][0];
        assert_eq!(
            (&r["title"], &r["artist"], &r["release"]["title"], &r["id"]),
            (
                &json!(cut_long),
                &json!(cut_long),
                &json!(cut_long),
                &json!(track.id)
            )
        );
        let release = AlbumWithTracks {
            album: Album {
                id: "317d1e85-3f9e-463e-939b-bebf47aa2178".into(),
                title: long.clone(),
                artist: long.clone(),
                release_date: None,
                mbid: None,
                cover_url: None,
                release_types: None,
            },
            tracks: vec![track],
        };
        let (fields, tracks) = release_fields(&release, Provider::MusicBrainz);
        assert_eq!(
            (
                &fields["release"]["title"],
                &fields["release"]["artist"],
                &tracks[0]["title"],
                &tracks[0]["artist"]
            ),
            (
                &json!(cut_long),
                &json!(cut_long),
                &json!(cut_long),
                &json!(cut_long)
            )
        );
        let mut found = silent_night_releases(1);
        found.recording.disambiguation = Some(long.clone());
        found.releases[0].country = Some(long.clone());
        found.releases[0].release_group.as_mut().unwrap().title = long.clone();
        let page = recording_page(json!({}));
        let doc = recording_releases_fields(&found, &page);
        assert_eq!(
            (
                &doc["recording"]["disambiguation"],
                &doc["releases"][0]["country"],
                &doc["releases"][0]["release_group"]["title"]
            ),
            (&json!(cut_long), &json!(cut_long), &json!(cut_long))
        );
        let artist = ArtistMatch {
            mbid: "2437980f-513a-44fc-80f1-b90d9d7fcf8f".into(),
            name: long.clone(),
            sort_name: long.clone(),
            disambiguation: Some(long.clone()),
            artist_type: None,
            country: Some(long),
            begin: None,
            end: None,
        };
        let a = &artist_fields("Bing Crosby", &[artist])["artists"][0];
        assert_eq!(
            (
                &a["name"],
                &a["sort_name"],
                &a["disambiguation"],
                &a["country"]
            ),
            (
                &json!(cut_long),
                &json!(cut_long),
                &json!(cut_long),
                &json!(cut_long)
            )
        );
    }
}
