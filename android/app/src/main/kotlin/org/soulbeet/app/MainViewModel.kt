package org.soulbeet.app

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import androidx.work.Constraints
import androidx.work.Data
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.WorkManager
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.soulbeet.app.data.AcquireAndPlayOutcome
import org.soulbeet.app.data.AcquireWsEvent
import org.soulbeet.app.data.AppPreferencesStore
import org.soulbeet.app.data.ContextualMixUi
import org.soulbeet.app.data.CultureSteeringOption
import org.soulbeet.app.data.DeleteGuardPreviewUi
import org.soulbeet.app.data.PendingDropAction
import org.soulbeet.app.data.PendingDropMode
import org.soulbeet.app.data.PendingDropUndoController
import org.soulbeet.app.data.PersistedAppConfig
import org.soulbeet.app.data.SoulbeetApiClient
import org.soulbeet.app.data.StoredAuthKind
import org.soulbeet.app.data.TrackLyricsData
import org.soulbeet.app.data.UserCultureProfileUi
import org.soulbeet.app.data.parseAudioQualityProfile
import org.soulbeet.core.CoreEventListener
import org.soulbeet.core.FfiAlbum
import org.soulbeet.core.FfiArtist
import org.soulbeet.core.FfiCacheStats
import org.soulbeet.core.FfiConnectCommand
import org.soulbeet.core.FfiConnectEvent
import org.soulbeet.core.FfiConnectState
import org.soulbeet.core.FfiDiscoveryTrack
import org.soulbeet.core.FfiDrainReport
import org.soulbeet.core.FfiOnlineTrackMatch
import org.soulbeet.core.FfiOutboxMutation
import org.soulbeet.core.FfiOutboxSummary
import org.soulbeet.core.FfiPinState
import org.soulbeet.core.FfiPinTarget
import org.soulbeet.core.FfiPlaybackSnapshot
import org.soulbeet.core.FfiPlaylist
import org.soulbeet.core.FfiSearchResults
import org.soulbeet.core.FfiSyncReport
import org.soulbeet.core.FfiTrack
import org.soulbeet.corebindings.CoreClient
import org.soulbeet.corebindings.SoulbeetCoreProvider
import org.soulbeet.media.AutoBrowseTree
import org.soulbeet.media.AutoCustomActions
import org.soulbeet.media.CoverArtContentProvider
import org.soulbeet.media.LibrarySecondaryAction
import org.soulbeet.media.MediaPlaybackUiState
import org.soulbeet.media.OfflineSyncWorker
import org.soulbeet.media.SoulbeetMediaRepository
import org.soulbeet.media.SoulbeetWorkScheduler
import java.io.File
import java.lang.ref.WeakReference
import java.time.LocalTime

enum class AppTab(val label: String) {
    HOME("Home"),
    DISCOVER("Discover"),
    SEARCH("Search"),
    LIBRARY("Library"),
}

enum class SearchFilter(val label: String) {
    ALL("All"),
    IN_LIBRARY("In Library"),
    CATALOG("Catalog"),
    ARTISTS("Artists"),
    ALBUMS("Albums"),
    PLAYLISTS("Playlists"),
}

enum class LibraryCategory(val label: String) {
    PLAYLISTS("Playlists"),
    ARTISTS("Artists"),
    ALBUMS("Albums"),
    STARRED("Starred"),
    DISCOVERY("Discovery"),
    OFFLINE("Cached"),
}

data class QuickResumeItem(
    val id: String,
    val title: String,
    val subtitle: String,
    val coverArtId: String?,
    val isOfflineReady: Boolean,
    val containerMediaId: String,
)

data class TriageCardItem(
    val discovery: FfiDiscoveryTrack,
    val track: FfiTrack?,
    val isOfflineCached: Boolean,
)

data class ContainerDetailUi(
    val title: String,
    val subtitle: String,
    val coverArtId: String?,
    val pinTarget: FfiPinTarget?,
    val isPinned: Boolean,
    val tracks: List<FfiTrack>,
)

data class AppUiState(
    val isBooting: Boolean = true,
    val config: PersistedAppConfig = PersistedAppConfig(),
    val isConnecting: Boolean = false,
    val connectError: String? = null,
    val selectedTab: AppTab = AppTab.HOME,
    val playback: MediaPlaybackUiState = MediaPlaybackUiState(),
    val livePositionMs: Long = 0L,
    val shuffleEnabled: Boolean = false,
    val repeatMode: Int = 0,
    val currentTrackDetails: FfiTrack? = null,
    val currentDiscoveryInfo: FfiDiscoveryTrack? = null,
    val queueTracks: List<FfiTrack> = emptyList(),
    // Home & Discover
    val quickResume: List<QuickResumeItem> = emptyList(),
    val contextualMixes: List<ContextualMixUi> = emptyList(),
    val cultureProfile: UserCultureProfileUi? = null,
    val steeringInFlight: Boolean = false,
    val triageDeck: List<TriageCardItem> = emptyList(),
    val inFlightTriageIds: Set<String> = emptySet(),
    val recentTracks: List<FfiTrack> = emptyList(),
    // Search
    val searchQuery: String = "",
    val searchFilter: SearchFilter = SearchFilter.ALL,
    val searchResults: FfiSearchResults? = null,
    val isSearching: Boolean = false,
    val acquiringStages: Map<String, String> = emptyMap(),
    // Library
    val libraryCategory: LibraryCategory = LibraryCategory.PLAYLISTS,
    val playlists: List<FfiPlaylist> = emptyList(),
    val artists: List<FfiArtist> = emptyList(),
    val albums: List<FfiAlbum> = emptyList(),
    val starredTracks: List<FfiTrack> = emptyList(),
    val discoveryTracks: List<FfiDiscoveryTrack> = emptyList(),
    val offlineTrackIds: Set<String> = emptySet(),
    val offlineTracks: List<FfiTrack> = emptyList(),
    val pinnedTargets: List<FfiPinState> = emptyList(),
    val cacheStats: FfiCacheStats? = null,
    val outboxSummary: FfiOutboxSummary? = null,
    val isSyncing: Boolean = false,
    // Overlays & Sheets
    val showNowPlayingSheet: Boolean = false,
    val showLyricsPanel: Boolean = false,
    val lyricsData: TrackLyricsData? = null,
    val isFetchingLyrics: Boolean = false,
    val lyricsFetched: Boolean = false,
    val lyricsError: String? = null,
    val showQueueSheet: Boolean = false,
    val showConnectSheet: Boolean = false,
    val connectState: FfiConnectState? = null,
    val showSettingsSheet: Boolean = false,
    val showCultureEditor: Boolean = false,
    val selectedContainer: ContainerDetailUi? = null,
    val guardSheetPreview: DeleteGuardPreviewUi? = null,
    val isLoadingGuard: Boolean = false,
    val pendingDrop: PendingDropAction? = null,
    val snackbarMessage: String? = null,
) {
    val activeAcquisitionsCount: Int get() = acquiringStages.size
    val pendingDiscoveryCount: Int get() = triageDeck.size
    val seasonalHeroMix: ContextualMixUi?
        get() = contextualMixes.firstOrNull { it.kind == "seasonal" && it.trackCount > 0 }
            ?: contextualMixes.firstOrNull { !(it.kind == "seasonal" && it.trackCount == 0) }
    val rollingAndMomentMixes: List<ContextualMixUi>
        get() = contextualMixes.filterNot {
            it.slug == seasonalHeroMix?.slug || (it.kind == "seasonal" && it.trackCount == 0)
        }
    val userPlaylists: List<FfiPlaylist>
        get() = playlists.filter { it.kind == "user" }
    val soulbeetMixPlaylists: List<FfiPlaylist>
        get() {
            val contextualPlaylistIds = contextualMixes
                .mapNotNull { it.navidromePlaylistId?.takeIf(String::isNotBlank) }
                .toSet()
            return playlists.filter { pl ->
                pl.kind in SOULBEET_MIX_KINDS &&
                    !(pl.kind == "seasonal" && pl.songCount == 0L) &&
                    pl.id !in contextualPlaylistIds
            }
        }
    val homeFallbackMixPlaylists: List<FfiPlaylist>
        get() {
            val heroPlaylistId = seasonalHeroMix?.navidromePlaylistId?.takeIf(String::isNotBlank)
            return playlists.filter { pl ->
                pl.kind in SOULBEET_MIX_KINDS &&
                    !(pl.kind == "seasonal" && pl.songCount == 0L) &&
                    pl.id != heroPlaylistId
            }
        }

    companion object {
        private val SOULBEET_MIX_KINDS = setOf("discovery", "seasonal", "moment", "smart")
    }
}

/**
 * Unified ViewModel for the Jetpack Compose mobile app, backed by [SoulbeetCoreProvider],
 * [SoulbeetMediaRepository.shared], [MediaControllerBridge], and [AppPreferencesStore].
 */
class MainViewModel(application: Application) : AndroidViewModel(application) {

    private val appContext = application.applicationContext
    private val appScope: CoroutineScope =
        (application as? SoulbeetApplication)?.appScope
            ?: CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val prefsStore: AppPreferencesStore =
        (application as? SoulbeetApplication)?.preferencesStore ?: AppPreferencesStore(appContext)
    private val apiClient = SoulbeetApiClient()
    private val mediaBridge = MediaControllerBridge(appContext)
    val artAuthority: String = CoverArtContentProvider.authorityFor(appContext)

    private val _uiState = MutableStateFlow(AppUiState())
    val uiState: StateFlow<AppUiState> = _uiState.asStateFlow()

    private val hiddenTriageIds = MutableStateFlow<Set<String>>(emptySet())
    private val inFlightTriageIds: MutableSet<String> = mutableSetOf()
    private var searchJob: Job? = null
    private var bannerClearJob: Job? = null
    private var connectWsJob: Job? = null
    private var lyricsJob: Job? = null
    private var lyricsFetchSeq: Long = 0L
    private var eventListenerAttachedCore: WeakReference<CoreClient>? = null

    private val undoController = PendingDropUndoController(
        timerScope = viewModelScope,
        commitScope = appScope,
        onCommit = { action -> commitDropMutation(action) },
        onUndo = { action ->
            val keysToRestore = listOfNotNull(action.trackId, action.discoveryTrackId)
                .filter { it.isNotBlank() }
                .toSet()
            if (keysToRestore.isNotEmpty()) {
                hiddenTriageIds.update { it - keysToRestore }
            }
            viewModelScope.launch { refreshLibrarySnapshot() }
            showNotice("Restored \"${action.title}\"")
        },
    )

    init {
        mediaBridge.connect()

        // Observe persisted preferences (OLED, Offline Only, Auto secondary action, recent searches).
        viewModelScope.launch {
            prefsStore.configFlow.collectLatest { cfg ->
                _uiState.update { it.copy(config = cfg) }
                SoulbeetMediaRepository.shared.setOfflineOnly(cfg.offlineOnly)
                SoulbeetMediaRepository.shared.setLibrarySecondaryAction(cfg.librarySecondaryAction)
                SoulbeetMediaRepository.shared.setAudioQualityProfile(cfg.toQualityProfile())
                val core = SoulbeetCoreProvider.getOrNull()
                core?.setOfflineOnly(cfg.offlineOnly)
                if (cfg.isConfigured && !cfg.offlineOnly) {
                    if (core != null && _uiState.value.connectState?.connected != true) {
                        connectWsIfOnline(core, cfg)
                    }
                    if (_uiState.value.contextualMixes.isEmpty()) {
                        refreshRemoteDiscoveryData(cfg)
                    }
                }
            }
        }

        // Observe shared playback state from SoulbeetMediaLibraryService.
        viewModelScope.launch {
            SoulbeetMediaRepository.shared.playbackState.collectLatest { pb ->
                val prev = _uiState.value.playback
                val trackChanged = pb.currentTrackId != prev.currentTrackId
                val queueChanged = pb.queueTrackIds != prev.queueTrackIds
                val playStateChanged = pb.isPlaying != prev.isPlaying

                _uiState.update { state ->
                    state.copy(
                        playback = pb,
                        livePositionMs = if (!pb.isPlaying) pb.positionMs else state.livePositionMs,
                    )
                }
                if (trackChanged || queueChanged) {
                    loadActiveTrackDetails(pb.currentTrackId, pb.queueTrackIds, clearLyrics = trackChanged)
                }
                if (trackChanged || playStateChanged) {
                    pushConnectPlaybackSnapshot(pb)
                }
            }
        }

        viewModelScope.launch {
            mediaBridge.playbackPositionMs.collectLatest { pos ->
                _uiState.update { it.copy(livePositionMs = pos) }
            }
        }
        viewModelScope.launch {
            mediaBridge.shuffleEnabled.collectLatest { sh ->
                _uiState.update { it.copy(shuffleEnabled = sh) }
            }
        }
        viewModelScope.launch {
            mediaBridge.repeatMode.collectLatest { rm ->
                _uiState.update { it.copy(repeatMode = rm) }
            }
        }
        viewModelScope.launch {
            undoController.pending.collectLatest { pending ->
                _uiState.update { it.copy(pendingDrop = pending) }
            }
        }

        // Boot core & load initial data (await persisted config first to avoid cold-start race).
        viewModelScope.launch {
            val snapshotCfg = withContext(Dispatchers.IO) { prefsStore.getSnapshot() }
            _uiState.update { it.copy(config = snapshotCfg) }
            val core = withContext(Dispatchers.IO) { SoulbeetCoreProvider.awaitOrNull() }
            _uiState.update { it.copy(isBooting = false) }
            if (core != null) {
                attachCoreListener(core)
                refreshLibrarySnapshot()
                refreshRemoteDiscoveryData(snapshotCfg)
                connectWsIfOnline(core, snapshotCfg)
            }
        }
    }

    override fun onCleared() {
        // Flush any in-flight 5-second Undo drop on application scope using identical PendingDropMode logic.
        undoController.flushAsync()
        eventListenerAttachedCore?.get()?.let { runCatching { it.clearEventListener() } }
        mediaBridge.release()
        super.onCleared()
    }

    // =========================================================================
    // Onboarding & Session Connection (Verify BEFORE Persisting!)
    // =========================================================================

    fun connectWithPassword(serverUrl: String, username: String, password: String) {
        if (serverUrl.isBlank() || username.isBlank() || password.isBlank()) {
            _uiState.update { it.copy(connectError = "Server URL, username, and password are required.") }
            return
        }
        viewModelScope.launch {
            _uiState.update { it.copy(isConnecting = true, connectError = null) }
            val loginResult = apiClient.login(serverUrl, username, password)
            loginResult.fold(
                onSuccess = { login ->
                    completeSessionSetup(
                        serverUrl = SoulbeetApiClient.normalizeBaseUrl(serverUrl),
                        authKind = login.authKind,
                        authToken = login.token,
                        username = login.username,
                    )
                },
                onFailure = { err ->
                    _uiState.update {
                        it.copy(
                            isConnecting = false,
                            connectError = err.localizedMessage ?: "Unable to connect to server",
                        )
                    }
                },
            )
        }
    }

    fun connectWithToken(serverUrl: String, rawToken: String, isBearer: Boolean) {
        val parsed = SoulbeetApiClient.parseTokenInput(rawToken, defaultIsBearer = isBearer)
        if (serverUrl.isBlank() || parsed.token.isBlank()) {
            _uiState.update { it.copy(connectError = "Server URL and token are required.") }
            return
        }
        viewModelScope.launch {
            _uiState.update { it.copy(isConnecting = true, connectError = null) }
            val normalized = runCatching { SoulbeetApiClient.normalizeBaseUrl(serverUrl) }
                .getOrElse { err ->
                    _uiState.update { it.copy(isConnecting = false, connectError = err.localizedMessage) }
                    return@launch
                }
            completeSessionSetup(
                serverUrl = normalized,
                authKind = parsed.authKind,
                authToken = parsed.token,
                username = "",
            )
        }
    }

    /**
     * Validates candidate credentials via `fetchBootstrap()` BEFORE persisting to DataStore so an
     * invalid token or unreachable server never locks the user past OnboardingScreen.
     */
    private suspend fun completeSessionSetup(
        serverUrl: String,
        authKind: StoredAuthKind,
        authToken: String,
        username: String,
    ) {
        try {
            val baseSnapshot = prefsStore.getSnapshot()
            val candidateConfig = baseSnapshot.copy(
                serverUrl = serverUrl,
                authKind = authKind,
                authToken = authToken,
                username = username,
            )

            val (core, bootstrap) = withContext(Dispatchers.IO) {
                SoulbeetCoreProvider.clear(closeExisting = true)
                val opened = SoulbeetCoreProvider.open(candidateConfig.toFfiClientConfig(appContext))
                val boot = try {
                    opened.fetchBootstrap()
                } catch (e: Exception) {
                    runCatching { opened.shutdown() }
                    SoulbeetCoreProvider.clear(closeExisting = true)
                    SoulbeetCoreProvider.markUnconfigured()
                    throw e
                }
                if (boot == null) {
                    runCatching { opened.shutdown() }
                    SoulbeetCoreProvider.clear(closeExisting = true)
                    SoulbeetCoreProvider.markUnconfigured()
                    throw IllegalStateException("Server did not return a valid client bootstrap payload.")
                }
                boot.navidrome?.let { opened.setNavidromeCredentials(it) }
                opened to boot
            }

            // Verified! Persist session + Navidrome credentials only now.
            val resolvedUser = bootstrap.username.ifBlank { username }
            prefsStore.saveSession(
                serverUrl = serverUrl,
                authKind = authKind,
                authToken = authToken,
                username = resolvedUser,
                navidrome = bootstrap.navidrome,
            )

            attachCoreListener(core)
            SoulbeetWorkScheduler.schedulePeriodicSync(
                context = appContext,
                qualityProfile = baseSnapshot.qualityProfileKey,
            )

            _uiState.update { it.copy(isConnecting = false, connectError = null, isSyncing = true) }
            withContext(Dispatchers.IO) {
                runCatching { core.syncNow(forceFull = false) }
            }
            _uiState.update { it.copy(isSyncing = false) }
            refreshLibrarySnapshot()
            refreshRemoteDiscoveryData()
            connectWsIfOnline(core)
            showNotice("Connected to Soulbeet")
        } catch (e: Exception) {
            _uiState.update {
                it.copy(
                    isConnecting = false,
                    isSyncing = false,
                    connectError = e.localizedMessage ?: "Connection bootstrap failed",
                )
            }
        }
    }

    fun disconnectSession() {
        val currentConfig = _uiState.value.config
        viewModelScope.launch {
            _uiState.update { it.copy(showSettingsSheet = false, showNowPlayingSheet = false) }
            undoController.undo()
            mediaBridge.stopAndClear()
            SoulbeetMediaRepository.shared.clearActiveTrack()

            withContext(Dispatchers.IO) {
                // 1. Best-effort server logout
                runCatching { apiClient.logout(currentConfig) }

                // 2. Cancel scheduled background sync & auto-cache jobs
                runCatching {
                    val wm = WorkManager.getInstance(appContext)
                    wm.cancelUniqueWork(SoulbeetWorkScheduler.WORK_PERIODIC_SYNC)
                    wm.cancelUniqueWork(SoulbeetWorkScheduler.WORK_PERIODIC_AUTO_CACHE)
                    wm.cancelUniqueWork(SoulbeetWorkScheduler.WORK_IMMEDIATE_OUTBOX_DRAIN)
                    wm.cancelUniqueWork(WORK_IMMEDIATE_AUTO_CACHE)
                }

                // 3. Shutdown and release CoreClient
                SoulbeetCoreProvider.getOrNull()?.let { core ->
                    runCatching { core.clearEventListener() }
                    runCatching { core.disconnectWs() }
                    runCatching { core.shutdown() }
                }
                SoulbeetCoreProvider.clear(closeExisting = true)
                SoulbeetCoreProvider.markUnconfigured()

                // 4. Wipe local SQLite mirror/outbox so another user never sees previous account data
                val dbDir = File(appContext.filesDir, "soulbeet_core")
                listOf("client_core.db", "client_core.db-wal", "client_core.db-shm").forEach { name ->
                    runCatching { File(dbDir, name).delete() }
                }

                // 5. Clear stored session credentials
                prefsStore.clearSession()
            }

            hiddenTriageIds.value = emptySet()
            inFlightTriageIds.clear()
            _uiState.update {
                AppUiState(
                    isBooting = false,
                    config = it.config.copy(
                        authKind = StoredAuthKind.NONE,
                        authToken = "",
                        username = "",
                        navidrome = null,
                    ),
                )
            }
        }
    }

    // =========================================================================
    // Navigation & Global Controls
    // =========================================================================

    fun selectTab(tab: AppTab) {
        _uiState.update { it.copy(selectedTab = tab) }
        if (tab == AppTab.DISCOVER && _uiState.value.contextualMixes.isEmpty()) {
            viewModelScope.launch { refreshRemoteDiscoveryData() }
        }
    }

    fun toggleOfflineOnly() {
        val next = !_uiState.value.config.offlineOnly
        viewModelScope.launch {
            prefsStore.setOfflineOnly(next)
            val core = SoulbeetCoreProvider.getOrNull()
            core?.setOfflineOnly(next)
            SoulbeetMediaRepository.shared.setOfflineOnly(next)

            if (next) {
                withContext(Dispatchers.IO) {
                    runCatching { core?.disconnectWs() }
                }
                _uiState.update { it.copy(connectState = null) }
            } else if (core != null) {
                connectWsIfOnline(core)
                SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
            }

            refreshLibrarySnapshot()
            if (_uiState.value.searchQuery.isNotBlank()) {
                runUnifiedSearch(_uiState.value.searchQuery)
            }
            showNotice(if (next) "Offline Only mode active" else "Back online")
        }
    }

    fun toggleOledMode(enabled: Boolean) {
        viewModelScope.launch { prefsStore.setOledMode(enabled) }
    }

    fun setLibrarySecondaryAction(action: LibrarySecondaryAction) {
        viewModelScope.launch {
            prefsStore.setLibrarySecondaryAction(action)
            SoulbeetMediaRepository.shared.setLibrarySecondaryAction(action)
        }
    }

    fun setAudioQualityProfile(profileKey: String) {
        viewModelScope.launch {
            prefsStore.setQualityProfile(profileKey)
            val parsed = parseAudioQualityProfile(profileKey)
            SoulbeetMediaRepository.shared.setAudioQualityProfile(parsed)
            runCatching {
                SoulbeetWorkScheduler.schedulePeriodicSync(
                    context = appContext,
                    qualityProfile = profileKey,
                )
            }
        }
    }

    fun setShowSettingsSheet(show: Boolean) {
        _uiState.update { it.copy(showSettingsSheet = show) }
        if (show) {
            viewModelScope.launch { refreshOutboxAndCacheStats() }
        }
    }

    fun setShowNowPlayingSheet(show: Boolean) {
        _uiState.update {
            it.copy(
                showNowPlayingSheet = show,
                showLyricsPanel = if (!show) false else it.showLyricsPanel,
            )
        }
    }

    fun setShowLyricsPanel(show: Boolean) {
        _uiState.update { it.copy(showLyricsPanel = show) }
        if (show && _uiState.value.lyricsData == null && !_uiState.value.isFetchingLyrics) {
            fetchCurrentTrackLyrics()
        }
    }

    fun setShowQueueSheet(show: Boolean) {
        _uiState.update { it.copy(showQueueSheet = show) }
    }

    fun setShowConnectSheet(show: Boolean) {
        _uiState.update { it.copy(showConnectSheet = show) }
        if (show) {
            reconnectConnect(force = false)
        }
    }

    fun reconnectConnect(force: Boolean = false) {
        val cfg = _uiState.value.config
        if (!cfg.isConfigured) return
        if (cfg.offlineOnly) {
            if (force) showNotice("Disable Offline Only to reconnect")
            return
        }
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            val liveState = withContext(Dispatchers.IO) {
                runCatching { core.connectState() }.getOrNull()
            }
            if (liveState != null) {
                _uiState.update { it.copy(connectState = liveState) }
            }
            if (force || liveState?.connected != true) {
                connectWsIfOnline(core, cfg, force = force)
            }
        }
    }

    fun setShowCultureEditor(show: Boolean) {
        _uiState.update { it.copy(showCultureEditor = show) }
    }

    fun triggerManualSync(forceFull: Boolean = false) {
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            if (core.isOfflineOnly()) {
                showNotice("Disable Offline Only to sync")
                return@launch
            }
            _uiState.update { it.copy(isSyncing = true) }
            val result = withContext(Dispatchers.IO) {
                runCatching {
                    core.drainOutbox()
                    core.syncNow(forceFull = forceFull)
                }
            }
            _uiState.update { it.copy(isSyncing = false) }
            result.fold(
                onSuccess = { report ->
                    refreshLibrarySnapshot()
                    refreshRemoteDiscoveryData()
                    showNotice("Synced ${report.tracksSynced} tracks · ${report.playlistsSynced} playlists")
                },
                onFailure = { err ->
                    showNotice("Sync error: ${err.localizedMessage ?: "unreachable"}")
                },
            )
        }
    }

    fun drainOutboxNow() {
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            val report = withContext(Dispatchers.IO) {
                runCatching {
                    core.retryDeadLetters()
                    core.drainOutbox()
                }.getOrNull()
            }
            refreshOutboxAndCacheStats()
            if (report != null) {
                showNotice("Replayed ${report.replayed} queued changes (${report.remaining} remaining)")
            }
        }
    }

    // =========================================================================
    // Playback Controls & Contextual Mix Resolution
    // =========================================================================

    fun playTrack(track: FfiTrack, sourceLabel: String? = null) {
        val offlineIds = _uiState.value.offlineTrackIds
        if (_uiState.value.config.offlineOnly && !offlineIds.contains(track.id)) {
            showNotice("Not cached offline")
            return
        }
        mediaBridge.playTrack(
            track = track,
            isOfflineCached = offlineIds.contains(track.id),
            sourceLabel = sourceLabel,
        )
    }

    fun playQueue(tracks: List<FfiTrack>, startIndex: Int = 0, sourceLabel: String? = null) {
        val offlineIds = _uiState.value.offlineTrackIds
        val playable = if (_uiState.value.config.offlineOnly) {
            tracks.filter { offlineIds.contains(it.id) }
        } else {
            tracks
        }
        if (playable.isEmpty()) {
            showNotice("No offline-cached tracks in this list")
            return
        }
        val targetTrack = tracks.getOrNull(startIndex)
        val adjustedStart = if (targetTrack != null) {
            playable.indexOfFirst { it.id == targetTrack.id }.coerceAtLeast(0)
        } else {
            0
        }
        mediaBridge.playQueue(
            tracks = playable,
            startIndex = adjustedStart,
            offlineIds = offlineIds,
            sourceLabel = sourceLabel,
        )
    }

    /**
     * Enqueues the full pending discovery triage deck starting at [card], or toggles play/pause
     * if [card] is already active inside the Discovery Triage queue so playback resumes in place.
     */
    fun playTriageFrom(card: TriageCardItem) {
        val selected = card.track ?: run {
            showNotice("Track is not indexed in Navidrome yet.")
            return
        }
        val currentState = _uiState.value
        val offlineIds = currentState.offlineTrackIds
        val allDeck = currentState.triageDeck.mapNotNull { it.track }
        val playableDeck = if (currentState.config.offlineOnly) {
            allDeck.filter { offlineIds.contains(it.id) }
        } else {
            allDeck
        }
        if (currentState.playback.currentTrackId == selected.id) {
            if (currentState.playback.isPlaying) {
                mediaBridge.togglePlayPause()
                return
            }
            if (currentState.playback.sourceLabel == "Discovery Triage" &&
                (currentState.queueTracks.size > 1 || playableDeck.size <= 1)
            ) {
                mediaBridge.togglePlayPause()
                return
            }
        }
        if (playableDeck.isEmpty()) {
            if (currentState.config.offlineOnly) {
                showNotice("No offline-cached tracks in this list")
            } else {
                playTrack(selected, sourceLabel = "Discovery Triage")
            }
            return
        }
        val startIndex = playableDeck.indexOfFirst { it.id == selected.id }
        if (startIndex < 0) {
            playTrack(selected, sourceLabel = "Discovery Triage")
            return
        }
        playQueue(
            tracks = playableDeck,
            startIndex = startIndex,
            sourceLabel = "Discovery Triage",
        )
    }

    fun playContainer(mediaId: String, title: String? = null) {
        mediaBridge.playContainer(mediaId, title)
    }

    /**
     * Plays a contextual or seasonal mix: uses its synced Navidrome playlist when available,
     * otherwise fetches the materialized mix playlist via `/api/discovery/contextual-mixes/playlist`
     * so Moment Mixes (`Drive`, `Morning`, `Focus`, `Late Night`) never fall back to `for_you:triage`.
     */
    fun playContextualMix(mix: ContextualMixUi) {
        val syncedId = mix.navidromePlaylistId
        if (!syncedId.isNullOrBlank()) {
            mediaBridge.playContainer("${AutoBrowseTree.PREFIX_PLAYLIST}$syncedId", mix.title)
            return
        }
        val cfg = _uiState.value.config
        if (cfg.offlineOnly || !cfg.isConfigured) {
            // Offline fallback: look for a local playlist matching the slug or title
            val localMatch = _uiState.value.playlists.firstOrNull { pl ->
                pl.slug.equals(mix.slug, ignoreCase = true) || pl.name.equals(mix.title, ignoreCase = true)
            }
            if (localMatch != null) {
                mediaBridge.playContainer("${AutoBrowseTree.PREFIX_PLAYLIST}${localMatch.id}", mix.title)
            } else {
                showNotice("Go online or pin ${mix.title} to play offline")
            }
            return
        }

        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            apiClient.fetchContextualMixPlaylist(cfg, mix.slug).fold(
                onSuccess = { mixTracks ->
                    if (mixTracks.isEmpty()) {
                        showNotice("${mix.title} has no playable tracks yet")
                        return@fold
                    }
                    val ids = mixTracks.map { it.songId }
                    val localById = withContext(Dispatchers.IO) {
                        runCatching { core.getTracksByIds(ids) }
                            .getOrDefault(emptyList())
                            .associateBy { it.id }
                    }
                    val resolved = mixTracks.map { mt ->
                        localById[mt.songId] ?: FfiTrack(
                            id = mt.songId,
                            title = mt.title,
                            artistId = null,
                            artistName = mt.artist,
                            albumId = null,
                            albumName = mt.album.orEmpty(),
                            trackNumber = null,
                            discNumber = null,
                            durationSecs = mt.durationSecs ?: 0L,
                            year = null,
                            genre = null,
                            bitRate = null,
                            suffix = null,
                            contentType = null,
                            sizeBytes = null,
                            coverArtId = null,
                            path = null,
                            starred = mt.starred,
                            userRating = mt.rating.toLong(),
                            playCount = 0L,
                            lastPlayedAt = null,
                            musicbrainzId = null,
                            isDiscovery = !mt.owned,
                            updatedAt = 0L,
                        )
                    }
                    playQueue(resolved, startIndex = 0, sourceLabel = mix.title)
                },
                onFailure = { err ->
                    showNotice(err.localizedMessage ?: "Failed to load ${mix.title}")
                },
            )
        }
    }

    fun togglePlayPause() = mediaBridge.togglePlayPause()
    fun skipNext() = mediaBridge.skipNext()
    fun skipPrevious() = mediaBridge.skipPrevious()
    fun seekTo(positionMs: Long) = mediaBridge.seekTo(positionMs)
    fun playQueueIndex(index: Int) = mediaBridge.playQueueIndex(index)
    fun toggleShuffle() = mediaBridge.toggleShuffle()
    fun cycleRepeatMode() = mediaBridge.cycleRepeatMode()
    fun startTrackRadio() {
        viewModelScope.launch {
            mediaBridge.sendCustomActionAndAwait(AutoCustomActions.ACTION_START_RADIO)
        }
        showNotice("Starting track radio…")
    }

    // =========================================================================
    // Curation (Keep, Star, Rate, 5-Second Undo Drop, Multi-Listener Guard)
    // =========================================================================

    fun keepDiscoveryTrack(trackId: String, discoveryTrackId: String? = null, title: String = "Track") {
        val guardKeys = listOfNotNull(trackId, discoveryTrackId)
            .filter { it.isNotBlank() }
            .distinct()
        if (guardKeys.isEmpty() || guardKeys.any { it in inFlightTriageIds }) return
        inFlightTriageIds.addAll(guardKeys)

        val hasTrackId = trackId.isNotBlank()
        val hasDiscoveryId = !discoveryTrackId.isNullOrBlank()
        val prevDeck = _uiState.value.triageDeck
        val prevPlaybackCtx = if (hasTrackId && _uiState.value.playback.currentTrackId == trackId) {
            _uiState.value.playback.toTrackActionContext()
        } else {
            null
        }

        // Synchronous 0ms optimistic UI update before launching IO/MediaController work.
        hiddenTriageIds.update { it + guardKeys }
        _uiState.update { state ->
            state.copy(
                inFlightTriageIds = state.inFlightTriageIds + guardKeys,
                triageDeck = state.triageDeck.filterNot {
                    (hasDiscoveryId && it.discovery.id == discoveryTrackId) ||
                        (hasTrackId && it.discovery.songId == trackId)
                },
                playback = if (hasTrackId && state.playback.currentTrackId == trackId) {
                    state.playback.copy(isPendingDiscovery = false)
                } else {
                    state.playback
                },
            )
        }
        prevPlaybackCtx?.let { ctx ->
            SoulbeetMediaRepository.shared.updateTrackContext(
                ctx.copy(isPendingDiscovery = false),
            )
        }

        viewModelScope.launch {
            try {
                val core = SoulbeetCoreProvider.getOrNull()
                    ?: throw IllegalStateException("Core client is not initialized")
                val handledBySession = if (hasTrackId && _uiState.value.playback.currentTrackId == trackId) {
                    mediaBridge.sendCustomActionAndAwait(AutoCustomActions.ACTION_KEEP_DISCOVERY)
                } else {
                    false
                }
                if (!handledBySession) {
                    withContext(Dispatchers.IO) {
                        core.enqueueMutation(
                            FfiOutboxMutation.Keep(
                                songId = trackId,
                                discoveryTrackId = discoveryTrackId,
                                targetFolderId = null,
                            ),
                        )
                        SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
                    }
                    if (hasTrackId && _uiState.value.playback.currentTrackId == trackId) {
                        _uiState.value.playback.toTrackActionContext()?.let { ctx ->
                            SoulbeetMediaRepository.shared.updateTrackContext(
                                ctx.copy(isPendingDiscovery = false),
                            )
                        }
                    }
                }
                refreshLibrarySnapshot()
                hiddenTriageIds.update { it - guardKeys.toSet() }
                showNotice("Kept \"$title\" in Library")
            } catch (e: Exception) {
                hiddenTriageIds.update { it - guardKeys.toSet() }
                _uiState.update { state ->
                    state.copy(
                        triageDeck = prevDeck,
                        playback = if (prevPlaybackCtx != null && hasTrackId && state.playback.currentTrackId == trackId) {
                            state.playback.copy(isPendingDiscovery = prevPlaybackCtx.isPendingDiscovery)
                        } else {
                            state.playback
                        },
                    )
                }
                if (prevPlaybackCtx != null && hasTrackId && _uiState.value.playback.currentTrackId == trackId) {
                    SoulbeetMediaRepository.shared.updateTrackContext(prevPlaybackCtx)
                }
                refreshLibrarySnapshot()
                showNotice("Failed to keep \"$title\": ${e.localizedMessage ?: "error"}")
            } finally {
                inFlightTriageIds.removeAll(guardKeys.toSet())
                _uiState.update { it.copy(inFlightTriageIds = it.inFlightTriageIds - guardKeys.toSet()) }
            }
        }
    }

    fun starAndKeepDiscoveryTrack(trackId: String, discoveryTrackId: String? = null, title: String = "Track") {
        val guardKeys = listOfNotNull(trackId, discoveryTrackId)
            .filter { it.isNotBlank() }
            .distinct()
        if (guardKeys.isEmpty() || guardKeys.any { it in inFlightTriageIds }) return
        inFlightTriageIds.addAll(guardKeys)

        val hasTrackId = trackId.isNotBlank()
        val hasDiscoveryId = !discoveryTrackId.isNullOrBlank()
        val prevDeck = _uiState.value.triageDeck
        val prevPlaybackCtx = if (hasTrackId && _uiState.value.playback.currentTrackId == trackId) {
            _uiState.value.playback.toTrackActionContext()
        } else {
            null
        }

        // Synchronous 0ms optimistic UI update before launching IO work.
        hiddenTriageIds.update { it + guardKeys }
        _uiState.update { state ->
            state.copy(
                inFlightTriageIds = state.inFlightTriageIds + guardKeys,
                triageDeck = state.triageDeck.filterNot {
                    (hasDiscoveryId && it.discovery.id == discoveryTrackId) ||
                        (hasTrackId && it.discovery.songId == trackId)
                },
                playback = if (hasTrackId && state.playback.currentTrackId == trackId) {
                    state.playback.copy(isPendingDiscovery = false, isStarred = true)
                } else {
                    state.playback
                },
            )
        }
        prevPlaybackCtx?.let { ctx ->
            SoulbeetMediaRepository.shared.updateTrackContext(
                ctx.copy(isStarred = true, isPendingDiscovery = false),
            )
        }

        viewModelScope.launch {
            try {
                val core = SoulbeetCoreProvider.getOrNull()
                    ?: throw IllegalStateException("Core client is not initialized")
                withContext(Dispatchers.IO) {
                    core.enqueueMutation(
                        FfiOutboxMutation.Star(
                            songId = trackId,
                            discoveryTrackId = discoveryTrackId,
                        ),
                    )
                    SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
                }
                if (hasTrackId && _uiState.value.playback.currentTrackId == trackId) {
                    _uiState.value.playback.toTrackActionContext()?.let { ctx ->
                        SoulbeetMediaRepository.shared.updateTrackContext(
                            ctx.copy(isStarred = true, isPendingDiscovery = false),
                        )
                    }
                }
                refreshLibrarySnapshot()
                hiddenTriageIds.update { it - guardKeys.toSet() }
                showNotice("Starred & kept \"$title\"")
            } catch (e: Exception) {
                hiddenTriageIds.update { it - guardKeys.toSet() }
                _uiState.update { state ->
                    state.copy(
                        triageDeck = prevDeck,
                        playback = if (prevPlaybackCtx != null && hasTrackId && state.playback.currentTrackId == trackId) {
                            state.playback.copy(
                                isPendingDiscovery = prevPlaybackCtx.isPendingDiscovery,
                                isStarred = prevPlaybackCtx.isStarred,
                            )
                        } else {
                            state.playback
                        },
                    )
                }
                if (prevPlaybackCtx != null && hasTrackId && _uiState.value.playback.currentTrackId == trackId) {
                    SoulbeetMediaRepository.shared.updateTrackContext(prevPlaybackCtx)
                }
                refreshLibrarySnapshot()
                showNotice("Failed to star & keep \"$title\": ${e.localizedMessage ?: "error"}")
            } finally {
                inFlightTriageIds.removeAll(guardKeys.toSet())
                _uiState.update { it.copy(inFlightTriageIds = it.inFlightTriageIds - guardKeys.toSet()) }
            }
        }
    }

    fun dropDiscoveryTrackWithUndo(
        trackId: String,
        title: String,
        artist: String,
        discoveryTrackId: String? = null,
        sourceLabel: String? = "Discovery",
    ) {
        val guardKeys = listOfNotNull(trackId, discoveryTrackId)
            .filter { it.isNotBlank() }
            .distinct()
        if (guardKeys.isEmpty() || guardKeys.any { it in inFlightTriageIds }) return
        val hasTrackId = trackId.isNotBlank()
        val hasDiscoveryId = !discoveryTrackId.isNullOrBlank()
        hiddenTriageIds.update { it + guardKeys }
        _uiState.update { state ->
            state.copy(
                triageDeck = state.triageDeck.filterNot {
                    (hasDiscoveryId && it.discovery.id == discoveryTrackId) ||
                        (hasTrackId && it.discovery.songId == trackId)
                },
            )
        }
        if (hasTrackId) {
            mediaBridge.removeTrackFromQueue(trackId)
        }
        undoController.scheduleDrop(
            PendingDropAction(
                trackId = trackId,
                title = title,
                artist = artist,
                discoveryTrackId = discoveryTrackId,
                mode = PendingDropMode.DropDiscovery,
                sourceLabel = sourceLabel,
                createdAtMs = System.currentTimeMillis(),
            ),
        )
    }

    fun undoPendingDrop() {
        undoController.undo()
    }

    fun toggleStarTrack(trackId: String, currentlyStarred: Boolean, discoveryTrackId: String? = null) {
        viewModelScope.launch {
            val handledBySession = if (_uiState.value.playback.currentTrackId == trackId) {
                mediaBridge.sendCustomActionAndAwait(AutoCustomActions.ACTION_STAR_TOGGLE)
            } else {
                false
            }
            if (!handledBySession) {
                val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
                val mutation = if (currentlyStarred) {
                    FfiOutboxMutation.Unstar(songId = trackId, discoveryTrackId = discoveryTrackId)
                } else {
                    FfiOutboxMutation.Star(songId = trackId, discoveryTrackId = discoveryTrackId)
                }
                withContext(Dispatchers.IO) {
                    runCatching {
                        core.enqueueMutation(mutation)
                        SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
                    }
                }
                if (_uiState.value.playback.currentTrackId == trackId) {
                    _uiState.value.playback.toTrackActionContext()?.let { ctx ->
                        SoulbeetMediaRepository.shared.updateTrackContext(
                            ctx.copy(
                                isStarred = !currentlyStarred,
                                isPendingDiscovery = if (!currentlyStarred) false else ctx.isPendingDiscovery,
                            ),
                        )
                    }
                }
            }
            refreshLibrarySnapshot()
        }
    }

    fun rateTrack(trackId: String, rating: Int, discoveryTrackId: String? = null) {
        val clamped = rating.coerceIn(0, 5)
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            withContext(Dispatchers.IO) {
                runCatching {
                    core.enqueueMutation(
                        FfiOutboxMutation.Rate(
                            songId = trackId,
                            rating = clamped.toUByte(),
                            discoveryTrackId = discoveryTrackId,
                        ),
                    )
                    SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
                }
            }
            if (_uiState.value.playback.currentTrackId == trackId) {
                _uiState.value.playback.toTrackActionContext()?.let { ctx ->
                    SoulbeetMediaRepository.shared.updateTrackContext(
                        ctx.copy(userRating = clamped),
                    )
                }
            }
            refreshLibrarySnapshot()
        }
    }

    /**
     * Initiates Multi-Listener Delete Guard inspection before removing a permanent library track.
     */
    fun requestLibraryTrackRemoval(songId: String, title: String, artist: String) {
        viewModelScope.launch {
            _uiState.update { it.copy(isLoadingGuard = true) }
            val cfg = _uiState.value.config
            val preview = if (cfg.offlineOnly || !cfg.isConfigured) {
                DeleteGuardPreviewUi(
                    songId = songId,
                    trackTitle = title,
                    trackArtist = artist,
                    isPendingDiscovery = false,
                    discoveryTrackId = null,
                    canDelete = false,
                    soleOwner = false,
                    otherListeners = emptyList(),
                    ownPlaylists = emptyList(),
                    blockReason = "Offline mode active — confirming will unstar and rate 1★ to hide from your mixes without deleting files.",
                    offlineFallback = true,
                )
            } else {
                apiClient.fetchGuardPreview(cfg, songId, title, artist).getOrElse {
                    DeleteGuardPreviewUi(
                        songId = songId,
                        trackTitle = title,
                        trackArtist = artist,
                        isPendingDiscovery = false,
                        discoveryTrackId = null,
                        canDelete = false,
                        soleOwner = false,
                        otherListeners = emptyList(),
                        ownPlaylists = emptyList(),
                        blockReason = "Could not verify household listeners right now — confirming will unstar and hide from your mixes safely.",
                        offlineFallback = true,
                    )
                }
            }
            _uiState.update { it.copy(isLoadingGuard = false, guardSheetPreview = preview) }
        }
    }

    fun dismissGuardSheet() {
        _uiState.update { it.copy(guardSheetPreview = null) }
    }

    fun confirmGuardedRemoval(preview: DeleteGuardPreviewUi) {
        _uiState.update { it.copy(guardSheetPreview = null) }
        val mode = preview.toPendingDropMode()
        if (_uiState.value.playback.currentTrackId == preview.songId) {
            mediaBridge.skipNext()
        }
        val sourceLabel = when (mode) {
            PendingDropMode.DropDiscovery -> "Discovery"
            PendingDropMode.DeleteFromLibrary -> "Library"
            PendingDropMode.UnstarAndHide -> "your mixes"
        }
        undoController.scheduleDrop(
            PendingDropAction(
                trackId = preview.songId,
                title = preview.trackTitle,
                artist = preview.trackArtist,
                discoveryTrackId = preview.discoveryTrackId,
                mode = mode,
                sourceLabel = sourceLabel,
                createdAtMs = System.currentTimeMillis(),
            ),
        )
    }

    private suspend fun commitDropMutation(action: PendingDropAction) {
        val core = SoulbeetCoreProvider.getOrNull() ?: return
        val keysToRelease = listOfNotNull(action.trackId, action.discoveryTrackId)
            .filter { it.isNotBlank() }
            .toSet()
        val enqueued = withContext(Dispatchers.IO) {
            runCatching {
                for (mutation in action.toMutations()) {
                    core.enqueueMutation(mutation)
                }
                SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
            }.isSuccess
        }
        if (!enqueued && keysToRelease.isNotEmpty()) {
            hiddenTriageIds.update { it - keysToRelease }
        }
        refreshLibrarySnapshot()
        if (enqueued && keysToRelease.isNotEmpty()) {
            hiddenTriageIds.update { it - keysToRelease }
        }
    }

    // =========================================================================
    // Discover 2.0: Culture Steering & Taste DNA
    // =========================================================================

    fun steerSeasonalPack(slug: String, steering: CultureSteeringOption) {
        val cfg = _uiState.value.config
        if (cfg.offlineOnly || !cfg.isConfigured) {
            showNotice("Connect online to steer seasonal culture ratio")
            return
        }
        viewModelScope.launch {
            _uiState.update { it.copy(steeringInFlight = true) }
            apiClient.steerSeasonalPack(cfg, slug, steering).fold(
                onSuccess = { updated ->
                    _uiState.update { state ->
                        val nextMixes = state.contextualMixes.map {
                            if (it.slug == updated.slug) updated else it
                        }
                        state.copy(steeringInFlight = false, contextualMixes = nextMixes)
                    }
                    showNotice("Steered mix to ${steering.shortLabel}")
                },
                onFailure = { err ->
                    _uiState.update { it.copy(steeringInFlight = false) }
                    showNotice(err.localizedMessage ?: "Unable to update mix steering")
                },
            )
        }
    }

    fun updateCultureProfile(
        homeCountry: String?,
        localCultureRatio: Float,
        weatherCity: String? = null,
    ) {
        val cfg = _uiState.value.config
        if (cfg.offlineOnly || !cfg.isConfigured) {
            showNotice("Connect online to edit Taste DNA")
            return
        }
        viewModelScope.launch {
            apiClient.updateCultureProfile(cfg, homeCountry, localCultureRatio, weatherCity).fold(
                onSuccess = { updated ->
                    _uiState.update { it.copy(cultureProfile = updated, showCultureEditor = false) }
                    showNotice("Updated Taste DNA")
                    val hour = LocalTime.now().hour
                    apiClient.fetchContextualMixes(cfg, hour)
                        .getOrNull()
                        ?.filterNot { it.kind == "seasonal" && it.trackCount == 0 }
                        ?.let { refreshedMixes ->
                            _uiState.update { it.copy(contextualMixes = refreshedMixes) }
                        }
                },
                onFailure = { err ->
                    showNotice(err.localizedMessage ?: "Failed to update Taste DNA")
                },
            )
        }
    }

    // =========================================================================
    // Unified Search & 1-Tap Acquire & Play (Live WebSocket Stage Tracking)
    // =========================================================================

    fun onSearchQueryChanged(query: String) {
        _uiState.update { it.copy(searchQuery = query) }
        searchJob?.cancel()
        if (query.isBlank()) {
            _uiState.update { it.copy(searchResults = null, isSearching = false) }
            return
        }
        searchJob = viewModelScope.launch {
            delay(160L)
            runUnifiedSearch(query)
        }
    }

    fun submitSearch(query: String) {
        val trimmed = query.trim()
        _uiState.update { it.copy(searchQuery = trimmed) }
        if (trimmed.isEmpty()) return
        viewModelScope.launch { prefsStore.addRecentSearch(trimmed) }
        searchJob?.cancel()
        searchJob = viewModelScope.launch { runUnifiedSearch(trimmed) }
    }

    fun clearRecentSearches() {
        viewModelScope.launch { prefsStore.clearRecentSearches() }
    }

    fun setSearchFilter(filter: SearchFilter) {
        _uiState.update { it.copy(searchFilter = filter) }
    }

    private suspend fun runUnifiedSearch(query: String) {
        val core = SoulbeetCoreProvider.getOrNull() ?: return
        _uiState.update { it.copy(isSearching = true) }
        val results = withContext(Dispatchers.IO) {
            runCatching { core.search(query = query, limit = 30u) }.getOrNull()
        }
        _uiState.update { it.copy(searchResults = results, isSearching = false) }
    }

    fun acquireAndPlayTrack(match: FfiOnlineTrackMatch) {
        val cfg = _uiState.value.config
        if (cfg.offlineOnly || !cfg.isConfigured) {
            showNotice("Acquire & Play requires an online connection")
            return
        }
        val key = "${match.artist.lowercase()}::${match.title.lowercase()}"
        if (_uiState.value.acquiringStages.containsKey(key)) return

        viewModelScope.launch {
            _uiState.update { it.copy(acquiringStages = it.acquiringStages + (key to "ACQUIRING…")) }
            val outcome = apiClient.acquireAndPlay(
                config = cfg,
                artist = match.artist,
                title = match.title,
                album = match.album,
                recordingMbid = match.mbid,
                durationSecs = match.durationSecs?.toLong(),
            )
            when (outcome) {
                is AcquireAndPlayOutcome.AlreadyOwned -> {
                    _uiState.update { it.copy(acquiringStages = it.acquiringStages - key) }
                    playAcquiredSongById(outcome.songId, match.title)
                }

                is AcquireAndPlayOutcome.Queued -> {
                    showNotice("Acquiring \"${match.title}\"…")
                    val terminal = apiClient.observeAcquireAndPlay(
                        config = cfg,
                        requestId = outcome.requestId,
                        onStage = { stageLabel ->
                            _uiState.update { st ->
                                st.copy(acquiringStages = st.acquiringStages + (key to stageLabel))
                            }
                        },
                    )
                    _uiState.update { it.copy(acquiringStages = it.acquiringStages - key) }
                    when (terminal) {
                        is AcquireWsEvent.Ready -> {
                            val songId = terminal.songId
                            if (!songId.isNullOrBlank()) {
                                playAcquiredSongById(songId, match.title)
                            } else {
                                withContext(Dispatchers.IO) {
                                    runCatching { SoulbeetCoreProvider.getOrNull()?.syncNow(forceFull = false) }
                                }
                                refreshLibrarySnapshot()
                                showNotice("\"${match.title}\" imported into library")
                            }
                        }

                        is AcquireWsEvent.NeedsManualChoice -> {
                            showNotice("\"${match.title}\" needs manual source selection on web")
                        }

                        is AcquireWsEvent.Failed -> {
                            showNotice(terminal.message)
                        }

                        is AcquireWsEvent.Progress -> Unit
                    }
                }

                is AcquireAndPlayOutcome.Failed -> {
                    _uiState.update { it.copy(acquiringStages = it.acquiringStages - key) }
                    showNotice(outcome.message)
                }
            }
        }
    }

    private suspend fun playAcquiredSongById(songId: String, fallbackTitle: String) {
        val core = SoulbeetCoreProvider.getOrNull()
        val track = withContext(Dispatchers.IO) {
            core?.getTrack(songId) ?: run {
                runCatching { core?.syncNow(forceFull = false) }
                core?.getTrack(songId)
            }
        }
        refreshLibrarySnapshot()
        if (track != null) {
            playTrack(track, sourceLabel = "Acquire & Play")
            showNotice("Playing \"${track.title}\"")
        } else {
            mediaBridge.playContainer("${AutoBrowseTree.PREFIX_TRACK}$songId", fallbackTitle)
        }
    }

    // =========================================================================
    // Library Categories, Pinning & Container Drill-Down
    // =========================================================================

    fun setLibraryCategory(category: LibraryCategory) {
        _uiState.update { it.copy(libraryCategory = category) }
    }

    fun isPinned(target: FfiPinTarget): Boolean =
        _uiState.value.pinnedTargets.any { it.target == target }

    fun togglePin(target: FfiPinTarget, label: String) {
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            val currentlyPinned = isPinned(target)
            val missingCount = withContext(Dispatchers.IO) {
                runCatching {
                    if (currentlyPinned) {
                        core.unpin(target)
                        0
                    } else {
                        val missing = core.pin(target)
                        if (!core.isOfflineOnly() && missing.isNotEmpty()) {
                            val profile = _uiState.value.config.toQualityProfile()
                            for (trackId in missing.take(IMMEDIATE_PIN_DOWNLOAD_CAP)) {
                                runCatching {
                                    core.downloadTrack(
                                        trackId = trackId,
                                        profile = profile,
                                        pinned = true,
                                        pinSource = label,
                                    )
                                }
                            }
                            if (missing.size > IMMEDIATE_PIN_DOWNLOAD_CAP) {
                                enqueueImmediateAutoCache(_uiState.value.config.qualityProfileKey)
                            }
                        }
                        missing.size
                    }
                }.getOrDefault(0)
            }
            refreshLibrarySnapshot()
            _uiState.update { state ->
                val detail = state.selectedContainer
                if (detail != null && detail.pinTarget == target) {
                    state.copy(selectedContainer = detail.copy(isPinned = !currentlyPinned))
                } else {
                    state
                }
            }
            val msg = when {
                currentlyPinned -> "Unpinned $label"
                missingCount > IMMEDIATE_PIN_DOWNLOAD_CAP -> "Pinned $label · caching remaining tracks in background"
                else -> "Pinned $label for offline"
            }
            showNotice(msg)
        }
    }

    private fun enqueueImmediateAutoCache(qualityProfile: String) {
        runCatching {
            val constraints = Constraints.Builder()
                .setRequiredNetworkType(NetworkType.CONNECTED)
                .build()
            val input = Data.Builder()
                .putBoolean(OfflineSyncWorker.KEY_DRAIN_OUTBOX, false)
                .putBoolean(OfflineSyncWorker.KEY_RUN_SYNC, false)
                .putBoolean(OfflineSyncWorker.KEY_RUN_AUTO_CACHE, true)
                .putString(OfflineSyncWorker.KEY_QUALITY_PROFILE, qualityProfile)
                .build()
            val request = OneTimeWorkRequestBuilder<OfflineSyncWorker>()
                .setConstraints(constraints)
                .setInputData(input)
                .build()
            WorkManager.getInstance(appContext).enqueueUniqueWork(
                WORK_IMMEDIATE_AUTO_CACHE,
                ExistingWorkPolicy.REPLACE,
                request,
            )
        }
    }

    fun openPlaylistDetail(playlist: FfiPlaylist) {
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            val tracks = withContext(Dispatchers.IO) {
                runCatching { core.listTracksByPlaylist(playlist.id) }.getOrDefault(emptyList())
            }
            val target = FfiPinTarget.Playlist(playlist.id)
            _uiState.update {
                it.copy(
                    selectedContainer = ContainerDetailUi(
                        title = playlist.name,
                        subtitle = "${tracks.size} tracks",
                        coverArtId = playlist.coverArtId,
                        pinTarget = target,
                        isPinned = isPinned(target),
                        tracks = tracks,
                    ),
                )
            }
        }
    }

    fun openAlbumDetail(album: FfiAlbum) {
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            val tracks = withContext(Dispatchers.IO) {
                runCatching { core.listTracksByAlbum(album.id) }.getOrDefault(emptyList())
            }
            val target = FfiPinTarget.Album(album.id)
            _uiState.update {
                it.copy(
                    selectedContainer = ContainerDetailUi(
                        title = album.name,
                        subtitle = buildString {
                            append(album.artistName)
                            album.year?.let { y -> append(" · $y") }
                        },
                        coverArtId = album.coverArtId,
                        pinTarget = target,
                        isPinned = isPinned(target),
                        tracks = tracks,
                    ),
                )
            }
        }
    }

    fun openArtistDetail(artist: FfiArtist) {
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            val tracks = withContext(Dispatchers.IO) {
                val albums = runCatching { core.listAlbums(artist.id) }.getOrDefault(emptyList())
                albums.flatMap { al ->
                    runCatching { core.listTracksByAlbum(al.id) }.getOrDefault(emptyList())
                }
            }
            val target = FfiPinTarget.Artist(artist.id)
            _uiState.update {
                it.copy(
                    selectedContainer = ContainerDetailUi(
                        title = artist.name,
                        subtitle = "${artist.albumCount} albums · ${tracks.size} tracks",
                        coverArtId = artist.coverArtId,
                        pinTarget = target,
                        isPinned = isPinned(target),
                        tracks = tracks,
                    ),
                )
            }
        }
    }

    fun closeContainerDetail() {
        _uiState.update { it.copy(selectedContainer = null) }
    }

    // =========================================================================
    // Synced Lyrics & Multi-Device Connect
    // =========================================================================

    fun fetchCurrentTrackLyrics() {
        val pb = _uiState.value.playback
        val songId = pb.currentTrackId ?: return
        val cfg = _uiState.value.config
        if (cfg.offlineOnly || !cfg.isConfigured) {
            _uiState.update {
                it.copy(
                    isFetchingLyrics = false,
                    lyricsError = "Go online to fetch synced lyrics",
                )
            }
            return
        }
        val fetchId = ++lyricsFetchSeq
        lyricsJob?.cancel()
        lyricsJob = viewModelScope.launch {
            _uiState.update { it.copy(isFetchingLyrics = true, lyricsError = null) }
            try {
                apiClient.fetchLyrics(cfg, songId, pb.artist, pb.title).fold(
                    onSuccess = { data ->
                        if (_uiState.value.playback.currentTrackId != songId) return@fold
                        _uiState.update {
                            it.copy(
                                lyricsData = data,
                                lyricsFetched = true,
                                lyricsError = if (data == null || data.lines.isEmpty()) {
                                    "No lyrics found for this track"
                                } else {
                                    null
                                },
                            )
                        }
                    },
                    onFailure = { err ->
                        if (_uiState.value.playback.currentTrackId != songId) return@fold
                        _uiState.update {
                            it.copy(
                                lyricsFetched = true,
                                lyricsError = err.localizedMessage ?: "Failed to load lyrics",
                            )
                        }
                    },
                )
            } finally {
                if (lyricsFetchSeq == fetchId) {
                    _uiState.update { it.copy(isFetchingLyrics = false) }
                }
            }
        }
    }

    fun claimActiveConnectDevice() {
        viewModelScope.launch(Dispatchers.IO) {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            runCatching { core.connectClaimActive() }
            runCatching { core.connectState() }.getOrNull()?.let { st ->
                _uiState.update { it.copy(connectState = st) }
            }
        }
    }

    fun transferPlaybackToDevice(targetDeviceId: String) {
        viewModelScope.launch(Dispatchers.IO) {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            val pb = _uiState.value.playback
            val snap = buildPlaybackSnapshot(pb, _uiState.value.livePositionMs)
            runCatching {
                core.connectTransferPlaybackTo(targetDeviceId, snap)
            }.onSuccess {
                withContext(Dispatchers.Main) {
                    if (pb.isPlaying) mediaBridge.togglePlayPause()
                    _uiState.update { it.copy(showConnectSheet = false) }
                    showNotice("Transferred playback")
                }
            }.onFailure { err ->
                withContext(Dispatchers.Main) {
                    showNotice(err.localizedMessage ?: "Transfer failed")
                }
            }
        }
    }

    private fun pushConnectPlaybackSnapshot(pb: MediaPlaybackUiState) {
        val core = SoulbeetCoreProvider.getOrNull() ?: return
        if (core.isOfflineOnly() || _uiState.value.connectState?.connected != true) return
        val snap = buildPlaybackSnapshot(pb, _uiState.value.livePositionMs)
        viewModelScope.launch(Dispatchers.IO) {
            runCatching { core.connectUpdateState(snap) }
        }
    }

    private fun buildPlaybackSnapshot(pb: MediaPlaybackUiState, positionMs: Long): FfiPlaybackSnapshot =
        FfiPlaybackSnapshot(
            songId = pb.currentTrackId,
            title = pb.title.takeIf { it.isNotBlank() },
            artist = pb.artist.takeIf { it.isNotBlank() },
            album = pb.album.takeIf { it.isNotBlank() },
            coverArtId = pb.coverArtId,
            isPlaying = pb.isPlaying,
            positionMs = positionMs.coerceAtLeast(0L).toULong(),
            durationMs = pb.durationMs.coerceAtLeast(0L).toULong(),
            queueSongIds = pb.queueTrackIds,
            queueIndex = pb.currentIndex.coerceAtLeast(0).toUInt(),
            volume = 1.0f,
            isPendingDiscovery = pb.isPendingDiscovery,
            updatedAtUnix = System.currentTimeMillis() / 1_000L,
        )

    private suspend fun applyIncomingConnectQueue(
        songIds: List<String>,
        startIndex: Int,
        startPositionMs: Long,
        autoPlay: Boolean,
        sourceLabel: String,
    ) {
        if (songIds.isEmpty()) return
        val core = SoulbeetCoreProvider.getOrNull() ?: return
        val tracks = withContext(Dispatchers.IO) {
            val byId = runCatching { core.getTracksByIds(songIds) }
                .getOrDefault(emptyList())
                .associateBy { it.id }
            songIds.mapNotNull { byId[it] }
        }
        if (tracks.isEmpty()) return
        withContext(Dispatchers.Main) {
            mediaBridge.playQueueAtPosition(
                tracks = tracks,
                startIndex = startIndex.coerceIn(0, tracks.lastIndex),
                startPositionMs = startPositionMs.coerceAtLeast(0L),
                autoPlay = autoPlay,
                offlineIds = _uiState.value.offlineTrackIds,
                sourceLabel = sourceLabel,
            )
            showNotice("Playback handed off to this phone")
        }
    }

    // =========================================================================
    // Internal Data Loaders & CoreEventListener
    // =========================================================================

    private fun attachCoreListener(core: CoreClient) {
        eventListenerAttachedCore = WeakReference(core)
        val vmRef = WeakReference(this)
        core.setEventListener(
            object : CoreEventListener {
                override fun onConnectStateChanged(state: FfiConnectState) {
                    vmRef.get()?._uiState?.update { it.copy(connectState = state) }
                }

                override fun onConnectEvent(event: FfiConnectEvent) {
                    val vm = vmRef.get() ?: return
                    if (event is FfiConnectEvent.Command) {
                        val cmd = event.command
                        vm.viewModelScope.launch {
                            when (cmd) {
                                is FfiConnectCommand.Play -> {
                                    if (!vm._uiState.value.playback.isPlaying) vm.togglePlayPause()
                                }
                                is FfiConnectCommand.Pause -> {
                                    if (vm._uiState.value.playback.isPlaying) vm.togglePlayPause()
                                }
                                is FfiConnectCommand.TogglePlay -> vm.togglePlayPause()
                                is FfiConnectCommand.Next -> vm.skipNext()
                                is FfiConnectCommand.Previous -> vm.skipPrevious()
                                is FfiConnectCommand.Seek -> vm.seekTo(cmd.positionMs.toLong())
                                is FfiConnectCommand.LoadQueue -> {
                                    vm.applyIncomingConnectQueue(
                                        songIds = cmd.songIds,
                                        startIndex = cmd.startIndex.toInt(),
                                        startPositionMs = cmd.startPositionMs.toLong(),
                                        autoPlay = cmd.autoplay,
                                        sourceLabel = "Connect",
                                    )
                                }
                                is FfiConnectCommand.TransferPlayback -> {
                                    val snap = cmd.snapshot
                                    val ids = snap.queueSongIds.ifEmpty {
                                        listOfNotNull(snap.songId)
                                    }
                                    vm.applyIncomingConnectQueue(
                                        songIds = ids,
                                        startIndex = snap.queueIndex.toInt(),
                                        startPositionMs = snap.positionMs.toLong(),
                                        autoPlay = snap.isPlaying,
                                        sourceLabel = "Transferred",
                                    )
                                }
                                else -> Unit
                            }
                        }
                    }
                }

                override fun onSyncCompleted(report: FfiSyncReport) {
                    vmRef.get()?.let { vm ->
                        vm.viewModelScope.launch { vm.refreshLibrarySnapshot() }
                    }
                }

                override fun onOutboxDrained(report: FfiDrainReport) {
                    vmRef.get()?.let { vm ->
                        vm.viewModelScope.launch { vm.refreshOutboxAndCacheStats() }
                    }
                }

                override fun onCacheChanged(stats: FfiCacheStats) {
                    vmRef.get()?._uiState?.update { it.copy(cacheStats = stats) }
                }
            },
        )
    }

    private fun connectWsIfOnline(
        core: CoreClient,
        cfg: PersistedAppConfig = _uiState.value.config,
        force: Boolean = false,
    ) {
        if (core.isOfflineOnly() || cfg.offlineOnly || !cfg.isConfigured) return
        if (!force && connectWsJob?.isActive == true) return
        if (!force && _uiState.value.connectState?.connected == true) return
        connectWsJob?.cancel()
        connectWsJob = viewModelScope.launch {
            val result = withContext(Dispatchers.IO) {
                runCatching { core.connectWs(heartbeatIntervalSecs = 30uL) }
            }
            result.onFailure { err ->
                android.util.Log.w("SoulbeetConnect", "Failed to connect Soulbeet Connect WebSocket", err)
                if (force) {
                    showNotice("Could not reconnect to Soulbeet Connect")
                }
            }
            val liveState = withContext(Dispatchers.IO) {
                runCatching { core.connectState() }.getOrNull()
            }
            if (liveState != null) {
                _uiState.update { it.copy(connectState = liveState) }
            }
        }
    }

    private suspend fun refreshLibrarySnapshot() {
        val core = SoulbeetCoreProvider.getOrNull() ?: return
        val hidden = hiddenTriageIds.value

        withContext(Dispatchers.IO) {
            val playlists = runCatching { core.listPlaylists() }.getOrDefault(emptyList())
            val artists = runCatching { core.listArtists() }.getOrDefault(emptyList())
            val albums = runCatching { core.listAlbums(null) }.getOrDefault(emptyList())
            val starred = runCatching { core.listStarredTracks() }.getOrDefault(emptyList())
            val discovery = runCatching { core.listDiscoveryTriage(pendingOnly = true) }
                .getOrDefault(emptyList())
            val offlineEntries = runCatching { core.listOfflineTracks() }.getOrDefault(emptyList())
            val offlineIds = offlineEntries.mapTo(HashSet()) { it.trackId }
            val offlineTrackObjs = if (offlineIds.isNotEmpty()) {
                runCatching { core.getTracksByIds(offlineEntries.map { it.trackId }) }
                    .getOrDefault(emptyList())
            } else {
                emptyList()
            }
            val pins = runCatching { core.listPinnedTargets() }.getOrDefault(emptyList())
            val cacheStats = runCatching { core.cacheStats() }.getOrNull()
            val outboxSummary = runCatching { core.outboxSummary() }.getOrNull()

            val triageSongIds = discovery.mapNotNull { it.songId }
            val triageTrackMap = if (triageSongIds.isNotEmpty()) {
                runCatching { core.getTracksByIds(triageSongIds) }
                    .getOrDefault(emptyList())
                    .associateBy { it.id }
            } else {
                emptyMap()
            }
            val deck = discovery
                .filterNot { d ->
                    (d.id.isNotBlank() && hidden.contains(d.id)) ||
                        (!d.songId.isNullOrBlank() && hidden.contains(d.songId))
                }
                .map { d ->
                    val tr = d.songId?.let { triageTrackMap[it] }
                    TriageCardItem(
                        discovery = d,
                        track = tr,
                        isOfflineCached = d.songId?.let { offlineIds.contains(it) } ?: false,
                    )
                }

            val userPlaylists = playlists.filter { it.kind == "user" }
            val quickItems = buildList {
                for (pl in userPlaylists.take(4)) {
                    val pinned = pins.any { it.target == FfiPinTarget.Playlist(pl.id) }
                    add(
                        QuickResumeItem(
                            id = "pl:${pl.id}",
                            title = pl.name,
                            subtitle = "${pl.songCount} tracks",
                            coverArtId = pl.coverArtId,
                            isOfflineReady = pinned,
                            containerMediaId = "${AutoBrowseTree.PREFIX_PLAYLIST}${pl.id}",
                        ),
                    )
                }
                for (al in albums.take(6 - size)) {
                    val pinned = pins.any { it.target == FfiPinTarget.Album(al.id) }
                    add(
                        QuickResumeItem(
                            id = "al:${al.id}",
                            title = al.name,
                            subtitle = al.artistName,
                            coverArtId = al.coverArtId,
                            isOfflineReady = pinned,
                            containerMediaId = "${AutoBrowseTree.PREFIX_ALBUM}${al.id}",
                        ),
                    )
                }
            }.take(6)

            val recent = (starred.take(8) + offlineTrackObjs.take(8))
                .distinctBy { it.id }
                .take(12)

            _uiState.update { state ->
                state.copy(
                    quickResume = quickItems,
                    playlists = playlists,
                    artists = artists,
                    albums = albums,
                    starredTracks = starred,
                    discoveryTracks = discovery,
                    triageDeck = deck,
                    offlineTrackIds = offlineIds,
                    offlineTracks = offlineTrackObjs,
                    pinnedTargets = pins,
                    cacheStats = cacheStats,
                    outboxSummary = outboxSummary,
                    recentTracks = recent,
                )
            }
        }
    }

    private suspend fun refreshOutboxAndCacheStats() {
        val core = SoulbeetCoreProvider.getOrNull() ?: return
        withContext(Dispatchers.IO) {
            val cacheStats = runCatching { core.cacheStats() }.getOrNull()
            val outbox = runCatching { core.outboxSummary() }.getOrNull()
            _uiState.update { it.copy(cacheStats = cacheStats, outboxSummary = outbox) }
        }
    }

    private suspend fun refreshRemoteDiscoveryData(
        cfg: PersistedAppConfig = _uiState.value.config,
    ) {
        if (!cfg.isConfigured || cfg.offlineOnly) return
        val hour = LocalTime.now().hour
        val mixes = apiClient.fetchContextualMixes(cfg, hour)
            .getOrNull()
            ?.filterNot { it.kind == "seasonal" && it.trackCount == 0 }
        val profile = apiClient.fetchCultureProfile(cfg).getOrNull()
        _uiState.update { state ->
            state.copy(
                contextualMixes = mixes ?: state.contextualMixes,
                cultureProfile = profile ?: state.cultureProfile,
            )
        }
    }

    private fun loadActiveTrackDetails(
        trackId: String?,
        queueIds: List<String>,
        clearLyrics: Boolean,
    ) {
        if (clearLyrics) {
            lyricsFetchSeq++
            lyricsJob?.cancel()
            _uiState.update {
                it.copy(
                    lyricsData = null,
                    lyricsError = null,
                    isFetchingLyrics = false,
                    lyricsFetched = false,
                )
            }
        }
        if (trackId.isNullOrBlank()) {
            _uiState.update {
                it.copy(currentTrackDetails = null, currentDiscoveryInfo = null, queueTracks = emptyList())
            }
            return
        }
        if (clearLyrics && _uiState.value.showLyricsPanel) {
            fetchCurrentTrackLyrics()
        }
        viewModelScope.launch(Dispatchers.IO) {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            val details = runCatching { core.getTrack(trackId) }.getOrNull()
            val discovery = runCatching { core.listDiscoveryTriage(pendingOnly = true) }
                .getOrDefault(emptyList())
                .firstOrNull { it.songId == trackId }
            val queue = if (queueIds.isNotEmpty()) {
                val byId = runCatching { core.getTracksByIds(queueIds) }
                    .getOrDefault(emptyList())
                    .associateBy { it.id }
                queueIds.mapNotNull { byId[it] }
            } else {
                emptyList()
            }
            _uiState.update {
                it.copy(
                    currentTrackDetails = details,
                    currentDiscoveryInfo = discovery,
                    queueTracks = queue,
                )
            }
        }
    }

    private fun showNotice(message: String) {
        bannerClearJob?.cancel()
        _uiState.update { it.copy(snackbarMessage = message) }
        bannerClearJob = viewModelScope.launch {
            delay(3_000L)
            _uiState.update { if (it.snackbarMessage == message) it.copy(snackbarMessage = null) else it }
        }
    }

    companion object {
        private const val IMMEDIATE_PIN_DOWNLOAD_CAP = 25
        private const val WORK_IMMEDIATE_AUTO_CACHE = "soulbeet_immediate_auto_cache"
    }
}
