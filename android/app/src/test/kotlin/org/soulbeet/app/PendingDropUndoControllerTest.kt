package org.soulbeet.app

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.test.StandardTestDispatcher
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.soulbeet.app.data.DeleteGuardPreviewUi
import org.soulbeet.app.data.ListenerProtectionUi
import org.soulbeet.app.data.PendingDropAction
import org.soulbeet.app.data.PendingDropMode
import org.soulbeet.app.data.PendingDropUndoController
import org.soulbeet.core.FfiOutboxMutation

@OptIn(ExperimentalCoroutinesApi::class)
class PendingDropUndoControllerTest {

    private fun sampleAction(
        id: String,
        title: String = "Track $id",
        discoveryId: String? = "disc-$id",
        mode: PendingDropMode = PendingDropMode.DropDiscovery,
    ) = PendingDropAction(
        trackId = id,
        title = title,
        artist = "Artist $id",
        discoveryTrackId = discoveryId,
        mode = mode,
        sourceLabel = "Weekly Radar",
        createdAtMs = 1_000L,
    )

    @Test
    fun undoWithinWindowRestoresAndCommitsZeroMutation() = runTest {
        val committed = mutableListOf<FfiOutboxMutation>()
        val undone = mutableListOf<String>()

        val controller = PendingDropUndoController(
            timerScope = this,
            onCommit = { committed.addAll(it.toMutations()) },
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

        advanceTimeBy(10_000L)
        runCurrent()
        assertTrue(committed.isEmpty())
        assertFalse(controller.undo())
    }

    @Test
    fun expiryCommitsExactMutationsForEachPendingDropMode() = runTest {
        val committed = mutableListOf<FfiOutboxMutation>()

        val controller = PendingDropUndoController(
            timerScope = this,
            onCommit = { committed.addAll(it.toMutations()) },
            undoWindowMs = 5_000L,
            clockMs = { testScheduler.currentTime },
        )

        // 1. DeleteFromLibrary -> Drop(confirmDelete = true, discoveryTrackId = null)
        controller.scheduleDrop(
            sampleAction(
                id = "song-42",
                discoveryId = "ignored",
                mode = PendingDropMode.DeleteFromLibrary,
            ),
        )
        advanceTimeBy(5_001L)
        runCurrent()
        assertEquals(
            listOf(
                FfiOutboxMutation.Drop(
                    songId = "song-42",
                    discoveryTrackId = null,
                    confirmDelete = true,
                ),
            ),
            committed,
        )

        // 2. UnstarAndHide -> [Unstar, Rate(1)] (never attempts file delete!)
        committed.clear()
        controller.scheduleDrop(
            sampleAction(
                id = "song-shared",
                discoveryId = null,
                mode = PendingDropMode.UnstarAndHide,
            ),
        )
        advanceTimeBy(5_001L)
        runCurrent()
        assertEquals(
            listOf(
                FfiOutboxMutation.Unstar(songId = "song-shared", discoveryTrackId = null),
                FfiOutboxMutation.Rate(songId = "song-shared", rating = 1u, discoveryTrackId = null),
            ),
            committed,
        )
    }

    @Test
    fun rapidSecondDropCommitsDisplacedFirstImmediately() = runTest {
        val committed = mutableListOf<FfiOutboxMutation>()
        val undone = mutableListOf<String>()

        val controller = PendingDropUndoController(
            timerScope = this,
            onCommit = { committed.addAll(it.toMutations()) },
            onUndo = { undone.add(it.trackId) },
            undoWindowMs = 5_000L,
            clockMs = { testScheduler.currentTime },
        )

        controller.scheduleDrop(sampleAction("song-1"))
        advanceTimeBy(1_000L)
        runCurrent()
        assertTrue(committed.isEmpty())

        controller.scheduleDrop(sampleAction("song-2"))
        runCurrent()

        assertEquals(1, committed.size)
        assertEquals("song-1", (committed[0] as FfiOutboxMutation.Drop).songId)
        assertEquals("song-2", controller.pending.value?.trackId)

        advanceTimeBy(2_000L)
        assertTrue(controller.undo())
        assertEquals(listOf("song-2"), undone)

        advanceTimeBy(10_000L)
        runCurrent()
        assertEquals(1, committed.size)
    }

    @Test
    fun flushAsyncSurvivesTimerScopeCancellationOnSeparateCommitScope() = runTest {
        val dispatcher = StandardTestDispatcher(testScheduler)
        val viewModelLikeScope = CoroutineScope(SupervisorJob() + dispatcher)
        val appLikeScope = CoroutineScope(SupervisorJob() + dispatcher)
        val committed = mutableListOf<FfiOutboxMutation>()

        val controller = PendingDropUndoController(
            timerScope = viewModelLikeScope,
            commitScope = appLikeScope,
            onCommit = { committed.addAll(it.toMutations()) },
            undoWindowMs = 5_000L,
            clockMs = { testScheduler.currentTime },
        )

        controller.scheduleDrop(sampleAction("song-7", mode = PendingDropMode.UnstarAndHide))
        advanceTimeBy(500L)

        // Simulate ViewModel.onCleared(): flushAsync on appScope, then cancel viewModelScope.
        controller.flushAsync()
        viewModelLikeScope.cancel()
        runCurrent()

        assertNull(controller.pending.value)
        assertEquals(2, committed.size)
        assertTrue(committed[0] is FfiOutboxMutation.Unstar)
        assertTrue(committed[1] is FfiOutboxMutation.Rate)

        appLikeScope.cancel()
    }

    @Test
    fun deleteGuardPreviewMapsAllFourBranchesCleanly() {
        val base = DeleteGuardPreviewUi(
            songId = "s1",
            trackTitle = "Title",
            trackArtist = "Artist",
            isPendingDiscovery = false,
            discoveryTrackId = null,
            canDelete = false,
            soleOwner = false,
            otherListeners = emptyList(),
            ownPlaylists = emptyList(),
            blockReason = null,
        )

        // 1. Pending Discovery -> DropDiscovery
        assertEquals(
            PendingDropMode.DropDiscovery,
            base.copy(isPendingDiscovery = true, discoveryTrackId = "d1").toPendingDropMode(),
        )

        // 2. Sole owner + deletable + online -> DeleteFromLibrary
        assertEquals(
            PendingDropMode.DeleteFromLibrary,
            base.copy(canDelete = true, soleOwner = true, offlineFallback = false).toPendingDropMode(),
        )

        // 3. Household listener protected -> UnstarAndHide
        val shared = base.copy(
            canDelete = false,
            soleOwner = false,
            otherListeners = listOf(ListenerProtectionUi("other_listener_mark", "Marie ★ 5")),
        )
        assertTrue(shared.hasHouseholdListenerProtection)
        assertEquals(PendingDropMode.UnstarAndHide, shared.toPendingDropMode())

        // 4. Hard blocked / offline fallback -> UnstarAndHide (even if canDelete were true offline)
        assertEquals(
            PendingDropMode.UnstarAndHide,
            base.copy(canDelete = true, soleOwner = true, offlineFallback = true).toPendingDropMode(),
        )
        val unreadable = base.copy(
            otherListeners = listOf(ListenerProtectionUi("unreadable_listener", "Could not inspect")),
        )
        assertFalse(unreadable.hasHouseholdListenerProtection)
        assertEquals(PendingDropMode.UnstarAndHide, unreadable.toPendingDropMode())
    }
}
