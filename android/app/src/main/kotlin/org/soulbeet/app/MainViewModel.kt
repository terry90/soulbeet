package org.soulbeet.app

import android.app.Application
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.soulbeet.app.data.AcquireAndPlayOutcome
import org.soulbeet.app.data.AppPreferencesStore
import org.soulbeet.app.data.ContextualMixUi
import org.soulbeet.app.data.CultureSteeringOption
import org.soulbeet.app.data.DeleteGuardPreviewUi
import org.soulbeet.app.data.PendingDropAction
import org.soulbeet.app.data.PendingDropUndoController
import org.soulbeet.app.data.PersistedAppConfig
import org.soulbeet.app.data.SoulbeetApiClient
import org.soulbeet.app.data.StoredAuthKind
import org.soulbeet.app.data.TrackLyricsData
import org.soulbeet.app.data.UserCultureProfileUi
import org.soulbeet.core.CoreEventListener
import org.soulbeet.core.FfiAlbum
import org.soulbeet.core.FfiArtist
import org.soulbeet.core.FfiCacheStats
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
    OFFLINE("Offline"),
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
    val recentTracks: List<FfiTrack> = emptyList(),
    // Search
    val searchQuery: String = "",
    val searchFilter: SearchFilter = SearchFilter.ALL,
    val searchResults: FfiSearchResults? = null,
    val isSearching: Boolean = false,
    val acquiringTrackKeys: Set<String> = emptySet(),
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
    val activeAcquisitionsCount: Int get() = acquiringTrackKeys.size
    val pendingDiscoveryCount: Int get() = triageDeck.size
    val seasonalHeroMix: ContextualMixUi?
        get() = contextualMixes.firstOrNull { it.kind == "seasonal" }
            ?: contextualMixes.firstOrNull()
    val rollingAndMomentMixes: List<ContextualMixUi>
        get() = contextualMixes.filterNot { it.slug == seasonalHeroMix?.slug }
}

/**
 * Unified ViewModel for the Jetpack Compose mobile app, backed by [SoulbeetCoreProvider],
 * [SoulbeetMediaRepository.shared], [MediaControllerBridge], and [AppPreferencesStore].
 */
class MainViewModel(application: Application) : AndroidViewModel(application) {

    private val appContext = application.applicationContext
    private val prefsStore: AppPreferencesStore =
        (application as? SoulbeetApplication)?.preferencesStore ?: AppPreferencesStore(appContext)
    private val apiClient = SoulbeetApiClient()
    private val mediaBridge = MediaControllerBridge(appContext)
    val artAuthority: String = CoverArtContentProvider.authorityFor(appContext)

    private val _uiState = MutableStateFlow(AppUiState())
    val uiState: StateFlow<AppUiState> = _uiState.asStateFlow()

    private val hiddenTriageIds = MutableStateFlow<Set<String>>(emptySet())
    private var searchJob: Job? = null
    private var bannerClearJob: Job? = null
    private var eventListenerAttachedCore: WeakReference<CoreClient>? = null

    private val undoController = PendingDropUndoController(
        scope = viewModelScope,
        onCommit = { action -> commitDropMutation(action) },
        onUndo = { action ->
            hiddenTriageIds.update { it - action.trackId - (action.discoveryTrackId ?: "") }
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
                SoulbeetCoreProvider.getOrNull()?.setOfflineOnly(cfg.offlineOnly)
            }
        }

        // Observe shared playback state from SoulbeetMediaLibraryService.
        viewModelScope.launch {
            SoulbeetMediaRepository.shared.playbackState.collectLatest { pb ->
                val prevTrackId = _uiState.value.playback.currentTrackId
                _uiState.update { state ->
                    state.copy(
                        playback = pb,
                        livePositionMs = if (!pb.isPlaying) pb.positionMs else state.livePositionMs,
                    )
                }
                if (pb.currentTrackId != prevTrackId) {
                    loadActiveTrackDetails(pb.currentTrackId, pb.queueTrackIds)
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

        // Boot core & load initial data.
        viewModelScope.launch {
            val core = withContext(Dispatchers.IO) { SoulbeetCoreProvider.awaitOrNull() }
            _uiState.update { it.copy(isBooting = false) }
            if (core != null) {
                attachCoreListener(core)
                refreshLibrarySnapshot()
                refreshRemoteDiscoveryData()
                connectWsIfOnline(core)
            }
        }
    }

    override fun onCleared() {
        // Flush any in-flight 5s Undo drop so closing the app never silently loses an intent.
        val pending = undoController.pending.value
        if (pending != null) {
            val core = SoulbeetCoreProvider.getOrNull()
            if (core != null) {
                kotlinx.coroutines.CoroutineScope(Dispatchers.IO).launch {
                    runCatching {
                        core.enqueueMutation(pending.toMutation())
                        SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
                    }
                }
            }
        }
        eventListenerAttachedCore?.get()?.let { runCatching { it.clearEventListener() } }
        mediaBridge.release()
        super.onCleared()
    }

    // =========================================================================
    // Onboarding & Session Connection
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

    fun connectWithToken(serverUrl: String, token: String, isBearer: Boolean) {
        if (serverUrl.isBlank() || token.isBlank()) {
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
                authKind = if (isBearer) StoredAuthKind.BEARER else StoredAuthKind.COOKIE,
                authToken = token.trim(),
                username = "",
            )
        }
    }

    private suspend fun completeSessionSetup(
        serverUrl: String,
        authKind: StoredAuthKind,
        authToken: String,
        username: String,
    ) {
        try {
            prefsStore.saveSession(
                serverUrl = serverUrl,
                authKind = authKind,
                authToken = authToken,
                username = username,
            )
            val snapshot = prefsStore.getSnapshot()

            val core = withContext(Dispatchers.IO) {
                SoulbeetCoreProvider.clear(closeExisting = true)
                val opened = SoulbeetCoreProvider.open(snapshot.toFfiClientConfig(appContext))
                val bootstrap = opened.fetchBootstrap()
                if (bootstrap != null) {
                    val resolvedUser = bootstrap.username.ifBlank { username }
                    prefsStore.saveSession(
                        serverUrl = serverUrl,
                        authKind = authKind,
                        authToken = authToken,
                        username = resolvedUser,
                        navidrome = bootstrap.navidrome,
                    )
                    bootstrap.navidrome?.let { opened.setNavidromeCredentials(it) }
                }
                opened
            }

            attachCoreListener(core)
            SoulbeetWorkScheduler.schedulePeriodicSync(
                context = appContext,
                qualityProfile = snapshot.qualityProfileKey,
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
        viewModelScope.launch {
            _uiState.update { it.copy(showSettingsSheet = false) }
            withContext(Dispatchers.IO) {
                SoulbeetCoreProvider.getOrNull()?.let { core ->
                    runCatching { core.disconnectWs() }
                    runCatching { core.shutdown() }
                }
                SoulbeetCoreProvider.clear(closeExisting = true)
                SoulbeetCoreProvider.markUnconfigured()
                prefsStore.clearSession()
            }
            SoulbeetMediaRepository.shared.clearActiveTrack()
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
            SoulbeetCoreProvider.getOrNull()?.setOfflineOnly(next)
            SoulbeetMediaRepository.shared.setOfflineOnly(next)
            refreshLibrarySnapshot()
            if (_uiState.value.searchQuery.isNotBlank()) {
                runUnifiedSearch(_uiState.value.searchQuery)
            }
            if (!next) {
                SoulbeetCoreProvider.getOrNull()?.let { core ->
                    connectWsIfOnline(core)
                    SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
                }
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
            val parsed = org.soulbeet.app.data.parseAudioQualityProfile(profileKey)
            SoulbeetMediaRepository.shared.setAudioQualityProfile(parsed)
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
            viewModelScope.launch(Dispatchers.IO) {
                val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
                runCatching { core.connectState() }.getOrNull()?.let { st ->
                    _uiState.update { it.copy(connectState = st) }
                }
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
    // Playback Controls
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

    fun playContainer(mediaId: String, title: String? = null) {
        mediaBridge.playContainer(mediaId, title)
    }

    fun togglePlayPause() = mediaBridge.togglePlayPause()
    fun skipNext() = mediaBridge.skipNext()
    fun skipPrevious() = mediaBridge.skipPrevious()
    fun seekTo(positionMs: Long) = mediaBridge.seekTo(positionMs)
    fun playQueueIndex(index: Int) = mediaBridge.playQueueIndex(index)
    fun toggleShuffle() = mediaBridge.toggleShuffle()
    fun cycleRepeatMode() = mediaBridge.cycleRepeatMode()
    fun startTrackRadio() {
        mediaBridge.sendCustomAction(AutoCustomActions.ACTION_START_RADIO)
        showNotice("Starting track radio…")
    }

    // =========================================================================
    // Curation (Keep, Star, Rate, 5-Second Undo Drop, Multi-Listener Guard)
    // =========================================================================

    fun keepDiscoveryTrack(trackId: String, discoveryTrackId: String? = null, title: String = "Track") {
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            if (_uiState.value.playback.currentTrackId == trackId) {
                mediaBridge.sendCustomAction(AutoCustomActions.ACTION_KEEP_DISCOVERY)
            } else {
                withContext(Dispatchers.IO) {
                    runCatching {
                        core.enqueueMutation(
                            FfiOutboxMutation.Keep(
                                songId = trackId,
                                discoveryTrackId = discoveryTrackId,
                                targetFolderId = null,
                            ),
                        )
                        SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
                    }
                }
            }
            hiddenTriageIds.update { it + trackId + listOfNotNull(discoveryTrackId) }
            refreshLibrarySnapshot()
            showNotice("Kept \"$title\" in Library")
        }
    }

    fun starAndKeepDiscoveryTrack(trackId: String, discoveryTrackId: String? = null, title: String = "Track") {
        viewModelScope.launch {
            val core = SoulbeetCoreProvider.getOrNull() ?: return@launch
            withContext(Dispatchers.IO) {
                runCatching {
                    core.enqueueMutation(
                        FfiOutboxMutation.Star(
                            songId = trackId,
                            discoveryTrackId = discoveryTrackId,
                        ),
                    )
                    SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
                }
            }
            if (_uiState.value.playback.currentTrackId == trackId) {
                val ctx = _uiState.value.playback.toTrackActionContext()
                if (ctx != null) {
                    SoulbeetMediaRepository.shared.updateTrackContext(
                        ctx.copy(isStarred = true, isPendingDiscovery = false),
                    )
                }
            }
            hiddenTriageIds.update { it + trackId + listOfNotNull(discoveryTrackId) }
            refreshLibrarySnapshot()
            showNotice("Starred & kept \"$title\"")
        }
    }

    fun dropDiscoveryTrackWithUndo(
        trackId: String,
        title: String,
        artist: String,
        discoveryTrackId: String? = null,
        sourceLabel: String? = "Discovery",
    ) {
        hiddenTriageIds.update { it + trackId + listOfNotNull(discoveryTrackId) }
        _uiState.update { state ->
            state.copy(
                triageDeck = state.triageDeck.filterNot {
                    it.discovery.id == discoveryTrackId || it.discovery.songId == trackId
                },
            )
        }
        // Immediately skip if dropping the active track for frictionless flow.
        if (_uiState.value.playback.currentTrackId == trackId) {
            mediaBridge.skipNext()
        }
        undoController.scheduleDrop(
            PendingDropAction(
                trackId = trackId,
                title = title,
                artist = artist,
                discoveryTrackId = discoveryTrackId,
                confirmDelete = false,
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
            if (_uiState.value.playback.currentTrackId == trackId) {
                mediaBridge.sendCustomAction(AutoCustomActions.ACTION_STAR_TOGGLE)
                delay(150L)
                refreshLibrarySnapshot()
                return@launch
            }
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
                val ctx = _uiState.value.playback.toTrackActionContext()
                if (ctx != null) {
                    SoulbeetMediaRepository.shared.updateTrackContext(
                        ctx.copy(
                            userRating = clamped,
                            isPendingDiscovery = if (clamped >= 4) false else ctx.isPendingDiscovery,
                        ),
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
                    blockReason = "Offline mode active — removal will queue as 1★ & unstar and run Soulbeet's Multi-Listener Guard on reconnect.",
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
                        blockReason = "Could not verify household listeners right now — removal will unstar & hide from your mixes safely.",
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
        if (_uiState.value.playback.currentTrackId == preview.songId) {
            mediaBridge.skipNext()
        }
        val confirmDelete = preview.canDelete && preview.soleOwner && !preview.offlineFallback
        undoController.scheduleDrop(
            PendingDropAction(
                trackId = preview.songId,
                title = preview.trackTitle,
                artist = preview.trackArtist,
                discoveryTrackId = preview.discoveryTrackId,
                confirmDelete = confirmDelete,
                sourceLabel = if (confirmDelete) "Library" else "Your mixes",
                createdAtMs = System.currentTimeMillis(),
            ),
        )
    }

    private suspend fun commitDropMutation(action: PendingDropAction) {
        val core = SoulbeetCoreProvider.getOrNull() ?: return
        withContext(Dispatchers.IO) {
            runCatching {
                core.enqueueMutation(action.toMutation())
                SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(appContext)
            }
        }
        refreshLibrarySnapshot()
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

    fun updateCultureProfile(homeCountry: String?, localCultureRatio: Float) {
        val cfg = _uiState.value.config
        if (cfg.offlineOnly || !cfg.isConfigured) {
            showNotice("Connect online to edit Taste DNA")
            return
        }
        viewModelScope.launch {
            apiClient.updateCultureProfile(cfg, homeCountry, localCultureRatio).fold(
                onSuccess = { updated ->
                    _uiState.update { it.copy(cultureProfile = updated, showCultureEditor = false) }
                    showNotice("Updated Taste DNA")
                },
                onFailure = { err ->
                    showNotice(err.localizedMessage ?: "Failed to update Taste DNA")
                },
            )
        }
    }

    // =========================================================================
    // Unified Search & 1-Tap Acquire & Play
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
        if (_uiState.value.acquiringTrackKeys.contains(key)) return

        viewModelScope.launch {
            _uiState.update { it.copy(acquiringTrackKeys = it.acquiringTrackKeys + key) }
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
                    _uiState.update { it.copy(acquiringTrackKeys = it.acquiringTrackKeys - key) }
                    val core = SoulbeetCoreProvider.getOrNull()
                    val track = withContext(Dispatchers.IO) {
                        core?.getTrack(outcome.songId) ?: run {
                            core?.syncNow(forceFull = false)
                            core?.getTrack(outcome.songId)
                        }
                    }
                    if (track != null) {
                        playTrack(track, sourceLabel = "Search")
                        showNotice("Playing \"${track.title}\" from your library")
                    } else {
                        mediaBridge.playContainer("${AutoBrowseTree.PREFIX_TRACK}${outcome.songId}", match.title)
                    }
                }
                is AcquireAndPlayOutcome.Queued -> {
                    showNotice("Acquiring \"${match.title}\"…")
                    // Keep seam-light shimmer visible while acquisition kicks off, then refresh mirror.
                    delay(4_500L)
                    withContext(Dispatchers.IO) {
                        runCatching { SoulbeetCoreProvider.getOrNull()?.syncNow(forceFull = false) }
                    }
                    refreshLibrarySnapshot()
                    _uiState.update { it.copy(acquiringTrackKeys = it.acquiringTrackKeys - key) }
                }
                is AcquireAndPlayOutcome.Failed -> {
                    _uiState.update { it.copy(acquiringTrackKeys = it.acquiringTrackKeys - key) }
                    showNotice(outcome.message)
                }
            }
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
            withContext(Dispatchers.IO) {
                runCatching {
                    if (currentlyPinned) {
                        core.unpin(target)
                    } else {
                        val missing = core.pin(target)
                        if (!core.isOfflineOnly() && missing.isNotEmpty()) {
                            val profile = _uiState.value.config.toQualityProfile()
                            for (trackId in missing.take(25)) {
                                runCatching {
                                    core.downloadTrack(
                                        trackId = trackId,
                                        profile = profile,
                                        pinned = true,
                                        pinSource = label,
                                    )
                                }
                            }
                        }
                    }
                }
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
            showNotice(if (currentlyPinned) "Unpinned $label" else "Pinned $label for offline")
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
        viewModelScope.launch {
            _uiState.update { it.copy(isFetchingLyrics = true, lyricsError = null) }
            apiClient.fetchLyrics(cfg, songId, pb.artist, pb.title).fold(
                onSuccess = { data ->
                    _uiState.update {
                        it.copy(
                            isFetchingLyrics = false,
                            lyricsData = data,
                            lyricsError = if (data == null || data.lines.isEmpty()) {
                                "No synced lyrics saved yet"
                            } else {
                                null
                            },
                        )
                    }
                },
                onFailure = { err ->
                    _uiState.update {
                        it.copy(
                            isFetchingLyrics = false,
                            lyricsError = err.localizedMessage ?: "Failed to load lyrics",
                        )
                    }
                },
            )
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
            val snap = FfiPlaybackSnapshot(
                songId = pb.currentTrackId,
                title = pb.title.takeIf { it.isNotBlank() },
                artist = pb.artist.takeIf { it.isNotBlank() },
                album = pb.album.takeIf { it.isNotBlank() },
                coverArtId = pb.coverArtId,
                isPlaying = pb.isPlaying,
                positionMs = _uiState.value.livePositionMs.coerceAtLeast(0L).toULong(),
                durationMs = pb.durationMs.coerceAtLeast(0L).toULong(),
                queueSongIds = pb.queueTrackIds,
                queueIndex = pb.currentIndex.coerceAtLeast(0).toUInt(),
                volume = 1.0f,
                isPendingDiscovery = pb.isPendingDiscovery,
                updatedAtUnix = System.currentTimeMillis() / 1_000L,
            )
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
                                is org.soulbeet.core.FfiConnectCommand.Play -> {
                                    if (!vm._uiState.value.playback.isPlaying) vm.togglePlayPause()
                                }
                                is org.soulbeet.core.FfiConnectCommand.Pause -> {
                                    if (vm._uiState.value.playback.isPlaying) vm.togglePlayPause()
                                }
                                is org.soulbeet.core.FfiConnectCommand.TogglePlay -> vm.togglePlayPause()
                                is org.soulbeet.core.FfiConnectCommand.Next -> vm.skipNext()
                                is org.soulbeet.core.FfiConnectCommand.Previous -> vm.skipPrevious()
                                is org.soulbeet.core.FfiConnectCommand.Seek -> {
                                    vm.seekTo(cmd.positionMs.toLong())
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

    private fun connectWsIfOnline(core: CoreClient) {
        if (core.isOfflineOnly() || !_uiState.value.config.isConfigured) return
        viewModelScope.launch(Dispatchers.IO) {
            runCatching { core.connectWs(heartbeatIntervalSecs = 30uL) }
            runCatching { core.connectState() }.getOrNull()?.let { st ->
                _uiState.update { it.copy(connectState = st) }
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

            // Hydrate triage deck with full FfiTrack metadata.
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
                    hidden.contains(d.id) || (d.songId != null && hidden.contains(d.songId))
                }
                .map { d ->
                    val tr = d.songId?.let { triageTrackMap[it] }
                    TriageCardItem(
                        discovery = d,
                        track = tr,
                        isOfflineCached = d.songId?.let { offlineIds.contains(it) } ?: false,
                    )
                }

            // Build Quick-Resume 2x3 grid from pinned/top playlists + recent albums.
            val quickItems = buildList {
                for (pl in playlists.take(4)) {
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

    private suspend fun refreshRemoteDiscoveryData() {
        val cfg = _uiState.value.config
        if (!cfg.isConfigured || cfg.offlineOnly) return
        val hour = LocalTime.now().hour
        val mixes = apiClient.fetchContextualMixes(cfg, hour).getOrNull()
        val profile = apiClient.fetchCultureProfile(cfg).getOrNull()
        _uiState.update { state ->
            state.copy(
                contextualMixes = mixes ?: state.contextualMixes,
                cultureProfile = profile ?: state.cultureProfile,
            )
        }
    }

    private fun loadActiveTrackDetails(trackId: String?, queueIds: List<String>) {
        _uiState.update { it.copy(lyricsData = null, lyricsError = null) }
        if (trackId.isNullOrBlank()) {
            _uiState.update {
                it.copy(currentTrackDetails = null, currentDiscoveryInfo = null, queueTracks = emptyList())
            }
            return
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
}
