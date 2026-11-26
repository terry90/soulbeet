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
 * Tracks real playback progress per track play and fires a [ScrobbleEvent] at most once once
 * playback reaches **>= 50% of track duration** or **>= 240 seconds** (standard OpenSubsonic
 * scrobble rule). Resets automatically on track transition.
 */
class ScrobbleTracker(
    private val clockEpochSeconds: () -> Long = { System.currentTimeMillis() / 1_000L },
) {
    private var currentTrackId: String? = null
    private var currentDurationMs: Long = 0L
    private var startedAtUnixSec: Long = 0L
    private var hasScrobbledCurrentPlay: Boolean = false

    fun activeTrackId(): String? = currentTrackId

    fun hasScrobbled(): Boolean = hasScrobbledCurrentPlay

    /**
     * Begins tracking a newly started track play, resetting scrobble state even if the same track
     * is replayed consecutively.
     */
    fun onTrackStarted(trackId: String, durationMs: Long = 0L) {
        currentTrackId = trackId
        currentDurationMs = durationMs.coerceAtLeast(0L)
        startedAtUnixSec = clockEpochSeconds()
        hasScrobbledCurrentPlay = false
    }

    /**
     * Evaluates playback progress against the >= 50% or >= 240s scrobble threshold.
     *
     * Returns a [ScrobbleEvent] exactly once when the threshold is crossed, and `null` otherwise.
     */
    fun onProgress(positionMs: Long, durationMs: Long = currentDurationMs): ScrobbleEvent? {
        val trackId = currentTrackId ?: return null
        if (hasScrobbledCurrentPlay) return null

        val effectiveDuration = if (durationMs > 0L) durationMs else currentDurationMs
        if (effectiveDuration <= 0L || positionMs < 0L) return null
        currentDurationMs = effectiveDuration

        val reachedHalf = positionMs >= (effectiveDuration + 1L) / 2L
        val reachedFourMinutes = positionMs >= SCROBBLE_MAX_THRESHOLD_MS
        if (!reachedHalf && !reachedFourMinutes) return null

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
        hasScrobbledCurrentPlay = false
    }

    companion object {
        const val SCROBBLE_MAX_THRESHOLD_MS: Long = 240_000L
    }
}
