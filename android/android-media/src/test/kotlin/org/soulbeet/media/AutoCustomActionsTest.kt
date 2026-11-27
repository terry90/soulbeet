package org.soulbeet.media

import androidx.annotation.OptIn
import androidx.media3.common.util.UnstableApi
import androidx.media3.session.CommandButton
import androidx.media3.session.SessionError
import androidx.media3.session.SessionResult
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.soulbeet.core.FfiDiscoveryTrack
import org.soulbeet.core.FfiException
import org.soulbeet.core.FfiOutboxMutation

@OptIn(UnstableApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class AutoCustomActionsTest {

    @Test
    fun pendingDiscoveryShowsKeepAndDropTwoSlotLayout() {
        val layout = AutoCustomActions.buildCustomLayout(
            TrackActionContext(
                trackId = "disc-1",
                isPendingDiscovery = true,
                isStarred = false,
            ),
        )

        assertEquals(2, layout.size)
        assertEquals(AutoCustomActions.ACTION_KEEP_DISCOVERY, layout[0].sessionCommand?.customAction)
        assertEquals(R.drawable.ic_soulbeet_keep, layout[0].iconResId)
        assertEquals(AutoCustomActions.ACTION_DROP_DISCOVERY, layout[1].sessionCommand?.customAction)
        assertEquals(R.drawable.ic_soulbeet_drop, layout[1].iconResId)
    }

    @Test
    fun permanentLibraryShowsStarToggleAndConfiguredSecondarySlot() {
        val unstarredDislike = AutoCustomActions.buildCustomLayout(
            trackContext = TrackActionContext(
                trackId = "lib-1",
                isPendingDiscovery = false,
                isStarred = false,
            ),
            secondaryLibraryAction = LibrarySecondaryAction.DISLIKE_SKIP,
        )

        assertEquals(2, unstarredDislike.size)
        assertEquals(AutoCustomActions.ACTION_STAR_TOGGLE, unstarredDislike[0].sessionCommand?.customAction)
        assertEquals(R.drawable.ic_soulbeet_star_outline, unstarredDislike[0].iconResId)
        assertEquals(AutoCustomActions.ACTION_DISLIKE_SKIP, unstarredDislike[1].sessionCommand?.customAction)
        assertEquals(R.drawable.ic_soulbeet_thumb_down, unstarredDislike[1].iconResId)

        val starredRadio = AutoCustomActions.buildCustomLayout(
            trackContext = TrackActionContext(
                trackId = "lib-1",
                isPendingDiscovery = false,
                isStarred = true,
            ),
            secondaryLibraryAction = LibrarySecondaryAction.START_RADIO,
        )

        assertEquals(2, starredRadio.size)
        assertEquals(AutoCustomActions.ACTION_STAR_TOGGLE, starredRadio[0].sessionCommand?.customAction)
        assertEquals(R.drawable.ic_soulbeet_star_filled, starredRadio[0].iconResId)
        assertEquals(AutoCustomActions.ACTION_START_RADIO, starredRadio[1].sessionCommand?.customAction)
        assertEquals(R.drawable.ic_soulbeet_radio, starredRadio[1].iconResId)
    }

    @Test
    fun keepActionEnqueuesBeforeUpdatingContextAndRunsUiCallbacksOnCallerThread() = runTest {
        val callerThreadName = Thread.currentThread().name
        val events = mutableListOf<String>()
        val core = FakeCoreClient().apply {
            discoveryTracks.add(
                FfiDiscoveryTrack(
                    id = "dt-42",
                    songId = "song-99",
                    artist = "Parcels",
                    title = "Tieduprightnow",
                    album = null,
                    folderPath = null,
                    profileSlug = "balanced",
                    mixSlug = "weekly-radar",
                    status = "pending",
                    createdAt = null,
                    expiresAt = null,
                    reasonJson = null,
                    excludeFromCoreTaste = false,
                    updatedAt = 1_700_000_000L,
                ),
            )
            onEnqueueMutationCallback = { events.add("enqueue") }
        }

        var activeContext: TrackActionContext? = TrackActionContext(
            trackId = "song-99",
            isPendingDiscovery = true,
            isStarred = false,
        )
        var latestLayout: List<CommandButton> = emptyList()
        var skipped = false

        val handler = AutoCustomActions.ActionHandler(
            core = core,
            ioDispatcher = Dispatchers.IO,
            currentTrackContextProvider = { activeContext },
            onUpdateTrackContext = {
                assertEquals(callerThreadName, Thread.currentThread().name)
                events.add("updateContext")
                activeContext = it
            },
            onRefreshCustomLayout = {
                assertEquals(callerThreadName, Thread.currentThread().name)
                events.add("refreshLayout")
                latestLayout = it
            },
            onSkipToNext = {
                skipped = true
                true
            },
        )

        val res = handler.handle(AutoCustomActions.ACTION_KEEP_DISCOVERY)

        assertEquals(SessionResult.RESULT_SUCCESS, res.resultCode)
        assertFalse("Keep must NOT skip playback", skipped)
        assertEquals(listOf("enqueue", "updateContext", "refreshLayout"), events)
        assertFalse(activeContext!!.isPendingDiscovery)
        assertEquals("Keep must NOT stamp a star rating", 0, activeContext!!.userRating)
        assertEquals(
            listOf(FfiOutboxMutation.Keep(songId = "song-99", discoveryTrackId = "dt-42", targetFolderId = null)),
            core.enqueuedMutations,
        )
        assertEquals(AutoCustomActions.ACTION_STAR_TOGGLE, latestLayout[0].sessionCommand?.customAction)
    }

    @Test
    fun failedEnqueueDoesNotDesyncContextOrLayout() = runTest {
        val core = FakeCoreClient().apply {
            enqueueMutationException = FfiException.Database("disk full")
        }
        val initial = TrackActionContext(
            trackId = "song-99",
            isPendingDiscovery = true,
            isStarred = false,
        )
        var activeContext: TrackActionContext? = initial
        var layoutRefreshed = false

        val handler = AutoCustomActions.ActionHandler(
            core = core,
            ioDispatcher = Dispatchers.Unconfined,
            currentTrackContextProvider = { activeContext },
            onUpdateTrackContext = { activeContext = it },
            onRefreshCustomLayout = { layoutRefreshed = true },
            onSkipToNext = { true },
        )

        val res = handler.handle(AutoCustomActions.ACTION_KEEP_DISCOVERY)

        assertEquals(SessionError.ERROR_IO, res.resultCode)
        assertEquals(initial, activeContext)
        assertFalse("Layout must not refresh when mutation enqueue fails", layoutRefreshed)
    }

    @Test
    fun dropDiscoveryOnLastTrackClearsContextAndCustomLayoutWhenStopped() = runTest {
        val core = FakeCoreClient()
        var activeContext: TrackActionContext? = TrackActionContext(
            trackId = "song-last",
            isPendingDiscovery = true,
            isStarred = false,
            discoveryTrackId = "dt-last",
        )
        var latestLayout: List<CommandButton> = AutoCustomActions.buildCustomLayout(activeContext)

        val handler = AutoCustomActions.ActionHandler(
            core = core,
            ioDispatcher = Dispatchers.Unconfined,
            currentTrackContextProvider = { activeContext },
            onUpdateTrackContext = { activeContext = it },
            onRefreshCustomLayout = { latestLayout = it },
            onSkipToNext = { false }, // no next item -> player stopped
        )

        val res = handler.handle(AutoCustomActions.ACTION_DROP_DISCOVERY)

        assertEquals(SessionResult.RESULT_SUCCESS, res.resultCode)
        assertNull("Stopping on last track must clear active context", activeContext)
        assertTrue("Stopping on last track must clear stale Keep/Drop custom layout", latestLayout.isEmpty())
        assertEquals(
            listOf(
                FfiOutboxMutation.Drop(
                    songId = "song-last",
                    discoveryTrackId = "dt-last",
                    confirmDelete = false,
                ),
            ),
            core.enqueuedMutations,
        )
    }

    @Test
    fun starToggleFlipsIconAndEnqueuesStarThenUnstar() = runTest {
        val core = FakeCoreClient()
        var activeContext: TrackActionContext? = TrackActionContext(
            trackId = "lib-7",
            isPendingDiscovery = false,
            isStarred = false,
        )
        var latestLayout: List<CommandButton> = emptyList()

        val handler = AutoCustomActions.ActionHandler(
            core = core,
            ioDispatcher = Dispatchers.Unconfined,
            currentTrackContextProvider = { activeContext },
            onUpdateTrackContext = { activeContext = it },
            onRefreshCustomLayout = { latestLayout = it },
            onSkipToNext = { true },
        )

        handler.handle(AutoCustomActions.ACTION_STAR_TOGGLE)
        assertTrue(activeContext!!.isStarred)
        assertEquals(R.drawable.ic_soulbeet_star_filled, latestLayout[0].iconResId)

        handler.handle(AutoCustomActions.ACTION_STAR_TOGGLE)
        assertFalse(activeContext!!.isStarred)
        assertEquals(R.drawable.ic_soulbeet_star_outline, latestLayout[0].iconResId)

        assertEquals(
            listOf(
                FfiOutboxMutation.Star(songId = "lib-7", discoveryTrackId = null),
                FfiOutboxMutation.Unstar(songId = "lib-7", discoveryTrackId = null),
            ),
            core.enqueuedMutations,
        )
    }

    @Test
    fun dislikeSkipEnqueuesOneStarRatingAndSkipsToNext() = runTest {
        val core = FakeCoreClient()
        var activeContext: TrackActionContext? = TrackActionContext(
            trackId = "lib-9",
            isPendingDiscovery = false,
            isStarred = false,
        )
        var skipped = false

        val handler = AutoCustomActions.ActionHandler(
            core = core,
            ioDispatcher = Dispatchers.Unconfined,
            currentTrackContextProvider = { activeContext },
            onUpdateTrackContext = { activeContext = it },
            onRefreshCustomLayout = {},
            onSkipToNext = {
                skipped = true
                true
            },
        )

        val res = handler.handle(AutoCustomActions.ACTION_DISLIKE_SKIP)

        assertEquals(SessionResult.RESULT_SUCCESS, res.resultCode)
        assertTrue(skipped)
        assertEquals(1, activeContext!!.userRating)
        assertEquals(
            listOf(FfiOutboxMutation.Rate(songId = "lib-9", rating = 1u, discoveryTrackId = null)),
            core.enqueuedMutations,
        )
    }
}
