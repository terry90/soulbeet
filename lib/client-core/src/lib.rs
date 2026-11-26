pub mod auto_browse;
pub mod cache;
pub mod connect;
pub mod db;
pub mod error;
pub mod models;
pub mod outbox;
pub mod search;
pub mod subsonic;
pub mod sync;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use sqlx::SqlitePool;
use tokio::sync::RwLock;

use crate::auto_browse::AutoBrowseEngine;
use crate::cache::CacheManager;
use crate::connect::ConnectClient;
use crate::error::{CoreError, CoreResult};
use crate::models::{
    AudioQualityProfile, CacheStats, ClientConfig, DrainReport, NavidromeCredentials,
    OutboxMutation, PersistedPlaybackSession, PinTarget, PlaybackSource, SearchResults,
    SoulbeetAuth, SyncReport, VoiceMatchResult,
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

    /// Runs sync against Soulbeet + Navidrome (refused if `offline_only == true`).
    pub async fn sync_now(&self, force_full: bool) -> CoreResult<SyncReport> {
        if self.is_offline_only() {
            return Err(CoreError::Offline("sync disabled in offline_only mode".into()));
        }
        self.sync.sync(force_full).await
    }

    pub async fn resolve_playback_source(
        &self,
        track_id: &str,
        profile: &AudioQualityProfile,
    ) -> CoreResult<PlaybackSource> {
        self.cache
            .resolve_playback_source(track_id, profile, self.is_offline_only())
            .await
    }

    pub async fn pin(&self, target: &PinTarget) -> CoreResult<Vec<String>> {
        self.cache.pin(target).await
    }

    pub async fn unpin(&self, target: &PinTarget) -> CoreResult<usize> {
        self.cache.unpin(target).await
    }

    pub async fn cache_stats(&self) -> CoreResult<CacheStats> {
        self.cache.stats().await
    }

    pub async fn enqueue_mutation(&self, mutation: OutboxMutation) -> CoreResult<i64> {
        self.outbox.enqueue(mutation).await
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

    pub async fn search(&self, query: &str, limit: usize) -> CoreResult<SearchResults> {
        self.search
            .search(query, limit, self.is_offline_only())
            .await
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
}
