package org.soulbeet.media

import org.soulbeet.core.FfiOutboxMutation

/**
 * Emitted once per track play when the OpenSubsonic scrobble threshold is reached.
 */
data class ScrobbleEvent(
    val songId: String,
    val playedAtUnixSec: Long,
) {
    fun toOutboxMutation(): FfiOutboxMutation.Scrobble =
        FfiOutboxMutation.Scrobble(
            songId = songId,
            submission = true,
            playedAtUnix = playedAtUnixSec,
        )
}

/**
 * Tracks real accumulated elapsed playback time per track play using a monotonic clock while
 * `isPlaying == true` (ignoring seeks and pauses).
 *
 * Enforces the standard OpenSubsonic / Last.fm scrobble rule:
 * - Track duration must be at least 30 seconds ([MIN_SCROBBLE_DURATION_MS]).
 * - Fires **at most once per play** once real accumulated listening time reaches
 *   `min(ceil(duration / 2), 240s)`.
 * - Resets automatically on track transition (including replaying the same track).
 */
class ScrobbleTracker(
    private val clockEpochSeconds: () -> Long = { System.currentTimeMillis() / 1_000L },
    private val monotonicClockMs: () -> Long = { System.nanoTime() / 1_000_000L },
) {
    private var currentTrackId: String? = null
    private var currentDurationMs: Long = 0L
    private var startedAtUnixSec: Long = 0L
    private var accumulatedPlayMs: Long = 0L
    private var playingSinceMonotonicMs: Long? = null
    private var hasScrobbledCurrentPlay: Boolean = false

    fun activeTrackId(): String? = currentTrackId

    fun hasScrobbled(): Boolean = hasScrobbledCurrentPlay

    fun accumulatedPlayMs(): Long {
        val base = accumulatedPlayMs
        val since = playingSinceMonotonicMs ?: return base
        val delta = (monotonicClockMs() - since).coerceAtLeast(0L)
        return base + delta
    }

    /**
     * Begins tracking a newly started track play, resetting accumulated playback time and scrobble
     * state even if the same track is replayed consecutively.
     */
    fun onTrackStarted(
        trackId: String,
        durationMs: Long = 0L,
        isPlaying: Boolean = false,
    ) {
        currentTrackId = trackId
        currentDurationMs = durationMs.coerceAtLeast(0L)
        startedAtUnixSec = clockEpochSeconds()
        accumulatedPlayMs = 0L
        playingSinceMonotonicMs = if (isPlaying) monotonicClockMs() else null
        hasScrobbledCurrentPlay = false
    }

    /**
     * Updates active playing/paused state, flushing elapsed playback time before pausing and
     * resuming the monotonic timer on play.
     */
    fun onPlayingStateChanged(
        isPlaying: Boolean,
        durationMs: Long = currentDurationMs,
    ): ScrobbleEvent? = onTick(isPlaying = isPlaying, durationMs = durationMs)

    /**
     * Advances accumulated listening time if [isPlaying] is true and evaluates the scrobble
     * threshold. Seeks never artificially advance accumulated listening time.
     */
    fun onTick(
        isPlaying: Boolean = (playingSinceMonotonicMs != null),
        durationMs: Long = currentDurationMs,
    ): ScrobbleEvent? {
        val trackId = currentTrackId ?: return null
        if (durationMs > 0L) {
            currentDurationMs = durationMs
        }

        val nowMs = monotonicClockMs()
        val prevSince = playingSinceMonotonicMs
        if (prevSince != null) {
            accumulatedPlayMs += (nowMs - prevSince).coerceAtLeast(0L)
        }
        playingSinceMonotonicMs = if (isPlaying) nowMs else null

        if (hasScrobbledCurrentPlay) return null
        if (currentDurationMs < MIN_SCROBBLE_DURATION_MS) return null

        val thresholdMs = minOf((currentDurationMs + 1L) / 2L, SCROBBLE_MAX_THRESHOLD_MS)
        if (accumulatedPlayMs < thresholdMs) return null

        hasScrobbledCurrentPlay = true
        return ScrobbleEvent(
            songId = trackId,
            playedAtUnixSec = startedAtUnixSec,
        )
    }

    /**
     * Clears tracking state (e.g., when playback stops or queue is cleared).
     */
    fun reset() {
        currentTrackId = null
        currentDurationMs = 0L
        startedAtUnixSec = 0L
        accumulatedPlayMs = 0L
        playingSinceMonotonicMs = null
        hasScrobbledCurrentPlay = false
    }

    companion object {
        const val MIN_SCROBBLE_DURATION_MS: Long = 30_000L
        const val SCROBBLE_MAX_THRESHOLD_MS: Long = 240_000L
    }
}
