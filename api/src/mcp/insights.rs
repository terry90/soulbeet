//! Read-only insight tools: the caller's listening history and taste profile, and a read-only
//! preview of Soulbeet's recommendation engine.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::recommendation::DiscoveryProfile;

use super::args::Args;
use super::contract::{answer, upstream_error};
use super::paging::{leave_out, length, Keep, RESULT_MAX_CHARS};
use super::server::{common_codes, ToolSpec, NAVIDROME_CODES};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::insights::{
    self, ListeningHistoryReport, RecommendationPreview, DEFAULT_LIMIT, MAX_LIMIT,
};
use crate::core::recommend::DiscoveryServices;

const LISTENING_HISTORY_ARGS: &[&str] = &["limit", "include_listenbrainz"];
const RECOMMENDATIONS_ARGS: &[&str] = &["profile", "limit"];
const PROFILES: &[&str] = &DiscoveryProfile::NAMES;

pub(crate) const GET_LISTENING_HISTORY: ToolSpec = ToolSpec {
    name: "get_listening_history",
    title: "Listening history",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Shows a read-only summary of this user's listening history and taste profile: recent_plays (most recently played songs with play_date, play_count, rating, star and Soulbeet folder when known), top_tracks (most played songs), top_artists (aggregated play counts, track counts, starred tracks and average ratings), top_genres (most played genres), summary totals (including any configured ListenBrainz username, never the token), and optionally ListenBrainz recent listens and top artists when a ListenBrainz username is saved. Each list holds at most limit items (1 to {MAX_LIMIT}, default {DEFAULT_LIMIT}). Set include_listenbrainz to false to read Navidrome only and return immediately. If ListenBrainz fails, Navidrome history is still returned with the failure noted in source_errors. Failure codes: {common}, {NAVIDROME_CODES}.")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "properties": {"limit": {"type": "integer", "minimum": 1, "maximum": 50}, "include_listenbrainz": {"type": "boolean"}}}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: true,
    call: |state, caller, args| Box::pin(get_listening_history(state, caller, args)),
};

pub(crate) const GET_RECOMMENDATIONS: ToolSpec = ToolSpec {
    name: "get_recommendations",
    title: "Preview recommendations",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Runs Soulbeet's recommendation engine in read-only preview mode for one discovery profile (Conservative, Balanced or Adventurous; default Balanced) and returns up to limit ranked track candidates (1 to {MAX_LIMIT}, default {DEFAULT_LIMIT}) with the seed_artists and sources used to generate them, plus any partial source_errors. Does not download anything, does not touch the discovery queue or database, and does not create playlists; discuss candidates with the user and pass chosen tracks to ensure_in_library to download them. Queries Navidrome, ListenBrainz, MusicBrainz and (when turned on in settings) Deezer, so a cold preview can take several seconds to a minute. Failure codes: {common}, {NAVIDROME_CODES}, no_listening_data (nothing played or starred in Navidrome yet and every listening history answered; user action listen_in_navidrome), metadata_unavailable (no listening data to start from while a listening history such as ListenBrainz failed, or a remote client could not be set up).")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "properties": {"profile": {"type": "string", "enum": ["Conservative", "Balanced", "Adventurous"]}, "limit": {"type": "integer", "minimum": 1, "maximum": 50}}}"#,
    read_only: true,
    destructive: false,
    idempotent: false,
    open_world: true,
    call: |state, caller, args| Box::pin(get_recommendations(state, caller, args)),
};

async fn get_listening_history(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let services = DiscoveryServices::for_db(state.db.clone());
    listening_history_with(state, caller, args, &services).await
}

async fn get_recommendations(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let services = DiscoveryServices::for_db(state.db.clone());
    recommendations_with(state, caller, args, &services).await
}

/// `get_listening_history` running against explicit `services` (offline seam for tests).
pub(crate) async fn listening_history_with(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
    services: &DiscoveryServices,
) -> CallToolResult {
    let args = Args::new(GET_LISTENING_HISTORY.name, args, LISTENING_HISTORY_ARGS);
    answer(
        async {
            let limit = args
                .opt_u32("limit", 1, MAX_LIMIT as u32)?
                .map_or(DEFAULT_LIMIT, |n| n as usize);
            let include_listenbrainz = args.opt_bool("include_listenbrainz")?.unwrap_or(true);
            args.finish()?;
            let report = insights::listening_history_in(
                &state.db,
                state.navidrome.as_ref(),
                services,
                &caller.ctx,
                state.navidrome_music_path.as_deref(),
                limit,
                include_listenbrainz,
            )
            .await
            .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            Ok(history_fields(report))
        }
        .await,
    )
}

/// `get_recommendations` running against explicit `services` (offline seam for tests).
pub(crate) async fn recommendations_with(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
    services: &DiscoveryServices,
) -> CallToolResult {
    let args = Args::new(GET_RECOMMENDATIONS.name, args, RECOMMENDATIONS_ARGS);
    answer(
        async {
            let profile = match args.opt_choice("profile", PROFILES)? {
                Some(name) => name
                    .parse::<DiscoveryProfile>()
                    .expect("opt_choice returns a known DiscoveryProfile name"),
                None => DiscoveryProfile::Balanced,
            };
            let limit = args
                .opt_u32("limit", 1, MAX_LIMIT as u32)?
                .map_or(DEFAULT_LIMIT, |n| n as usize);
            args.finish()?;
            let preview = insights::recommendations_preview_in(
                &state.db,
                state.navidrome.as_ref(),
                services,
                &caller.ctx.id(),
                profile,
                limit,
            )
            .await
            .map_err(|f| upstream_error(f, Map::new(), args.same_call()))?;
            Ok(preview_fields(preview))
        }
        .await,
    )
}

/// Serializes `report` and trims list tails if needed to stay within `RESULT_MAX_CHARS`.
fn history_fields(report: ListeningHistoryReport) -> Map<String, Value> {
    let Value::Object(mut fields) = json!(report) else {
        unreachable!("ListeningHistoryReport serializes as a JSON object");
    };
    for list in ["recent_plays", "top_tracks", "top_artists", "top_genres"] {
        let sent = length(&fields) + r#""ok":true,"#.len();
        if sent <= RESULT_MAX_CHARS {
            break;
        }
        leave_out(&mut fields, list, Keep::First, sent - RESULT_MAX_CHARS);
    }
    fields
}

/// Serializes `preview` and trims candidate tails if needed to stay within `RESULT_MAX_CHARS`.
fn preview_fields(preview: RecommendationPreview) -> Map<String, Value> {
    let Value::Object(mut fields) = json!(preview) else {
        unreachable!("RecommendationPreview serializes as a JSON object");
    };
    let sent = length(&fields) + r#""ok":true,"#.len();
    leave_out(
        &mut fields,
        "candidates",
        Keep::First,
        sent.saturating_sub(RESULT_MAX_CHARS),
    );
    fields
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{from_str, json, Value};
    use soulbeet::knowledge::KnowledgeHosts;
    use soulbeet::polite::Pace;
    use soulbeet::traits::MusicBrainzLookup;

    use super::*;
    use crate::base_url::BaseUrl;
    use crate::connection::test_util::text_of;
    use crate::core::test_ctx;
    use crate::db::test_db;
    use crate::models::knowledge_cache::SqliteKnowledgeCache;
    use crate::models::user_settings::{UpdateUserSettings, UserSettings};
    use crate::test_navidrome::{subsonic_ok, NavidromeFor, NavidromeStub};

    const UNREACHED: &str = "http://127.0.0.1:9";
    const SH: &str = "1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
    const VN: &str = "2e3f4a5b-6c7d-4e8f-9a0b-1c2d3e4f5a6b";
    const REC1: &str = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";

    struct StubMusicBrainz;

    #[async_trait::async_trait]
    impl MusicBrainzLookup for StubMusicBrainz {
        async fn search_artists(
            &self,
            _: &str,
            _: u8,
        ) -> Result<Vec<soulbeet::musicbrainz::ArtistMatch>, soulbeet::MetadataError> {
            Ok(Vec::new())
        }

        async fn genre_names(&self) -> Result<Vec<String>, soulbeet::MetadataError> {
            Ok(vec!["dream pop".into(), "shoegaze".into()])
        }
    }

    fn caller() -> McpCaller {
        McpCaller {
            ctx: test_ctx("alice"),
            grant_id: "g1".into(),
            allow_delete: false,
            base: BaseUrl::parse("https://music.example.com").unwrap(),
        }
    }

    fn services_at(db: &sqlx::SqlitePool, lb: &str, labs: &str) -> DiscoveryServices {
        let pace = Pace {
            min_interval: Duration::ZERO,
            backoff: Duration::ZERO,
            retries: 0,
            timeout: Duration::from_secs(5),
        };
        DiscoveryServices {
            hosts: KnowledgeHosts {
                listenbrainz: lb.into(),
                labs: labs.into(),
                deezer: UNREACHED.into(),
            },
            cache: Arc::new(SqliteKnowledgeCache::new(db.clone())),
            musicbrainz: Arc::new(StubMusicBrainz),
            metabrainz_pace: pace,
            deezer_pace: pace,
        }
    }

    fn obj(v: Value) -> Option<JsonObject> {
        v.as_object().cloned()
    }

    fn doc_of(result: &CallToolResult) -> Value {
        from_str(text_of(result)).unwrap()
    }

    #[test]
    fn specs_match_mcp_contract_invariants() {
        for spec in [&GET_LISTENING_HISTORY, &GET_RECOMMENDATIONS] {
            assert!(spec.name.len() <= 64, "{}", spec.name);
            assert!(
                (spec.description)(120).contains("Failure codes:"),
                "{}",
                spec.name
            );
            assert!(
                (spec.description)(120).contains(
                    "rate_limited (more than 120 tool calls in a minute; retry_after_seconds says when)"
                ),
                "{}",
                spec.name
            );
            let schema: Value = from_str(spec.input_schema).unwrap();
            assert_eq!(schema["additionalProperties"], false, "{}", spec.name);
            assert!(spec.read_only && !spec.destructive && spec.open_world);
        }
        const { assert!(GET_LISTENING_HISTORY.idempotent) };
        const { assert!(!GET_RECOMMENDATIONS.idempotent) };
    }

    #[tokio::test]
    async fn listening_history_tool_validates_args_and_answers_json() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        UserSettings::upsert_in(
            &db,
            "alice",
            UpdateUserSettings {
                listenbrainz_username: Some("alice_lb".into()),
                listenbrainz_token: Some("do-not-leak".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let navi = NavidromeStub::start();
        let song = json!([{
            "id": "s1",
            "path": "alice/a.flac",
            "title": "Northern Line",
            "size": 1000,
            "libraryId": 1,
            "libraryPath": "/music",
            "missing": false,
            "updatedAt": "2026-11-15T10:00:00Z",
            "artist": "Static Harbor",
            "album": "Winter Songs",
            "albumArtist": "Static Harbor",
            "playCount": 6,
            "playDate": "2026-11-19T15:00:00Z",
            "rating": 5,
            "starred": true
        }]);
        navi.answer("/api/song", &[&song.to_string()]);
        navi.header("/api/song", "X-Total-Count", "1");
        navi.answer(
            "/rest/search3",
            &[&subsonic_ok(
                r#""searchResult3":{"song":[{"id":"s1","title":"Northern Line","genre":"Dream Pop"}]}"#,
            )],
        );

        let state = ConnectionState {
            navidrome: Arc::new(NavidromeFor(Ok(navi.client()))),
            ..ConnectionState::for_test(db.clone())
        };
        let services = services_at(&db, UNREACHED, UNREACHED);
        let c = caller();

        // Invalid limit refused before reaching Navidrome
        let bad = listening_history_with(&state, &c, obj(json!({"limit": 0})), &services).await;
        assert_eq!(doc_of(&bad)["error"]["code"], "invalid_input");

        // Unknown argument refused
        let extra =
            listening_history_with(&state, &c, obj(json!({"zzz": 1})), &services).await;
        assert_eq!(doc_of(&extra)["error"]["code"], "invalid_input");

        // Valid call with include_listenbrainz: false
        let res = listening_history_with(
            &state,
            &c,
            obj(json!({"limit": 5, "include_listenbrainz": false})),
            &services,
        )
        .await;
        let doc = doc_of(&res);
        assert_eq!(doc["ok"], true);
        assert_eq!(doc["summary"]["total_plays"], 6);
        assert_eq!(doc["summary"]["listenbrainz_username"], "alice_lb");
        assert_eq!(doc["recent_plays"][0]["song_id"], "s1");
        assert_eq!(doc["top_genres"][0]["genre"], "Dream Pop");
        assert!(doc["listenbrainz"].is_null());
        assert!(!doc.to_string().contains("do-not-leak"));
    }

    #[tokio::test]
    async fn recommendations_tool_previews_offline_and_validates_profile() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;

        let navi = NavidromeStub::start();
        let song = json!([{
            "id": "s1",
            "path": "alice/a.flac",
            "title": "Northern Line",
            "size": 1000,
            "libraryId": 1,
            "libraryPath": "/music",
            "missing": false,
            "updatedAt": "2026-11-15T10:00:00Z",
            "artist": "Static Harbor",
            "album": "Winter Songs",
            "albumArtist": "Static Harbor",
            "playCount": 6,
            "playDate": "2026-11-19T15:00:00Z",
            "rating": 5,
            "starred": true,
            "participants": {
                "artist": [{"id": "ar-sh", "name": "Static Harbor"}],
                "albumartist": [{"id": "ar-sh", "name": "Static Harbor"}]
            }
        }]);
        navi.answer("/api/song", &[&song.to_string()]);
        navi.header("/api/song", "X-Total-Count", "1");
        navi.answer(
            "/api/artist",
            &[&json!([{"id": "ar-sh", "name": "Static Harbor", "mbzArtistId": SH}]).to_string()],
        );

        let lb = NavidromeStub::start();
        lb.answer_when(
            "/1/metadata/artist/",
            SH,
            200,
            &json!([{"artist_mbid": SH, "tag": {"artist": [{"tag": "dream pop", "count": 10}]}}])
                .to_string(),
        );
        lb.answer_when(
            "/1/metadata/artist/",
            VN,
            200,
            &json!([{"artist_mbid": VN, "tag": {"artist": [{"tag": "shoegaze", "count": 8}]}}])
                .to_string(),
        );
        lb.answer(
            "/1/popularity/artist",
            &[&json!([
                {"artist_mbid": SH, "total_user_count": 500},
                {"artist_mbid": VN, "total_user_count": 300}
            ])
            .to_string()],
        );
        lb.answer(
            "/1/stats/sitewide/artists",
            &[&json!({"payload": {"artists": [{"artist_mbid": SH, "artist_name": "Static Harbor"}]}})
                .to_string()],
        );
        lb.answer(
            &format!("/1/lb-radio/artist/{VN}"),
            &[&json!({VN: [{"recording_mbid": REC1, "total_listen_count": 420}]}).to_string()],
        );
        lb.answer(
            "/1/metadata/recording/",
            &[&json!({
                REC1: {
                    "artist": {"name": "Vela Norte", "artists": [{"artist_mbid": VN}]},
                    "recording": {"name": "Foglight", "first_release_date": "2025-05-01"}
                }
            })
            .to_string()],
        );

        let labs = NavidromeStub::start();
        labs.answer("/tag-similarity/json", &["[]"]);
        labs.answer_when(
            "/similar-artists/json",
            SH,
            200,
            &json!([{"artist_mbid": VN, "name": "Vela Norte", "score": 1000}]).to_string(),
        );
        labs.answer_when("/similar-artists/json", VN, 200, "[]");

        let state = ConnectionState {
            navidrome: Arc::new(NavidromeFor(Ok(navi.client()))),
            ..ConnectionState::for_test(db.clone())
        };
        let services = services_at(&db, &lb.url, &labs.url);
        let c = caller();

        // Invalid profile refused
        let bad = recommendations_with(
            &state,
            &c,
            obj(json!({"profile": "Wild"})),
            &services,
        )
        .await;
        assert_eq!(doc_of(&bad)["error"]["code"], "invalid_input");

        // Valid preview
        let res = recommendations_with(
            &state,
            &c,
            obj(json!({"profile": "Adventurous", "limit": 5})),
            &services,
        )
        .await;
        let doc = doc_of(&res);
        assert_eq!(doc["ok"], true);
        assert_eq!(doc["profile"], "Adventurous");
        assert_eq!(doc["seed_artists"][0]["name"], "Static Harbor");
        assert_eq!(doc["candidates"][0]["artist"], "Vela Norte");
        assert_eq!(doc["candidates"][0]["title"], "Foglight");
        assert_eq!(doc["candidates"][0]["recording_mbid"], REC1);
    }
}
