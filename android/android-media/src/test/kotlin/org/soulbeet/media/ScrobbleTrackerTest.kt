package org.soulbeet.media

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.soulbeet.core.FfiOutboxMutation

class ScrobbleTrackerTest {

    @Test
    fun firesExactlyOnceAtFiftyPercentDuration() {
        var nowSec = 1_700_000_100L
        val tracker = ScrobbleTracker(clockEpochSeconds = { nowSec })

        tracker.onTrackStarted("track-1", durationMs = 200_000L)

        // Below 50% (99.999s < 100s)
        assertNull(tracker.onProgress(positionMs = 99_999L, durationMs = 200_000L))
        assertFalse(tracker.hasScrobbled())

        // Exactly 50% (100s)
        val first = tracker.onProgress(positionMs = 100_000L, durationMs = 200_000L)
        assertNotNull(first)
        assertEquals("track-1", first!!.songId)
        assertEquals(1_700_000_100L, first.playedAtUnixSec)
        assertEquals(
            FfiOutboxMutation.Scrobble(
                songId = "track-1",
                submission = true,
                playedAtUnix = 1_700_000_100L,
            ),
            first.toOutboxMutation(),
        )
        assertTrue(tracker.hasScrobbled())

        // Subsequent updates in the same play MUST NOT re-fire
        nowSec += 30
        assertNull(tracker.onProgress(positionMs = 150_000L, durationMs = 200_000L))
        assertNull(tracker.onProgress(positionMs = 199_000L, durationMs = 200_000L))
    }

    @Test
    fun firesAt240SecondsForLongTracksBeforeHalfway() {
        val tracker = ScrobbleTracker(clockEpochSeconds = { 1_700_001_000L })

        // 12-minute track (720s -> 50% is 360s, but 240s rule fires first)
        tracker.onTrackStarted("long-track", durationMs = 720_000L)

        assertNull(tracker.onProgress(positionMs = 239_999L, durationMs = 720_000L))

        val event = tracker.onProgress(positionMs = 240_000L, durationMs = 720_000L)
        assertNotNull(event)
        assertEquals("long-track", event!!.songId)

        // Never fires a second time even after crossing 50%
        assertNull(tracker.onProgress(positionMs = 400_000L, durationMs = 720_000L))
    }

    @Test
    fun resetsCleanlyOnTrackTransitionIncludingReplayOfSameTrack() {
        var nowSec = 1_700_002_000L
        val tracker = ScrobbleTracker(clockEpochSeconds = { nowSec })

        tracker.onTrackStarted("track-a", durationMs = 120_000L)
        assertNotNull(tracker.onProgress(60_000L, 120_000L))
        assertNull(tracker.onProgress(80_000L, 120_000L))

        // Transition to new track resets scrobble flag
        nowSec = 1_700_002_200L
        tracker.onTrackStarted("track-b", durationMs = 100_000L)
        assertFalse(tracker.hasScrobbled())
        val second = tracker.onProgress(50_000L, 100_000L)
        assertNotNull(second)
        assertEquals("track-b", second!!.songId)
        assertEquals(1_700_002_200L, second.playedAtUnixSec)

        // Replaying track-b again starts a fresh play
        nowSec = 1_700_002_500L
        tracker.onTrackStarted("track-b", durationMs = 100_000L)
        val replay = tracker.onProgress(55_000L, 100_000L)
        assertNotNull(replay)
        assertEquals(1_700_002_500L, replay!!.playedAtUnixSec)
    }

    @Test
    fun ignoresUnknownDurationAndNegativePosition() {
        val tracker = ScrobbleTracker()
        tracker.onTrackStarted("track-x", durationMs = 0L)

        assertNull(tracker.onProgress(positionMs = 50_000L, durationMs = 0L))
        assertNull(tracker.onProgress(positionMs = -10L, durationMs = 100_000L))
    }
}
