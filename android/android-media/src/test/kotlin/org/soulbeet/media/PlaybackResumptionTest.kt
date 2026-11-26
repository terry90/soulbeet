package org.soulbeet.media

import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.soulbeet.core.FfiPersistedPlaybackSession

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class PlaybackResumptionTest {

    private val authority = "org.soulbeet.app.art"

    @Test
    fun restoreSessionPreservesExactPersistedQueueOrderAndPosition() = runTest {
        val core = FakeCoreClient()
        // Simulate DB returning tracks in arbitrary order; restoreSession MUST preserve queue order.
        core.returnTracksOutOfOrder = true
        core.tracksById["t1"] = FakeCoreClient.sampleTrack("t1", durationSecs = 180L)
        core.tracksById["t2"] = FakeCoreClient.sampleTrack("t2", durationSecs = 210L)
        core.tracksById["t3"] = FakeCoreClient.sampleTrack("t3", durationSecs = 240L)
        core.persistedSession = FfiPersistedPlaybackSession(
            queueTrackIds = listOf("t1", "t2", "t3"),
            currentIndex = 1u,
            positionMs = 45_000uL,
            isPlaying = true,
            sourceLabel = "Drive Mix",
            updatedAt = 1_700_000_000L,
        )

        val restored = PlaybackSessionStore.restoreSession(core, authority)

        assertNotNull(restored)
        assertEquals(listOf("t1", "t2", "t3"), restored!!.items.map { it.mediaId })
        assertEquals(1, restored.startIndex)
        assertEquals(45_000L, restored.startPositionMs)
        assertEquals("Drive Mix", restored.sourceLabel)
        assertEquals(
            "soulbeet://track/t2",
            restored.items[1].localConfiguration?.uri?.toString(),
        )
    }

    @Test
    fun restoreSessionClampsOutOfRangeIndexAndPosition() = runTest {
        val core = FakeCoreClient()
        core.tracksById["t1"] = FakeCoreClient.sampleTrack("t1", durationSecs = 120L)
        core.tracksById["t2"] = FakeCoreClient.sampleTrack("t2", durationSecs = 150L)
        core.persistedSession = FfiPersistedPlaybackSession(
            queueTrackIds = listOf("t1", "t2"),
            currentIndex = 99u,
            positionMs = 999_999uL,
            isPlaying = false,
            sourceLabel = null,
            updatedAt = 1_700_000_000L,
        )

        val restored = PlaybackSessionStore.restoreSession(core, authority)

        assertNotNull(restored)
        assertEquals(1, restored!!.startIndex)
        assertEquals(150_000L, restored.startPositionMs)
    }

    @Test
    fun restoreSessionRemapsIndexWhenEarlierTracksWereDeleted() = runTest {
        val core = FakeCoreClient()
        // t1 was deleted from library; t2 and t3 remain
        core.tracksById["t2"] = FakeCoreClient.sampleTrack("t2", durationSecs = 180L)
        core.tracksById["t3"] = FakeCoreClient.sampleTrack("t3", durationSecs = 200L)
        core.persistedSession = FfiPersistedPlaybackSession(
            queueTrackIds = listOf("t1", "t2", "t3"),
            currentIndex = 2u, // pointed at t3
            positionMs = 30_000uL,
            isPlaying = true,
            sourceLabel = "Weekly Radar",
            updatedAt = 1_700_000_000L,
        )

        val restored = PlaybackSessionStore.restoreSession(core, authority)

        assertNotNull(restored)
        assertEquals(listOf("t2", "t3"), restored!!.items.map { it.mediaId })
        assertEquals(1, restored.startIndex)
        assertEquals(30_000L, restored.startPositionMs)
    }

    @Test
    fun restoreSessionReturnsNullWhenEmptyOrAllTracksMissing() = runTest {
        val core = FakeCoreClient()
        assertNull(PlaybackSessionStore.restoreSession(core, authority))

        core.persistedSession = FfiPersistedPlaybackSession(
            queueTrackIds = listOf("deleted-1", "deleted-2"),
            currentIndex = 0u,
            positionMs = 10_000uL,
            isPlaying = false,
            sourceLabel = null,
            updatedAt = 1_700_000_000L,
        )
        assertNull(PlaybackSessionStore.restoreSession(core, authority))
    }

    @Test
    fun saveSessionClampsNegativeIndexAndPosition() = runTest {
        val core = FakeCoreClient()

        PlaybackSessionStore.saveSession(
            core = core,
            queueTrackIds = listOf("a", "b"),
            currentIndex = -5,
            positionMs = -100L,
            isPlaying = true,
            sourceLabel = "Starred",
            nowUnixSec = 1_700_005_000L,
        )

        val saved = core.persistedSession
        assertNotNull(saved)
        assertEquals(0u, saved!!.currentIndex)
        assertEquals(0uL, saved.positionMs)
        assertEquals(listOf("a", "b"), saved.queueTrackIds)
        assertEquals("Starred", saved.sourceLabel)
    }
}
