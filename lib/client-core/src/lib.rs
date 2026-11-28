pub mod auto_browse;
pub mod cache;
pub mod connect;
pub mod db;
pub mod error;
pub mod ffi;
pub mod models;
pub mod outbox;
pub mod search;
pub mod subsonic;
pub mod sync;

uniffi::setup_scaffolding!();

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use shared::metadata::CatalogProvider;
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::auto_browse::AutoBrowseEngine;
use crate::cache::CacheManager;
use crate::connect::{ConnectClient, ConnectState};
use crate::error::{CoreError, CoreResult};
use crate::models::{
    AudioQualityProfile, AutoBrowseNode, CachePolicy, CacheStats, ClientBootstrapDto, ClientConfig,
    ConnectCommandDto, DrainReport, LibraryStats, LocalAlbum, LocalArtist, LocalDiscoveryTrack,
    LocalPlaylist, LocalTrack, NavidromeCredentials, OfflineTrackEntry, OutboxMutation, OutboxRow,
    OutboxSummary, PersistedPlaybackSession, PinRecord, PinTarget, PlaybackSnapshotDto,
    PlaybackSource, RemoteAlbumDetail, RemoteArtistDetail, RemoteCatalogAlbum,
    RemoteCatalogResults, SearchResults, SoulbeetAuth, SyncReport, VoiceMatchResult,
};
use crate::outbox::MutationOutbox;
use crate::search::SearchEngine;
use crate::sync::SyncEngine;

/// Unified offline-first Soulbeet client engine shared across Desktop and Android (via UniFFI).
pub struct SoulbeetCore {
    pool: SqlitePool,
    config: ClientConfig,
    soulbeet_auth: Arc<RwLock<Option<SoulbeetAuth>>>,
    navidrome: Arc<RwLock<Option<NavidromeCredentials>>>,
    offline_only: AtomicBool,
    sync: SyncEngine,
    cache: CacheManager,
    outbox: MutationOutbox,
    search: SearchEngine,
    auto_browse: AutoBrowseEngine,
    connect: ConnectClient,
}

impl SoulbeetCore {
    pub async fn init(config: ClientConfig) -> CoreResult<Self> {
        let pool = db::open_pool(&config.db_path).await?;
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .build()?;

        let soulbeet_auth = Arc::new(RwLock::new(config.soulbeet_auth.clone()));
        let navidrome = Arc::new(RwLock::new(config.navidrome.clone()));
        let sync = SyncEngine::new(
            pool.clone(),
            http.clone(),
            config.soulbeet_base_url.clone(),
            soulbeet_auth.clone(),
            navidrome.clone(),
        );
        let cache = CacheManager::new(
            pool.clone(),
            http.clone(),
            config.cache_dir.clone(),
            config.cover_art_dir.clone(),
            config.cache_policy.clone(),
            navidrome.clone(),
        );
        cache.sweep_orphaned_parts().await;
        let outbox = MutationOutbox::new(
            pool.clone(),
            http.clone(),
            config.soulbeet_base_url.clone(),
            soulbeet_auth.clone(),
            navidrome.clone(),
        );
        let search = SearchEngine::new(
            pool.clone(),
            http,
            config.soulbeet_base_url.clone(),
            soulbeet_auth.clone(),
        );
        let auto_browse = AutoBrowseEngine::new(pool.clone());
        let connect = ConnectClient::new(
            config.device_id.clone(),
            config.device_name.clone(),
            config.platform,
            soulbeet_auth.clone(),
        );
        let offline_only = AtomicBool::new(config.offline_only);

        Ok(Self {
            pool,
            config,
            soulbeet_auth,
            navidrome,
            offline_only,
            sync,
            cache,
            outbox,
            search,
            auto_browse,
            connect,
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn sync_engine(&self) -> &SyncEngine {
        &self.sync
    }

    pub fn cache(&self) -> &CacheManager {
        &self.cache
    }

    pub fn outbox(&self) -> &MutationOutbox {
        &self.outbox
    }

    pub fn search_engine(&self) -> &SearchEngine {
        &self.search
    }

    pub fn auto_browse(&self) -> &AutoBrowseEngine {
        &self.auto_browse
    }

    pub fn connect_client(&self) -> &ConnectClient {
        &self.connect
    }

    pub fn is_offline_only(&self) -> bool {
        self.offline_only.load(Ordering::Relaxed)
    }

    pub fn set_offline_only(&self, offline_only: bool) {
        self.offline_only.store(offline_only, Ordering::Relaxed);
    }

    pub async fn set_soulbeet_auth(&self, auth: Option<SoulbeetAuth>) {
        *self.soulbeet_auth.write().await = auth;
    }

    pub async fn set_navidrome_credentials(&self, creds: NavidromeCredentials) {
        *self.navidrome.write().await = Some(creds);
    }

    pub async fn clear_navidrome_credentials(&self) {
        *self.navidrome.write().await = None;
    }

    pub async fn fetch_bootstrap(&self) -> CoreResult<Option<ClientBootstrapDto>> {
        if self.is_offline_only() {
            return Err(CoreError::Offline(
                "bootstrap disabled in offline_only mode".into(),
            ));
        }
        self.sync.fetch_bootstrap().await
    }

    /// Runs sync against Soulbeet + Navidrome (refused if `offline_only == true`).
    pub async fn sync_now(&self, force_full: bool) -> CoreResult<SyncReport> {
        if self.is_offline_only() {
            return Err(CoreError::Offline("sync disabled in offline_only mode".into()));
        }
        self.sync.sync(force_full).await
    }

    pub async fn sync_now_cancellable(
        &self,
        force_full: bool,
        cancelled: &AtomicBool,
    ) -> CoreResult<SyncReport> {
        if self.is_offline_only() {
            return Err(CoreError::Offline("sync disabled in offline_only mode".into()));
        }
        self.sync.sync_cancellable(force_full, cancelled).await
    }

    // --- Local Library Queries (Zero Network) ---

    pub async fn get_track(&self, track_id: &str) -> CoreResult<Option<LocalTrack>> {
        db::get_track(&self.pool, track_id).await
    }

    pub async fn get_tracks_by_ids(&self, ids: &[String]) -> CoreResult<Vec<LocalTrack>> {
        db::get_tracks_by_ids(&self.pool, ids).await
    }

    pub async fn get_album(&self, album_id: &str) -> CoreResult<Option<LocalAlbum>> {
        db::get_album(&self.pool, album_id).await
    }

    pub async fn get_artist(&self, artist_id: &str) -> CoreResult<Option<LocalArtist>> {
        db::get_artist(&self.pool, artist_id).await
    }

    pub async fn list_artists(&self) -> CoreResult<Vec<LocalArtist>> {
        db::list_artists(&self.pool).await
    }

    pub async fn list_albums(&self, artist_id: Option<&str>) -> CoreResult<Vec<LocalAlbum>> {
        db::list_albums(&self.pool, artist_id).await
    }

    pub async fn list_tracks_by_album(&self, album_id: &str) -> CoreResult<Vec<LocalTrack>> {
        db::list_tracks_by_album(&self.pool, album_id).await
    }

    pub async fn list_tracks_by_playlist(&self, playlist_id: &str) -> CoreResult<Vec<LocalTrack>> {
        db::list_tracks_for_playlist(&self.pool, playlist_id).await
    }

    pub async fn list_playlists(&self) -> CoreResult<Vec<LocalPlaylist>> {
        db::list_playlists(&self.pool).await
    }

    pub async fn list_starred_tracks(&self) -> CoreResult<Vec<LocalTrack>> {
        db::list_starred_tracks(&self.pool).await
    }

    pub async fn list_discovery_triage(
        &self,
        pending_only: bool,
    ) -> CoreResult<Vec<LocalDiscoveryTrack>> {
        db::list_discovery_triage(&self.pool, pending_only).await
    }

    pub async fn library_stats(&self) -> CoreResult<LibraryStats> {
        db::library_stats(&self.pool).await
    }

    // --- Playback, Cache & Cover Art ---

    pub async fn resolve_playback_source(
        &self,
        track_id: &str,
        profile: &AudioQualityProfile,
    ) -> CoreResult<PlaybackSource> {
        self.cache
            .resolve_playback_source(track_id, profile, self.is_offline_only())
            .await
    }

    pub async fn download_track(
        &self,
        track_id: &str,
        profile: &AudioQualityProfile,
        pinned: bool,
        pin_source: Option<&str>,
    ) -> CoreResult<OfflineTrackEntry> {
        if self.is_offline_only() {
            return Err(CoreError::Offline(
                "downloads disabled in offline_only mode".into(),
            ));
        }
        self.cache
            .download_track(track_id, profile, pinned, pin_source)
            .await
    }

    pub async fn download_track_cancellable(
        &self,
        track_id: &str,
        profile: &AudioQualityProfile,
        pinned: bool,
        pin_source: Option<&str>,
        cancelled: &AtomicBool,
    ) -> CoreResult<OfflineTrackEntry> {
        if self.is_offline_only() {
            return Err(CoreError::Offline(
                "downloads disabled in offline_only mode".into(),
            ));
        }
        self.cache
            .download_track_cancellable(track_id, profile, pinned, pin_source, cancelled)
            .await
    }

    pub async fn prefetch_queue(
        &self,
        queue_track_ids: &[String],
        current_index: usize,
        profile: &AudioQualityProfile,
        is_wifi: bool,
    ) -> CoreResult<Vec<String>> {
        if self.is_offline_only() {
            return Ok(Vec::new());
        }
        self.cache
            .prefetch_queue(queue_track_ids, current_index, profile, is_wifi)
            .await
    }

    pub async fn prefetch_queue_cancellable(
        &self,
        queue_track_ids: &[String],
        current_index: usize,
        profile: &AudioQualityProfile,
        is_wifi: bool,
        cancelled: &AtomicBool,
    ) -> CoreResult<Vec<String>> {
        if self.is_offline_only() {
            return Ok(Vec::new());
        }
        self.cache
            .prefetch_queue_cancellable(queue_track_ids, current_index, profile, is_wifi, cancelled)
            .await
    }

    pub async fn run_auto_cache_rules(
        &self,
        profile: &AudioQualityProfile,
        is_wifi: bool,
    ) -> CoreResult<Vec<String>> {
        if self.is_offline_only() {
            return Ok(Vec::new());
        }
        self.cache.run_auto_cache_rules(profile, is_wifi).await
    }

    pub async fn run_auto_cache_rules_cancellable(
        &self,
        profile: &AudioQualityProfile,
        is_wifi: bool,
        cancelled: &AtomicBool,
    ) -> CoreResult<Vec<String>> {
        if self.is_offline_only() {
            return Ok(Vec::new());
        }
        self.cache
            .run_auto_cache_rules_cancellable(profile, is_wifi, cancelled)
            .await
    }

    pub async fn pin(&self, target: &PinTarget) -> CoreResult<Vec<String>> {
        self.cache.pin(target).await
    }

    pub async fn unpin(&self, target: &PinTarget) -> CoreResult<usize> {
        self.cache.unpin(target).await
    }

    pub async fn list_pinned_targets(&self) -> CoreResult<Vec<PinRecord>> {
        self.cache.list_pinned_targets().await
    }

    pub async fn list_offline_tracks(&self) -> CoreResult<Vec<OfflineTrackEntry>> {
        self.cache.list_offline_tracks().await
    }

    pub async fn cache_policy(&self) -> CachePolicy {
        self.cache.policy().await
    }

    pub async fn set_cache_policy(&self, policy: CachePolicy) -> CoreResult<()> {
        self.cache.set_policy(policy).await
    }

    pub async fn enforce_cache_budget(&self) -> CoreResult<usize> {
        self.cache.enforce_budget().await
    }

    pub async fn cache_stats(&self) -> CoreResult<CacheStats> {
        self.cache.stats().await
    }

    /// Pure local cover-art path lookup with zero network I/O (safe for Android Auto `ContentProvider`).
    pub async fn cover_art_local_path(&self, cover_art_id: &str) -> CoreResult<Option<PathBuf>> {
        self.cache.cover_art_local_path(cover_art_id).await
    }

    pub async fn get_or_fetch_cover_art(
        &self,
        cover_art_id: &str,
        size: Option<u32>,
    ) -> CoreResult<PathBuf> {
        self.cache
            .get_or_fetch_cover_art(cover_art_id, size, self.is_offline_only())
            .await
    }

    pub async fn get_or_fetch_cover_art_with_status(
        &self,
        cover_art_id: &str,
        size: Option<u32>,
    ) -> CoreResult<(PathBuf, bool)> {
        self.cache
            .get_or_fetch_cover_art_with_status(cover_art_id, size, self.is_offline_only())
            .await
    }

    // --- Mutation Outbox ---

    pub async fn enqueue_mutation(&self, mutation: OutboxMutation) -> CoreResult<i64> {
        self.outbox.enqueue(mutation).await
    }

    pub async fn enqueue_mutation_with_key(
        &self,
        mutation: OutboxMutation,
        idempotency_key: String,
    ) -> CoreResult<i64> {
        self.outbox.enqueue_with_key(mutation, idempotency_key).await
    }

    pub async fn outbox_summary(&self) -> CoreResult<OutboxSummary> {
        self.outbox.summary().await
    }

    pub async fn pending_outbox(&self) -> CoreResult<Vec<OutboxRow>> {
        self.outbox.pending().await
    }

    pub async fn dead_letter_outbox(&self) -> CoreResult<Vec<OutboxRow>> {
        self.outbox.dead_lettered().await
    }

    pub async fn retry_dead_letters(&self) -> CoreResult<usize> {
        self.outbox.retry_dead_letters().await
    }

    pub async fn drain_outbox(&self) -> CoreResult<DrainReport> {
        if self.is_offline_only() {
            return Ok(DrainReport {
                replayed: 0,
                failed: 0,
                remaining: self.outbox.pending_count().await?,
            });
        }
        self.outbox.drain().await
    }

    pub async fn drain_outbox_cancellable(
        &self,
        cancelled: &AtomicBool,
    ) -> CoreResult<DrainReport> {
        if self.is_offline_only() {
            return Ok(DrainReport {
                replayed: 0,
                failed: 0,
                remaining: self.outbox.pending_count().await?,
            });
        }
        self.outbox.drain_cancellable(cancelled).await
    }

    // --- Unified Search & Android Auto ---

    pub async fn search(&self, query: &str, limit: usize) -> CoreResult<SearchResults> {
        self.search
            .search(query, limit, self.is_offline_only())
            .await
    }

    pub async fn search_local(&self, query: &str, limit: usize) -> CoreResult<SearchResults> {
        self.search
            .search_local(query, limit, self.is_offline_only())
            .await
    }

    pub async fn search_remote(
        &self,
        query: &str,
        provider: Option<&str>,
        limit: usize,
    ) -> CoreResult<RemoteCatalogResults> {
        let parsed_provider = parse_catalog_provider(provider)?;
        self.search
            .search_remote(query, parsed_provider, limit, self.is_offline_only())
            .await
    }

    pub async fn get_remote_album(&self, album_id: &str) -> CoreResult<RemoteAlbumDetail> {
        if self.is_offline_only() {
            return Err(CoreError::Offline(
                "remote album detail disabled in offline_only mode".into(),
            ));
        }
        self.search.get_remote_album(album_id).await
    }

    pub async fn get_remote_artist(
        &self,
        artist_id: &str,
        name: Option<&str>,
    ) -> CoreResult<Option<RemoteArtistDetail>> {
        if self.is_offline_only() {
            return Err(CoreError::Offline(
                "remote artist detail disabled in offline_only mode".into(),
            ));
        }
        self.search.get_remote_artist(artist_id, name).await
    }

    pub async fn download_remote_album(&self, album: &RemoteCatalogAlbum) -> CoreResult<String> {
        if self.is_offline_only() {
            return Err(CoreError::Offline(
                "remote album download disabled in offline_only mode".into(),
            ));
        }
        self.search.download_remote_album(album).await
    }

    pub fn auto_root_tabs(&self) -> Vec<AutoBrowseNode> {
        self.auto_browse.root_tabs()
    }

    pub async fn auto_children(&self, parent_id: &str) -> CoreResult<Vec<AutoBrowseNode>> {
        self.auto_browse
            .children(parent_id, self.is_offline_only())
            .await
    }

    pub async fn auto_children_with_offline(
        &self,
        parent_id: &str,
        offline_only: bool,
    ) -> CoreResult<Vec<AutoBrowseNode>> {
        self.auto_browse.children(parent_id, offline_only).await
    }

    pub async fn voice_search(&self, query: &str) -> CoreResult<Option<VoiceMatchResult>> {
        self.auto_browse
            .voice_search(query, self.is_offline_only())
            .await
    }

    pub async fn save_playback_session(
        &self,
        session: &PersistedPlaybackSession,
    ) -> CoreResult<()> {
        self.auto_browse.save_resumption_session(session).await
    }

    pub async fn load_playback_session(&self) -> CoreResult<Option<PersistedPlaybackSession>> {
        self.auto_browse.load_resumption_session().await
    }

    // --- Multi-Device Connect WS ---

    pub async fn connect_ws(&self, heartbeat_interval: Duration) -> CoreResult<()> {
        if self.is_offline_only() {
            return Err(CoreError::Offline(
                "connect websocket disabled in offline_only mode".into(),
            ));
        }
        let base = self
            .config
            .soulbeet_base_url
            .as_deref()
            .ok_or(CoreError::NotConfigured("soulbeet_base_url"))?;
        self.connect.connect(base, heartbeat_interval).await
    }

    pub async fn disconnect_ws(&self) {
        self.connect.disconnect().await;
    }

    pub async fn connect_state(&self) -> ConnectState {
        self.connect.state().await
    }

    pub async fn connect_update_state(&self, snapshot: PlaybackSnapshotDto) -> CoreResult<()> {
        self.connect.update_state(snapshot).await
    }

    pub async fn connect_send_command(
        &self,
        target_device_id: String,
        command: ConnectCommandDto,
    ) -> CoreResult<()> {
        self.connect.send_command(target_device_id, command).await
    }

    pub async fn connect_claim_active(&self) -> CoreResult<()> {
        self.connect.claim_active().await
    }

    pub async fn connect_transfer_playback_to(
        &self,
        target_device_id: String,
        snapshot: PlaybackSnapshotDto,
    ) -> CoreResult<()> {
        self.connect
            .transfer_playback_to(target_device_id, snapshot)
            .await
    }
}

fn parse_catalog_provider(provider: Option<&str>) -> CoreResult<Option<CatalogProvider>> {
    let Some(raw) = provider.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if raw.eq_ignore_ascii_case("deezer") {
        Ok(Some(CatalogProvider::Deezer))
    } else if raw.eq_ignore_ascii_case("musicbrainz") {
        Ok(Some(CatalogProvider::Musicbrainz))
    } else {
        Err(CoreError::InvalidInput(format!(
            "unsupported catalog provider: {raw}"
        )))
    }
}
