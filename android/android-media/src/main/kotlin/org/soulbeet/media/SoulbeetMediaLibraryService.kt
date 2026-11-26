package org.soulbeet.media

import android.content.Intent
import android.os.Bundle
import androidx.annotation.OptIn
import androidx.media3.common.AudioAttributes
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
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
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.guava.future
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import org.soulbeet.core.FfiAutoNodeKind
import org.soulbeet.core.FfiTrack
import org.soulbeet.corebindings.CoreClient
import org.soulbeet.corebindings.SoulbeetCoreProvider

/**
 * Hydrated queue resolution result produced for Media3 `onSetMediaItems` / voice playback.
 */
internal data class HydratedQueueResult(
    val items: List<MediaItem>,
    val startIndex: Int,
    val startPositionMs: Long,
)

/**
 * Thread-safe bounded LRU cache for Android Auto `onSearch` -> `onGetSearchResult` queries,
 * keyed by `(normalizedQuery, offlineOnly)`.
 */
internal class BoundedSearchCache(
    private val maxEntries: Int = MAX_SEARCH_CACHE_ENTRIES,
) {
    private val map = object : LinkedHashMap<Pair<String, Boolean>, List<MediaItem>>(maxEntries, 0.75f, true) {
        override fun removeEldestEntry(
            eldest: MutableMap.MutableEntry<Pair<String, Boolean>, List<MediaItem>>?,
        ): Boolean = size > maxEntries
    }

    @Synchronized
    fun get(normalizedQuery: String, offlineOnly: Boolean): List<MediaItem>? =
        map[normalizedQuery to offlineOnly]

    @Synchronized
    fun put(normalizedQuery: String, offlineOnly: Boolean, items: List<MediaItem>) {
        map[normalizedQuery to offlineOnly] = items
    }

    @Synchronized
    fun clear() {
        map.clear()
    }

    @Synchronized
    fun size(): Int = map.size

    companion object {
        const val MAX_SEARCH_CACHE_ENTRIES: Int = 16
    }
}

/**
 * Pure/testable queue hydration, voice playback, and container-item resolution helpers shared
 * by [SoulbeetMediaLibraryService] and JVM unit tests.
 */
@OptIn(UnstableApi::class)
internal object MediaLibraryResolver {

    private const val MAX_RADIO_QUEUE_SIZE: Int = 50

    /**
     * Resolves any browse or track [mediaId] for Media3 `onGetItem`, supporting root, recent root,
     * 4 tabs, known folders, playlists, albums, artists, radio seeds, and contextual/bare tracks.
     */
    suspend fun resolveItemById(
        core: CoreClient,
        mediaId: String,
        artAuthority: String,
    ): MediaItem? {
        if (mediaId == AutoBrowseTree.ROOT_ID) return AutoBrowseTree.rootMediaItem()
        if (mediaId == AutoBrowseTree.RECENT_ROOT_ID) return AutoBrowseTree.recentRootMediaItem()

        core.autoRootTabs().firstOrNull { it.id == mediaId }?.let { tab ->
            return AutoBrowseTree.mapNode(tab, artAuthority)
        }

        if (mediaId == AutoBrowseTree.LIB_OFFLINE ||
            mediaId == AutoBrowseTree.LIB_RECENT ||
            mediaId.startsWith(AutoBrowseTree.PREFIX_AZ)
        ) {
            val libNode = runCatching { core.autoChildren(AutoBrowseTree.TAB_LIBRARY) }
                .getOrDefault(emptyList())
                .firstOrNull { it.id == mediaId }
            if (libNode != null) return AutoBrowseTree.mapNode(libNode, artAuthority)
        }

        if (mediaId == AutoBrowseTree.FOR_YOU_TRIAGE) {
            val triageNode = runCatching { core.autoChildren(AutoBrowseTree.TAB_FOR_YOU) }
                .getOrDefault(emptyList())
                .firstOrNull { it.id == mediaId }
            if (triageNode != null) return AutoBrowseTree.mapNode(triageNode, artAuthority)
        }

        if (mediaId.startsWith(AutoBrowseTree.PREFIX_PLAYLIST)) {
            val playlistId = mediaId.removePrefix(AutoBrowseTree.PREFIX_PLAYLIST)
            val playlist = runCatching { core.listPlaylists() }
                .getOrDefault(emptyList())
                .firstOrNull { it.id == playlistId }
            if (playlist != null) {
                val metadata = MediaMetadata.Builder()
                    .setTitle(playlist.name)
                    .setSubtitle("${playlist.songCount} tracks")
                    .setIsBrowsable(true)
                    .setIsPlayable(true)
                    .setMediaType(MediaMetadata.MEDIA_TYPE_PLAYLIST)
                    .apply {
                        playlist.coverArtId?.takeIf { it.isNotBlank() }?.let { cid ->
                            setArtworkUri(CoverArtContentProvider.uriFor(artAuthority, cid))
                        }
                    }
                    .build()
                return MediaItem.Builder()
                    .setMediaId(mediaId)
                    .setMediaMetadata(metadata)
                    .build()
            }
            return null
        }

        if (mediaId.startsWith(AutoBrowseTree.PREFIX_ALBUM)) {
            val albumId = mediaId.removePrefix(AutoBrowseTree.PREFIX_ALBUM)
            val album = runCatching { core.getAlbum(albumId) }.getOrNull() ?: return null
            val metadata = MediaMetadata.Builder()
                .setTitle(album.name)
                .setArtist(album.artistName)
                .setSubtitle(album.artistName)
                .setIsBrowsable(true)
                .setIsPlayable(true)
                .setMediaType(MediaMetadata.MEDIA_TYPE_ALBUM)
                .apply {
                    album.coverArtId?.takeIf { it.isNotBlank() }?.let { cid ->
                        setArtworkUri(CoverArtContentProvider.uriFor(artAuthority, cid))
                    }
                }
                .build()
            return MediaItem.Builder()
                .setMediaId(mediaId)
                .setMediaMetadata(metadata)
                .build()
        }

        if (mediaId.startsWith(AutoBrowseTree.PREFIX_ARTIST) ||
            mediaId.startsWith(AutoBrowseTree.PREFIX_RADIO_ARTIST)
        ) {
            val isRadio = mediaId.startsWith(AutoBrowseTree.PREFIX_RADIO_ARTIST)
            val artistId = mediaId
                .removePrefix(AutoBrowseTree.PREFIX_RADIO_ARTIST)
                .removePrefix(AutoBrowseTree.PREFIX_ARTIST)
            val artist = runCatching { core.getArtist(artistId) }.getOrNull() ?: return null
            val metadata = MediaMetadata.Builder()
                .setTitle(if (isRadio) "${artist.name} Radio" else artist.name)
                .setSubtitle(if (isRadio) "Instant station" else "${artist.albumCount} albums")
                .setIsBrowsable(!isRadio)
                .setIsPlayable(true)
                .setMediaType(
                    if (isRadio) MediaMetadata.MEDIA_TYPE_RADIO_STATION else MediaMetadata.MEDIA_TYPE_ARTIST,
                )
                .apply {
                    artist.coverArtId?.takeIf { it.isNotBlank() }?.let { cid ->
                        setArtworkUri(CoverArtContentProvider.uriFor(artAuthority, cid))
                    }
                }
                .build()
            return MediaItem.Builder()
                .setMediaId(mediaId)
                .setMediaMetadata(metadata)
                .build()
        }

        val trackId = AutoBrowseTree.extractTrackId(mediaId)
        val track = runCatching { core.getTrack(trackId) }.getOrNull() ?: return null
        val isCached = runCatching {
            core.listOfflineTracks().any { it.trackId == track.id }
        }.getOrDefault(false)

        return AutoBrowseTree.mapTrack(
            track = track,
            artAuthority = artAuthority,
            isOfflineCached = isCached,
            mediaUri = SoulbeetDataSourceFactory.trackUri(track.id),
        )
    }

    /**
     * Resolves a single-item or multi-item `onSetMediaItems` request, supporting:
     * - Google Assistant voice search (`requestMetadata.searchQuery`)
     * - Generic *"Hey Google, play music"* (blank `mediaId` + blank `searchQuery`)
     * - Contextual sibling queue expansion (`ctx:<parentId>|track:<trackId>`) preserving tapped index
     * - Playable containers (`playlist:*`, `album:*`, `artist:*`, `radio:artist:*`, `lib:offline`, `for_you:triage`)
     * - Bare track IDs (`track:<id>` or `<id>`)
     */
    suspend fun resolveSetMediaItems(
        core: CoreClient,
        requestedItems: List<MediaItem>,
        startIndex: Int,
        startPositionMs: Long,
        artAuthority: String,
    ): HydratedQueueResult {
        if (requestedItems.isEmpty()) {
            return HydratedQueueResult(emptyList(), 0, C.TIME_UNSET)
        }

        if (requestedItems.size == 1) {
            val single = requestedItems[0]
            val searchQuery = single.requestMetadata.searchQuery?.trim()

            // 1. Google Assistant voice search (`playFromSearch` -> `requestMetadata.searchQuery`)
            if (!searchQuery.isNullOrBlank()) {
                val items = executeVoiceSearch(core, searchQuery, artAuthority)
                return HydratedQueueResult(
                    items = items,
                    startIndex = 0,
                    startPositionMs = C.TIME_UNSET,
                )
            }

            // 2. Generic "Hey Google, play music" (blank mediaId + blank searchQuery)
            if (single.mediaId.isBlank()) {
                return resolveGenericPlayMusic(core, artAuthority)
            }

            // 3. Contextual track tap inside a container (`ctx:<parentId>|track:<trackId>`)
            val contextual = AutoBrowseTree.parseContextualTrackId(single.mediaId)
            if (contextual != null) {
                val containerItems = expandContainerToTracks(
                    core = core,
                    containerId = contextual.parentId,
                    fallbackLabel = single.mediaMetadata.extras?.getString(AutoBrowseTree.EXTRA_SOURCE_LABEL),
                    artAuthority = artAuthority,
                )
                val tappedIdx = containerItems.indexOfFirst { it.mediaId == contextual.trackId }
                if (tappedIdx >= 0) {
                    val safePos = if (startPositionMs == C.TIME_UNSET) C.TIME_UNSET else startPositionMs.coerceAtLeast(0L)
                    return HydratedQueueResult(
                        items = containerItems,
                        startIndex = tappedIdx,
                        startPositionMs = safePos,
                    )
                }
            }
        }

        val hydrated = hydrateMediaItems(core, requestedItems, artAuthority)
        val safeIndex = when {
            hydrated.isEmpty() -> 0
            requestedItems.size == 1 && hydrated.size > 1 -> 0
            startIndex == C.INDEX_UNSET -> 0
            else -> startIndex.coerceIn(0, hydrated.lastIndex)
        }
        val safePos = if (startPositionMs == C.TIME_UNSET) {
            C.TIME_UNSET
        } else {
            startPositionMs.coerceAtLeast(0L)
        }
        return HydratedQueueResult(hydrated, safeIndex, safePos)
    }

    suspend fun hydrateMediaItems(
        core: CoreClient,
        requestedItems: List<MediaItem>,
        artAuthority: String,
    ): List<MediaItem> {
        val offlineIds = runCatching {
            core.listOfflineTracks().mapTo(HashSet()) { it.trackId }
        }.getOrDefault(emptySet())

        val resolvedTracks = mutableListOf<Pair<FfiTrack, String?>>()

        for (item in requestedItems) {
            val searchQuery = item.requestMetadata.searchQuery?.trim()
            if (!searchQuery.isNullOrBlank()) {
                val voiceMatch = runCatching { core.voiceSearch(searchQuery) }.getOrNull()
                if (voiceMatch != null && voiceMatch.trackIds.isNotEmpty()) {
                    appendTracksByIds(core, voiceMatch.trackIds, voiceMatch.label, resolvedTracks)
                }
                continue
            }

            val mediaId = item.mediaId
            when {
                mediaId.isBlank() -> {
                    val fallback = resolveGenericPlayMusic(core, artAuthority)
                    val ids = fallback.items.map { AutoBrowseTree.extractTrackId(it.mediaId) }
                    val label = fallback.items.firstOrNull()?.mediaMetadata?.extras
                        ?.getString(AutoBrowseTree.EXTRA_SOURCE_LABEL)
                    appendTracksByIds(core, ids, label, resolvedTracks)
                }

                isTrackContainerId(mediaId) -> {
                    val childNodes = runCatching { core.autoChildren(mediaId) }.getOrDefault(emptyList())
                    val ids = childNodes
                        .filter { it.kind == FfiAutoNodeKind.TRACK }
                        .map { AutoBrowseTree.extractTrackId(it.id) }
                    val label = resolveContainerSourceLabel(core, mediaId, item.mediaMetadata.title?.toString())
                    appendTracksByIds(core, ids, label, resolvedTracks)
                }

                mediaId.startsWith(AutoBrowseTree.PREFIX_ARTIST) ||
                    mediaId.startsWith(AutoBrowseTree.PREFIX_RADIO_ARTIST) -> {
                    val artistId = mediaId
                        .removePrefix(AutoBrowseTree.PREFIX_RADIO_ARTIST)
                        .removePrefix(AutoBrowseTree.PREFIX_ARTIST)
                    val ids = collectArtistTrackIds(core, artistId)
                    val label = resolveContainerSourceLabel(core, mediaId, item.mediaMetadata.title?.toString())
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

    suspend fun executeVoiceSearch(
        core: CoreClient,
        query: String,
        artAuthority: String,
    ): List<MediaItem> {
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

    suspend fun collectArtistTrackIds(core: CoreClient, artistId: String): List<String> {
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

    suspend fun resolveContainerSourceLabel(
        core: CoreClient,
        containerId: String,
        providedTitle: String?,
    ): String? {
        providedTitle?.trim()?.takeIf { it.isNotEmpty() }?.let { return it }
        return when {
            containerId == AutoBrowseTree.LIB_OFFLINE -> "Offline Downloaded"
            containerId == AutoBrowseTree.FOR_YOU_TRIAGE -> "Discovery Triage"
            containerId.startsWith(AutoBrowseTree.PREFIX_PLAYLIST) -> {
                val plId = containerId.removePrefix(AutoBrowseTree.PREFIX_PLAYLIST)
                runCatching { core.listPlaylists() }.getOrDefault(emptyList())
                    .firstOrNull { it.id == plId }
                    ?.name
            }
            containerId.startsWith(AutoBrowseTree.PREFIX_ALBUM) -> {
                val alId = containerId.removePrefix(AutoBrowseTree.PREFIX_ALBUM)
                runCatching { core.getAlbum(alId) }.getOrNull()?.name
            }
            containerId.startsWith(AutoBrowseTree.PREFIX_RADIO_ARTIST) -> {
                val arId = containerId.removePrefix(AutoBrowseTree.PREFIX_RADIO_ARTIST)
                runCatching { core.getArtist(arId) }.getOrNull()?.let { "${it.name} Radio" }
            }
            containerId.startsWith(AutoBrowseTree.PREFIX_ARTIST) -> {
                val arId = containerId.removePrefix(AutoBrowseTree.PREFIX_ARTIST)
                runCatching { core.getArtist(arId) }.getOrNull()?.name
            }
            else -> null
        }
    }

    private suspend fun expandContainerToTracks(
        core: CoreClient,
        containerId: String,
        fallbackLabel: String?,
        artAuthority: String,
    ): List<MediaItem> {
        val childNodes = runCatching { core.autoChildren(containerId) }.getOrDefault(emptyList())
        val trackIds = childNodes
            .filter { it.kind == FfiAutoNodeKind.TRACK }
            .map { AutoBrowseTree.extractTrackId(it.id) }
        if (trackIds.isEmpty()) return emptyList()

        val sourceLabel = resolveContainerSourceLabel(core, containerId, fallbackLabel)
        val offlineIds = runCatching {
            core.listOfflineTracks().mapTo(HashSet()) { it.trackId }
        }.getOrDefault(emptySet())
        val tracksById = runCatching { core.getTracksByIds(trackIds) }
            .getOrDefault(emptyList())
            .associateBy { it.id }

        return trackIds.mapNotNull { id ->
            val track = tracksById[id] ?: return@mapNotNull null
            AutoBrowseTree.mapTrack(
                track = track,
                artAuthority = artAuthority,
                isOfflineCached = offlineIds.contains(track.id),
                mediaUri = SoulbeetDataSourceFactory.trackUri(track.id),
                sourceLabel = sourceLabel,
            )
        }
    }

    private suspend fun resolveGenericPlayMusic(
        core: CoreClient,
        artAuthority: String,
    ): HydratedQueueResult {
        val restored = PlaybackSessionStore.restoreSession(core, artAuthority)
        if (restored != null && restored.items.isNotEmpty()) {
            return HydratedQueueResult(
                items = restored.items,
                startIndex = restored.startIndex,
                startPositionMs = restored.startPositionMs,
            )
        }

        // Fall back to first playable For You mix/playlist
        val forYouNodes = runCatching { core.autoChildren(AutoBrowseTree.TAB_FOR_YOU) }
            .getOrDefault(emptyList())
        for (node in forYouNodes) {
            val tracks = expandContainerToTracks(core, node.id, node.title, artAuthority)
            if (tracks.isNotEmpty()) {
                return HydratedQueueResult(tracks, 0, C.TIME_UNSET)
            }
        }

        // Fall back to offline downloaded tracks
        val offlineTracks = expandContainerToTracks(
            core = core,
            containerId = AutoBrowseTree.LIB_OFFLINE,
            fallbackLabel = "Offline Downloaded",
            artAuthority = artAuthority,
        )
        return HydratedQueueResult(offlineTracks, 0, C.TIME_UNSET)
    }

    private fun isTrackContainerId(mediaId: String): Boolean =
        mediaId.startsWith(AutoBrowseTree.PREFIX_PLAYLIST) ||
            mediaId.startsWith(AutoBrowseTree.PREFIX_ALBUM) ||
            mediaId == AutoBrowseTree.LIB_OFFLINE ||
            mediaId == AutoBrowseTree.FOR_YOU_TRIAGE

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
}

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
    private val searchResultCache = BoundedSearchCache()

    private var player: ExoPlayer? = null
    private var mediaLibrarySession: MediaLibrarySession? = null
    private var progressPollingJob: Job? = null
    private var trackRefreshJob: Job? = null
    private var currentTrackContext: TrackActionContext? = null
    private var trackContextGeneration: Long = 0L

    private val artAuthority: String
        get() = CoverArtContentProvider.authorityFor(applicationContext)

    private fun coreOrNull(): CoreClient? = SoulbeetCoreProvider.getOrNull()

    private suspend fun awaitCore(): CoreClient? = SoulbeetCoreProvider.awaitOrNull()

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

        // Refresh Android Auto custom layout live when Settings change the secondary library action.
        serviceScope.launch {
            repository.playbackState
                .map { it.librarySecondaryAction }
                .distinctUntilChanged()
                .drop(1)
                .collect { secondaryAction ->
                    mediaLibrarySession?.setCustomLayout(
                        AutoCustomActions.buildCustomLayout(currentTrackContext, secondaryAction),
                    )
                }
        }
    }

    override fun onGetSession(controllerInfo: MediaSession.ControllerInfo): MediaLibrarySession? =
        mediaLibrarySession

    override fun onTaskRemoved(rootIntent: Intent?) {
        val currentPlayer = player
        persistSessionDetached(coreOrNull(), captureSessionSnapshot(currentPlayer))
        if (currentPlayer == null || !currentPlayer.playWhenReady || currentPlayer.mediaItemCount == 0) {
            stopSelf()
        }
    }

    override fun onDestroy() {
        progressPollingJob?.cancel()
        progressPollingJob = null
        trackRefreshJob?.cancel()
        trackRefreshJob = null

        val currentPlayer = player
        val snapshot = captureSessionSnapshot(currentPlayer)?.copy(isPlaying = false)
        persistSessionDetached(coreOrNull(), snapshot)

        // Release MediaLibrarySession BEFORE releasing ExoPlayer per Media3 lifecycle contract.
        mediaLibrarySession?.release()
        mediaLibrarySession = null

        if (currentPlayer != null) {
            currentPlayer.removeListener(playerListener)
            currentPlayer.release()
        }
        player = null
        searchResultCache.clear()
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
            val isRecent = params?.isRecent == true
            val responseParams = LibraryParams.Builder()
                .setExtras(rootExtras)
                .setOffline(params?.isOffline ?: false)
                .setRecent(isRecent)
                .setSuggested(params?.isSuggested ?: false)
                .build()
            val rootItem = if (isRecent) {
                AutoBrowseTree.recentRootMediaItem()
            } else {
                AutoBrowseTree.rootMediaItem()
            }
            LibraryResult.ofItem(rootItem, responseParams)
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
                val core = awaitCore()
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_INVALID_STATE)
                val offlineOnly = (params?.isOffline == true) || core.isOfflineOnly()
                val items = try {
                    AutoBrowseTree.loadChildren(
                        core = core,
                        parentId = parentId,
                        page = page,
                        pageSize = pageSize,
                        artAuthority = artAuthority,
                        offlineOnly = offlineOnly,
                    )
                } catch (e: CancellationException) {
                    throw e
                } catch (_: Exception) {
                    return@future LibraryResult.ofError(SessionError.ERROR_IO)
                }

                if (!offlineOnly) {
                    val coverArtIds = items.mapNotNull {
                        it.mediaMetadata.extras?.getString(AutoBrowseTree.EXTRA_COVER_ART_ID)
                    }
                    prefetchCoverArtAsync(core, coverArtIds)
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
                if (mediaId == AutoBrowseTree.RECENT_ROOT_ID) {
                    return@future LibraryResult.ofItem(AutoBrowseTree.recentRootMediaItem(), null)
                }
                val core = awaitCore()
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_INVALID_STATE)
                val resolved = MediaLibraryResolver.resolveItemById(core, mediaId, artAuthority)
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_BAD_VALUE)
                LibraryResult.ofItem(resolved, null)
            }

        override fun onAddMediaItems(
            mediaSession: MediaSession,
            controller: MediaSession.ControllerInfo,
            mediaItems: MutableList<MediaItem>,
        ): ListenableFuture<MutableList<MediaItem>> =
            serviceScope.future(Dispatchers.IO) {
                val core = awaitCore() ?: return@future mutableListOf()
                MediaLibraryResolver.hydrateMediaItems(core, mediaItems, artAuthority).toMutableList()
            }

        override fun onSetMediaItems(
            mediaSession: MediaSession,
            controller: MediaSession.ControllerInfo,
            mediaItems: MutableList<MediaItem>,
            startIndex: Int,
            startPositionMs: Long,
        ): ListenableFuture<MediaSession.MediaItemsWithStartPosition> =
            serviceScope.future(Dispatchers.IO) {
                val core = awaitCore()
                    ?: return@future MediaSession.MediaItemsWithStartPosition(
                        emptyList(),
                        C.INDEX_UNSET,
                        C.TIME_UNSET,
                    )
                val resolved = MediaLibraryResolver.resolveSetMediaItems(
                    core = core,
                    requestedItems = mediaItems,
                    startIndex = startIndex,
                    startPositionMs = startPositionMs,
                    artAuthority = artAuthority,
                )
                MediaSession.MediaItemsWithStartPosition(
                    resolved.items,
                    resolved.startIndex,
                    resolved.startPositionMs,
                )
            }

        override fun onSearch(
            session: MediaLibrarySession,
            browser: MediaSession.ControllerInfo,
            query: String,
            params: LibraryParams?,
        ): ListenableFuture<LibraryResult<Void>> =
            serviceScope.future(Dispatchers.IO) {
                val core = awaitCore()
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_INVALID_STATE)
                val normalizedKey = query.trim().lowercase()
                val offlineOnly = (params?.isOffline == true) || core.isOfflineOnly()
                val items = MediaLibraryResolver.executeVoiceSearch(core, query, artAuthority)
                searchResultCache.put(normalizedKey, offlineOnly, items)
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
                val core = awaitCore()
                    ?: return@future LibraryResult.ofError(SessionError.ERROR_INVALID_STATE)
                val normalizedKey = query.trim().lowercase()
                val offlineOnly = (params?.isOffline == true) || core.isOfflineOnly()
                val items = searchResultCache.get(normalizedKey, offlineOnly)
                    ?: MediaLibraryResolver.executeVoiceSearch(core, query, artAuthority).also {
                        searchResultCache.put(normalizedKey, offlineOnly, it)
                    }
                val paged = AutoBrowseTree.paginate(items, page, pageSize)
                LibraryResult.ofItemList(ImmutableList.copyOf(paged), params)
            }

        override fun onPlaybackResumption(
            mediaSession: MediaSession,
            controller: MediaSession.ControllerInfo,
        ): ListenableFuture<MediaSession.MediaItemsWithStartPosition> =
            serviceScope.future(Dispatchers.IO) {
                val core = awaitCore()
                    ?: throw UnsupportedOperationException("SoulbeetCore is not initialized for resumption")
                val restored = PlaybackSessionStore.restoreSession(
                    core = core,
                    artAuthority = artAuthority,
                ) ?: throw UnsupportedOperationException("No persisted playback session to resume")
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
        ): ListenableFuture<SessionResult> = serviceScope.future(Dispatchers.Main.immediate) {
            val core = withContext(Dispatchers.IO) { awaitCore() }
                ?: return@future SessionResult(SessionError.ERROR_INVALID_STATE)

            // All player, MediaSession, and currentTrackContext access stays strictly on Main.
            val handler = AutoCustomActions.ActionHandler(
                core = core,
                ioDispatcher = Dispatchers.IO,
                secondaryLibraryActionProvider = { repository.playbackState.value.librarySecondaryAction },
                currentTrackContextProvider = { currentTrackContext },
                onUpdateTrackContext = { updated ->
                    trackContextGeneration++
                    currentTrackContext = updated
                    if (updated != null) {
                        repository.updateTrackContext(updated)
                    } else {
                        repository.clearActiveTrack()
                    }
                },
                onRefreshCustomLayout = { layout ->
                    mediaLibrarySession?.setCustomLayout(layout)
                },
                onSkipToNext = {
                    val p = player
                    if (p != null && p.hasNextMediaItem()) {
                        p.seekToNextMediaItem()
                        true
                    } else {
                        p?.stop()
                        false
                    }
                },
                onStartTrackRadio = { seedTrackId ->
                    startArtistRadioForTrack(core, seedTrackId)
                },
                onMutationEnqueued = {
                    runCatching { SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(applicationContext) }
                },
            )

            handler.handle(customCommand.customAction)
        }
    }

    private val playerListener = object : Player.Listener {
        override fun onMediaItemTransition(mediaItem: MediaItem?, reason: Int) {
            val currentPlayer = player ?: return
            handleTrackTransition(currentPlayer, mediaItem)
        }

        override fun onTimelineChanged(timeline: androidx.media3.common.Timeline, reason: Int) {
            val currentPlayer = player ?: return
            val queueIds = extractQueueTrackIds(currentPlayer)
            val currentIdx = currentPlayer.currentMediaItemIndex.coerceAtLeast(0)
            repository.updateQueue(queueIds, currentIdx)
        }

        override fun onIsPlayingChanged(isPlaying: Boolean) {
            val currentPlayer = player ?: return
            val durMs = currentPlayer.duration.takeIf { it > 0L }
                ?: repository.playbackState.value.durationMs
            repository.updatePlaybackStatus(
                isPlaying = isPlaying,
                isBuffering = currentPlayer.playbackState == Player.STATE_BUFFERING,
                positionMs = currentPlayer.currentPosition,
                durationMs = durMs,
            )

            scrobbleTracker.onPlayingStateChanged(isPlaying, durMs)?.let { event ->
                dispatchScrobbleEvent(event)
            }

            if (isPlaying) {
                startProgressPolling()
            } else {
                progressPollingJob?.cancel()
                progressPollingJob = null
                persistSessionDetached(coreOrNull(), captureSessionSnapshot(currentPlayer))
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
            if (isOfflineOrNetworkFailure(error) || core.isOfflineOnly()) {
                skipToNextCachedTrack(currentPlayer, core)
            }
        }
    }

    private fun handleTrackTransition(currentPlayer: ExoPlayer, mediaItem: MediaItem?) {
        trackRefreshJob?.cancel()
        trackRefreshJob = null
        val expectedGeneration = ++trackContextGeneration

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
            repository.clearActiveTrack()
            return
        }

        val queueIds = extractQueueTrackIds(currentPlayer)
        val currentIdx = currentPlayer.currentMediaItemIndex.coerceAtLeast(0)
        val extras = mediaItem.mediaMetadata.extras
        val durationMs = mediaItem.mediaMetadata.durationMs
            ?: currentPlayer.duration.takeIf { it > 0L }
            ?: 0L

        scrobbleTracker.onTrackStarted(
            trackId = baseContext.trackId,
            durationMs = durationMs,
            isPlaying = currentPlayer.isPlaying,
        )

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

        // Collect cover art IDs for current + next 2 tracks for headless Android Auto art prefetch.
        val upcomingCoverArtIds = buildList {
            for (idx in currentIdx..minOf(currentIdx + 2, currentPlayer.mediaItemCount - 1)) {
                currentPlayer.getMediaItemAt(idx).mediaMetadata.extras
                    ?.getString(AutoBrowseTree.EXTRA_COVER_ART_ID)
                    ?.takeIf { it.isNotBlank() }
                    ?.let { add(it) }
            }
        }

        trackRefreshJob = serviceScope.launch(Dispatchers.IO) {
            val freshTrack = runCatching { core.getTrack(baseContext.trackId) }.getOrNull()
            if (freshTrack != null) {
                val refreshedContext = TrackActionContext(
                    trackId = freshTrack.id,
                    isPendingDiscovery = freshTrack.isDiscovery,
                    isStarred = freshTrack.starred,
                    userRating = freshTrack.userRating.toInt(),
                )
                withContext(Dispatchers.Main.immediate) {
                    if (trackContextGeneration == expectedGeneration &&
                        currentTrackContext?.trackId == refreshedContext.trackId
                    ) {
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

            if (!core.isOfflineOnly()) {
                if (upcomingCoverArtIds.isNotEmpty()) {
                    prefetchCoverArtAsync(core, upcomingCoverArtIds)
                }
                if (queueIds.isNotEmpty()) {
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

                scrobbleTracker.onTick(
                    isPlaying = currentPlayer.isPlaying,
                    durationMs = durMs,
                )?.let { event ->
                    dispatchScrobbleEvent(event)
                }

                delay(PROGRESS_POLL_INTERVAL_MS)
            }
        }
    }

    private fun dispatchScrobbleEvent(event: ScrobbleEvent) {
        val core = coreOrNull() ?: return
        serviceScope.launch(Dispatchers.IO) {
            runCatching {
                core.enqueueMutation(event.toOutboxMutation())
                SoulbeetWorkScheduler.enqueueImmediateOutboxDrain(applicationContext)
            }
        }
    }

    private fun prefetchCoverArtAsync(core: CoreClient, coverArtIds: Collection<String>) {
        if (core.isOfflineOnly()) return
        val boundedIds = coverArtIds
            .asSequence()
            .map { it.trim() }
            .filter { it.isNotEmpty() }
            .distinct()
            .take(MAX_COVER_ART_PREFETCH_BATCH)
            .toList()
        if (boundedIds.isEmpty()) return

        serviceScope.launch(Dispatchers.IO) {
            for (artId in boundedIds) {
                if (!isActive || core.isOfflineOnly()) break
                runCatching { core.getOrFetchCoverArt(artId, COVER_ART_PREFETCH_SIZE_PX) }
            }
        }
    }

    private suspend fun startArtistRadioForTrack(core: CoreClient, seedTrackId: String) {
        val radioItems = withContext(Dispatchers.IO) {
            val seedTrack = runCatching { core.getTrack(seedTrackId) }.getOrNull() ?: return@withContext emptyList()
            val artistId = seedTrack.artistId ?: return@withContext emptyList()
            val trackIds = MediaLibraryResolver.collectArtistTrackIds(core, artistId)
                .filterNot { it == seedTrackId }
            if (trackIds.isEmpty()) return@withContext emptyList()

            val offlineIds = runCatching {
                core.listOfflineTracks().mapTo(HashSet()) { it.trackId }
            }.getOrDefault(emptySet())
            val byId = runCatching { core.getTracksByIds(trackIds) }
                .getOrDefault(emptyList())
                .associateBy { it.id }

            trackIds.mapNotNull { id ->
                val track = byId[id] ?: return@mapNotNull null
                AutoBrowseTree.mapTrack(
                    track = track,
                    artAuthority = artAuthority,
                    isOfflineCached = offlineIds.contains(track.id),
                    mediaUri = SoulbeetDataSourceFactory.trackUri(track.id),
                    sourceLabel = "${seedTrack.artistName} Radio",
                )
            }
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
            val expectedTrackId = queueIds[nextCachedIndex]

            withContext(Dispatchers.Main.immediate) {
                val activePlayer = player ?: return@withContext
                if (activePlayer !== currentPlayer) return@withContext
                if (nextCachedIndex >= activePlayer.mediaItemCount) return@withContext
                if (extractQueueTrackIds(activePlayer).getOrNull(nextCachedIndex) != expectedTrackId) {
                    return@withContext
                }
                activePlayer.seekTo(nextCachedIndex, 0L)
                activePlayer.prepare()
                activePlayer.play()
            }
        }
    }

    private fun isOfflineOrNetworkFailure(error: PlaybackException): Boolean =
        error.cause is OfflineTrackUnavailableException ||
            error.errorCode == PlaybackException.ERROR_CODE_IO_NETWORK_CONNECTION_FAILED ||
            error.errorCode == PlaybackException.ERROR_CODE_IO_NETWORK_CONNECTION_TIMEOUT

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
        private const val PROGRESS_POLL_INTERVAL_MS: Long = 1_000L
        private const val DETACHED_SAVE_TIMEOUT_MS: Long = 2_000L
        private const val MAX_COVER_ART_PREFETCH_BATCH: Int = 12
        private const val COVER_ART_PREFETCH_SIZE_PX: UInt = 512u

        /**
         * Persists an immutable [SessionSnapshot] inside a detached short-lived IO scope so
         * `onDestroy()` / `onTaskRemoved()` cancellation never aborts the final SQLite write and
         * neither the Service nor ExoPlayer is retained by the coroutine.
         */
        private fun persistSessionDetached(core: CoreClient?, snapshot: SessionSnapshot?) {
            if (core == null || snapshot == null) return
            val detachedScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
            detachedScope.launch {
                try {
                    withTimeout(DETACHED_SAVE_TIMEOUT_MS) {
                        PlaybackSessionStore.saveSession(
                            core = core,
                            queueTrackIds = snapshot.queueTrackIds,
                            currentIndex = snapshot.currentIndex,
                            positionMs = snapshot.positionMs,
                            isPlaying = snapshot.isPlaying,
                            sourceLabel = snapshot.sourceLabel,
                        )
                    }
                } catch (_: Exception) {
                    // Best-effort final persistence during teardown.
                } finally {
                    detachedScope.cancel()
                }
            }
        }
    }
}
