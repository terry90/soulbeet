package org.soulbeet.app

import android.content.ComponentName
import android.content.Context
import android.os.Bundle
import androidx.annotation.OptIn
import androidx.core.content.ContextCompat
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.session.MediaController
import androidx.media3.session.SessionCommand
import androidx.media3.session.SessionResult
import androidx.media3.session.SessionToken
import com.google.common.util.concurrent.ListenableFuture
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.guava.await
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.soulbeet.core.FfiTrack
import org.soulbeet.media.AutoBrowseTree
import org.soulbeet.media.CoverArtContentProvider
import org.soulbeet.media.SoulbeetDataSourceFactory
import org.soulbeet.media.SoulbeetMediaLibraryService
import org.soulbeet.media.SoulbeetMediaRepository

/**
 * Thin bridge connecting the Jetpack Compose UI to [SoulbeetMediaLibraryService] via Media3
 * [MediaController], ensuring phone playback and Android Auto share one single `ExoPlayer` and
 * queue state.
 *
 * Threading & lifecycle safety:
 * - Future listener executes strictly on the main looper via `ContextCompat.getMainExecutor`.
 * - Tracks [released] so a `release()` racing ahead of `MediaController` connection immediately
 *   releases the late-arriving controller without mutating state or leaking listeners.
 * - Live position ticker runs ONLY while `isPlaying == true` (zero idle battery drain).
 */
@OptIn(UnstableApi::class)
class MediaControllerBridge(
    private val context: Context,
) {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private val artAuthority: String = CoverArtContentProvider.authorityFor(context)

    @Volatile
    private var released: Boolean = false
    private var controllerFuture: ListenableFuture<MediaController>? = null
    private var controller: MediaController? = null
    private var positionTickerJob: Job? = null

    private val _isConnected = MutableStateFlow(false)
    val isConnected: StateFlow<Boolean> = _isConnected.asStateFlow()

    private val _playbackPositionMs = MutableStateFlow(0L)
    val playbackPositionMs: StateFlow<Long> = _playbackPositionMs.asStateFlow()

    private val _shuffleEnabled = MutableStateFlow(false)
    val shuffleEnabled: StateFlow<Boolean> = _shuffleEnabled.asStateFlow()

    private val _repeatMode = MutableStateFlow(Player.REPEAT_MODE_OFF)
    val repeatMode: StateFlow<Int> = _repeatMode.asStateFlow()

    private val playerListener = object : Player.Listener {
        override fun onIsPlayingChanged(isPlaying: Boolean) {
            val active = controller ?: return
            _playbackPositionMs.value = active.currentPosition.coerceAtLeast(0L)
            if (isPlaying) {
                startPositionTicker()
            } else {
                stopPositionTicker()
            }
        }

        override fun onMediaItemTransition(mediaItem: MediaItem?, reason: Int) {
            val active = controller ?: return
            _playbackPositionMs.value = active.currentPosition.coerceAtLeast(0L)
        }

        override fun onPositionDiscontinuity(
            oldPosition: Player.PositionInfo,
            newPosition: Player.PositionInfo,
            reason: Int,
        ) {
            _playbackPositionMs.value = newPosition.positionMs.coerceAtLeast(0L)
        }

        override fun onShuffleModeEnabledChanged(shuffleModeEnabled: Boolean) {
            _shuffleEnabled.value = shuffleModeEnabled
        }

        override fun onRepeatModeChanged(repeatMode: Int) {
            _repeatMode.value = repeatMode
        }
    }

    fun connect() {
        if (released || controllerFuture != null) return
        val token = SessionToken(
            context,
            ComponentName(context, SoulbeetMediaLibraryService::class.java),
        )
        val future = MediaController.Builder(context, token).buildAsync()
        controllerFuture = future
        future.addListener(
            {
                val bound = runCatching { future.get() }.getOrNull() ?: return@addListener
                if (released) {
                    bound.release()
                    return@addListener
                }
                controller = bound
                bound.addListener(playerListener)
                _isConnected.value = true
                _shuffleEnabled.value = bound.shuffleModeEnabled
                _repeatMode.value = bound.repeatMode
                _playbackPositionMs.value = bound.currentPosition.coerceAtLeast(0L)
                if (bound.isPlaying) {
                    startPositionTicker()
                }
            },
            ContextCompat.getMainExecutor(context),
        )
    }

    fun playTrack(
        track: FfiTrack,
        isOfflineCached: Boolean = false,
        sourceLabel: String? = null,
    ) {
        playQueue(
            tracks = listOf(track),
            startIndex = 0,
            offlineIds = if (isOfflineCached) setOf(track.id) else emptySet(),
            sourceLabel = sourceLabel,
        )
    }

    fun playQueue(
        tracks: List<FfiTrack>,
        startIndex: Int = 0,
        offlineIds: Set<String> = emptySet(),
        sourceLabel: String? = null,
    ) {
        playQueueAtPosition(
            tracks = tracks,
            startIndex = startIndex,
            startPositionMs = 0L,
            autoPlay = true,
            offlineIds = offlineIds,
            sourceLabel = sourceLabel,
        )
    }

    fun playQueueAtPosition(
        tracks: List<FfiTrack>,
        startIndex: Int = 0,
        startPositionMs: Long = 0L,
        autoPlay: Boolean = true,
        offlineIds: Set<String> = emptySet(),
        sourceLabel: String? = null,
    ) {
        if (tracks.isEmpty()) return
        val active = controller ?: return
        val safeIdx = startIndex.coerceIn(0, tracks.lastIndex)
        val safePos = startPositionMs.coerceAtLeast(0L)
        val mediaItems = tracks.map { track ->
            AutoBrowseTree.mapTrack(
                track = track,
                artAuthority = artAuthority,
                isOfflineCached = offlineIds.contains(track.id),
                mediaUri = SoulbeetDataSourceFactory.trackUri(track.id),
                sourceLabel = sourceLabel,
            )
        }
        active.setMediaItems(mediaItems, safeIdx, safePos)
        active.prepare()
        if (autoPlay) {
            active.play()
        } else {
            active.pause()
        }
        _playbackPositionMs.value = safePos
    }

    fun playContainer(mediaId: String, title: String? = null) {
        val active = controller ?: return
        val item = MediaItem.Builder()
            .setMediaId(mediaId)
            .setMediaMetadata(
                MediaMetadata.Builder()
                    .setTitle(title)
                    .setIsPlayable(true)
                    .build(),
            )
            .build()
        active.setMediaItem(item)
        active.prepare()
        active.play()
        _playbackPositionMs.value = 0L
    }

    fun togglePlayPause() {
        val active = controller ?: return
        if (active.isPlaying) {
            active.pause()
        } else {
            if (active.playbackState == Player.STATE_IDLE && active.mediaItemCount > 0) {
                active.prepare()
            }
            active.play()
        }
    }

    fun skipNext() {
        val active = controller ?: return
        if (active.hasNextMediaItem()) {
            active.seekToNextMediaItem()
        } else {
            active.pause()
        }
    }

    /**
     * Removes every occurrence of [trackId] from the active Media3 queue.
     * When [trackId] is currently playing, advances to the next queued item first (respecting
     * shuffle and repeat) before removing the dropped track, or pauses cleanly if it was the
     * final item in the queue.
     */
    fun removeTrackFromQueue(trackId: String) {
        if (trackId.isEmpty()) return
        val active = controller ?: return
        val wasCurrent = active.currentMediaItem?.let {
            AutoBrowseTree.resolveTrackId(it) == trackId
        } ?: false
        AutoBrowseTree.removeTrackFromQueue(active, trackId)
        if (wasCurrent) {
            _playbackPositionMs.value = 0L
        }
    }

    fun skipPrevious() {
        val active = controller ?: return
        if (active.currentPosition > 3_000L) {
            active.seekTo(0L)
            _playbackPositionMs.value = 0L
        } else if (active.hasPreviousMediaItem()) {
            active.seekToPreviousMediaItem()
        } else {
            active.seekTo(0L)
            _playbackPositionMs.value = 0L
        }
    }

    fun seekTo(positionMs: Long) {
        val active = controller ?: return
        val clamped = positionMs.coerceAtLeast(0L)
        active.seekTo(clamped)
        _playbackPositionMs.value = clamped
        val state = SoulbeetMediaRepository.shared.playbackState.value
        SoulbeetMediaRepository.shared.updatePlaybackStatus(
            isPlaying = state.isPlaying,
            isBuffering = state.isBuffering,
            positionMs = clamped,
            durationMs = state.durationMs,
        )
    }

    fun playQueueIndex(index: Int) {
        val active = controller ?: return
        if (index in 0 until active.mediaItemCount) {
            active.seekTo(index, 0L)
            active.play()
            _playbackPositionMs.value = 0L
        }
    }

    fun toggleShuffle() {
        val active = controller ?: return
        val next = !active.shuffleModeEnabled
        active.shuffleModeEnabled = next
        _shuffleEnabled.value = next
    }

    fun cycleRepeatMode() {
        val active = controller ?: return
        val next = when (active.repeatMode) {
            Player.REPEAT_MODE_OFF -> Player.REPEAT_MODE_ALL
            Player.REPEAT_MODE_ALL -> Player.REPEAT_MODE_ONE
            else -> Player.REPEAT_MODE_OFF
        }
        active.repeatMode = next
        _repeatMode.value = next
    }

    fun stopAndClear() {
        stopPositionTicker()
        controller?.let { active ->
            active.stop()
            active.clearMediaItems()
        }
        _playbackPositionMs.value = 0L
    }

    fun sendCustomAction(action: String): ListenableFuture<SessionResult>? {
        val active = controller ?: return null
        return active.sendCustomCommand(SessionCommand(action, Bundle.EMPTY), Bundle.EMPTY)
    }

    /**
     * Dispatches a MediaSession custom action and suspends until [SessionResult] completes.
     * Returns `true` if the session handled the command with `RESULT_SUCCESS`, or `false` if no
     * controller is connected or the command returned an error.
     */
    suspend fun sendCustomActionAndAwait(action: String): Boolean {
        val future = sendCustomAction(action) ?: return false
        return runCatching {
            future.await().resultCode == SessionResult.RESULT_SUCCESS
        }.getOrDefault(false)
    }

    fun release() {
        released = true
        stopPositionTicker()
        controller?.removeListener(playerListener)
        controller = null
        controllerFuture?.let { MediaController.releaseFuture(it) }
        controllerFuture = null
        _isConnected.value = false
        scope.cancel()
    }

    private fun startPositionTicker() {
        if (positionTickerJob?.isActive == true) return
        positionTickerJob = scope.launch {
            while (isActive) {
                val active = controller ?: break
                if (!active.isPlaying) break
                _playbackPositionMs.value = active.currentPosition.coerceAtLeast(0L)
                delay(POSITION_TICK_MS)
            }
        }
    }

    private fun stopPositionTicker() {
        positionTickerJob?.cancel()
        positionTickerJob = null
    }

    companion object {
        private const val POSITION_TICK_MS: Long = 250L
    }
}
