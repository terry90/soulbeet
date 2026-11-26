package org.soulbeet.media

import androidx.media3.session.CommandButton
import androidx.media3.session.SessionResult
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.soulbeet.core.FfiDiscoveryTrack
import org.soulbeet.core.FfiOutboxMutation

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
    fun keepActionEnqueuesKeepMutationAndFlipsCustomLayoutToLibraryBar() = runTest {
        val core = FakeCoreClient()
        core.discoveryTracks.add(
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

        var activeContext: TrackActionContext? = TrackActionContext(
            trackId = "song-99",
            isPendingDiscovery = true,
            isStarred = false,
        )
        var latestLayout: List<CommandButton> = emptyList()
        var skipped = false

        val handler = AutoCustomActions.ActionHandler(
            core = core,
            currentTrackContextProvider = { activeContext },
            onUpdateTrackContext = { activeContext = it },
            onRefreshCustomLayout = { latestLayout = it },
            onSkipToNext = { skipped = true },
        )

        val res = handler.handle(AutoCustomActions.ACTION_KEEP_DISCOVERY)

        assertEquals(SessionResult.RESULT_SUCCESS, res.resultCode)
        assertFalse("Keep must NOT skip playback", skipped)
        assertFalse("Track should optimistically transition out of discovery", activeContext!!.isPendingDiscovery)
        assertEquals(1, core.enqueuedMutations.size)
        assertEquals(
            FfiOutboxMutation.Keep(songId = "song-99", discoveryTrackId = "dt-42", targetFolderId = null),
            core.enqueuedMutations[0],
        )
        assertEquals(2, latestLayout.size)
        assertEquals(AutoCustomActions.ACTION_STAR_TOGGLE, latestLayout[0].sessionCommand?.customAction)
    }

    @Test
    fun dropDiscoveryEnqueuesDropMutationAndSkipsToNext() = runTest {
        val core = FakeCoreClient()
        var activeContext: TrackActionContext? = TrackActionContext(
            trackId = "song-drop",
            isPendingDiscovery = true,
            isStarred = false,
            discoveryTrackId = "dt-7",
        )
        var skipped = false

        val handler = AutoCustomActions.ActionHandler(
            core = core,
            currentTrackContextProvider = { activeContext },
            onUpdateTrackContext = { activeContext = it },
            onRefreshCustomLayout = {},
            onSkipToNext = { skipped = true },
        )

        val res = handler.handle(AutoCustomActions.ACTION_DROP_DISCOVERY)

        assertEquals(SessionResult.RESULT_SUCCESS, res.resultCode)
        assertTrue("Drop must skip immediately", skipped)
        assertEquals(
            listOf(
                FfiOutboxMutation.Drop(
                    songId = "song-drop",
                    discoveryTrackId = "dt-7",
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
            currentTrackContextProvider = { activeContext },
            onUpdateTrackContext = { activeContext = it },
            onRefreshCustomLayout = { latestLayout = it },
            onSkipToNext = {},
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
            currentTrackContextProvider = { activeContext },
            onUpdateTrackContext = { activeContext = it },
            onRefreshCustomLayout = {},
            onSkipToNext = { skipped = true },
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
