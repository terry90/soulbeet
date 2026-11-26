package org.soulbeet.media

import android.content.Intent
import android.os.Bundle
import androidx.annotation.OptIn
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.source.DefaultMediaSourceFactory
import androidx.media3.session.LibraryResult
import androidx.media3.session.MediaConstants
import androidx.media3.session.MediaLibraryService
import androidx.media3.session.MediaSession
import androidx.media3.session.SessionCommand
import androidx.media3.session.SessionError
import androidx.media3.session.SessionResult
import com.google.common.collect.ImmutableList
import com.google.common.util.concurrent.ListenableFuture
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.guava.future
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import org.soulbeet.core.FfiAutoNodeKind
import org.soulbeet.core.FfiTrack
import org.soulbeet.corebindings.CoreClient
import org.soulbeet.corebindings.SoulbeetCoreProvider
import java.util.concurrent.ConcurrentHashMap

/**
 * Unified Media3 [MediaLibraryService] powering both Android Auto (4-tab `CAR.VALIDATOR` browse
 * tree, 2-slot context-aware custom actions, voice search, car-ignition resumption) and the
 * Jetpack Compose mobile client through a single [ExoPlayer] + [CoreClient] instance.
 */
@OptIn(UnstableApi::class)
class SoulbeetMediaLibraryService : MediaLibraryService() {

    private val serviceScope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val repository: SoulbeetMediaRepository = SoulbeetMediaRepository.shared
    private val scrobbleTracker = ScrobbleTracker()
    private val searchResultCache = ConcurrentHashMap<String, List<MediaItem>>()

    private var player: ExoPlayer? = null
    private var mediaLibrarySession: MediaLibrarySession? = null
    private var progressPollingJob: Job? = null
    private var currentTrackContext: TrackActionContext? = null

    private val artAuthority: String
        get() = CoverArtContentProvider.authorityFor(applicationContext)

    private fun coreOrNull(): CoreClient? = SoulbeetCoreProvider.getOrNull()

    override fun onCreate() {
        super.onCreate()

        val dataSourceFactory = SoulbeetDataSourceFactory(
            coreProvider = { coreOrNull() },
            qualityProfileProvider = { repository.playbackState.value.audioQualityProfile },
        )

        val audioAttributes = AudioAttributes.Builder()
            .setUsage(C.USAGE_MEDIA)
            .setContentType(C.AUDIO_CONTENT_TYPE_MUSIC)
            .build()

        val exoPlayer = ExoPlayer.Builder(this)
            .setMediaSourceFactory(DefaultMediaSourceFactory(dataSourceFactory))
            .setAudioAttributes(audioAttributes, /* handleAudioFocus = */ true)
            .setHandleAudioBecomingNoisy(true)
            .setWakeMode(C.WAKE_MODE_NETWORK)
            .build()

        exoPlayer.addListener(playerListener)
        player = exoPlayer

        mediaLibrarySession = MediaLibrarySession.Builder(this, exoPlayer, libraryCallback)
            .build()
    }

    override fun onGetSession(controllerInfo: MediaSession.ControllerInfo): MediaLibrarySession? =
        mediaLibrarySession

    override fun onTaskRemoved(rootIntent: Intent?) {
        val currentPlayer = player
        persistCurrentSessionAsync(currentPlayer)
        if (currentPlayer == null || !currentPlayer.playWhenReady || currentPlayer.mediaItemCount == 0) {
            stopSelf()
        }
    }

    override fun onDestroy() {
        progressPollingJob?.cancel()
        progressPollingJob = null
        val currentPlayer = player
        if (currentPlayer != null) {
            val snapshot = captureSessionSnapshot(currentPlayer)
            val core = coreOrNull()
            if (core != null && snapshot != null) {
                serviceScope.launch(Dispatchers.IO) {
                    runCatching {
                        PlaybackSessionStore.saveSession(
                            core = core,
                            queueTrackIds = snapshot.queueTrackIds,
                            currentIndex = snapshot.currentIndex,
                            positionMs = snapshot.positionMs,
                            isPlaying = false,
                            sourceLabel = snapshot.sourceLabel,
                        )
                    }
                }
            }
            currentPlayer.removeListener(playerListener)
            currentPlayer.release()
        }
        mediaLibrarySession?.release()
        mediaLibrarySession = null
        player = null
        serviceScope.cancel()
        super.onDestroy()
    }

    private val libraryCallback = object : MediaLibrarySession.Callback {

        override fun onConnect(
            session: MediaSession,
            controller: MediaSession.ControllerInfo,
        ): MediaSession.ConnectionResult {
            val availableSessionCommands = MediaSession.ConnectionResult.DEFAULT_SESSION_AND_LIBRARY_COMMANDS
                .buildUpon()
                .apply {
                    for (cmd in AutoCustomActions.ALL_SESSION_COMMANDS) {
                        add(cmd)
                    }
                }
                .build()

            val customLayout = AutoCustomActions.buildCustomLayout(
                trackContext = currentTrackContext,
                secondaryLibraryAction = repository.playbackState.value.librarySecondaryAction,
            )

            return MediaSession.ConnectionResult.AcceptedResultBuilder(session)
                .setAvailableSessionCommands(availableSessionCommands)
                .setCustomLayout(customLayout)
                .build()
        }

        override fun onGetLibraryRoot(
            session: MediaLibrarySession,
            browser: MediaSession.ControllerInfo,
            params: LibraryParams?,
        ): ListenableFuture<LibraryResult<MediaItem>> = serviceScope.future {
            val rootExtras = Bundle().apply {
                putBoolean(AutoBrowseTree.EXTRAS_KEY_CONTENT_STYLE_SUPPORTED, true)
                putBoolean(AutoBrowseTree.EXTRAS_KEY_SEARCH_SUPPORTED, true)
                putInt(
                    MediaConstants.EXTRAS_KEY_CONTENT_STYLE_BROWSABLE,
                    MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_GRID_ITEM,
                )
                putInt(
                    MediaConstants.EXTRAS_KEY_CONTENT_STYLE_PLAYABLE,
                    MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM,
                )
            }
            val responseParams = LibraryParams.Builder()
                .setExtras(rootExtras)
                .setOffline(params?.isOffline ?: false)
                .setRecent(params?.isRecent ?: false)
                .setSuggested(params?.isSuggested ?: false)
                .build()
            LibraryResult.ofItem(AutoBrowseTree.rootMediaItem(), responseParams)
        }

        override fun onGetChildren(
            session: MediaLibrarySession,
            browser: MediaSession.ControllerInfo,
            parentId: String,
            page: Int,
            pageSize: Int,
            params: LibraryParams?,
        ): ListenableFuture<LibraryResult<ImmutableList<MediaItem>>> =
            serviceScope.future(Dispatchers.IO) {
                val core = coreOrNull()
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_INVALID_STATE)
                val offlineOnly = (params?.isOffline == true) || core.isOfflineOnly()
                val items = runCatching {
                    AutoBrowseTree.loadChildren(
                        core = core,
                        parentId = parentId,
                        page = page,
                        pageSize = pageSize,
                        artAuthority = artAuthority,
                        offlineOnly = offlineOnly,
                    )
                }.getOrElse {
                    return@future LibraryResult.ofError(SessionError.ERROR_IO)
                }
                LibraryResult.ofItemList(ImmutableList.copyOf(items), params)
            }

        override fun onGetItem(
            session: MediaLibrarySession,
            browser: MediaSession.ControllerInfo,
            mediaId: String,
        ): ListenableFuture<LibraryResult<MediaItem>> =
            serviceScope.future(Dispatchers.IO) {
                if (mediaId == AutoBrowseTree.ROOT_ID) {
                    return@future LibraryResult.ofItem(AutoBrowseTree.rootMediaItem(), null)
                }
                val core = coreOrNull()
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_INVALID_STATE)

                val matchingTab = core.autoRootTabs().firstOrNull { it.id == mediaId }
                if (matchingTab != null) {
                    return@future LibraryResult.ofItem(
                        AutoBrowseTree.mapNode(matchingTab, artAuthority),
                        null,
                    )
                }

                val trackId = AutoBrowseTree.extractTrackId(mediaId)
                val track = runCatching { core.getTrack(trackId) }.getOrNull()
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_BAD_VALUE)
                val isCached = runCatching {
                    core.listOfflineTracks().any { it.trackId == track.id }
                }.getOrDefault(false)

                LibraryResult.ofItem(
                    AutoBrowseTree.mapTrack(
                        track = track,
                        artAuthority = artAuthority,
                        isOfflineCached = isCached,
                        mediaUri = SoulbeetDataSourceFactory.trackUri(track.id),
                    ),
                    null,
                )
            }

        override fun onAddMediaItems(
            mediaSession: MediaSession,
            controller: MediaSession.ControllerInfo,
            mediaItems: MutableList<MediaItem>,
        ): ListenableFuture<MutableList<MediaItem>> =
            serviceScope.future(Dispatchers.IO) {
                val core = coreOrNull() ?: return@future mediaItems
                hydrateMediaItems(core, mediaItems).toMutableList()
            }

        override fun onSetMediaItems(
            mediaSession: MediaSession,
            controller: MediaSession.ControllerInfo,
            mediaItems: MutableList<MediaItem>,
            startIndex: Int,
            startPositionMs: Long,
        ): ListenableFuture<MediaSession.MediaItemsWithStartPosition> =
            serviceScope.future(Dispatchers.IO) {
                val core = coreOrNull()
                    ?: return@future MediaSession.MediaItemsWithStartPosition(
                        mediaItems,
                        startIndex,
                        startPositionMs,
                    )
                val hydrated = hydrateMediaItems(core, mediaItems)
                val safeIndex = when {
                    hydrated.isEmpty() -> 0
                    mediaItems.size == 1 && hydrated.size > 1 -> 0
                    startIndex == C.INDEX_UNSET -> 0
                    else -> startIndex.coerceIn(0, hydrated.lastIndex)
                }
                val safePos = if (startPositionMs == C.TIME_UNSET) {
                    C.TIME_UNSET
                } else {
                    startPositionMs.coerceAtLeast(0L)
                }
                MediaSession.MediaItemsWithStartPosition(hydrated, safeIndex, safePos)
            }

        override fun onSearch(
            session: MediaLibrarySession,
            browser: MediaSession.ControllerInfo,
            query: String,
            params: LibraryParams?,
        ): ListenableFuture<LibraryResult<Void>> =
            serviceScope.future(Dispatchers.IO) {
                val core = coreOrNull()
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_INVALID_STATE)
                val normalizedKey = query.trim().lowercase()
                val items = executeVoiceSearch(core, query)
                searchResultCache[normalizedKey] = items
                withContext(Dispatchers.Main.immediate) {
                    session.notifySearchResultChanged(browser, query, items.size, params)
                }
                LibraryResult.ofVoid(params)
            }

        override fun onGetSearchResult(
            session: MediaLibrarySession,
            browser: MediaSession.ControllerInfo,
            query: String,
            page: Int,
            pageSize: Int,
            params: LibraryParams?,
        ): ListenableFuture<LibraryResult<ImmutableList<MediaItem>>> =
            serviceScope.future(Dispatchers.IO) {
                val core = coreOrNull()
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_INVALID_STATE)
                val normalizedKey = query.trim().lowercase()
                val items = searchResultCache[normalizedKey] ?: executeVoiceSearch(core, query).also {
                    searchResultCache[normalizedKey] = it
                }
                val paged = AutoBrowseTree.paginate(items, page, pageSize)
                LibraryResult.ofItemList(ImmutableList.copyOf(paged), params)
            }

        override fun onPlaybackResumption(
            mediaSession: MediaSession,
            controller: MediaSession.ControllerInfo,
        ): ListenableFuture<MediaSession.MediaItemsWithStartPosition> =
            serviceScope.future(Dispatchers.IO) {
                val core = coreOrNull()
                    ?: return@future MediaSession.MediaItemsWithStartPosition(
                        emptyList(),
                        0,
                        C.TIME_UNSET,
                    )
                val restored = PlaybackSessionStore.restoreSession(
                    core = core,
                    artAuthority = artAuthority,
                ) ?: return@future MediaSession.MediaItemsWithStartPosition(
                    emptyList(),
                    0,
                    C.TIME_UNSET,
                )
                MediaSession.MediaItemsWithStartPosition(
                    restored.items,
                    restored.startIndex,
                    restored.startPositionMs,
                )
            }

        override fun onCustomCommand(
            session: MediaSession,
            controller: MediaSession.ControllerInfo,
            customCommand: SessionCommand,
            args: Bundle,
        ): ListenableFuture<SessionResult> = serviceScope.future {
            val core = coreOrNull()
                ?: return@future SessionResult(SessionError.ERROR_INVALID_STATE)

            val handler = AutoCustomActions.ActionHandler(
                core = core,
                secondaryLibraryActionProvider = { repository.playbackState.value.librarySecondaryAction },
                currentTrackContextProvider = { currentTrackContext },
                onUpdateTrackContext = { updated ->
                    currentTrackContext = updated
                    repository.updateTrackContext(updated)
                },
                onRefreshCustomLayout = { layout ->
                    mediaLibrarySession?.setCustomLayout(layout)
                },
                onSkipToNext = {
                    player?.let { p ->
                        if (p.hasNextMediaItem()) {
                            p.seekToNextMediaItem()
                        } else {
                            p.stop()
                        }
                    }
                },
                onStartTrackRadio = { seedTrackId ->
                    startArtistRadioForTrack(core, seedTrackId)
                },
                onMutationEnqueued = {
                    runCatching { SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(applicationContext) }
                },
            )

            withContext(Dispatchers.IO) {
                handler.handle(customCommand.customAction)
            }
        }
    }

    private val playerListener = object : Player.Listener {
        override fun onMediaItemTransition(mediaItem: MediaItem?, reason: Int) {
            val currentPlayer = player ?: return
            handleTrackTransition(currentPlayer, mediaItem)
        }

        override fun onIsPlayingChanged(isPlaying: Boolean) {
            val currentPlayer = player ?: return
            repository.updatePlaybackStatus(
                isPlaying = isPlaying,
                isBuffering = currentPlayer.playbackState == Player.STATE_BUFFERING,
                positionMs = currentPlayer.currentPosition,
                durationMs = currentPlayer.duration.coerceAtLeast(0L),
            )
            if (isPlaying) {
                startProgressPolling()
            } else {
                progressPollingJob?.cancel()
                progressPollingJob = null
                persistCurrentSessionAsync(currentPlayer)
            }
        }

        override fun onPlaybackStateChanged(playbackState: Int) {
            val currentPlayer = player ?: return
            repository.updatePlaybackStatus(
                isPlaying = currentPlayer.isPlaying,
                isBuffering = playbackState == Player.STATE_BUFFERING,
                positionMs = currentPlayer.currentPosition,
                durationMs = currentPlayer.duration.coerceAtLeast(0L),
            )
        }

        override fun onPlayerError(error: PlaybackException) {
            repository.setLastError(error.localizedMessage ?: "Playback failed")
            val currentPlayer = player ?: return
            val core = coreOrNull() ?: return
            if (core.isOfflineOnly() || isNetworkUnavailableError(error)) {
                skipToNextCachedTrack(currentPlayer, core)
            }
        }
    }

    private fun handleTrackTransition(currentPlayer: ExoPlayer, mediaItem: MediaItem?) {
        val baseContext = AutoCustomActions.contextFromMediaItem(mediaItem)
        currentTrackContext = baseContext
        mediaLibrarySession?.setCustomLayout(
            AutoCustomActions.buildCustomLayout(
                trackContext = baseContext,
                secondaryLibraryAction = repository.playbackState.value.librarySecondaryAction,
            ),
        )

        if (mediaItem == null || baseContext == null) {
            scrobbleTracker.reset()
            return
        }

        val queueIds = extractQueueTrackIds(currentPlayer)
        val currentIdx = currentPlayer.currentMediaItemIndex.coerceAtLeast(0)
        val extras = mediaItem.mediaMetadata.extras
        val durationMs = mediaItem.mediaMetadata.durationMs
            ?: currentPlayer.duration.takeIf { it > 0L }
            ?: 0L

        scrobbleTracker.onTrackStarted(baseContext.trackId, durationMs)

        repository.updateActiveTrack(
            trackId = baseContext.trackId,
            title = mediaItem.mediaMetadata.title?.toString().orEmpty(),
            artist = mediaItem.mediaMetadata.artist?.toString().orEmpty(),
            album = mediaItem.mediaMetadata.albumTitle?.toString().orEmpty(),
            coverArtId = extras?.getString(AutoBrowseTree.EXTRA_COVER_ART_ID),
            durationMs = durationMs,
            queueTrackIds = queueIds,
            currentIndex = currentIdx,
            isPendingDiscovery = baseContext.isPendingDiscovery,
            isStarred = baseContext.isStarred,
            userRating = baseContext.userRating,
            isOfflineCached = extras?.getBoolean(AutoBrowseTree.EXTRA_IS_OFFLINE_CACHED, false) ?: false,
            sourceLabel = extras?.getString(AutoBrowseTree.EXTRA_SOURCE_LABEL),
        )

        val core = coreOrNull() ?: return
        val sourceLabel = extras?.getString(AutoBrowseTree.EXTRA_SOURCE_LABEL)
            ?: repository.playbackState.value.sourceLabel
        val positionMs = currentPlayer.currentPosition.coerceAtLeast(0L)
        val isPlaying = currentPlayer.isPlaying
        val qualityProfile = repository.playbackState.value.audioQualityProfile

        serviceScope.launch(Dispatchers.IO) {
            // Refresh live discovery/star state from local SQLite so custom bar stays 100% accurate.
            val freshTrack = runCatching { core.getTrack(baseContext.trackId) }.getOrNull()
            if (freshTrack != null) {
                val refreshedContext = TrackActionContext(
                    trackId = freshTrack.id,
                    isPendingDiscovery = freshTrack.isDiscovery,
                    isStarred = freshTrack.starred,
                    userRating = freshTrack.userRating.toInt(),
                )
                withContext(Dispatchers.Main.immediate) {
                    if (currentTrackContext?.trackId == refreshedContext.trackId) {
                        currentTrackContext = refreshedContext
                        repository.updateTrackContext(refreshedContext)
                        mediaLibrarySession?.setCustomLayout(
                            AutoCustomActions.buildCustomLayout(
                                trackContext = refreshedContext,
                                secondaryLibraryAction = repository.playbackState.value.librarySecondaryAction,
                            ),
                        )
                    }
                }
            }

            runCatching {
                PlaybackSessionStore.saveSession(
                    core = core,
                    queueTrackIds = queueIds,
                    currentIndex = currentIdx,
                    positionMs = positionMs,
                    isPlaying = isPlaying,
                    sourceLabel = sourceLabel,
                )
            }

            if (!core.isOfflineOnly() && queueIds.isNotEmpty()) {
                val isWifi = OfflineSyncWorker.isUnmeteredConnection(applicationContext)
                runCatching {
                    core.prefetchQueue(
                        queueTrackIds = queueIds,
                        currentIndex = currentIdx.toUInt(),
                        profile = qualityProfile,
                        isWifi = isWifi,
                    )
                }
            }
        }
    }

    private fun startProgressPolling() {
        progressPollingJob?.cancel()
        progressPollingJob = serviceScope.launch {
            while (isActive) {
                val currentPlayer = player ?: break
                val posMs = currentPlayer.currentPosition.coerceAtLeast(0L)
                val durMs = currentPlayer.duration.takeIf { it > 0L }
                    ?: repository.playbackState.value.durationMs

                repository.updatePlaybackStatus(
                    isPlaying = currentPlayer.isPlaying,
                    isBuffering = currentPlayer.playbackState == Player.STATE_BUFFERING,
                    positionMs = posMs,
                    durationMs = durMs,
                )

                val scrobbleEvent = scrobbleTracker.onProgress(posMs, durMs)
                if (scrobbleEvent != null) {
                    val core = coreOrNull()
                    if (core != null) {
                        launch(Dispatchers.IO) {
                            runCatching {
                                core.enqueueMutation(scrobbleEvent.toOutboxMutation())
                                SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(applicationContext)
                            }
                        }
                    }
                }

                delay(PROGRESS_POLL_INTERVAL_MS)
            }
        }
    }

    private suspend fun hydrateMediaItems(
        core: CoreClient,
        requestedItems: List<MediaItem>,
    ): List<MediaItem> {
        val offlineIds = runCatching {
            core.listOfflineTracks().mapTo(HashSet()) { it.trackId }
        }.getOrDefault(emptySet())

        val resolvedTracks = mutableListOf<Pair<FfiTrack, String?>>()

        for (item in requestedItems) {
            val mediaId = item.mediaId
            when {
                mediaId.startsWith(AutoBrowseTree.PREFIX_PLAYLIST) ||
                    mediaId.startsWith(AutoBrowseTree.PREFIX_ALBUM) ||
                    mediaId == AutoBrowseTree.LIB_OFFLINE ||
                    mediaId == AutoBrowseTree.FOR_YOU_TRIAGE -> {
                    val childNodes = runCatching { core.autoChildren(mediaId) }.getOrDefault(emptyList())
                    val ids = childNodes
                        .filter { it.kind == FfiAutoNodeKind.TRACK }
                        .map { AutoBrowseTree.extractTrackId(it.id) }
                    val label = item.mediaMetadata.title?.toString()
                    appendTracksByIds(core, ids, label, resolvedTracks)
                }

                mediaId.startsWith(AutoBrowseTree.PREFIX_ARTIST) ||
                    mediaId.startsWith(AutoBrowseTree.PREFIX_RADIO_ARTIST) -> {
                    val artistId = mediaId
                        .removePrefix(AutoBrowseTree.PREFIX_RADIO_ARTIST)
                        .removePrefix(AutoBrowseTree.PREFIX_ARTIST)
                    val ids = collectArtistTrackIds(core, artistId)
                    val label = item.mediaMetadata.title?.toString()
                    appendTracksByIds(core, ids, label, resolvedTracks)
                }

                else -> {
                    val trackId = AutoBrowseTree.extractTrackId(mediaId)
                    val track = runCatching { core.getTrack(trackId) }.getOrNull()
                    if (track != null) {
                        val label = item.mediaMetadata.extras?.getString(AutoBrowseTree.EXTRA_SOURCE_LABEL)
                        resolvedTracks.add(track to label)
                    }
                }
            }
        }

        return resolvedTracks.map { (track, label) ->
            AutoBrowseTree.mapTrack(
                track = track,
                artAuthority = artAuthority,
                isOfflineCached = offlineIds.contains(track.id),
                mediaUri = SoulbeetDataSourceFactory.trackUri(track.id),
                sourceLabel = label,
            )
        }
    }

    private suspend fun appendTracksByIds(
        core: CoreClient,
        trackIds: List<String>,
        sourceLabel: String?,
        out: MutableList<Pair<FfiTrack, String?>>,
    ) {
        if (trackIds.isEmpty()) return
        val byId = runCatching { core.getTracksByIds(trackIds) }
            .getOrDefault(emptyList())
            .associateBy { it.id }
        for (id in trackIds) {
            val track = byId[id] ?: continue
            out.add(track to sourceLabel)
        }
    }

    private suspend fun collectArtistTrackIds(core: CoreClient, artistId: String): List<String> {
        val albumNodes = runCatching {
            core.autoChildren("${AutoBrowseTree.PREFIX_ARTIST}$artistId")
        }.getOrDefault(emptyList())

        val trackIds = ArrayList<String>()
        for (albumNode in albumNodes) {
            if (trackIds.size >= MAX_RADIO_QUEUE_SIZE) break
            val childTracks = runCatching { core.autoChildren(albumNode.id) }.getOrDefault(emptyList())
            for (node in childTracks) {
                if (node.kind == FfiAutoNodeKind.TRACK) {
                    trackIds.add(AutoBrowseTree.extractTrackId(node.id))
                    if (trackIds.size >= MAX_RADIO_QUEUE_SIZE) break
                }
            }
        }
        return trackIds
    }

    private suspend fun executeVoiceSearch(core: CoreClient, query: String): List<MediaItem> {
        val match = runCatching { core.voiceSearch(query) }.getOrNull() ?: return emptyList()
        if (match.trackIds.isEmpty()) return emptyList()

        val offlineIds = runCatching {
            core.listOfflineTracks().mapTo(HashSet()) { it.trackId }
        }.getOrDefault(emptySet())

        val tracksById = runCatching { core.getTracksByIds(match.trackIds) }
            .getOrDefault(emptyList())
            .associateBy { it.id }

        return match.trackIds.mapNotNull { id ->
            val track = tracksById[id] ?: return@mapNotNull null
            AutoBrowseTree.mapTrack(
                track = track,
                artAuthority = artAuthority,
                isOfflineCached = offlineIds.contains(track.id),
                mediaUri = SoulbeetDataSourceFactory.trackUri(track.id),
                sourceLabel = match.label,
            )
        }
    }

    private suspend fun startArtistRadioForTrack(core: CoreClient, seedTrackId: String) {
        val seedTrack = runCatching { core.getTrack(seedTrackId) }.getOrNull() ?: return
        val artistId = seedTrack.artistId ?: return
        val trackIds = collectArtistTrackIds(core, artistId)
            .filterNot { it == seedTrackId }
        if (trackIds.isEmpty()) return

        val offlineIds = runCatching {
            core.listOfflineTracks().mapTo(HashSet()) { it.trackId }
        }.getOrDefault(emptySet())
        val byId = runCatching { core.getTracksByIds(trackIds) }
            .getOrDefault(emptyList())
            .associateBy { it.id }
        val radioItems = trackIds.mapNotNull { id ->
            val track = byId[id] ?: return@mapNotNull null
            AutoBrowseTree.mapTrack(
                track = track,
                artAuthority = artAuthority,
                isOfflineCached = offlineIds.contains(track.id),
                mediaUri = SoulbeetDataSourceFactory.trackUri(track.id),
                sourceLabel = "${seedTrack.artistName} Radio",
            )
        }
        if (radioItems.isEmpty()) return

        withContext(Dispatchers.Main.immediate) {
            val currentPlayer = player ?: return@withContext
            val insertIndex = (currentPlayer.currentMediaItemIndex + 1)
                .coerceAtMost(currentPlayer.mediaItemCount)
            currentPlayer.addMediaItems(insertIndex, radioItems)
        }
    }

    private fun skipToNextCachedTrack(currentPlayer: ExoPlayer, core: CoreClient) {
        val currentIdx = currentPlayer.currentMediaItemIndex
        val total = currentPlayer.mediaItemCount
        if (currentIdx + 1 >= total) return

        val queueIds = extractQueueTrackIds(currentPlayer)
        serviceScope.launch(Dispatchers.IO) {
            val offlineIds = runCatching {
                core.listOfflineTracks().mapTo(HashSet()) { it.trackId }
            }.getOrDefault(emptySet())

            val nextCachedIndex = ((currentIdx + 1) until queueIds.size)
                .firstOrNull { offlineIds.contains(queueIds[it]) }
                ?: return@launch

            withContext(Dispatchers.Main.immediate) {
                currentPlayer.seekTo(nextCachedIndex, 0L)
                currentPlayer.prepare()
                currentPlayer.play()
            }
        }
    }

    private fun isNetworkUnavailableError(error: PlaybackException): Boolean =
        error.errorCode == PlaybackException.ERROR_CODE_IO_NETWORK_CONNECTION_FAILED ||
            error.errorCode == PlaybackException.ERROR_CODE_IO_NETWORK_CONNECTION_TIMEOUT ||
            error.errorCode == PlaybackException.ERROR_CODE_IO_UNSPECIFIED

    private fun persistCurrentSessionAsync(currentPlayer: ExoPlayer?) {
        val core = coreOrNull() ?: return
        val snapshot = captureSessionSnapshot(currentPlayer) ?: return
        serviceScope.launch(Dispatchers.IO) {
            runCatching {
                PlaybackSessionStore.saveSession(
                    core = core,
                    queueTrackIds = snapshot.queueTrackIds,
                    currentIndex = snapshot.currentIndex,
                    positionMs = snapshot.positionMs,
                    isPlaying = snapshot.isPlaying,
                    sourceLabel = snapshot.sourceLabel,
                )
            }
        }
    }

    private data class SessionSnapshot(
        val queueTrackIds: List<String>,
        val currentIndex: Int,
        val positionMs: Long,
        val isPlaying: Boolean,
        val sourceLabel: String?,
    )

    private fun captureSessionSnapshot(currentPlayer: ExoPlayer?): SessionSnapshot? {
        if (currentPlayer == null) return null
        val queueIds = extractQueueTrackIds(currentPlayer)
        if (queueIds.isEmpty()) return null
        return SessionSnapshot(
            queueTrackIds = queueIds,
            currentIndex = currentPlayer.currentMediaItemIndex.coerceAtLeast(0),
            positionMs = currentPlayer.currentPosition.coerceAtLeast(0L),
            isPlaying = currentPlayer.isPlaying,
            sourceLabel = repository.playbackState.value.sourceLabel,
        )
    }

    private fun extractQueueTrackIds(currentPlayer: ExoPlayer): List<String> {
        val count = currentPlayer.mediaItemCount
        if (count <= 0) return emptyList()
        val ids = ArrayList<String>(count)
        for (i in 0 until count) {
            val item = currentPlayer.getMediaItemAt(i)
            val id = item.mediaMetadata.extras?.getString(AutoBrowseTree.EXTRA_TRACK_ID)
                ?.takeIf { it.isNotBlank() }
                ?: AutoBrowseTree.extractTrackId(item.mediaId)
            if (id.isNotBlank()) {
                ids.add(id)
            }
        }
        return ids
    }

    companion object {
        const val EXTRAS_KEY_SEARCH_SUPPORTED: String = "android.media.browse.SEARCH_SUPPORTED"
        private const val PROGRESS_POLL_INTERVAL_MS: Long = 1_000L
        private const val MAX_RADIO_QUEUE_SIZE: Int = 50
    }
}
