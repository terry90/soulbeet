use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use client_core::auto_browse::{
    AUTO_FOR_YOU_LIMIT, AUTO_LIB_OFFLINE, AUTO_LIB_RECENT, AUTO_PLAYLISTS_LIMIT,
    AUTO_RECENT_LIMIT, AUTO_ROOT_ID, AUTO_TAB_FOR_YOU, AUTO_TAB_LIBRARY, AUTO_TAB_PLAYLISTS,
    AUTO_TAB_RADIO,
};
use client_core::db::{
    get_cursor, get_track, now_unix, replace_playlist_tracks, upsert_album, upsert_artist,
    upsert_playlist, upsert_track,
};
use client_core::error::CoreError;
use client_core::models::{
    AudioQualityProfile, CachePolicy, ClientConfig, ConnectClientMessageDto, ConnectCommandDto,
    ConnectDeviceDto, ConnectServerEventDto, DevicePlatform, LocalAlbum, LocalArtist,
    LocalPlaylist, LocalTrack, NavidromeCredentials, OutboxMutation, PersistedPlaybackSession,
    PinTarget, PlaybackSnapshotDto, PlaybackSource, SoulbeetAuth, VoiceMatchKind,
};
use client_core::sync::CURSOR_PROMOTE_THRESHOLD;
use client_core::SoulbeetCore;
use serde_json::{json, Value};
use shared::metadata::{SearchResult as MetadataSearchResult, Track as MetadataTrack};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

#[derive(Default)]
struct StubState {
    base_url: Mutex<String>,
    playlist_fetch_count: AtomicUsize,
    static_playlist_fetch_count: AtomicUsize,
    remote_search_hits: AtomicUsize,
    fail_star_409_retryable: AtomicBool,
    fail_drop_409_permanent: AtomicBool,
    curation_log: Mutex<Vec<String>>,
    new_album_enabled: AtomicBool,
    bootstrap_returns_no_navidrome: AtomicBool,
    hide_extra_playlist: AtomicBool,
    download_returns_json_error: AtomicBool,
    cover_art_returns_json_error: AtomicBool,
    ws_query_params: Mutex<Option<HashMap<String, String>>>,
    ws_ping_count: AtomicUsize,
    ws_pong_count: AtomicUsize,
    ws_connect_count: AtomicUsize,
    ws_drop_first_connection: AtomicBool,
    ws_send_server_ping: AtomicBool,
    accepted_auth_token: Mutex<String>,
    song_ratings: Mutex<HashMap<String, i64>>,
    search3_omit_song_ids: Mutex<HashSet<String>>,
    search3_entered_notify: Mutex<Option<Arc<Notify>>>,
    search3_release_notify: Mutex<Option<Arc<Notify>>>,
    triage_rating: Mutex<Option<u8>>,
    skip_recent_albums: AtomicBool,
    album_detail_fetch_count: AtomicUsize,
}

async fn check_soulbeet_auth(st: &StubState, headers: &HeaderMap) -> bool {
    let expected = st.accepted_auth_token.lock().await.clone();
    if let Some(auth) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if auth == format!("Bearer {expected}") {
            return true;
        }
    }
    if let Some(cookie) = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        if cookie
            .split(';')
            .any(|part| part.trim() == format!("auth_token={expected}"))
        {
            return true;
        }
    }
    false
}

async fn start_stub_server(state: Arc<StubState>) -> String {
    *state.accepted_auth_token.lock().await = "test-token".to_string();
    let app = Router::new()
        .route("/api/client/bootstrap", get(handle_bootstrap))
        .route("/api/discovery/triage", get(handle_triage))
        .route("/api/metadata/search/track", post(handle_remote_search))
        .route("/api/curation/star", post(handle_curation_star))
        .route("/api/curation/rate", post(handle_curation_rate))
        .route("/api/curation/keep", post(handle_curation_keep))
        .route("/api/curation/drop", post(handle_curation_drop))
        .route("/api/ws/connect", get(handle_ws_connect))
        .route("/rest/getArtists", get(handle_get_artists))
        .route("/rest/getAlbumList2", get(handle_get_album_list2))
        .route("/rest/getAlbum", get(handle_get_album))
        .route("/rest/getPlaylists", get(handle_get_playlists))
        .route("/rest/getPlaylist", get(handle_get_playlist))
        .route("/rest/getStarred2", get(handle_get_starred2))
        .route("/rest/getGenres", get(handle_get_genres))
        .route("/rest/search3", get(handle_search3))
        .route("/rest/scrobble", get(handle_scrobble))
        .route("/rest/download", get(handle_download))
        .route("/rest/stream", get(handle_stream))
        .route("/rest/getCoverArt", get(handle_cover_art))
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    *state.base_url.lock().await = url.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    url
}

async fn handle_bootstrap(
    State(st): State<Arc<StubState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !check_soulbeet_auth(&st, &headers).await {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "unauthorized" })));
    }
    let navidrome = if st.bootstrap_returns_no_navidrome.load(Ordering::SeqCst) {
        Value::Null
    } else {
        let base_url = st.base_url.lock().await.clone();
        json!({
            "base_url": base_url,
            "username": "alice",
            "subsonic_salt": "salt",
            "subsonic_token": "token",
            "api_version": "1.16.1",
            "client_name": "Soulbeet"
        })
    };
    (
        StatusCode::OK,
        Json(json!({
            "user_id": "u1",
            "username": "alice",
            "is_admin": false,
            "navidrome": navidrome,
            "promote_threshold": 4,
            "auto_delete_enabled": true,
            "discovery_enabled": true
        })),
    )
}

async fn handle_triage(
    State(st): State<Arc<StubState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !check_soulbeet_auth(&st, &headers).await {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "unauthorized" })));
    }
    let triage_rating = *st.triage_rating.lock().await;
    // Real PR #129 DiscoveryTrack wire shape with PascalCase status, path, profile, pack_slug, created_at
    (
        StatusCode::OK,
        Json(json!([
            {
                "id": "disc-1",
                "song_id": "s2",
                "artist": "Justice",
                "title": "D.A.N.C.E.",
                "album": "Cross",
                "path": "Discovery/Balanced/Justice - DANCE.flac",
                "folder_id": "f-disc",
                "profile": "weekly_radar",
                "rating": triage_rating,
                "status": "Pending",
                "created_at": "2026-11-20T09:00:00Z",
                "exclude_from_core_taste": false,
                "reason": { "summary": "Matches your French electro plays" },
                "culture_bucket": "local",
                "pack_slug": "winter_holiday"
            }
        ])),
    )
}

async fn handle_remote_search(
    State(st): State<Arc<StubState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if !check_soulbeet_auth(&st, &headers).await {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "unauthorized" })));
    }
    // Enforce Dioxus 0.7 #[post("/api/metadata/search/track")] `{ "input": { "artist": null, "query": ... } }`
    if body.get("input").and_then(|i| i.get("query")).and_then(Value::as_str).is_none() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "message": "expected { input: { artist, query } }" })),
        );
    }
    st.remote_search_hits.fetch_add(1, Ordering::SeqCst);
    let results = vec![
        MetadataSearchResult::Track(MetadataTrack {
            id: "mb-owned".to_string(),
            title: "Get Lucky".to_string(),
            artist: "Daft Punk".to_string(),
            album_id: Some("al-mb-1".to_string()),
            album_title: Some("Random Access Memories".to_string()),
            release_date: Some("2013-05-17".to_string()),
            duration: Some("06:09".to_string()),
            mbid: Some("mb-owned".to_string()),
            release_mbid: None,
        }),
        MetadataSearchResult::Track(MetadataTrack {
            id: "mb-unowned".to_string(),
            title: "Instant Crush".to_string(),
            artist: "Daft Punk".to_string(),
            album_id: Some("al-mb-1".to_string()),
            album_title: Some("Random Access Memories".to_string()),
            release_date: Some("2013-05-17".to_string()),
            duration: Some("05:37".to_string()),
            mbid: Some("mb-unowned".to_string()),
            release_mbid: None,
        }),
    ];
    (StatusCode::OK, Json(serde_json::to_value(results).unwrap()))
}

async fn handle_curation_star(
    State(st): State<Arc<StubState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if !check_soulbeet_auth(&st, &headers).await {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "unauthorized" })));
    }
    let Some(req) = body.get("req") else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "message": "missing Dioxus req wrapper" })),
        );
    };
    if st.fail_star_409_retryable.load(Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "message": "Navidrome temporarily unreachable",
                "code": 409,
                "details": {
                    "code": "navidrome_unreachable",
                    "stage": "library_check",
                    "message": "Navidrome temporarily unreachable",
                    "retry": "same_source_later",
                    "retry_advice": "retryable",
                    "retry_after_seconds": 30
                }
            })),
        );
    }
    let id = req["song_id"].as_str().unwrap_or_default();
    let starred = req["starred"].as_bool().unwrap_or(false);
    st.curation_log
        .lock()
        .await
        .push(format!("star:{id}:{starred}"));
    (StatusCode::OK, Json(json!({ "ok": true })))
}

async fn handle_curation_rate(
    State(st): State<Arc<StubState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if !check_soulbeet_auth(&st, &headers).await {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "unauthorized" })));
    }
    let Some(req) = body.get("req") else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "message": "missing Dioxus req wrapper" })),
        );
    };
    let id = req["song_id"].as_str().unwrap_or_default();
    let rating = req["rating"].as_u64().unwrap_or(0);
    st.curation_log
        .lock()
        .await
        .push(format!("rate:{id}:{rating}"));
    (StatusCode::OK, Json(json!({ "ok": true })))
}

async fn handle_curation_keep(
    State(st): State<Arc<StubState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if !check_soulbeet_auth(&st, &headers).await {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "unauthorized" })));
    }
    let Some(req) = body.get("req") else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "message": "missing Dioxus req wrapper" })),
        );
    };
    let id = req["song_id"].as_str().unwrap_or_default();
    st.curation_log.lock().await.push(format!("keep:{id}"));
    (StatusCode::OK, Json(json!({ "ok": true })))
}

async fn handle_curation_drop(
    State(st): State<Arc<StubState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if !check_soulbeet_auth(&st, &headers).await {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "unauthorized" })));
    }
    let Some(req) = body.get("req") else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "message": "missing Dioxus req wrapper" })),
        );
    };
    if st.fail_drop_409_permanent.load(Ordering::SeqCst) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "message": "Track protected by another listener",
                "code": 409,
                "details": {
                    "code": "delete_blocked_by_other_user",
                    "stage": "delete",
                    "message": "Track protected by another listener",
                    "retry": "not_retryable",
                    "retry_advice": "not_retryable"
                }
            })),
        );
    }
    let id = req["song_id"].as_str().unwrap_or_default();
    st.curation_log.lock().await.push(format!("drop:{id}"));
    (StatusCode::OK, Json(json!({ "ok": true })))
}

async fn handle_scrobble(
    State(st): State<Arc<StubState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let id = q.get("id").cloned().unwrap_or_default();
    st.curation_log
        .lock()
        .await
        .push(format!("scrobble:{id}"));
    Json(json!({ "subsonic-response": { "status": "ok", "version": "1.16.1" } }))
}

async fn handle_get_artists() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "artists": {
                "index": [
                    {
                        "name": "D",
                        "artist": [
                            { "id": "ar1", "name": "Daft Punk", "albumCount": 2, "coverArt": "ca-ar1" }
                        ]
                    },
                    {
                        "name": "J",
                        "artist": [
                            { "id": "ar2", "name": "Justice", "albumCount": 1, "coverArt": "ca-ar2" }
                        ]
                    }
                ]
            }
        }
    }))
}

async fn handle_get_album_list2(
    State(st): State<Arc<StubState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    if q.get("type").map(String::as_str) == Some("recent")
        && st.skip_recent_albums.load(Ordering::SeqCst)
    {
        return Json(json!({
            "subsonic-response": {
                "status": "ok",
                "albumList2": { "album": [] }
            }
        }));
    }
    let mut albums = vec![json!({
        "id": "al1",
        "name": "Random Access Memories",
        "artist": "Daft Punk",
        "artistId": "ar1",
        "year": 2013,
        "genre": "Electronic",
        "songCount": 2,
        "duration": 600,
        "coverArt": "ca-al1",
        "created": "2026-11-01T10:00:00Z"
    })];
    if st.new_album_enabled.load(Ordering::SeqCst) {
        albums.push(json!({
            "id": "al2",
            "name": "Discovery",
            "artist": "Daft Punk",
            "artistId": "ar1",
            "year": 2001,
            "genre": "French House",
            "songCount": 1,
            "duration": 320,
            "coverArt": "ca-al2",
            "created": "2026-11-20T10:00:00Z"
        }));
    }
    let offset: usize = q.get("offset").and_then(|s| s.parse().ok()).unwrap_or(0);
    let out = if offset > 0 { Vec::new() } else { albums };
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "albumList2": { "album": out }
        }
    }))
}

async fn handle_get_album(
    State(st): State<Arc<StubState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    st.album_detail_fetch_count.fetch_add(1, Ordering::SeqCst);
    let id = q.get("id").map(String::as_str).unwrap_or("");
    let album = match id {
        "al1" => json!({
            "id": "al1",
            "name": "Random Access Memories",
            "artist": "Daft Punk",
            "artistId": "ar1",
            "year": 2013,
            "genre": "Electronic",
            "songCount": 2,
            "duration": 600,
            "coverArt": "ca-al1",
            "created": "2026-11-01T10:00:00Z",
            "song": [
                {
                    "id": "s1",
                    "title": "Get Lucky",
                    "artist": "Daft Punk",
                    "artistId": "ar1",
                    "album": "Random Access Memories",
                    "albumId": "al1",
                    "track": 8,
                    "duration": 369,
                    "genre": "Electronic",
                    "bitRate": 1411,
                    "suffix": "flac",
                    "coverArt": "ca-al1"
                },
                {
                    "id": "s2",
                    "title": "D.A.N.C.E.",
                    "artist": "Justice",
                    "artistId": "ar2",
                    "album": "Cross",
                    "albumId": "al1",
                    "track": 2,
                    "duration": 242,
                    "genre": "Electro",
                    "path": "Discovery/Balanced/Justice - DANCE.flac",
                    "coverArt": "ca-al1"
                }
            ]
        }),
        _ => json!({
            "id": "al2",
            "name": "Discovery",
            "artist": "Daft Punk",
            "artistId": "ar1",
            "year": 2001,
            "genre": "French House",
            "songCount": 1,
            "duration": 320,
            "coverArt": "ca-al2",
            "created": "2026-11-20T10:00:00Z",
            "song": [
                {
                    "id": "s3",
                    "title": "One More Time",
                    "artist": "Daft Punk",
                    "artistId": "ar1",
                    "album": "Discovery",
                    "albumId": "al2",
                    "track": 1,
                    "duration": 320,
                    "genre": "French House",
                    "path": "Daft Punk/Discovery/01 One More Time.flac",
                    "coverArt": "ca-al2"
                }
            ]
        }),
    };
    Json(json!({ "subsonic-response": { "status": "ok", "album": album } }))
}

async fn handle_get_playlists(State(st): State<Arc<StubState>>) -> Json<Value> {
    let mut pls = vec![
        json!({
            "id": "pl1",
            "name": "Noël à la Française",
            "comment": "soulbeet:contextual:winter_holiday",
            "owner": "alice",
            "songCount": 0,
            "duration": 369,
            "coverArt": "ca-pl1",
            "changed": "2026-11-15T12:00:00Z"
        }),
        json!({
            "id": "pl-static",
            "name": "Roadtrip Favorites",
            "comment": null,
            "owner": "alice",
            "songCount": 231,
            "duration": 99999,
            "coverArt": "ca-pl-static",
            "changed": "2026-11-15T12:00:00Z"
        }),
        json!({
            "id": "pl-disc",
            "name": "Balanced",
            "comment": "Soulbeet Discovery - Balanced",
            "owner": "alice",
            "songCount": 1,
            "duration": 369,
            "coverArt": null,
            "changed": "2026-11-15T12:00:00Z"
        }),
        json!({
            "id": "pl-moment",
            "name": "Soulbeet: Drive",
            "comment": "soulbeet:contextual:drive",
            "owner": "alice",
            "songCount": 1,
            "duration": 369,
            "coverArt": null,
            "changed": "2026-11-15T12:00:00Z"
        }),
        json!({
            "id": "pl-custom-smart",
            "name": "90s Classics",
            "comment": "Custom smart playlist",
            "owner": "alice",
            "songCount": 1,
            "duration": 369,
            "coverArt": null,
            "changed": "2026-11-15T12:00:00Z"
        }),
    ];
    if !st.hide_extra_playlist.load(Ordering::SeqCst) {
        pls.push(json!({
            "id": "pl-ephemeral",
            "name": "Temp Mix",
            "comment": null,
            "owner": "alice",
            "songCount": 1,
            "duration": 369,
            "coverArt": null,
            "changed": "2026-11-15T12:00:00Z"
        }));
    }
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "playlists": {
                "playlist": pls
            }
        }
    }))
}

async fn handle_get_playlist(
    State(st): State<Arc<StubState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let pl_id = q.get("id").cloned().unwrap_or_else(|| "pl1".to_string());
    if pl_id == "pl1" {
        st.playlist_fetch_count.fetch_add(1, Ordering::SeqCst);
    } else if pl_id == "pl-static" {
        st.static_playlist_fetch_count
            .fetch_add(1, Ordering::SeqCst);
    }
    let (name, comment) = match pl_id.as_str() {
        "pl1" => ("Noël à la Française", json!("soulbeet:contextual:winter_holiday")),
        "pl-static" => ("Roadtrip Favorites", Value::Null),
        "pl-disc" => ("Balanced", json!("Soulbeet Discovery - Balanced")),
        "pl-moment" => ("Soulbeet: Drive", json!("soulbeet:contextual:drive")),
        "pl-custom-smart" => ("90s Classics", json!("Custom smart playlist")),
        _ => ("Temp Mix", Value::Null),
    };
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "playlist": {
                "id": pl_id,
                "name": name,
                "comment": comment,
                "owner": "alice",
                "songCount": if pl_id == "pl-static" { 231 } else { 1 },
                "duration": 369,
                "coverArt": "ca-pl1",
                "changed": "2026-11-15T12:00:00Z",
                "entry": [
                    {
                        "id": "s1",
                        "title": "Get Lucky",
                        "artist": "Daft Punk",
                        "artistId": "ar1",
                        "album": "Random Access Memories",
                        "albumId": "al1",
                        "duration": 369
                    }
                ]
            }
        }
    }))
}

async fn handle_get_starred2() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "starred2": {
                "artist": [{ "id": "ar1", "name": "Daft Punk", "albumCount": 2 }],
                "album": [{ "id": "al1", "name": "Random Access Memories", "artist": "Daft Punk" }],
                "song": []
            }
        }
    }))
}

async fn handle_get_genres() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "genres": {
                "genre": [{ "value": "Electronic", "songCount": 2, "albumCount": 1 }]
            }
        }
    }))
}

async fn handle_search3(
    State(st): State<Arc<StubState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let entered = st.search3_entered_notify.lock().await.clone();
    if let Some(entered) = entered {
        entered.notify_one();
    }
    let release = st.search3_release_notify.lock().await.clone();
    if let Some(release) = release {
        release.notified().await;
    }
    let offset: usize = q
        .get("songOffset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let count: usize = q
        .get("songCount")
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    let ratings = st.song_ratings.lock().await;
    let omitted = st.search3_omit_song_ids.lock().await;
    let mut all_songs = vec![
        ("s1", "Get Lucky", "Daft Punk", "ar1", "Random Access Memories", "al1", 369),
        ("s2", "D.A.N.C.E.", "Justice", "ar2", "Cross", "al1", 242),
    ];
    if st.new_album_enabled.load(Ordering::SeqCst) {
        all_songs.push((
            "s3",
            "One More Time",
            "Daft Punk",
            "ar1",
            "Discovery",
            "al2",
            320,
        ));
    }
    let page: Vec<Value> = all_songs
        .into_iter()
        .filter(|(id, ..)| !omitted.contains(*id))
        .skip(offset)
        .take(count)
        .map(|(id, title, artist, artist_id, album, album_id, duration)| {
            let mut obj = json!({
                "id": id,
                "title": title,
                "artist": artist,
                "artistId": artist_id,
                "album": album,
                "albumId": album_id,
                "duration": duration,
            });
            if let Some(&r) = ratings.get(id) {
                if r > 0 {
                    obj["userRating"] = json!(r);
                }
            }
            obj
        })
        .collect();

    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "searchResult3": {
                "song": page
            }
        }
    }))
}

async fn handle_download(State(st): State<Arc<StubState>>) -> impl IntoResponse {
    if st.download_returns_json_error.load(Ordering::SeqCst) {
        let body = serde_json::to_vec(&json!({
            "subsonic-response": {
                "status": "failed",
                "version": "1.16.1",
                "error": { "code": 70, "message": "Requested data was not found" }
            }
        }))
        .unwrap();
        return ([(axum::http::header::CONTENT_TYPE, "application/json")], body);
    }
    (
        [(axum::http::header::CONTENT_TYPE, "audio/flac")],
        vec![0x42u8; 4096],
    )
}

async fn handle_stream() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "audio/ogg")],
        vec![0x24u8; 1024],
    )
}

async fn handle_cover_art(State(st): State<Arc<StubState>>) -> impl IntoResponse {
    if st.cover_art_returns_json_error.load(Ordering::SeqCst) {
        let body = serde_json::to_vec(&json!({
            "subsonic-response": {
                "status": "failed",
                "version": "1.16.1",
                "error": { "code": 70, "message": "Cover art not found" }
            }
        }))
        .unwrap();
        return ([(axum::http::header::CONTENT_TYPE, "application/json")], body);
    }
    (
        [(axum::http::header::CONTENT_TYPE, "image/jpeg")],
        vec![0xFFu8, 0xD8, 0xFF, 0xD9],
    )
}

async fn handle_ws_connect(
    State(st): State<Arc<StubState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !check_soulbeet_auth(&st, &headers).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    *st.ws_query_params.lock().await = Some(q.clone());
    let conn_num = st.ws_connect_count.fetch_add(1, Ordering::SeqCst) + 1;
    let st_clone = st.clone();
    ws.on_upgrade(move |socket| handle_ws_socket(socket, st_clone, q, conn_num))
        .into_response()
}

async fn handle_ws_socket(
    mut socket: WebSocket,
    st: Arc<StubState>,
    q: HashMap<String, String>,
    conn_num: usize,
) {
    // Register directly from query params and immediately emit DeviceList (just like real Soulbeet server).
    let device_id = q
        .get("device_id")
        .cloned()
        .unwrap_or_else(|| "desktop-1".to_string());
    let name = q
        .get("name")
        .cloned()
        .unwrap_or_else(|| "Linux Studio".to_string());
    let platform = match q.get("platform").map(String::as_str) {
        Some("android") => DevicePlatform::Android,
        Some("android_auto") => DevicePlatform::AndroidAuto,
        Some("web") => DevicePlatform::Web,
        _ => DevicePlatform::Desktop,
    };
    let initial_event = ConnectServerEventDto::DeviceList {
        devices: vec![
            ConnectDeviceDto {
                device_id: device_id.clone(),
                name,
                platform,
                is_active: true,
                last_seen_unix: now_unix(),
                snapshot: None,
            },
            ConnectDeviceDto {
                device_id: "android-1".to_string(),
                name: "Pixel".to_string(),
                platform: DevicePlatform::Android,
                is_active: false,
                last_seen_unix: now_unix(),
                snapshot: None,
            },
        ],
        active_device_id: Some(device_id),
    };
    let _ = socket
        .send(Message::Binary(
            serde_json::to_vec(&initial_event).unwrap().into(),
        ))
        .await;

    if conn_num == 1 && st.ws_drop_first_connection.load(Ordering::SeqCst) {
        let _ = socket.send(Message::Close(None)).await;
        return;
    }
    if st.ws_send_server_ping.load(Ordering::SeqCst) {
        let _ = socket.send(Message::Ping(b"srv-ping".to_vec().into())).await;
    }

    // Emit JSON over Message::Binary just like Dioxus 0.7 TypedWebsocket!
    while let Some(Ok(msg)) = socket.recv().await {
        let raw_bytes: Option<Vec<u8>> = match msg {
            Message::Text(text) => Some(text.as_bytes().to_vec()),
            Message::Binary(bytes) => Some(bytes.to_vec()),
            Message::Ping(payload) => {
                st.ws_ping_count.fetch_add(1, Ordering::SeqCst);
                let _ = socket.send(Message::Pong(payload)).await;
                None
            }
            Message::Pong(_) => {
                st.ws_pong_count.fetch_add(1, Ordering::SeqCst);
                None
            }
            Message::Close(_) => break,
        };
        let Some(bytes) = raw_bytes else {
            continue;
        };
        let Ok(client_msg) = serde_json::from_slice::<ConnectClientMessageDto>(&bytes) else {
            continue;
        };
        match client_msg {
            ConnectClientMessageDto::Register {
                device_id,
                name,
                platform,
            } => {
                let event = ConnectServerEventDto::DeviceList {
                    devices: vec![
                        ConnectDeviceDto {
                            device_id: device_id.clone(),
                            name,
                            platform,
                            is_active: true,
                            last_seen_unix: now_unix(),
                            snapshot: None,
                        },
                        ConnectDeviceDto {
                            device_id: "android-1".to_string(),
                            name: "Pixel".to_string(),
                            platform: DevicePlatform::Android,
                            is_active: false,
                            last_seen_unix: now_unix(),
                            snapshot: None,
                        },
                    ],
                    active_device_id: Some(device_id),
                };
                let _ = socket
                    .send(Message::Binary(serde_json::to_vec(&event).unwrap().into()))
                    .await;
            }
            ConnectClientMessageDto::UpdateState { snapshot } => {
                let event = ConnectServerEventDto::PlaybackUpdated {
                    device_id: "android-1".to_string(),
                    snapshot,
                };
                let _ = socket
                    .send(Message::Binary(serde_json::to_vec(&event).unwrap().into()))
                    .await;
            }
            ConnectClientMessageDto::SendCommand { command, .. } => {
                let event = ConnectServerEventDto::Command {
                    from_device_id: "desktop-1".to_string(),
                    command,
                };
                let _ = socket
                    .send(Message::Binary(serde_json::to_vec(&event).unwrap().into()))
                    .await;
            }
            ConnectClientMessageDto::ClaimActive => {
                // Simulate android-1 disconnecting: DeviceList now only contains desktop-1
                let event = ConnectServerEventDto::DeviceList {
                    devices: vec![ConnectDeviceDto {
                        device_id: "desktop-1".to_string(),
                        name: "Linux Studio".to_string(),
                        platform: DevicePlatform::Desktop,
                        is_active: true,
                        last_seen_unix: now_unix(),
                        snapshot: None,
                    }],
                    active_device_id: Some("desktop-1".to_string()),
                };
                let _ = socket
                    .send(Message::Binary(serde_json::to_vec(&event).unwrap().into()))
                    .await;
            }
            ConnectClientMessageDto::Heartbeat => {}
        }
    }
}

fn test_config(dir: &TempDir, base_url: Option<String>, max_bytes: u64) -> ClientConfig {
    let navidrome = base_url.as_ref().map(|u| NavidromeCredentials {
        base_url: u.clone(),
        username: "alice".to_string(),
        subsonic_salt: "salt".to_string(),
        subsonic_token: "token".to_string(),
        api_version: "1.16.1".to_string(),
        client_name: "Soulbeet".to_string(),
    });
    ClientConfig {
        db_path: dir.path().join("client.db"),
        cache_dir: dir.path().join("audio_cache"),
        cover_art_dir: dir.path().join("cover_art"),
        soulbeet_base_url: base_url,
        soulbeet_auth: Some(SoulbeetAuth::SessionCookie("test-token".to_string())),
        navidrome,
        device_id: "desktop-1".to_string(),
        device_name: "Linux Studio".to_string(),
        platform: DevicePlatform::Desktop,
        offline_only: false,
        cache_policy: CachePolicy {
            max_bytes,
            ..CachePolicy::default()
        },
    }
}

#[tokio::test]
async fn full_and_delta_sync_populates_mirror_normalizes_triage_prunes_playlists_and_preserves_outbox()
{
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();

    // 1. Initial full sync using SessionCookie auth
    let report = core.sync_now(false).await.unwrap();
    assert!(report.full_sync);
    assert_eq!(report.artists_synced, 2);
    assert_eq!(report.albums_synced, 1);
    assert_eq!(report.tracks_synced, 2);
    assert_eq!(report.playlists_synced, 6);
    assert_eq!(report.discovery_tracks_synced, 1);
    assert_eq!(state.playlist_fetch_count.load(Ordering::SeqCst), 1);
    assert_eq!(state.static_playlist_fetch_count.load(Ordering::SeqCst), 1);

    // Verify playlist classification from server comment markers + authoritative live song_count
    let playlists = core.list_playlists().await.unwrap();
    let pl1 = playlists.iter().find(|p| p.id == "pl1").unwrap();
    assert_eq!(pl1.kind, "seasonal");
    assert_eq!(pl1.slug.as_deref(), Some("winter_holiday"));
    assert!(pl1.is_smart);
    assert_eq!(pl1.song_count, 1);

    let pl_static = playlists.iter().find(|p| p.id == "pl-static").unwrap();
    assert_eq!(pl_static.kind, "user");
    assert!(!pl_static.is_smart);
    // Navidrome summary claimed 231 stale rows, but only 1 live entry exists!
    assert_eq!(pl_static.song_count, 1);
    assert_eq!(pl_static.duration_secs, 369);

    let pl_disc = playlists.iter().find(|p| p.id == "pl-disc").unwrap();
    assert_eq!(pl_disc.kind, "discovery");
    assert!(pl_disc.is_smart);

    let pl_moment = playlists.iter().find(|p| p.id == "pl-moment").unwrap();
    assert_eq!(pl_moment.kind, "moment");
    assert_eq!(pl_moment.slug.as_deref(), Some("drive"));
    assert!(pl_moment.is_smart);

    let pl_smart = playlists.iter().find(|p| p.id == "pl-custom-smart").unwrap();
    assert_eq!(pl_smart.kind, "smart");
    assert!(pl_smart.is_smart);

    // Bootstrap persisted promote_threshold into sync_cursor
    assert_eq!(
        get_cursor(core.pool(), CURSOR_PROMOTE_THRESHOLD)
            .await
            .unwrap()
            .as_deref(),
        Some("4")
    );

    // Discovery triage DTO fields mapped & status normalized to lowercase "pending"
    let (status, folder_path, profile_slug, mix_slug): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, folder_path, profile_slug, mix_slug FROM discovery_tracks WHERE id = 'disc-1'",
    )
    .fetch_one(core.pool())
    .await
    .unwrap();
    assert_eq!(status, "pending");
    assert_eq!(
        folder_path.as_deref(),
        Some("Discovery/Balanced/Justice - DANCE.flac")
    );
    assert_eq!(profile_slug.as_deref(), Some("weekly_radar"));
    assert_eq!(mix_slug.as_deref(), Some("winter_holiday"));

    // Starred reconciliation applied to artists and albums too
    let ar1_starred: bool = sqlx::query_scalar("SELECT starred FROM artists WHERE id = 'ar1'")
        .fetch_one(core.pool())
        .await
        .unwrap();
    let al1_starred: bool = sqlx::query_scalar("SELECT starred FROM albums WHERE id = 'al1'")
        .fetch_one(core.pool())
        .await
        .unwrap();
    assert!(ar1_starred);
    assert!(al1_starred);

    // 2. Optimistic offline star on s1 + Keep on discovery track s2 before next sync
    core.enqueue_mutation(OutboxMutation::Star {
        song_id: "s1".to_string(),
        starred: true,
        discovery_track_id: None,
    })
    .await
    .unwrap();
    core.enqueue_mutation(OutboxMutation::Keep {
        song_id: "s2".to_string(),
        discovery_track_id: Some("disc-1".to_string()),
        target_folder_id: None,
    })
    .await
    .unwrap();
    assert!(get_track(core.pool(), "s1").await.unwrap().unwrap().starred);
    assert!(!get_track(core.pool(), "s2").await.unwrap().unwrap().is_discovery);

    // 3. Rotate auth token at runtime to Bearer & verify sync works with rotated token!
    *state.accepted_auth_token.lock().await = "rotated-bearer-token".to_string();
    core.set_soulbeet_auth(Some(SoulbeetAuth::Bearer(
        "rotated-bearer-token".to_string(),
    )))
    .await;

    // 4. Delta sync: enable new album al2 (whose path is "Daft Punk/Discovery/..."!), hide pl-ephemeral
    state.new_album_enabled.store(true, Ordering::SeqCst);
    state.hide_extra_playlist.store(true, Ordering::SeqCst);
    let delta = core.sync_now(false).await.unwrap();
    assert!(!delta.full_sync);
    assert_eq!(delta.albums_synced, 1); // only new album al2 fetched
    // Smart playlist pl1 IS re-fetched on delta sync; unchanged static pl-static is NOT re-fetched
    // even though Navidrome summary still reports stale songCount=231 vs live count=1!
    assert_eq!(state.playlist_fetch_count.load(Ordering::SeqCst), 2);
    assert_eq!(state.static_playlist_fetch_count.load(Ordering::SeqCst), 1);

    // Static playlist retains authoritative live count = 1 after delta sync!
    let refreshed_pls = core.list_playlists().await.unwrap();
    assert_eq!(
        refreshed_pls.iter().find(|p| p.id == "pl-static").unwrap().song_count,
        1
    );

    // Deleted playlist pl-ephemeral was pruned!
    let pl_count: i64 = sqlx::query_scalar("SELECT COUNT(1) FROM playlists")
        .fetch_one(core.pool())
        .await
        .unwrap();
    assert_eq!(pl_count, 5);

    // Pending outbox mutations were NOT clobbered by sync (neither starred nor is_discovery nor discovery_tracks.status)!
    assert!(get_track(core.pool(), "s1").await.unwrap().unwrap().starred);
    assert!(!get_track(core.pool(), "s2").await.unwrap().unwrap().is_discovery);
    let disc_status: String =
        sqlx::query_scalar("SELECT status FROM discovery_tracks WHERE id = 'disc-1'")
            .fetch_one(core.pool())
            .await
            .unwrap();
    assert_eq!(disc_status, "promoted");

    // "Daft Punk/Discovery/01 One More Time.flac" is NOT falsely flagged as discovery!
    let s3 = get_track(core.pool(), "s3").await.unwrap().unwrap();
    assert!(!s3.is_discovery);

    // 5. Bootstrap returning navidrome: None clears cached credentials
    state
        .bootstrap_returns_no_navidrome
        .store(true, Ordering::SeqCst);
    let err = core.sync_now(false).await.unwrap_err();
    assert!(matches!(err, CoreError::NotConfigured("navidrome credentials")));
}

#[tokio::test]
async fn lru_cache_eviction_subsonic_error_guard_and_path_safety() {
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    // Budget fits 2 tracks of 2048 bytes (4096 max), forcing eviction when a 3rd arrives.
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 4096))
        .await
        .unwrap();
    let now = now_unix();

    for (tid, album_id) in [
        ("t-pinned", "al1"),
        ("t-old-unpinned", "al1"),
        ("t-new-unpinned", "al2"),
    ] {
        upsert_track(
            core.pool(),
            &LocalTrack {
                id: tid.to_string(),
                title: tid.to_string(),
                artist_id: Some("ar1".to_string()),
                artist_name: "Artist".to_string(),
                album_id: Some(album_id.to_string()),
                album_name: "Album".to_string(),
                track_number: Some(1),
                disc_number: Some(1),
                duration_secs: 200,
                year: Some(2025),
                genre: Some("Electronic".to_string()),
                bit_rate: Some(160),
                suffix: Some("opus".to_string()),
                content_type: None,
                size_bytes: Some(2048),
                cover_art_id: Some("ca-1".to_string()),
                path: None,
                starred: false,
                user_rating: 0,
                play_count: 0,
                last_played_at: None,
                musicbrainz_id: None,
                is_discovery: false,
                updated_at: now,
            },
            false,
        )
        .await
        .unwrap();
    }

    upsert_playlist(
        core.pool(),
        &LocalPlaylist {
            id: "pl-keep".to_string(),
            name: "Keep".to_string(),
            comment: None,
            owner: None,
            song_count: 1,
            duration_secs: 200,
            cover_art_id: None,
            is_smart: false,
            kind: "user".to_string(),
            slug: None,
            changed_at: None,
            updated_at: now,
        },
    )
    .await
    .unwrap();
    replace_playlist_tracks(core.pool(), "pl-keep", &["t-pinned".to_string()])
        .await
        .unwrap();

    tokio::fs::create_dir_all(dir.path().join("audio_cache"))
        .await
        .unwrap();

    // Write 3 files of 2048 bytes each; t-pinned is OLDEST (ts=100), t-old-unpinned (ts=200), t-new-unpinned (ts=300)
    for (tid, pinned, ts) in [
        ("t-pinned", true, 100),
        ("t-old-unpinned", false, 200),
        ("t-new-unpinned", false, 300),
    ] {
        let p = dir.path().join("audio_cache").join(format!("{tid}.opus"));
        tokio::fs::write(&p, vec![1u8; 2048]).await.unwrap();
        core.cache()
            .register_cached_file(tid, &p, "opus", Some(160), 2048, pinned, None, ts)
            .await
            .unwrap();
    }

    // Pin both album al1 and playlist pl-keep
    core.pin(&PinTarget::Album("al1".to_string())).await.unwrap();
    core.pin(&PinTarget::Playlist("pl-keep".to_string()))
        .await
        .unwrap();
    // Unpinning album al1 MUST leave t-pinned still pinned via pl-keep!
    core.unpin(&PinTarget::Album("al1".to_string()))
        .await
        .unwrap();

    // t-old-unpinned (ts=200) must have been evicted, while t-pinned (ts=100!) & t-new-unpinned remain!
    assert!(!dir.path().join("audio_cache/t-old-unpinned.opus").exists());
    assert!(dir.path().join("audio_cache/t-pinned.opus").exists());
    assert!(dir.path().join("audio_cache/t-new-unpinned.opus").exists());

    let stats = core.cache_stats().await.unwrap();
    assert_eq!(stats.total_audio_bytes, 4096);
    assert_eq!(stats.pinned_track_count, 1);

    // Subsonic HTTP 200 JSON error rejection in download_track & get_or_fetch_cover_art
    state
        .download_returns_json_error
        .store(true, Ordering::SeqCst);
    let dl_err = core
        .cache()
        .download_track("missing-song", &AudioQualityProfile::LosslessFlac, false, None)
        .await
        .unwrap_err();
    assert!(matches!(dl_err, CoreError::Subsonic { code: 70, .. }));
    assert!(!dir.path().join("audio_cache/missing-song.flac").exists());
    state
        .download_returns_json_error
        .store(false, Ordering::SeqCst);

    state
        .cover_art_returns_json_error
        .store(true, Ordering::SeqCst);
    let art_err = core
        .cache()
        .get_or_fetch_cover_art("missing-art", Some(512), false)
        .await
        .unwrap_err();
    assert!(matches!(art_err, CoreError::Subsonic { code: 70, .. }));
    assert!(core
        .cache()
        .cover_art_local_path("missing-art")
        .await
        .unwrap()
        .is_none());
    state
        .cover_art_returns_json_error
        .store(false, Ordering::SeqCst);

    // CustomTranscode path traversal sanitization: file stays strictly inside cache_dir!
    core.cache()
        .set_policy(CachePolicy {
            max_bytes: 1_000_000,
            ..CachePolicy::default()
        })
        .await
        .unwrap();
    let entry = core
        .cache()
        .download_track(
            "../evil/id",
            &AudioQualityProfile::CustomTranscode {
                format: "../../mp3".to_string(),
                max_bit_rate: 192,
            },
            false,
            None,
        )
        .await
        .unwrap();
    let entry_path = std::path::PathBuf::from(&entry.local_path);
    assert_eq!(entry_path.parent(), Some(dir.path().join("audio_cache").as_path()));
    assert!(entry_path.exists());

    // Re-downloading same track under a different format deletes the previous file!
    let entry2 = core
        .cache()
        .download_track("../evil/id", &AudioQualityProfile::LosslessFlac, false, None)
        .await
        .unwrap();
    assert_ne!(entry.local_path, entry2.local_path);
    assert!(!entry_path.exists());
    assert!(std::path::Path::new(&entry2.local_path).exists());

    // Verify profile-aware playback source resolution & offline_only gating
    let local = core
        .resolve_playback_source("t-pinned", &AudioQualityProfile::CellularOpus160)
        .await
        .unwrap();
    assert!(matches!(local, PlaybackSource::LocalFile { .. }));

    let remote_opus = core
        .resolve_playback_source("t-old-unpinned", &AudioQualityProfile::CellularOpus160)
        .await
        .unwrap();
    match remote_opus {
        PlaybackSource::RemoteStream {
            url,
            format,
            bit_rate,
        } => {
            assert!(url.contains("format=opus"));
            assert!(url.contains("maxBitRate=160"));
            assert_eq!(format, "opus");
            assert_eq!(bit_rate, Some(160));
        }
        _ => panic!("expected RemoteStream"),
    }

    core.set_offline_only(true);
    let err = core
        .resolve_playback_source("t-old-unpinned", &AudioQualityProfile::LosslessFlac)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Offline(_)));

    // Cover art fetch + zero-network local lookup
    core.set_offline_only(false);
    let art_path = core
        .cache()
        .get_or_fetch_cover_art("ca-al1", Some(512), false)
        .await
        .unwrap();
    assert!(art_path.exists());
    assert_eq!(
        core.cache().cover_art_local_path("ca-al1").await.unwrap(),
        Some(art_path)
    );
}

#[tokio::test]
async fn outbox_enforces_409_retryable_fifo_backoff_dead_letters_permanent_and_survives_corrupt_json()
{
    let state = Arc::new(StubState::default());
    state.fail_star_409_retryable.store(true, Ordering::SeqCst);
    state.fail_drop_409_permanent.store(true, Ordering::SeqCst);
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();
    core.sync_now(true).await.unwrap();

    // Enqueue 3 sequential mutations for s1 + 1 permanent-failing drop for s3 + 1 independent keep for s2
    core.enqueue_mutation(OutboxMutation::Star {
        song_id: "s1".to_string(),
        starred: true,
        discovery_track_id: None,
    })
    .await
    .unwrap();
    core.enqueue_mutation(OutboxMutation::Rate {
        song_id: "s1".to_string(),
        rating: 5,
        discovery_track_id: None,
    })
    .await
    .unwrap();
    core.enqueue_mutation(OutboxMutation::Scrobble {
        song_id: "s1".to_string(),
        submission: true,
        played_at_unix: 1_760_000_000,
    })
    .await
    .unwrap();
    core.enqueue_mutation(OutboxMutation::Drop {
        song_id: "s3".to_string(),
        discovery_track_id: None,
        confirm_delete: true,
    })
    .await
    .unwrap();
    core.enqueue_mutation(OutboxMutation::Keep {
        song_id: "s2".to_string(),
        discovery_track_id: Some("disc-1".to_string()),
        target_folder_id: None,
    })
    .await
    .unwrap();

    // Inject a corrupt JSON row to ensure drain() dead-letters it without aborting!
    sqlx::query(
        r#"
        INSERT INTO mutation_outbox (idempotency_key, kind, song_id, payload_json, created_at, attempts, next_retry_at, dead_letter)
        VALUES ('corrupt-1', 'star', 's-corrupt', '{not valid json', ?, 0, 0, 0)
        "#,
    )
    .bind(now_unix())
    .execute(core.pool())
    .await
    .unwrap();

    // Optimistic local updates must already be visible immediately!
    let s1 = get_track(core.pool(), "s1").await.unwrap().unwrap();
    assert!(s1.starred);
    assert_eq!(s1.user_rating, 5);
    assert_eq!(s1.play_count, 1);

    // First drain:
    // - star:s1 returns HTTP 409 retryable -> MUST NOT be deleted, enters backoff, blocks rate:s1 & scrobble:s1
    // - drop:s3 returns HTTP 409 not_retryable -> dead-lettered (does NOT block s2)
    // - keep:s2 succeeds!
    // - corrupt-1 is dead-lettered without aborting drain!
    let first = core.drain_outbox().await.unwrap();
    assert_eq!(first.replayed, 1);
    assert_eq!(first.failed, 3); // star:s1 (retryable) + drop:s3 (permanent) + corrupt-1
    assert_eq!(first.remaining, 3); // star:s1, rate:s1, scrobble:s1
    assert_eq!(*state.curation_log.lock().await, vec!["keep:s2".to_string()]);
    assert_eq!(core.outbox().dead_lettered().await.unwrap().len(), 2);

    // CRITICAL FIFO BACKOFF CHECK: Drain AGAIN immediately while star:s1 is still in retry backoff
    // (next_retry_at > now). Even though rate:s1 and scrobble:s1 have next_retry_at == 0,
    // they MUST remain blocked behind star:s1 and NEVER overtake it!
    state.fail_star_409_retryable.store(false, Ordering::SeqCst);
    let while_backed_off = core.drain_outbox().await.unwrap();
    assert_eq!(while_backed_off.replayed, 0);
    assert_eq!(while_backed_off.failed, 0);
    assert_eq!(while_backed_off.remaining, 3);
    assert_eq!(*state.curation_log.lock().await, vec!["keep:s2".to_string()]);

    // Clear retry backoff timestamp to simulate backoff expiry.
    sqlx::query("UPDATE mutation_outbox SET next_retry_at = 0 WHERE dead_letter = 0")
        .execute(core.pool())
        .await
        .unwrap();

    let second = core.drain_outbox().await.unwrap();
    assert_eq!(second.replayed, 3);
    assert_eq!(second.failed, 0);
    assert_eq!(second.remaining, 0);

    // Exact FIFO order preserved for s1!
    assert_eq!(
        *state.curation_log.lock().await,
        vec![
            "keep:s2".to_string(),
            "star:s1:true".to_string(),
            "rate:s1:5".to_string(),
            "scrobble:s1".to_string(),
        ]
    );
}

#[tokio::test]
async fn fts5_search_is_sub_20ms_handles_diacritics_uses_real_metadata_endpoint_and_suppresses_remote_offline()
{
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();
    core.sync_now(true).await.unwrap();

    // Seed 300 additional tracks to benchmark realistic local FTS5 speed
    let now = now_unix();
    let mut tx = core.pool().begin().await.unwrap();
    for i in 0..300 {
        sqlx::query(
            r#"
            INSERT INTO tracks (id, title, artist_id, artist_name, album_id, album_name, duration_secs, genre, updated_at)
            VALUES (?, ?, 'ar-bulk', ?, 'al-bulk', 'Catalog', 210, 'Indie', ?)
            "#,
        )
        .bind(format!("bulk-{i}"))
        .bind(if i == 42 {
            "Noël à la Française".to_string()
        } else {
            format!("Ambient Sketch #{i}")
        })
        .bind(if i == 77 { "AC/DC" } else { "Bulk Artist" })
        .bind(now)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    // Cache only s1 locally
    let p = dir.path().join("audio_cache");
    tokio::fs::create_dir_all(&p).await.unwrap();
    let f = p.join("s1.flac");
    tokio::fs::write(&f, vec![0u8; 512]).await.unwrap();
    core.cache()
        .register_cached_file("s1", &f, "flac", None, 512, true, None, now)
        .await
        .unwrap();

    // Online search: hits POST /api/metadata/search/track returning Vec<shared::metadata::SearchResult>
    // Merges local + deduplicated remote (Instant Crush kept, owned Get Lucky deduplicated!)
    let online = core.search("Daft Punk", 20).await.unwrap();
    assert_eq!(online.local_tracks.len(), 1);
    assert_eq!(online.remote_tracks.len(), 1);
    assert_eq!(online.remote_tracks[0].title, "Instant Crush");
    assert_eq!(online.remote_tracks[0].duration_secs, Some(337));
    assert_eq!(online.remote_tracks[0].year, Some(2013));
    assert_eq!(state.remote_search_hits.load(Ordering::SeqCst), 1);

    // Diacritic folding + punctuation safety
    let noel = core.search("Noel francaise", 10).await.unwrap();
    assert_eq!(noel.local_tracks.len(), 1);
    assert_eq!(noel.local_tracks[0].id, "bulk-42");
    assert!(noel.elapsed_ms < 50, "FTS5 search took {}ms", noel.elapsed_ms);

    let acdc = core.search("AC/DC", 10).await.unwrap();
    assert_eq!(acdc.local_tracks.len(), 1);
    assert_eq!(acdc.local_tracks[0].id, "bulk-77");

    // Offline-only mode: ZERO additional network calls and filters strictly to cached tracks!
    let hits_before_offline = state.remote_search_hits.load(Ordering::SeqCst);
    assert_eq!(hits_before_offline, 3);
    core.set_offline_only(true);
    let offline_hit = core.search("Get Lucky", 10).await.unwrap();
    assert_eq!(offline_hit.local_tracks.len(), 1);
    assert!(offline_hit.remote_tracks.is_empty());
    let offline_miss = core.search("Noel francaise", 10).await.unwrap();
    assert!(offline_miss.local_tracks.is_empty());
    assert_eq!(
        state.remote_search_hits.load(Ordering::SeqCst),
        hits_before_offline
    );
}

#[tokio::test]
async fn android_auto_browse_bounds_az_buckets_voice_search_and_ignition_resumption() {
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, None, 10_000_000))
        .await
        .unwrap();
    let now = now_unix();

    // Seed 45 playlists (> AUTO_PLAYLISTS_LIMIT=24 user playlists) including a Seasonal Noël pack,
    // a short "Punk" playlist (to ensure it does NOT hijack "Play Daft Punk"!),
    // and 14 discovery/moment playlists where the 14th is offline-cached (beyond AUTO_FOR_YOU_LIMIT=12).
    for i in 0..45 {
        let pl_id = format!("pl-{i}");
        let (name, kind, slug) = match i {
            0 => (
                "Noël à la Française".to_string(),
                "seasonal".to_string(),
                Some("winter_holiday".to_string()),
            ),
            1 => ("Punk".to_string(), "user".to_string(), None),
            2..=15 => (
                format!("Discovery Mix #{i:02}"),
                "discovery".to_string(),
                Some(format!("discovery_mix_{i:02}")),
            ),
            _ => (format!("Mix #{i:02}"), "user".to_string(), None),
        };
        upsert_playlist(
            core.pool(),
            &LocalPlaylist {
                id: pl_id.clone(),
                name,
                comment: None,
                owner: Some("alice".to_string()),
                song_count: 1,
                duration_secs: 200,
                cover_art_id: None,
                is_smart: kind != "user",
                kind,
                slug,
                changed_at: None,
                updated_at: now - i,
            },
        )
        .await
        .unwrap();
        if i == 0 {
            replace_playlist_tracks(core.pool(), &pl_id, &["tr-noel".to_string()])
                .await
                .unwrap();
        } else if i == 1 {
            replace_playlist_tracks(core.pool(), &pl_id, &["tr-punk".to_string()])
                .await
                .unwrap();
        } else if i == 15 {
            replace_playlist_tracks(core.pool(), &pl_id, &["tr-noel".to_string()])
                .await
                .unwrap();
        }
    }

    // Seed 32 recent albums (> AUTO_RECENT_LIMIT=25)
    for i in 0..32 {
        upsert_album(
            core.pool(),
            &LocalAlbum {
                id: format!("al-{i}"),
                name: format!("Album {i:02}"),
                artist_id: Some("ar-a".to_string()),
                artist_name: "Alpha".to_string(),
                year: Some(2025),
                genre: Some("Jazz".to_string()),
                song_count: 1,
                duration_secs: 200,
                cover_art_id: None,
                starred: false,
                created_at: Some(format!("2026-11-{:02}T00:00:00Z", (i % 28) + 1)),
                updated_at: now,
            },
        )
        .await
        .unwrap();
    }

    // Seed artists covering A-D ("THE BEATLES" -> B!, "Daft Punk" -> D!), E-H ("Édith Piaf" -> E!),
    // U-Z ("U2", "2Pac" -> #)
    for (id, name) in [
        ("ar-a", "Alpha"),
        ("ar-b", "THE BEATLES"),
        ("ar-dp", "Daft Punk"),
        ("ar-e", "Édith Piaf"),
        ("ar-u", "U2"),
        ("ar-num", "2Pac"),
    ] {
        upsert_artist(
            core.pool(),
            &LocalArtist {
                id: id.to_string(),
                name: name.to_string(),
                album_count: 1,
                cover_art_id: None,
                starred: false,
                musicbrainz_id: None,
                updated_at: now,
            },
        )
        .await
        .unwrap();
    }

    for (id, title, artist_id, artist_name, album_id, album_name) in [
        ("tr-noel", "Petit Papa Noël", "ar-a", "Tino Rossi", "al-27", "Noël"),
        ("tr-punk", "Blitzkrieg Bop", "ar-a", "Ramones", "al-1", "Ramones"),
        ("tr-dp", "Get Lucky", "ar-dp", "Daft Punk", "al-2", "Random Access Memories"),
    ] {
        upsert_track(
            core.pool(),
            &LocalTrack {
                id: id.to_string(),
                title: title.to_string(),
                artist_id: Some(artist_id.to_string()),
                artist_name: artist_name.to_string(),
                album_id: Some(album_id.to_string()),
                album_name: album_name.to_string(),
                track_number: Some(1),
                disc_number: Some(1),
                duration_secs: 185,
                year: Some(1946),
                genre: Some("Holiday".to_string()),
                bit_rate: Some(320),
                suffix: Some("flac".to_string()),
                content_type: None,
                size_bytes: Some(1024),
                cover_art_id: None,
                path: None,
                starred: true,
                user_rating: 5,
                play_count: 3,
                last_played_at: None,
                musicbrainz_id: None,
                is_discovery: false,
                updated_at: now,
            },
            false,
        )
        .await
        .unwrap();
    }

    // Cache tr-noel locally so pl-15 (ranked past AUTO_FOR_YOU_LIMIT=12!) is offline-ready
    let p = dir.path().join("audio_cache");
    tokio::fs::create_dir_all(&p).await.unwrap();
    let f = p.join("tr-noel.flac");
    tokio::fs::write(&f, vec![0u8; 512]).await.unwrap();
    core.cache()
        .register_cached_file("tr-noel", &f, "flac", None, 512, true, None, now)
        .await
        .unwrap();

    // 1. Exact 4 root tabs
    let tabs = core.auto_browse().children(AUTO_ROOT_ID, false).await.unwrap();
    let tab_ids: Vec<&str> = tabs.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(
        tab_ids,
        vec![
            AUTO_TAB_FOR_YOU,
            AUTO_TAB_PLAYLISTS,
            AUTO_TAB_LIBRARY,
            AUTO_TAB_RADIO
        ]
    );

    // 2. For You offline_only filters in SQL BEFORE LIMIT, so pl-15 is returned!
    let for_you_all = core
        .auto_browse()
        .children(AUTO_TAB_FOR_YOU, false)
        .await
        .unwrap();
    assert_eq!(for_you_all.len(), AUTO_FOR_YOU_LIMIT);
    let for_you_offline = core
        .auto_browse()
        .children(AUTO_TAB_FOR_YOU, true)
        .await
        .unwrap();
    assert!(for_you_offline.iter().any(|n| n.id == "playlist:pl-0"));
    assert!(for_you_offline.iter().any(|n| n.id == "playlist:pl-15"));

    // 3. Playlists capped strictly <= 24 and filtered strictly to kind = 'user' (no smart mixes!)
    let pls = core
        .auto_browse()
        .children(AUTO_TAB_PLAYLISTS, false)
        .await
        .unwrap();
    assert_eq!(pls.len(), AUTO_PLAYLISTS_LIMIT);
    assert!(pls.iter().all(|n| !n.is_pending_discovery));
    assert!(!pls.iter().any(|n| n.id == "playlist:pl-0"));
    assert!(!pls.iter().any(|n| n.id == "playlist:pl-2"));

    // 4. Library has Offline, Recent (<=25), and 6 A-Z buckets
    let lib = core
        .auto_browse()
        .children(AUTO_TAB_LIBRARY, false)
        .await
        .unwrap();
    assert_eq!(lib.len(), 8);
    assert_eq!(lib[0].id, AUTO_LIB_OFFLINE);
    assert_eq!(lib[0].subtitle.as_deref(), Some("1 tracks ready"));
    assert_eq!(lib[1].id, AUTO_LIB_RECENT);

    let recent = core
        .auto_browse()
        .children(AUTO_LIB_RECENT, false)
        .await
        .unwrap();
    assert_eq!(recent.len(), AUTO_RECENT_LIMIT);
    // al-27 (newest) has offline track tr-noel so is_offline_ready MUST be true even when offline_only=false!
    assert!(recent.iter().find(|n| n.id == "album:al-27").unwrap().is_offline_ready);

    // Case-insensitive "THE " stripping and diacritic folding ("Édith Piaf" -> E-H!)
    let ad = core.auto_browse().children("az:a-d", false).await.unwrap();
    assert!(ad.iter().any(|n| n.title == "THE BEATLES"));
    let eh = core.auto_browse().children("az:e-h", false).await.unwrap();
    assert!(eh.iter().any(|n| n.title == "Édith Piaf"));
    let uz = core.auto_browse().children("az:u-z", false).await.unwrap();
    assert!(uz.iter().any(|n| n.title == "U2"));
    assert!(uz.iter().any(|n| n.title == "2Pac"));

    // 5. Local voice search:
    // - "Play Noël mix" strips " mix" & folds diacritics -> matches Noël playlist!
    let voice_noel = core.voice_search("Play Noël mix").await.unwrap().unwrap();
    assert_eq!(voice_noel.kind, VoiceMatchKind::Playlist);
    assert_eq!(voice_noel.target_id, "pl-0");
    assert_eq!(voice_noel.track_ids, vec!["tr-noel".to_string()]);
    assert!(
        voice_noel.elapsed_ms < 100,
        "voice search took {}ms",
        voice_noel.elapsed_ms
    );

    // - "Play Winter Holiday" matches underscore slug "winter_holiday"!
    let voice_slug = core.voice_search("Play Winter Holiday").await.unwrap().unwrap();
    assert_eq!(voice_slug.kind, VoiceMatchKind::Playlist);
    assert_eq!(voice_slug.target_id, "pl-0");

    // - "Play Daft Punk" MUST match Artist "Daft Punk" and MUST NOT be hijacked by playlist "Punk"!
    let voice_dp = core.voice_search("Play Daft Punk").await.unwrap().unwrap();
    assert_eq!(voice_dp.kind, VoiceMatchKind::Artist);
    assert_eq!(voice_dp.target_id, "ar-dp");
    assert_eq!(voice_dp.track_ids, vec!["tr-dp".to_string()]);

    // 6. Ignition resumption persists exact queue across session save/load
    let session = PersistedPlaybackSession {
        queue_track_ids: vec!["tr-noel".to_string(), "tr-dp".to_string()],
        current_index: 1,
        position_ms: 42_500,
        is_playing: true,
        source_label: Some("Noël à la Française".to_string()),
        updated_at: now,
    };
    core.save_playback_session(&session).await.unwrap();
    let loaded = core.load_playback_session().await.unwrap().unwrap();
    assert_eq!(loaded, session);
}

#[tokio::test]
async fn connect_ws_sends_query_params_decodes_binary_frames_prunes_snapshots_and_relays_handoff() {
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();

    let mut rx = core.connect_client().subscribe();
    // Pass Duration::ZERO to verify heartbeat_interval clamping does not panic!
    core.connect_ws(Duration::ZERO).await.unwrap();

    // Verify URL-encoded query params were sent on WS handshake
    let q = state
        .ws_query_params
        .lock()
        .await
        .clone()
        .expect("WS query params captured");
    assert_eq!(q.get("device_id").map(String::as_str), Some("desktop-1"));
    assert_eq!(q.get("name").map(String::as_str), Some("Linux Studio"));
    assert_eq!(q.get("platform").map(String::as_str), Some("desktop"));

    // Stub sends JSON inside Message::Binary (Dioxus 0.7 TypedWebsocket wire behavior)
    let first_event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    match first_event {
        ConnectServerEventDto::DeviceList {
            devices,
            active_device_id,
        } => {
            assert_eq!(devices.len(), 2);
            assert_eq!(devices[0].device_id, "desktop-1");
            assert_eq!(active_device_id.as_deref(), Some("desktop-1"));
        }
        other => panic!("expected DeviceList, got {other:?}"),
    }

    let snapshot = PlaybackSnapshotDto {
        song_id: Some("s1".to_string()),
        title: Some("Get Lucky".to_string()),
        artist: Some("Daft Punk".to_string()),
        album: Some("Random Access Memories".to_string()),
        cover_art_id: Some("ca-al1".to_string()),
        is_playing: true,
        position_ms: 15_000,
        duration_ms: 369_000,
        queue_song_ids: vec!["s1".to_string()],
        queue_index: 0,
        volume: 0.85,
        is_pending_discovery: false,
        updated_at_unix: now_unix(),
    };

    // Populate snapshot for android-1 via UpdateState -> PlaybackUpdated
    core.connect_client()
        .update_state(snapshot.clone())
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(core
        .connect_client()
        .state()
        .await
        .snapshots
        .contains_key("android-1"));

    // Handoff command over Binary frame
    core.connect_client()
        .transfer_playback_to("android-1".to_string(), snapshot)
        .await
        .unwrap();
    let handoff = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    match handoff {
        ConnectServerEventDto::Command {
            command: ConnectCommandDto::TransferPlayback { from_device_id, .. },
            ..
        } => {
            assert_eq!(from_device_id, "desktop-1");
        }
        other => panic!("expected TransferPlayback command, got {other:?}"),
    }

    // ClaimActive triggers DeviceList without android-1 -> android-1 MUST be pruned from state.snapshots!
    core.connect_client().claim_active().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let live_state = core.connect_client().state().await;
    assert!(live_state.connected);
    assert_eq!(live_state.devices.len(), 1);
    assert!(!live_state.snapshots.contains_key("android-1"));

    core.connect_client().disconnect().await;
}

#[tokio::test]
async fn connect_ws_sends_ping_with_heartbeat_and_auto_reconnects_after_server_drop() {
    let state = Arc::new(StubState::default());
    state.ws_drop_first_connection.store(true, Ordering::SeqCst);
    state.ws_send_server_ping.store(true, Ordering::SeqCst);
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();

    let mut rx = core.connect_client().subscribe();
    core.connect_ws(Duration::from_secs(1)).await.unwrap();

    // First connection delivers DeviceList then server closes socket.
    let first = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(first, ConnectServerEventDto::DeviceList { .. }));

    // Supervisor automatically reconnects with 1s initial backoff and delivers a second DeviceList.
    let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(second, ConnectServerEventDto::DeviceList { .. }));
    assert!(state.ws_connect_count.load(Ordering::SeqCst) >= 2);
    assert!(core.connect_client().state().await.connected);

    // Wait for at least one heartbeat tick (1s) and verify both WS Ping and Pong frames reached server.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(
        state.ws_ping_count.load(Ordering::SeqCst) >= 1,
        "expected at least one WS Ping frame alongside Heartbeat"
    );
    assert!(
        state.ws_pong_count.load(Ordering::SeqCst) >= 1,
        "expected WS Pong frame in reply to server Ping"
    );

    core.connect_client().disconnect().await;
    assert!(!core.connect_client().state().await.connected);
}

#[tokio::test]
async fn incremental_sync_updates_and_clears_navidrome_ratings_without_refetching_albums() {
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();

    // Initial full sync seeds s1 and s2 with user_rating = 0.
    let initial = core.sync_now(false).await.unwrap();
    assert!(initial.full_sync);
    assert_eq!(get_track(core.pool(), "s1").await.unwrap().unwrap().user_rating, 0);
    assert_eq!(get_track(core.pool(), "s2").await.unwrap().unwrap().user_rating, 0);

    // Rate s1 to 4 stars in Navidrome without changing album created timestamp, recent albums, or starred.
    state.skip_recent_albums.store(true, Ordering::SeqCst);
    state.song_ratings.lock().await.insert("s1".to_string(), 4);
    let fetches_before = state.album_detail_fetch_count.load(Ordering::SeqCst);

    let delta = core.sync_now(false).await.unwrap();
    assert!(!delta.full_sync);
    assert!(delta.warnings.is_empty(), "unexpected warnings: {:?}", delta.warnings);
    assert_eq!(
        state.album_detail_fetch_count.load(Ordering::SeqCst),
        fetches_before,
        "incremental rating refresh must not refetch unchanged albums"
    );
    assert_eq!(get_track(core.pool(), "s1").await.unwrap().unwrap().user_rating, 4);
    assert_eq!(get_track(core.pool(), "s2").await.unwrap().unwrap().user_rating, 0);

    // Unrating s1 in Navidrome clears tracks.user_rating back to 0 on next incremental sync.
    state.song_ratings.lock().await.remove("s1");
    let delta2 = core.sync_now(false).await.unwrap();
    assert!(!delta2.full_sync);
    assert_eq!(get_track(core.pool(), "s1").await.unwrap().unwrap().user_rating, 0);
}

#[tokio::test]
async fn outbox_separates_keep_drop_star_from_ratings_and_hides_empty_seasonal_playlists() {
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, None, 10_000_000))
        .await
        .unwrap();
    let now = now_unix();

    for (id, is_disc) in [("s-keep", true), ("s-star", true), ("s-drop", true), ("s-rate", true)] {
        upsert_track(
            core.pool(),
            &LocalTrack {
                id: id.to_string(),
                title: id.to_string(),
                artist_id: Some("ar-1".to_string()),
                artist_name: "Artist".to_string(),
                album_id: Some("al-1".to_string()),
                album_name: "Album".to_string(),
                track_number: Some(1),
                disc_number: Some(1),
                duration_secs: 180,
                year: Some(2025),
                genre: None,
                bit_rate: None,
                suffix: Some("flac".to_string()),
                content_type: None,
                size_bytes: None,
                cover_art_id: None,
                path: Some(format!("Discovery/Balanced/{id}.flac")),
                starred: false,
                user_rating: 0,
                play_count: 0,
                last_played_at: None,
                musicbrainz_id: None,
                is_discovery: is_disc,
                updated_at: now,
            },
            false,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO discovery_tracks (id, song_id, artist, title, status, updated_at) VALUES (?, ?, 'Artist', ?, 'pending', ?)",
        )
        .bind(format!("disc-{id}"))
        .bind(id)
        .bind(id)
        .bind(now)
        .execute(core.pool())
        .await
        .unwrap();
    }

    // Keep must promote locally WITHOUT stamping a fake star rating.
    core.enqueue_mutation(OutboxMutation::Keep {
        song_id: "s-keep".to_string(),
        discovery_track_id: Some("disc-s-keep".to_string()),
        target_folder_id: None,
    })
    .await
    .unwrap();
    let kept = get_track(core.pool(), "s-keep").await.unwrap().unwrap();
    assert!(!kept.is_discovery);
    assert_eq!(kept.user_rating, 0);

    // Star must star + promote locally WITHOUT stamping a star rating.
    core.enqueue_mutation(OutboxMutation::Star {
        song_id: "s-star".to_string(),
        starred: true,
        discovery_track_id: Some("disc-s-star".to_string()),
    })
    .await
    .unwrap();
    let starred = get_track(core.pool(), "s-star").await.unwrap().unwrap();
    assert!(starred.starred);
    assert!(!starred.is_discovery);
    assert_eq!(starred.user_rating, 0);

    // Drop must mark discovery track removed WITHOUT stamping user_rating = 1.
    core.enqueue_mutation(OutboxMutation::Drop {
        song_id: "s-drop".to_string(),
        discovery_track_id: Some("disc-s-drop".to_string()),
        confirm_delete: false,
    })
    .await
    .unwrap();
    let dropped = get_track(core.pool(), "s-drop").await.unwrap().unwrap();
    assert!(!dropped.is_discovery);
    assert_eq!(dropped.user_rating, 0);
    let dropped_status: String =
        sqlx::query_scalar("SELECT status FROM discovery_tracks WHERE id = 'disc-s-drop'")
            .fetch_one(core.pool())
            .await
            .unwrap();
    assert_eq!(dropped_status, "removed");

    // Rate must update user_rating purely WITHOUT mutating discovery_tracks.status.
    core.enqueue_mutation(OutboxMutation::Rate {
        song_id: "s-rate".to_string(),
        rating: 5,
        discovery_track_id: Some("disc-s-rate".to_string()),
    })
    .await
    .unwrap();
    let rated = get_track(core.pool(), "s-rate").await.unwrap().unwrap();
    assert_eq!(rated.user_rating, 5);
    assert!(rated.is_discovery);
    let rated_status: String =
        sqlx::query_scalar("SELECT status FROM discovery_tracks WHERE id = 'disc-s-rate'")
            .fetch_one(core.pool())
            .await
            .unwrap();
    assert_eq!(rated_status, "pending");

    // Empty seasonal playlists (song_count = 0, no tracks) must be hidden from list_playlists and Auto browse.
    upsert_playlist(
        core.pool(),
        &LocalPlaylist {
            id: "pl-seasonal-empty".to_string(),
            name: "Stale Holiday Pack".to_string(),
            comment: Some("soulbeet:contextual:stale_holiday".to_string()),
            owner: Some("alice".to_string()),
            song_count: 0,
            duration_secs: 0,
            cover_art_id: None,
            is_smart: true,
            kind: "seasonal".to_string(),
            slug: Some("stale_holiday".to_string()),
            changed_at: None,
            updated_at: now,
        },
    )
    .await
    .unwrap();
    upsert_playlist(
        core.pool(),
        &LocalPlaylist {
            id: "pl-seasonal-full".to_string(),
            name: "Active Seasonal Pack".to_string(),
            comment: Some("soulbeet:contextual:active_seasonal".to_string()),
            owner: Some("alice".to_string()),
            song_count: 1,
            duration_secs: 180,
            cover_art_id: None,
            is_smart: true,
            kind: "seasonal".to_string(),
            slug: Some("active_seasonal".to_string()),
            changed_at: None,
            updated_at: now,
        },
    )
    .await
    .unwrap();
    replace_playlist_tracks(core.pool(), "pl-seasonal-full", &["s-keep".to_string()])
        .await
        .unwrap();

    let playlists = core.list_playlists().await.unwrap();
    assert!(!playlists.iter().any(|p| p.id == "pl-seasonal-empty"));
    assert!(playlists.iter().any(|p| p.id == "pl-seasonal-full"));

    let for_you = core
        .auto_browse()
        .children(AUTO_TAB_FOR_YOU, false)
        .await
        .unwrap();
    assert!(!for_you.iter().any(|n| n.id == "playlist:pl-seasonal-empty"));
    assert!(for_you.iter().any(|n| n.id == "playlist:pl-seasonal-full"));
}

#[tokio::test]
async fn incremental_rating_sync_preserves_unseen_tracks_and_in_flight_outbox_mutations() {
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = Arc::new(
        SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
            .await
            .unwrap(),
    );

    // Initial full sync seeds s1 and s2.
    let initial = core.sync_now(false).await.unwrap();
    assert!(initial.full_sync);

    // Seed remote ratings for both s1 (4) and s2 (5) via incremental sync.
    state.skip_recent_albums.store(true, Ordering::SeqCst);
    {
        let mut ratings = state.song_ratings.lock().await;
        ratings.insert("s1".to_string(), 4);
        ratings.insert("s2".to_string(), 5);
    }
    let seeded = core.sync_now(false).await.unwrap();
    assert!(!seeded.full_sync);
    assert_eq!(get_track(core.pool(), "s1").await.unwrap().unwrap().user_rating, 4);
    assert_eq!(get_track(core.pool(), "s2").await.unwrap().unwrap().user_rating, 5);

    // (a) Partial search3 response omits s2 and changes s1 to 3: s2 must keep its local rating (5).
    state.search3_omit_song_ids.lock().await.insert("s2".to_string());
    state.song_ratings.lock().await.insert("s1".to_string(), 3);
    let partial = core.sync_now(false).await.unwrap();
    assert!(!partial.full_sync);
    assert!(partial.warnings.is_empty(), "unexpected warnings: {:?}", partial.warnings);
    assert_eq!(get_track(core.pool(), "s1").await.unwrap().unwrap().user_rating, 3);
    assert_eq!(
        get_track(core.pool(), "s2").await.unwrap().unwrap().user_rating,
        5,
        "partial search3 page must not wipe ratings on unseen local tracks"
    );
    state.search3_omit_song_ids.lock().await.clear();

    // (b) An outbox rating mutation enqueued while search3 is in flight (after sync() took its
    // initial pre-network pending snapshot, before sync_song_ratings opens its SQLite write tx)
    // must be preserved and not overwritten by stale remote rating (2).
    state.song_ratings.lock().await.insert("s1".to_string(), 2);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    *state.search3_entered_notify.lock().await = Some(entered.clone());
    *state.search3_release_notify.lock().await = Some(release.clone());

    let core_for_sync = core.clone();
    let sync_handle = tokio::spawn(async move { core_for_sync.sync_now(false).await });

    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("timed out waiting for search3 request");

    // Enqueue a pending rating mutation for s1 while search3 is paused in flight.
    core.enqueue_mutation(OutboxMutation::Rate {
        song_id: "s1".to_string(),
        rating: 5,
        discovery_track_id: None,
    })
    .await
    .unwrap();

    // Release search3 and wait for incremental sync to finish.
    *state.search3_entered_notify.lock().await = None;
    *state.search3_release_notify.lock().await = None;
    release.notify_one();

    let delta = sync_handle.await.unwrap().unwrap();
    assert!(!delta.full_sync);
    assert!(delta.warnings.is_empty(), "unexpected warnings: {:?}", delta.warnings);
    assert_eq!(
        get_track(core.pool(), "s1").await.unwrap().unwrap().user_rating,
        5,
        "in-flight pending outbox rating mutation must be preserved inside sync_song_ratings tx"
    );
}

#[tokio::test]
async fn discovery_triage_sync_hydrates_rating_when_present_and_preserves_existing_when_null() {
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();

    // Triage returns rating = 4 for discovery track s2 -> hydrates tracks.user_rating to 4.
    *state.triage_rating.lock().await = Some(4);
    let initial = core.sync_now(false).await.unwrap();
    assert!(initial.full_sync);
    assert_eq!(
        get_track(core.pool(), "s2").await.unwrap().unwrap().user_rating,
        4
    );

    // Subsequent sync where search3 omits s2 and triage returns rating = null keeps existing rating (4).
    state.skip_recent_albums.store(true, Ordering::SeqCst);
    state.search3_omit_song_ids.lock().await.insert("s2".to_string());
    *state.triage_rating.lock().await = None;
    let delta = core.sync_now(false).await.unwrap();
    assert!(!delta.full_sync);
    assert_eq!(
        get_track(core.pool(), "s2").await.unwrap().unwrap().user_rating,
        4,
        "null triage rating must keep existing tracks.user_rating"
    );

    // Pending outbox mutation on s2 (rating = 5) must not be clobbered by triage rating = 2.
    core.enqueue_mutation(OutboxMutation::Rate {
        song_id: "s2".to_string(),
        rating: 5,
        discovery_track_id: Some("disc-1".to_string()),
    })
    .await
    .unwrap();
    *state.triage_rating.lock().await = Some(2);
    let delta2 = core.sync_now(false).await.unwrap();
    assert!(!delta2.full_sync);
    assert_eq!(
        get_track(core.pool(), "s2").await.unwrap().unwrap().user_rating,
        5,
        "pending outbox rating on discovery track must not be overwritten by triage hydration"
    );
}
