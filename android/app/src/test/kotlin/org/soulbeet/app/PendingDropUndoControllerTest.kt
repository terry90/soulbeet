package org.soulbeet.app

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.soulbeet.app.data.PendingDropAction
import org.soulbeet.app.data.PendingDropUndoController
import org.soulbeet.core.FfiOutboxMutation

@OptIn(ExperimentalCoroutinesApi::class)
class PendingDropUndoControllerTest {

    private fun sampleAction(
        id: String,
        title: String = "Track $id",
        discoveryId: String? = "disc-$id",
        confirmDelete: Boolean = false,
    ) = PendingDropAction(
        trackId = id,
        title = title,
        artist = "Artist $id",
        discoveryTrackId = discoveryId,
        confirmDelete = confirmDelete,
        sourceLabel = "Weekly Radar",
        createdAtMs = 1_000L,
    )

    @Test
    fun undoWithinWindowRestoresAndCommitsZeroMutation() = runTest {
        val committed = mutableListOf<FfiOutboxMutation>()
        val undone = mutableListOf<String>()

        val controller = PendingDropUndoController(
            scope = this,
            onCommit = { committed.add(it.toMutation()) },
            onUndo = { undone.add(it.trackId) },
            undoWindowMs = 5_000L,
            clockMs = { testScheduler.currentTime },
        )

        controller.scheduleDrop(sampleAction("song-1"))
        runCurrent()
        assertEquals("song-1", controller.pending.value?.trackId)

        advanceTimeBy(2_500L)
        assertTrue(controller.undo())
        assertNull(controller.pending.value)
        assertEquals(listOf("song-1"), undone)

        // Advance well past original 5s deadline — zero mutations should ever be committed.
        advanceTimeBy(10_000L)
        runCurrent()
        assertTrue(committed.isEmpty())
        assertFalse(controller.undo())
    }

    @Test
    fun expiryCommitsDropMutationWithExactPayload() = runTest {
        val committed = mutableListOf<FfiOutboxMutation>()

        val controller = PendingDropUndoController(
            scope = this,
            onCommit = { committed.add(it.toMutation()) },
            undoWindowMs = 5_000L,
            clockMs = { testScheduler.currentTime },
        )

        controller.scheduleDrop(
            sampleAction(
                id = "song-42",
                discoveryId = "disc-99",
                confirmDelete = true,
            ),
        )
        runCurrent()

        advanceTimeBy(4_999L)
        runCurrent()
        assertTrue("Mutation must not commit before 5s window expires", committed.isEmpty())

        advanceTimeBy(2L)
        runCurrent()
        assertNull(controller.pending.value)
        assertEquals(1, committed.size)
        assertEquals(
            FfiOutboxMutation.Drop(
                songId = "song-42",
                discoveryTrackId = "disc-99",
                confirmDelete = true,
            ),
            committed.single(),
        )
    }

    @Test
    fun rapidSecondDropCommitsDisplacedFirstImmediately() = runTest {
        val committed = mutableListOf<FfiOutboxMutation>()
        val undone = mutableListOf<String>()

        val controller = PendingDropUndoController(
            scope = this,
            onCommit = { committed.add(it.toMutation()) },
            onUndo = { undone.add(it.trackId) },
            undoWindowMs = 5_000L,
            clockMs = { testScheduler.currentTime },
        )

        controller.scheduleDrop(sampleAction("song-1"))
        advanceTimeBy(1_000L)
        runCurrent()
        assertTrue(committed.isEmpty())

        // Second rapid swipe displaces song-1 -> song-1 commits immediately, song-2 enters 5s window.
        controller.scheduleDrop(sampleAction("song-2"))
        runCurrent()

        assertEquals(1, committed.size)
        assertEquals("song-1", (committed[0] as FfiOutboxMutation.Drop).songId)
        assertEquals("song-2", controller.pending.value?.trackId)

        // Undo cancels song-2 without affecting song-1.
        advanceTimeBy(2_000L)
        assertTrue(controller.undo())
        assertEquals(listOf("song-2"), undone)

        advanceTimeBy(10_000L)
        runCurrent()
        assertEquals(1, committed.size)
    }

    @Test
    fun flushNowCommitsPendingImmediately() = runTest {
        val committed = mutableListOf<FfiOutboxMutation>()

        val controller = PendingDropUndoController(
            scope = this,
            onCommit = { committed.add(it.toMutation()) },
            undoWindowMs = 5_000L,
            clockMs = { testScheduler.currentTime },
        )

        controller.scheduleDrop(sampleAction("song-7"))
        advanceTimeBy(500L)
        controller.flushNow()
        runCurrent()

        assertNull(controller.pending.value)
        assertEquals(1, committed.size)
        assertEquals("song-7", (committed[0] as FfiOutboxMutation.Drop).songId)

        // Ensure old timer doesn't double-commit.
        advanceTimeBy(10_000L)
        runCurrent()
        assertEquals(1, committed.size)
    }
}
