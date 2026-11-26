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
    fun firesExactlyOnceAtFiftyPercentRealListeningTime() {
        var epochSec = 1_700_000_100L
        var monoMs = 10_000L
        val tracker = ScrobbleTracker(
            clockEpochSeconds = { epochSec },
            monotonicClockMs = { monoMs },
        )

        tracker.onTrackStarted("track-1", durationMs = 200_000L, isPlaying = true)

        // 99.999s of real playback (< 100s) -> must NOT fire
        monoMs += 99_999L
        assertNull(tracker.onTick(isPlaying = true, durationMs = 200_000L))
        assertFalse(tracker.hasScrobbled())

        // +1ms -> 100s reached (50%) -> fires exactly once
        monoMs += 1L
        val first = tracker.onTick(isPlaying = true, durationMs = 200_000L)
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

        // Further listening in the same play MUST NOT re-fire
        epochSec += 60
        monoMs += 60_000L
        assertNull(tracker.onTick(isPlaying = true, durationMs = 200_000L))
    }

    @Test
    fun seekingStraightToNinetyPercentDoesNotScrobbleWithoutRealListeningTime() {
        var monoMs = 1_000L
        val tracker = ScrobbleTracker(
            clockEpochSeconds = { 1_700_000_200L },
            monotonicClockMs = { monoMs },
        )

        // Track started and user immediately seeks to 90% after only 2 seconds of listening
        tracker.onTrackStarted("track-seek", durationMs = 200_000L, isPlaying = true)
        monoMs += 2_000L
        assertNull(tracker.onTick(isPlaying = true, durationMs = 200_000L))
        assertEquals(2_000L, tracker.accumulatedPlayMs())
        assertFalse(tracker.hasScrobbled())
    }

    @Test
    fun resumingAtSeventyFivePercentAfterIgnitionDoesNotDuplicateScrobbleImmediately() {
        var monoMs = 5_000L
        val tracker = ScrobbleTracker(
            clockEpochSeconds = { 1_700_000_300L },
            monotonicClockMs = { monoMs },
        )

        // Car ignition restores playback at 75% (150s of 200s); only 50s remain so track ends before 100s real listening.
        tracker.onTrackStarted("resumed-track", durationMs = 200_000L, isPlaying = true)
        monoMs += 50_000L
        assertNull(tracker.onTick(isPlaying = true, durationMs = 200_000L))
        assertFalse(tracker.hasScrobbled())
    }

    @Test
    fun pauseStopsAccumulationAndResumeContinuesAccurately() {
        var monoMs = 0L
        val tracker = ScrobbleTracker(
            clockEpochSeconds = { 1_700_000_400L },
            monotonicClockMs = { monoMs },
        )

        tracker.onTrackStarted("paused-track", durationMs = 120_000L, isPlaying = true)

        // Listen for 40s then pause
        monoMs += 40_000L
        assertNull(tracker.onPlayingStateChanged(isPlaying = false, durationMs = 120_000L))
        assertEquals(40_000L, tracker.accumulatedPlayMs())

        // Stay paused for 5 minutes -> must NOT advance accumulated time
        monoMs += 300_000L
        assertNull(tracker.onTick(isPlaying = false, durationMs = 120_000L))
        assertEquals(40_000L, tracker.accumulatedPlayMs())

        // Resume and listen for 19s (total 59s < 60s)
        assertNull(tracker.onPlayingStateChanged(isPlaying = true, durationMs = 120_000L))
        monoMs += 19_000L
        assertNull(tracker.onTick(isPlaying = true, durationMs = 120_000L))

        // +1s -> 60s real listening (50% of 120s) -> scrobbles!
        monoMs += 1_000L
        assertNotNull(tracker.onTick(isPlaying = true, durationMs = 120_000L))
    }

    @Test
    fun firesAt240SecondsForLongTracksBeforeHalfway() {
        var monoMs = 0L
        val tracker = ScrobbleTracker(
            clockEpochSeconds = { 1_700_001_000L },
            monotonicClockMs = { monoMs },
        )

        // 12-minute track (720s -> 50% is 360s, but 240s rule fires first)
        tracker.onTrackStarted("long-track", durationMs = 720_000L, isPlaying = true)

        monoMs += 239_999L
        assertNull(tracker.onTick(isPlaying = true, durationMs = 720_000L))

        monoMs += 1L
        val event = tracker.onTick(isPlaying = true, durationMs = 720_000L)
        assertNotNull(event)
        assertEquals("long-track", event!!.songId)

        // Never fires a second time even after crossing 50%
        monoMs += 200_000L
        assertNull(tracker.onTick(isPlaying = true, durationMs = 720_000L))
    }

    @Test
    fun ignoresInterludesShorterThanThirtySeconds() {
        var monoMs = 0L
        val tracker = ScrobbleTracker(
            clockEpochSeconds = { 1_700_001_500L },
            monotonicClockMs = { monoMs },
        )

        tracker.onTrackStarted("interlude", durationMs = 29_999L, isPlaying = true)
        monoMs += 29_999L
        assertNull(tracker.onTick(isPlaying = true, durationMs = 29_999L))
        assertFalse(tracker.hasScrobbled())
    }

    @Test
    fun resetsCleanlyOnTrackTransitionIncludingReplayOfSameTrack() {
        var epochSec = 1_700_002_000L
        var monoMs = 0L
        val tracker = ScrobbleTracker(
            clockEpochSeconds = { epochSec },
            monotonicClockMs = { monoMs },
        )

        tracker.onTrackStarted("track-a", durationMs = 120_000L, isPlaying = true)
        monoMs += 60_000L
        assertNotNull(tracker.onTick(isPlaying = true, durationMs = 120_000L))
        monoMs += 20_000L
        assertNull(tracker.onTick(isPlaying = true, durationMs = 120_000L))

        // Transition to new track resets scrobble flag
        epochSec = 1_700_002_200L
        tracker.onTrackStarted("track-b", durationMs = 100_000L, isPlaying = true)
        assertFalse(tracker.hasScrobbled())
        monoMs += 50_000L
        val second = tracker.onTick(isPlaying = true, durationMs = 100_000L)
        assertNotNull(second)
        assertEquals("track-b", second!!.songId)
        assertEquals(1_700_002_200L, second.playedAtUnixSec)

        // Replaying track-b again starts a fresh play
        epochSec = 1_700_002_500L
        tracker.onTrackStarted("track-b", durationMs = 100_000L, isPlaying = true)
        monoMs += 55_000L
        val replay = tracker.onTick(isPlaying = true, durationMs = 100_000L)
        assertNotNull(replay)
        assertEquals(1_700_002_500L, replay!!.playedAtUnixSec)
    }
}
