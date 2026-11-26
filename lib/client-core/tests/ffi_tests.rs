use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message as AxumWsMsg, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::Notify;

use client_core::db::{
    now_unix, replace_playlist_tracks, upsert_album, upsert_artist, upsert_discovery_track,
    upsert_playlist, upsert_track,
};
use client_core::ffi::{
    default_cache_policy, CoreEventListener, FfiAudioQualityProfile, FfiAutoNodeKind,
    FfiCachePolicy, FfiCacheStats, FfiClientConfig, FfiConnectCommand, FfiConnectEvent,
    FfiConnectState, FfiDevicePlatform, FfiDrainReport, FfiError, FfiNavidromeCredentials,
    FfiOutboxMutation, FfiPersistedPlaybackSession, FfiPinTarget, FfiPlaybackSnapshot,
    FfiPlaybackSource, FfiSoulbeetAuth, FfiSyncReport, FfiVoiceMatchKind, SoulbeetCoreHandle,
};
use client_core::models::{
    LocalAlbum, LocalArtist, LocalDiscoveryTrack, LocalPlaylist, LocalTrack,
};

fn test_config(tmp: &TempDir, base_url: Option<String>, offline_only: bool) -> FfiClientConfig {
    let navidrome = base_url.as_ref().map(|u| FfiNavidromeCredentials {
        base_url: u.clone(),
        username: "alice".to_string(),
        subsonic_salt: "salt123".to_string(),
        subsonic_token: "tok123".to_string(),
        api_version: "1.16.1".to_string(),
        client_name: "Soulbeet".to_string(),
    });
    FfiClientConfig {
        db_path: tmp.path().join("client.db").display().to_string(),
        cache_dir: tmp.path().join("audio").display().to_string(),
        cover_art_dir: tmp.path().join("art").display().to_string(),
        soulbeet_base_url: base_url,
        soulbeet_auth: FfiSoulbeetAuth::Bearer {
            token: "initial-jwt".to_string(),
        },
        navidrome,
        device_id: "android-pixel-1".to_string(),
        device_name: "Pixel 9 Pro".to_string(),
        platform: FfiDevicePlatform::Android,
        offline_only,
        cache_policy: default_cache_policy(),
    }
}

async fn seed_sample_library(handle: &SoulbeetCoreHandle) {
    let pool = handle.core().pool();
    let now = now_unix();
    upsert_artist(
        pool,
        &LocalArtist {
            id: "art-1".into(),
            name: "Daft Punk".into(),
            album_count: 1,
            cover_art_id: Some("ca-art-1".into()),
            starred: true,
            musicbrainz_id: Some("mbid-dp".into()),
            updated_at: now,
        },
    )
    .await
    .unwrap();

    upsert_album(
        pool,
        &LocalAlbum {
            id: "alb-1".into(),
            name: "Random Access Memories".into(),
            artist_id: Some("art-1".into()),
            artist_name: "Daft Punk".into(),
            year: Some(2013),
            genre: Some("Electronic".into()),
            song_count: 2,
            duration_secs: 600,
            cover_art_id: Some("ca-alb-1".into()),
            starred: false,
            created_at: Some("2026-01-01T00:00:00Z".into()),
            updated_at: now,
        },
    )
    .await
    .unwrap();

    for (id, title, track_no) in [("trk-1", "Get Lucky", 1), ("trk-2", "Instant Crush", 2)] {
        upsert_track(
            pool,
            &LocalTrack {
                id: id.into(),
                title: title.into(),
                artist_id: Some("art-1".into()),
                artist_name: "Daft Punk".into(),
                album_id: Some("alb-1".into()),
                album_name: "Random Access Memories".into(),
                track_number: Some(track_no),
                disc_number: Some(1),
                duration_secs: 300,
                year: Some(2013),
                genre: Some("Electronic".into()),
                bit_rate: Some(1000),
                suffix: Some("flac".into()),
                content_type: Some("audio/flac".into()),
                size_bytes: Some(30_000_000),
                cover_art_id: Some("ca-alb-1".into()),
                path: Some(format!("Daft Punk/RAM/{title}.flac")),
                starred: false,
                user_rating: 0,
                play_count: 3,
                last_played_at: None,
                musicbrainz_id: None,
                is_discovery: id == "trk-2",
                updated_at: now,
            },
            false,
        )
        .await
        .unwrap();
    }

    upsert_playlist(
        pool,
        &LocalPlaylist {
            id: "pl-1".into(),
            name: "Drive Mix".into(),
            comment: Some("moment:drive".into()),
            owner: Some("alice".into()),
            song_count: 2,
            duration_secs: 600,
            cover_art_id: Some("ca-alb-1".into()),
            is_smart: false,
            kind: "moment".into(),
            slug: Some("drive-mix".into()),
            changed_at: Some("2026-01-02T00:00:00Z".into()),
            updated_at: now,
        },
    )
    .await
    .unwrap();
    replace_playlist_tracks(pool, "pl-1", &["trk-1".into(), "trk-2".into()])
        .await
        .unwrap();

    upsert_discovery_track(
        pool,
        &LocalDiscoveryTrack {
            id: "disc-1".into(),
            song_id: Some("trk-2".into()),
            artist: "Daft Punk".into(),
            title: "Instant Crush".into(),
            album: Some("Random Access Memories".into()),
            folder_path: Some("Discovery/Balanced/Instant Crush.flac".into()),
            profile_slug: Some("balanced".into()),
            mix_slug: Some("weekly-radar".into()),
            status: "pending".into(),
            created_at: Some("2026-01-01T00:00:00Z".into()),
            expires_at: None,
            reason_json: None,
            exclude_from_core_taste: false,
            updated_at: now,
        },
        false,
    )
    .await
    .unwrap();
}

#[test]
fn non_tokio_thread_sync_and_async_calls_never_panic() {
    // Spawn a plain OS thread with ZERO Tokio runtime entered on it.
    let join = std::thread::spawn(|| {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp, None, true);

        // 1. Synchronous constructor + sync methods on a non-Tokio thread.
        let handle = SoulbeetCoreHandle::open_sync(cfg.clone()).unwrap();
        assert!(handle.is_offline_only());
        handle.set_offline_only(false);
        assert!(!handle.is_offline_only());
        handle.set_offline_only(true);

        let tabs = handle.auto_root_tabs();
        assert_eq!(tabs.len(), 4);
        assert_eq!(tabs[0].kind, FfiAutoNodeKind::Tab);

        // Zero-network sync cover-art miss.
        let miss = handle
            .cover_art_local_path_sync("missing-art".to_string())
            .unwrap();
        assert!(miss.is_none());

        // 2. Poll UniFFI async methods using a non-Tokio executor (`futures::executor::block_on`)
        //    proving our internal OwnedRuntime runs all Tokio I/O & timers safely.
        let stats = futures::executor::block_on(handle.cache_stats()).unwrap();
        assert_eq!(stats.cached_track_count, 0);

        let lib_stats = futures::executor::block_on(handle.library_stats()).unwrap();
        assert_eq!(lib_stats.track_count, 0);

        // Async constructor polled on a non-Tokio executor.
        let tmp2 = TempDir::new().unwrap();
        let handle2 = futures::executor::block_on(SoulbeetCoreHandle::open(test_config(
            &tmp2, None, false,
        )))
        .unwrap();
        futures::executor::block_on(handle2.set_soulbeet_auth(FfiSoulbeetAuth::SessionCookie {
            token: "cookie-val".to_string(),
        }))
        .unwrap();
        futures::executor::block_on(handle2.set_soulbeet_auth(FfiSoulbeetAuth::None)).unwrap();

        let session = FfiPersistedPlaybackSession {
            queue_track_ids: vec!["trk-1".into(), "trk-2".into()],
            current_index: 1,
            position_ms: 42_500,
            is_playing: true,
            source_label: Some("Drive Mix".into()),
            updated_at: 1_700_000_000,
        };
        futures::executor::block_on(handle2.save_playback_session(session.clone())).unwrap();
        let loaded = futures::executor::block_on(handle2.load_playback_session())
            .unwrap()
            .expect("session should persist");
        assert_eq!(loaded, session);
    });

    join.join()
        .expect("non-Tokio thread must not panic with missing reactor");
}

#[tokio::test]
async fn library_queries_auto_browse_voice_search_and_outbox_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let handle = SoulbeetCoreHandle::open(test_config(&tmp, None, false))
        .await
        .unwrap();
    seed_sample_library(&handle).await;

    // Library queries
    let track = handle
        .get_track("trk-1".into())
        .await
        .unwrap()
        .expect("trk-1 exists");
    assert_eq!(track.title, "Get Lucky");
    assert!(!track.starred);

    let by_ids = handle
        .get_tracks_by_ids(vec!["trk-1".into(), "trk-2".into()])
        .await
        .unwrap();
    assert_eq!(by_ids.len(), 2);

    let album_tracks = handle.list_tracks_by_album("alb-1".into()).await.unwrap();
    assert_eq!(album_tracks.len(), 2);

    let pl_tracks = handle.list_tracks_by_playlist("pl-1".into()).await.unwrap();
    assert_eq!(pl_tracks.len(), 2);

    let playlists = handle.list_playlists().await.unwrap();
    assert_eq!(playlists.len(), 1);
    assert_eq!(playlists[0].name, "Drive Mix");

    let triage = handle.list_discovery_triage(true).await.unwrap();
    assert_eq!(triage.len(), 1);
    assert_eq!(triage[0].title, "Instant Crush");

    let stats = handle.library_stats().await.unwrap();
    assert_eq!(stats.artist_count, 1);
    assert_eq!(stats.album_count, 1);
    assert_eq!(stats.track_count, 2);
    assert_eq!(stats.playlist_count, 1);
    assert_eq!(stats.pending_discovery_count, 1);

    // Android Auto browse & voice search
    let for_you = handle.auto_children("tab:for_you".into()).await.unwrap();
    assert!(for_you.iter().any(|n| n.id == "for_you:triage"));
    assert!(for_you.iter().any(|n| n.id == "playlist:pl-1"));

    let voice = handle
        .voice_search("Play Drive Mix".into())
        .await
        .unwrap()
        .expect("voice search should match Drive Mix");
    assert_eq!(voice.kind, FfiVoiceMatchKind::Playlist);
    assert_eq!(voice.target_id, "pl-1");
    assert_eq!(voice.track_ids, vec!["trk-1".to_string(), "trk-2".to_string()]);

    // Outbox mutations (Star, Unstar, Rate, Keep, Drop, Scrobble)
    let seq1 = handle
        .enqueue_mutation(FfiOutboxMutation::Star {
            song_id: "trk-1".into(),
            discovery_track_id: None,
        })
        .await
        .unwrap();
    assert!(seq1 > 0);
    assert!(handle.get_track("trk-1".into()).await.unwrap().unwrap().starred);

    handle
        .enqueue_mutation(FfiOutboxMutation::Unstar {
            song_id: "trk-1".into(),
            discovery_track_id: None,
        })
        .await
        .unwrap();
    assert!(!handle.get_track("trk-1".into()).await.unwrap().unwrap().starred);

    handle
        .enqueue_mutation(FfiOutboxMutation::Rate {
            song_id: "trk-1".into(),
            rating: 5,
            discovery_track_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        handle.get_track("trk-1".into()).await.unwrap().unwrap().user_rating,
        5
    );

    handle
        .enqueue_mutation_with_key(
            FfiOutboxMutation::Keep {
                song_id: "trk-2".into(),
                discovery_track_id: Some("disc-1".into()),
                target_folder_id: None,
            },
            "keep-trk-2-idempotent".into(),
        )
        .await
        .unwrap();
    // Duplicate idempotency key is a no-op
    handle
        .enqueue_mutation_with_key(
            FfiOutboxMutation::Keep {
                song_id: "trk-2".into(),
                discovery_track_id: Some("disc-1".into()),
                target_folder_id: None,
            },
            "keep-trk-2-idempotent".into(),
        )
        .await
        .unwrap();

    let summary = handle.outbox_summary().await.unwrap();
    assert_eq!(summary.pending_count, 4);
    assert_eq!(summary.dead_letter_count, 0);
    assert!(summary.oldest_pending_at.is_some());

    // Pinning & list_pinned_targets
    let missing = handle
        .pin(FfiPinTarget::Playlist { id: "pl-1".into() })
        .await
        .unwrap();
    assert_eq!(missing.len(), 2);
    let pins = handle.list_pinned_targets().await.unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].target, FfiPinTarget::Playlist { id: "pl-1".into() });

    // Error mapping checks
    handle.set_offline_only(true);
    let err = handle.sync_now(false).await.unwrap_err();
    assert!(matches!(err, FfiError::Offline { .. }));

    let err = handle
        .resolve_playback_source("trk-1".into(), FfiAudioQualityProfile::LosslessFlac)
        .await
        .unwrap_err();
    assert!(matches!(err, FfiError::Offline { .. }));

    handle.set_offline_only(false);
    let err = handle
        .resolve_playback_source("trk-1".into(), FfiAudioQualityProfile::LosslessFlac)
        .await
        .unwrap_err();
    assert!(matches!(err, FfiError::NotConfigured { .. }));

    // Drop inside #[tokio::test] runtime context must NEVER panic!
    drop(handle);
}

#[tokio::test]
async fn cover_art_local_path_sync_hit_miss_and_stale_pruning() {
    let tmp = TempDir::new().unwrap();
    let handle = SoulbeetCoreHandle::open(test_config(&tmp, None, true))
        .await
        .unwrap();

    // Miss on empty cache
    assert!(handle
        .cover_art_local_path_sync("ca-1".into())
        .unwrap()
        .is_none());

    // Create backing image + DB cache entry directly (simulating prior fetch)
    let art_dir = tmp.path().join("art");
    std::fs::create_dir_all(&art_dir).unwrap();
    let art_file = art_dir.join("ca-1.jpg");
    std::fs::write(&art_file, b"\xff\xd8\xff\xe0FAKEJPEG").unwrap();
    let now = now_unix();
    sqlx::query(
        "INSERT INTO cover_art_cache (cover_art_id, local_path, content_type, size_bytes, downloaded_at, last_accessed_at) VALUES (?, ?, 'image/jpeg', 12, ?, ?)",
    )
    .bind("ca-1")
    .bind(art_file.display().to_string())
    .bind(now)
    .bind(now)
    .execute(handle.core().pool())
    .await
    .unwrap();

    // Sync call from inside Tokio test context (uses bridge thread safely)
    let found = handle
        .cover_art_local_path_sync("ca-1".into())
        .unwrap()
        .expect("cover art should be found locally");
    assert_eq!(found, art_file.display().to_string());

    // Sync call from non-Tokio thread (fast path)
    let h_clone = handle.clone();
    let found_from_os_thread = std::thread::spawn(move || {
        h_clone.cover_art_local_path_sync("ca-1".into()).unwrap()
    })
    .join()
    .unwrap();
    assert_eq!(found_from_os_thread, Some(art_file.display().to_string()));

    // Remove backing file -> stale row pruned and None returned
    std::fs::remove_file(&art_file).unwrap();
    assert!(handle
        .cover_art_local_path_sync("ca-1".into())
        .unwrap()
        .is_none());
}

// ============================================================================
// Mock Server + Full Event Listener & Re-entrancy Deadlock Tests
// ============================================================================

#[derive(Clone, Default)]
struct MockServerState {
    received_curation: Arc<Mutex<Vec<String>>>,
}

async fn start_mock_server() -> (String, MockServerState) {
    let state = MockServerState::default();
    let app = Router::new()
        .route("/api/client/bootstrap", get(mock_bootstrap))
        .route("/api/discovery/triage", get(mock_triage))
        .route("/api/curation/star", post(mock_curation_star))
        .route("/api/ws/connect", get(mock_connect_ws))
        .route("/rest/getArtists", get(mock_get_artists))
        .route("/rest/getAlbumList2", get(mock_get_album_list2))
        .route("/rest/getAlbum", get(mock_get_album))
        .route("/rest/getPlaylists", get(mock_get_playlists))
        .route("/rest/getStarred2", get(mock_get_starred2))
        .route("/rest/getGenres", get(mock_get_genres))
        .route("/rest/download", get(mock_download))
        .route("/rest/getCoverArt", get(mock_cover_art))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state)
}

async fn mock_bootstrap() -> Json<Value> {
    Json(json!({
        "user_id": "u-1",
        "username": "alice",
        "is_admin": false,
        "navidrome": null,
        "promote_threshold": 4,
        "auto_delete_enabled": false,
        "discovery_enabled": true
    }))
}

async fn mock_triage() -> Json<Value> {
    Json(json!([]))
}

async fn mock_curation_star(
    State(st): State<MockServerState>,
    Json(body): Json<Value>,
) -> StatusCode {
    if let Some(song_id) = body.pointer("/req/song_id").and_then(|v| v.as_str()) {
        st.received_curation.lock().unwrap().push(song_id.to_string());
    }
    StatusCode::OK
}

async fn mock_get_artists() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "artists": {
                "index": [{
                    "artist": [{
                        "id": "art-1",
                        "name": "Daft Punk",
                        "albumCount": 1
                    }]
                }]
            }
        }
    }))
}

async fn mock_get_album_list2() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "albumList2": {
                "album": [{
                    "id": "alb-1",
                    "name": "Discovery",
                    "artist": "Daft Punk",
                    "artistId": "art-1",
                    "songCount": 1,
                    "duration": 320,
                    "created": "2026-01-01T00:00:00Z"
                }]
            }
        }
    }))
}

async fn mock_get_album() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "album": {
                "id": "alb-1",
                "name": "Discovery",
                "artist": "Daft Punk",
                "artistId": "art-1",
                "songCount": 1,
                "duration": 320,
                "song": [{
                    "id": "trk-1",
                    "title": "One More Time",
                    "artist": "Daft Punk",
                    "artistId": "art-1",
                    "album": "Discovery",
                    "albumId": "alb-1",
                    "duration": 320,
                    "coverArt": "ca-alb-1"
                }]
            }
        }
    }))
}

async fn mock_get_playlists() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "playlists": { "playlist": [] }
        }
    }))
}

async fn mock_get_starred2() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "starred2": { "artist": [], "album": [], "song": [] }
        }
    }))
}

async fn mock_get_genres() -> Json<Value> {
    Json(json!({
        "subsonic-response": {
            "status": "ok",
            "genres": { "genre": [{ "value": "Electronic", "songCount": 1, "albumCount": 1 }] }
        }
    }))
}

async fn mock_download(Query(params): Query<HashMap<String, String>>) -> impl IntoResponse {
    if params.get("id").map(String::as_str) == Some("bad-subsonic") {
        return (
            StatusCode::OK,
            [("content-type", "application/json")],
            r#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"Song not found"}}}"#.as_bytes().to_vec(),
        );
    }
    (
        StatusCode::OK,
        [("content-type", "audio/flac")],
        b"fLaCFAKEAUDIOBYTES1234567890".to_vec(),
    )
}

async fn mock_cover_art() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "image/jpeg")],
        b"\xff\xd8\xff\xe0COVERARTBYTES".to_vec(),
    )
}

async fn mock_connect_ws(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move {
        // Send DeviceList + Command immediately after connection
        let device_list = json!({
            "type": "device_list",
            "devices": [{
                "device_id": "android-pixel-1",
                "name": "Pixel 9 Pro",
                "platform": "android",
                "is_active": true,
                "last_seen_unix": 1_700_000_000i64,
                "snapshot": null
            }],
            "active_device_id": "android-pixel-1"
        });
        let _ = socket
            .send(AxumWsMsg::Text(device_list.to_string().into()))
            .await;

        let cmd = json!({
            "type": "command",
            "from_device_id": "desktop-1",
            "command": { "type": "play" }
        });
        let _ = socket.send(AxumWsMsg::Text(cmd.to_string().into())).await;

        // Keep open briefly to receive client messages, then exit cleanly
        while let Some(Ok(msg)) = socket.recv().await {
            if matches!(msg, AxumWsMsg::Close(_)) {
                break;
            }
        }
    })
}

struct ReentrantListener {
    handle: Arc<SoulbeetCoreHandle>,
    sync_count: Arc<AtomicUsize>,
    outbox_count: Arc<AtomicUsize>,
    cache_count: Arc<AtomicUsize>,
    connect_state_count: Arc<AtomicUsize>,
    connect_event_count: Arc<AtomicUsize>,
    saw_command_play: Arc<AtomicBool>,
    reentered_ok: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CoreEventListener for ReentrantListener {
    fn on_connect_state_changed(&self, _state: FfiConnectState) {
        // Re-enter handle synchronously from inside the callback!
        let _ = self.handle.is_offline_only();
        let _ = self.handle.auto_root_tabs();
        self.reentered_ok.store(true, Ordering::SeqCst);
        self.connect_state_count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn on_connect_event(&self, event: FfiConnectEvent) {
        // Re-enter handle's blocking SQLite method from inside the callback!
        let _ = self.handle.cover_art_local_path_sync("ca-alb-1".into());
        if matches!(
            event,
            FfiConnectEvent::Command {
                command: FfiConnectCommand::Play,
                ..
            }
        ) {
            self.saw_command_play.store(true, Ordering::SeqCst);
        }
        self.connect_event_count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn on_sync_completed(&self, _report: FfiSyncReport) {
        let _ = self.handle.cover_art_local_path_sync("ca-alb-1".into());
        self.sync_count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn on_outbox_drained(&self, _report: FfiDrainReport) {
        let _ = self.handle.is_offline_only();
        self.outbox_count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn on_cache_changed(&self, _stats: FfiCacheStats) {
        let _ = self.handle.is_offline_only();
        self.cache_count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

#[tokio::test]
async fn event_listener_callbacks_fire_and_reentrancy_never_deadlocks() {
    let (base_url, mock_state) = start_mock_server().await;
    let tmp = TempDir::new().unwrap();
    let cfg = test_config(&tmp, Some(base_url.clone()), false);
    let handle = SoulbeetCoreHandle::open(cfg).await.unwrap();

    // Ensure Navidrome creds stay set even after bootstrap returns null navidrome by testing
    // set_navidrome_credentials + sync without bootstrap overriding, OR wait:
    // mock_bootstrap returns navidrome: null which overwrites navidrome in fetch_bootstrap!
    // Let's point soulbeet_base_url to base_url AND make sure navidrome stays configured:
    // Actually let's test set_navidrome_credentials after fetch_bootstrap or make a config
    // where soulbeet_base_url is set and we check both!
    let sync_count = Arc::new(AtomicUsize::new(0));
    let outbox_count = Arc::new(AtomicUsize::new(0));
    let cache_count = Arc::new(AtomicUsize::new(0));
    let connect_state_count = Arc::new(AtomicUsize::new(0));
    let connect_event_count = Arc::new(AtomicUsize::new(0));
    let saw_command_play = Arc::new(AtomicBool::new(false));
    let reentered_ok = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());

    handle.set_event_listener(Box::new(ReentrantListener {
        handle: handle.clone(),
        sync_count: sync_count.clone(),
        outbox_count: outbox_count.clone(),
        cache_count: cache_count.clone(),
        connect_state_count: connect_state_count.clone(),
        connect_event_count: connect_event_count.clone(),
        saw_command_play: saw_command_play.clone(),
        reentered_ok: reentered_ok.clone(),
        notify: notify.clone(),
    }));

    // 1. Download track + cover art -> fires on_cache_changed + verifies playback source switches to LocalFile
    let src_remote = handle
        .resolve_playback_source("trk-1".into(), FfiAudioQualityProfile::CellularOpus160)
        .await
        .unwrap();
    assert!(matches!(src_remote, FfiPlaybackSource::StreamUrl { .. }));

    let dl = handle
        .download_track(
            "trk-1".into(),
            FfiAudioQualityProfile::LosslessFlac,
            true,
            Some("manual".into()),
        )
        .await
        .unwrap();
    assert_eq!(dl.track_id, "trk-1");
    assert!(dl.pinned);

    let src_local = handle
        .resolve_playback_source("trk-1".into(), FfiAudioQualityProfile::LosslessFlac)
        .await
        .unwrap();
    assert!(matches!(src_local, FfiPlaybackSource::LocalFile { .. }));

    // Subsonic HTTP 200 JSON error mapped cleanly to FfiError::Subsonic
    let sub_err = handle
        .download_track(
            "bad-subsonic".into(),
            FfiAudioQualityProfile::LosslessFlac,
            false,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(sub_err, FfiError::Subsonic { code: 70, .. }));

    // Cover art fetch -> local path lookup works synchronously
    let art_path = handle
        .get_or_fetch_cover_art("ca-alb-1".into(), Some(300))
        .await
        .unwrap();
    assert_eq!(
        handle.cover_art_local_path_sync("ca-alb-1".into()).unwrap(),
        Some(art_path)
    );

    // 2. Outbox enqueue & drain -> fires on_outbox_drained
    handle
        .enqueue_mutation(FfiOutboxMutation::Star {
            song_id: "trk-1".into(),
            discovery_track_id: None,
        })
        .await
        .unwrap();
    let drain = handle.drain_outbox().await.unwrap();
    assert_eq!(drain.replayed, 1);
    assert_eq!(
        mock_state.received_curation.lock().unwrap().as_slice(),
        &["trk-1"]
    );

    // 3. Connect WS -> fires on_connect_state_changed & on_connect_event
    handle.connect_ws(5).await.unwrap();
    handle
        .connect_update_state(FfiPlaybackSnapshot {
            song_id: Some("trk-1".into()),
            title: Some("One More Time".into()),
            artist: Some("Daft Punk".into()),
            album: Some("Discovery".into()),
            cover_art_id: Some("ca-alb-1".into()),
            is_playing: true,
            position_ms: 12_000,
            duration_ms: 320_000,
            queue_song_ids: vec!["trk-1".into()],
            queue_index: 0,
            volume: 0.9,
            is_pending_discovery: false,
            updated_at_unix: 1_700_000_010,
        })
        .await
        .unwrap();
    handle.connect_claim_active().await.unwrap();

    // Wait for events to be dispatched
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if cache_count.load(Ordering::SeqCst) >= 1
            && outbox_count.load(Ordering::SeqCst) >= 1
            && connect_state_count.load(Ordering::SeqCst) >= 1
            && saw_command_play.load(Ordering::SeqCst)
        {
            break;
        }
        let _ = tokio::time::timeout(Duration::from_millis(50), notify.notified()).await;
    }

    assert!(cache_count.load(Ordering::SeqCst) >= 1, "on_cache_changed must fire");
    assert!(outbox_count.load(Ordering::SeqCst) >= 1, "on_outbox_drained must fire");
    assert!(
        connect_state_count.load(Ordering::SeqCst) >= 1,
        "on_connect_state_changed must fire"
    );
    assert!(
        connect_event_count.load(Ordering::SeqCst) >= 2,
        "on_connect_event must receive DeviceList + Command"
    );
    assert!(saw_command_play.load(Ordering::SeqCst));
    assert!(reentered_ok.load(Ordering::SeqCst));

    let st = handle.connect_state().await.unwrap();
    assert!(st.connected);
    assert_eq!(st.active_device_id.as_deref(), Some("android-pixel-1"));

    // 4. Sync test with navidrome-only handle (so mock_bootstrap's null navidrome doesn't wipe creds)
    let tmp_sync = TempDir::new().unwrap();
    let mut sync_cfg = test_config(&tmp_sync, Some(base_url), false);
    sync_cfg.soulbeet_base_url = None;
    let sync_handle = SoulbeetCoreHandle::open(sync_cfg).await.unwrap();
    let sync_notify = Arc::new(Notify::new());
    let sync_fired = Arc::new(AtomicUsize::new(0));
    sync_handle.set_event_listener(Box::new(ReentrantListener {
        handle: sync_handle.clone(),
        sync_count: sync_fired.clone(),
        outbox_count: Arc::new(AtomicUsize::new(0)),
        cache_count: Arc::new(AtomicUsize::new(0)),
        connect_state_count: Arc::new(AtomicUsize::new(0)),
        connect_event_count: Arc::new(AtomicUsize::new(0)),
        saw_command_play: Arc::new(AtomicBool::new(false)),
        reentered_ok: Arc::new(AtomicBool::new(false)),
        notify: sync_notify.clone(),
    }));
    let rep = sync_handle.sync_now(true).await.unwrap();
    assert_eq!(rep.artists_synced, 1);
    assert_eq!(rep.albums_synced, 1);
    assert_eq!(rep.tracks_synced, 1);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && sync_fired.load(Ordering::SeqCst) == 0 {
        let _ = tokio::time::timeout(Duration::from_millis(50), sync_notify.notified()).await;
    }
    assert_eq!(sync_fired.load(Ordering::SeqCst), 1);

    // 5. Cache policy round-trip + clear listener + disconnect
    let custom_policy = FfiCachePolicy {
        max_bytes: 512 * 1024 * 1024,
        cover_art_max_bytes: 32 * 1024 * 1024,
        wifi_only_auto_cache: false,
        prefetch_queue_count: 5,
        auto_pin_starred: true,
        auto_pin_weekly_radar: false,
        auto_pin_active_seasonal: false,
    };
    handle.set_cache_policy(custom_policy.clone()).await.unwrap();
    assert_eq!(handle.cache_policy().await.unwrap(), custom_policy);

    handle.disconnect_ws().await.unwrap();
    assert!(!handle.connect_state().await.unwrap().connected);

    // Clear listener before dropping handle (breaks Arc cycle in test listener cleanly)
    handle.clear_event_listener();
    sync_handle.clear_event_listener();
}
