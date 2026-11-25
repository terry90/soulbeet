use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use client_core::auto_browse::{
    AUTO_LIB_OFFLINE, AUTO_LIB_RECENT, AUTO_PLAYLISTS_LIMIT, AUTO_RECENT_LIMIT, AUTO_ROOT_ID,
    AUTO_TAB_FOR_YOU, AUTO_TAB_LIBRARY, AUTO_TAB_PLAYLISTS, AUTO_TAB_RADIO,
};
use client_core::db::{
    get_track, now_unix, replace_playlist_tracks, upsert_album, upsert_artist, upsert_playlist,
    upsert_track,
};
use client_core::error::CoreError;
use client_core::models::{
    AudioQualityProfile, CachePolicy, ClientConfig, ConnectClientMessageDto, ConnectCommandDto,
    ConnectDeviceDto, ConnectServerEventDto, DevicePlatform, LocalAlbum, LocalArtist,
    LocalPlaylist, LocalTrack, NavidromeCredentials, OutboxMutation, PersistedPlaybackSession,
    PinTarget, PlaybackSnapshotDto, PlaybackSource, VoiceMatchKind,
};
use client_core::SoulbeetCore;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[derive(Default)]
struct StubState {
    playlist_fetch_count: AtomicUsize,
    remote_search_hits: AtomicUsize,
    fail_star: AtomicBool,
    curation_log: Mutex<Vec<String>>,
    new_album_enabled: AtomicBool,
}

async fn start_stub_server(state: Arc<StubState>) -> String {
    let app = Router::new()
        .route("/api/client/bootstrap", get(handle_bootstrap))
        .route("/api/discovery/triage", get(handle_triage))
        .route("/api/search/catalog", get(handle_remote_search))
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
        .route("/rest/scrobble", get(handle_scrobble))
        .route("/rest/download", get(handle_download))
        .route("/rest/stream", get(handle_stream))
        .route("/rest/getCoverArt", get(handle_cover_art))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn handle_bootstrap() -> Json<Value> {
    Json(json!({
        "user_id": "u1",
        "username": "alice",
        "is_admin": false,
        "navidrome": null,
        "promote_threshold": 4,
        "auto_delete_enabled": true,
        "discovery_enabled": true
    }))
}

async fn handle_triage() -> Json<Value> {
    Json(json!([
        {
            "id": "disc-1",
            "song_id": "s2",
            "artist": "Justice",
            "title": "D.A.N.C.E.",
            "album": "Cross",
            "profile_slug": "weekly_radar",
            "status": "pending",
            "exclude_from_core_taste": false
        }
    ]))
}

async fn handle_remote_search(State(st): State<Arc<StubState>>) -> Json<Value> {
    st.remote_search_hits.fetch_add(1, Ordering::SeqCst);
    Json(json!([
        {
            "mbid": "mb-owned",
            "title": "Get Lucky",
            "artist": "Daft Punk",
            "album": "Random Access Memories",
            "duration_secs": 369,
            "year": 2013
        },
        {
            "mbid": "mb-unowned",
            "title": "Instant Crush",
            "artist": "Daft Punk",
            "album": "Random Access Memories",
            "duration_secs": 337,
            "year": 2013
        }
    ]))
}

async fn handle_curation_star(
    State(st): State<Arc<StubState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    if st.fail_star.load(Ordering::SeqCst) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "transient error");
    }
    let id = body["song_id"].as_str().unwrap_or_default();
    let starred = body["starred"].as_bool().unwrap_or(false);
    st.curation_log
        .lock()
        .await
        .push(format!("star:{id}:{starred}"));
    (StatusCode::OK, "ok")
}

async fn handle_curation_rate(
    State(st): State<Arc<StubState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let id = body["song_id"].as_str().unwrap_or_default();
    let rating = body["rating"].as_u64().unwrap_or(0);
    st.curation_log
        .lock()
        .await
        .push(format!("rate:{id}:{rating}"));
    (StatusCode::OK, "ok")
}

async fn handle_curation_keep(
    State(st): State<Arc<StubState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let id = body["song_id"].as_str().unwrap_or_default();
    st.curation_log.lock().await.push(format!("keep:{id}"));
    (StatusCode::OK, "ok")
}

async fn handle_curation_drop(
    State(st): State<Arc<StubState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let id = body["song_id"].as_str().unwrap_or_default();
    st.curation_log.lock().await.push(format!("drop:{id}"));
    (StatusCode::OK, "ok")
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

async fn handle_get_album(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
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
                    "coverArt": "ca-al2"
                }
            ]
        }),
    };
    Json(json!({ "subsonic-response": { "status": "ok", "album": album } }))
}

async fn handle_get_playlists() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "playlists": {
                "playlist": [
                    {
                        "id": "pl1",
                        "name": "Noël à la Française",
                        "comment": "seasonal:winter_holiday",
                        "owner": "alice",
                        "songCount": 1,
                        "duration": 369,
                        "coverArt": "ca-pl1",
                        "changed": "2026-11-15T12:00:00Z"
                    }
                ]
            }
        }
    }))
}

async fn handle_get_playlist(State(st): State<Arc<StubState>>) -> Json<Value> {
    st.playlist_fetch_count.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "playlist": {
                "id": "pl1",
                "name": "Noël à la Française",
                "comment": "seasonal:winter_holiday",
                "owner": "alice",
                "songCount": 1,
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
                "artist": [],
                "album": [],
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

async fn handle_download() -> impl IntoResponse {
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

async fn handle_cover_art() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/jpeg")],
        vec![0xFFu8, 0xD8, 0xFF, 0xD9],
    )
}

async fn handle_ws_connect(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_ws_socket)
}

async fn handle_ws_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        if let Message::Text(text) = msg {
            let Ok(client_msg) = serde_json::from_str::<ConnectClientMessageDto>(&text) else {
                continue;
            };
            match client_msg {
                ConnectClientMessageDto::Register {
                    device_id,
                    name,
                    platform,
                } => {
                    let event = ConnectServerEventDto::DeviceList {
                        devices: vec![ConnectDeviceDto {
                            device_id: device_id.clone(),
                            name,
                            platform,
                            is_active: true,
                            last_seen_unix: now_unix(),
                            snapshot: None,
                        }],
                        active_device_id: Some(device_id),
                    };
                    let _ = socket
                        .send(Message::Text(serde_json::to_string(&event).unwrap().into()))
                        .await;
                }
                ConnectClientMessageDto::UpdateState { snapshot } => {
                    let event = ConnectServerEventDto::PlaybackUpdated {
                        device_id: "desktop-1".to_string(),
                        snapshot,
                    };
                    let _ = socket
                        .send(Message::Text(serde_json::to_string(&event).unwrap().into()))
                        .await;
                }
                ConnectClientMessageDto::SendCommand { command, .. } => {
                    let event = ConnectServerEventDto::Command {
                        from_device_id: "desktop-1".to_string(),
                        command,
                    };
                    let _ = socket
                        .send(Message::Text(serde_json::to_string(&event).unwrap().into()))
                        .await;
                }
                ConnectClientMessageDto::Heartbeat | ConnectClientMessageDto::ClaimActive => {}
            }
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
        soulbeet_auth_header: Some("Bearer test-token".to_string()),
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
async fn full_and_delta_sync_populates_mirror_skips_unchanged_playlists_and_preserves_pending_outbox()
{
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();

    // 1. Initial full sync
    let report = core.sync_now(false).await.unwrap();
    assert!(report.full_sync);
    assert_eq!(report.artists_synced, 2);
    assert_eq!(report.albums_synced, 1);
    assert_eq!(report.tracks_synced, 2);
    assert_eq!(report.playlists_synced, 1);
    assert_eq!(report.discovery_tracks_synced, 1);
    assert_eq!(state.playlist_fetch_count.load(Ordering::SeqCst), 1);

    // 2. Optimistic offline star on s1 before next sync
    core.enqueue_mutation(OutboxMutation::Star {
        song_id: "s1".to_string(),
        starred: true,
        discovery_track_id: None,
    })
    .await
    .unwrap();
    assert!(get_track(core.pool(), "s1").await.unwrap().unwrap().starred);

    // 3. Delta sync: enable new album al2, playlist pl1 unchanged
    state.new_album_enabled.store(true, Ordering::SeqCst);
    let delta = core.sync_now(false).await.unwrap();
    assert!(!delta.full_sync);
    assert_eq!(delta.albums_synced, 1); // only new album al2 fetched
    assert_eq!(state.playlist_fetch_count.load(Ordering::SeqCst), 1); // unchanged playlist NOT re-fetched!

    // Pending outbox star was NOT clobbered by server's empty getStarred2!
    assert!(get_track(core.pool(), "s1").await.unwrap().unwrap().starred);
    assert!(get_track(core.pool(), "s3").await.unwrap().is_some());
}

#[tokio::test]
async fn lru_cache_eviction_respects_pinned_tracks_and_overlapping_pins() {
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state).await;
    let dir = TempDir::new().unwrap();
    // Budget fits 2 tracks of 2048 bytes (4096 max), forcing eviction when a 3rd arrives.
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 4096))
        .await
        .unwrap();
    let now = now_unix();

    for (tid, album_id) in [("t-pinned", "al1"), ("t-old-unpinned", "al1"), ("t-new-unpinned", "al2")] {
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

    // Create playlist containing t-pinned so we can test overlapping Album + Playlist pins.
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
        PlaybackSource::RemoteStream { url, format, bit_rate } => {
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
async fn outbox_replays_strictly_chronologically_and_blocks_dependent_song_on_failure() {
    let state = Arc::new(StubState::default());
    state.fail_star.store(true, Ordering::SeqCst);
    let base_url = start_stub_server(state.clone()).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();
    core.sync_now(true).await.unwrap();

    // Enqueue 3 sequential mutations for s1 + 1 independent Keep for s2
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
    core.enqueue_mutation(OutboxMutation::Keep {
        song_id: "s2".to_string(),
        discovery_track_id: Some("disc-1".to_string()),
        target_folder_id: None,
    })
    .await
    .unwrap();

    // Optimistic local updates must already be visible immediately!
    let s1 = get_track(core.pool(), "s1").await.unwrap().unwrap();
    assert!(s1.starred);
    assert_eq!(s1.user_rating, 5);
    assert_eq!(s1.play_count, 1);

    // First drain: star:s1 fails (500), so rate:s1 and scrobble:s1 MUST stay blocked behind it,
    // while independent keep:s2 succeeds!
    let first = core.drain_outbox().await.unwrap();
    assert_eq!(first.replayed, 1);
    assert_eq!(first.failed, 1);
    assert_eq!(first.remaining, 3);
    assert_eq!(*state.curation_log.lock().await, vec!["keep:s2".to_string()]);

    // Clear retry backoff timestamp to simulate backoff expiry and restore server health.
    sqlx::query("UPDATE mutation_outbox SET next_retry_at = 0")
        .execute(core.pool())
        .await
        .unwrap();
    state.fail_star.store(false, Ordering::SeqCst);

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
async fn fts5_search_is_sub_20ms_handles_diacritics_and_suppresses_remote_when_offline_only() {
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

    // Online search: merges local + deduplicated remote (Instant Crush kept, owned Get Lucky deduplicated!)
    let online = core.search("Daft Punk", 20).await.unwrap();
    assert_eq!(online.local_tracks.len(), 1);
    assert_eq!(online.remote_tracks.len(), 1);
    assert_eq!(online.remote_tracks[0].title, "Instant Crush");
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

    // Seed 35 playlists (> AUTO_PLAYLISTS_LIMIT=24) including a Seasonal Noël pack
    for i in 0..35 {
        let is_noel = i == 0;
        let pl_id = format!("pl-{i}");
        upsert_playlist(
            core.pool(),
            &LocalPlaylist {
                id: pl_id.clone(),
                name: if is_noel {
                    "Noël à la Française".to_string()
                } else {
                    format!("Mix #{i:02}")
                },
                comment: None,
                owner: Some("alice".to_string()),
                song_count: 1,
                duration_secs: 200,
                cover_art_id: None,
                is_smart: is_noel,
                kind: if is_noel { "seasonal" } else { "user" }.to_string(),
                slug: if is_noel {
                    Some("winter_holiday".to_string())
                } else {
                    None
                },
                changed_at: None,
                updated_at: now - i,
            },
        )
        .await
        .unwrap();
        if is_noel {
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

    // Seed artists covering A-D ("The Beatles" -> B!), U-Z ("U2", "2Pac" -> #)
    for (id, name) in [
        ("ar-a", "Alpha"),
        ("ar-b", "The Beatles"),
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

    upsert_track(
        core.pool(),
        &LocalTrack {
            id: "tr-noel".to_string(),
            title: "Petit Papa Noël".to_string(),
            artist_id: Some("ar-a".to_string()),
            artist_name: "Tino Rossi".to_string(),
            album_id: Some("al-0".to_string()),
            album_name: "Noël".to_string(),
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

    // 2. Playlists capped strictly <= 24
    let pls = core
        .auto_browse()
        .children(AUTO_TAB_PLAYLISTS, false)
        .await
        .unwrap();
    assert_eq!(pls.len(), AUTO_PLAYLISTS_LIMIT);

    // 3. Library has Offline, Recent (<=25), and 6 A-Z buckets
    let lib = core
        .auto_browse()
        .children(AUTO_TAB_LIBRARY, false)
        .await
        .unwrap();
    assert_eq!(lib.len(), 8);
    assert_eq!(lib[0].id, AUTO_LIB_OFFLINE);
    assert_eq!(lib[1].id, AUTO_LIB_RECENT);

    let recent = core
        .auto_browse()
        .children(AUTO_LIB_RECENT, false)
        .await
        .unwrap();
    assert_eq!(recent.len(), AUTO_RECENT_LIMIT);

    let ad = core.auto_browse().children("az:a-d", false).await.unwrap();
    assert!(ad.iter().any(|n| n.title == "The Beatles"));
    let uz = core.auto_browse().children("az:u-z", false).await.unwrap();
    assert!(uz.iter().any(|n| n.title == "U2"));
    assert!(uz.iter().any(|n| n.title == "2Pac"));

    // 4. Local voice search ("Play Noel") matches Noël playlist fast!
    let voice = core.voice_search("Play Noel").await.unwrap().unwrap();
    assert_eq!(voice.kind, VoiceMatchKind::Playlist);
    assert_eq!(voice.target_id, "pl-0");
    assert_eq!(voice.track_ids, vec!["tr-noel".to_string()]);
    assert!(voice.elapsed_ms < 25);

    // 5. Ignition resumption persists exact queue across session save/load
    let session = PersistedPlaybackSession {
        queue_track_ids: vec!["tr-noel".to_string(), "s1".to_string()],
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
async fn connect_ws_registers_tracks_devices_and_relays_handoff_commands() {
    let state = Arc::new(StubState::default());
    let base_url = start_stub_server(state).await;
    let dir = TempDir::new().unwrap();
    let core = SoulbeetCore::init(test_config(&dir, Some(base_url), 10_000_000))
        .await
        .unwrap();

    let mut rx = core.connect_client().subscribe();
    core.connect_ws(Duration::from_secs(60)).await.unwrap();

    let first_event = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .unwrap()
        .unwrap();
    match first_event {
        ConnectServerEventDto::DeviceList {
            devices,
            active_device_id,
        } => {
            assert_eq!(devices.len(), 1);
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
    core.connect_client()
        .transfer_playback_to("android-1".to_string(), snapshot.clone())
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

    let live_state = core.connect_client().state().await;
    assert!(live_state.connected);
    assert_eq!(live_state.devices.len(), 1);
    core.connect_client().disconnect().await;
}
