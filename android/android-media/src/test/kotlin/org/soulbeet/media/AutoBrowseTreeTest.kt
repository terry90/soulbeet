package org.soulbeet.media

import androidx.media3.common.MediaMetadata
import androidx.media3.session.MediaConstants
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.soulbeet.core.FfiAutoNodeKind

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class AutoBrowseTreeTest {

    private val authority = "org.soulbeet.app.art"

    @Test
    fun rootTabsExposeExactFourCarValidatorTabsWithStyleHints() = runTest {
        val core = FakeCoreClient()
        // Even if a future core returned >4 tabs, CAR.VALIDATOR strictly caps at 4.
        core.rootTabs = FakeCoreClient.defaultFourRootTabs() + FakeCoreClient.browseNode(
            id = "tab:extra",
            title = "Extra",
            kind = FfiAutoNodeKind.TAB,
            playable = false,
            browsable = true,
        )

        val tabs = AutoBrowseTree.loadChildren(
            core = core,
            parentId = AutoBrowseTree.ROOT_ID,
            page = 0,
            pageSize = Int.MAX_VALUE,
            artAuthority = authority,
        )

        assertEquals(4, tabs.size)
        assertEquals(
            listOf("For You", "Playlists", "Library", "Radio"),
            tabs.map { it.mediaMetadata.title?.toString() },
        )
        tabs.forEach { tab ->
            assertTrue("Tab ${tab.mediaId} must be browsable", tab.mediaMetadata.isBrowsable == true)
            assertFalse("Tab ${tab.mediaId} must not be playable", tab.mediaMetadata.isPlayable == true)
        }

        // For You, Playlists, Radio use Grid; Library uses List
        val forYouExtras = tabs[0].mediaMetadata.extras!!
        assertEquals(
            MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_GRID_ITEM,
            forYouExtras.getInt(MediaConstants.EXTRAS_KEY_CONTENT_STYLE_BROWSABLE),
        )
        val libraryExtras = tabs[2].mediaMetadata.extras!!
        assertEquals(
            MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM,
            libraryExtras.getInt(MediaConstants.EXTRAS_KEY_CONTENT_STYLE_BROWSABLE),
        )
        assertEquals(MediaMetadata.MEDIA_TYPE_FOLDER_PLAYLISTS, tabs[1].mediaMetadata.mediaType)
        assertEquals(MediaMetadata.MEDIA_TYPE_FOLDER_RADIO_STATIONS, tabs[3].mediaMetadata.mediaType)
    }

    @Test
    fun paginationRespectsBoundsAndHandlesMaxValueWithoutOverflow() {
        val items = (1..10).toList()

        assertEquals(listOf(1, 2, 3), AutoBrowseTree.paginate(items, page = 0, pageSize = 3))
        assertEquals(listOf(4, 5, 6), AutoBrowseTree.paginate(items, page = 1, pageSize = 3))
        assertEquals(listOf(10), AutoBrowseTree.paginate(items, page = 3, pageSize = 3))
        assertEquals(emptyList<Int>(), AutoBrowseTree.paginate(items, page = 4, pageSize = 3))

        // Android Auto commonly requests page=0/1 with pageSize=Int.MAX_VALUE
        assertEquals(items, AutoBrowseTree.paginate(items, page = 0, pageSize = Int.MAX_VALUE))
        assertEquals(emptyList<Int>(), AutoBrowseTree.paginate(items, page = 1, pageSize = Int.MAX_VALUE))

        // Defensive guards
        assertEquals(emptyList<Int>(), AutoBrowseTree.paginate(items, page = -1, pageSize = 5))
        assertEquals(emptyList<Int>(), AutoBrowseTree.paginate(items, page = 0, pageSize = 0))
    }

    @Test
    fun trackLeafMapsContentProviderArtworkUriAndOmitsWhenNull() = runTest {
        val core = FakeCoreClient()
        val withArt = FakeCoreClient.sampleTrack(
            id = "s1",
            title = "Instant Crush",
            artist = "Daft Punk",
            album = "RAM",
            coverArtId = "cov-123",
            starred = true,
            userRating = 5L,
            isDiscovery = false,
        )
        val withoutArt = FakeCoreClient.sampleTrack(
            id = "s2",
            title = "Fresh Find",
            artist = "New Artist",
            album = "EP",
            coverArtId = null,
            starred = false,
            userRating = 0L,
            isDiscovery = true,
        )
        core.tracksById["s1"] = withArt
        core.tracksById["s2"] = withoutArt
        core.childrenByParent["playlist:p1"] = listOf(
            FakeCoreClient.browseNode(
                id = "track:s1",
                title = "Instant Crush",
                subtitle = "Daft Punk",
                kind = FfiAutoNodeKind.TRACK,
                coverArtId = "cov-123",
                isOfflineReady = true,
                isPendingDiscovery = false,
            ),
            FakeCoreClient.browseNode(
                id = "track:s2",
                title = "Fresh Find",
                subtitle = "New Artist",
                kind = FfiAutoNodeKind.TRACK,
                coverArtId = null,
                isOfflineReady = false,
                isPendingDiscovery = true,
            ),
        )

        val items = AutoBrowseTree.loadChildren(
            core = core,
            parentId = "playlist:p1",
            page = 0,
            pageSize = 25,
            artAuthority = authority,
        )

        assertEquals(2, items.size)

        val item1 = items[0]
        assertEquals(MediaMetadata.MEDIA_TYPE_MUSIC, item1.mediaMetadata.mediaType)
        assertEquals("content://$authority/art/cov-123", item1.mediaMetadata.artworkUri?.toString())
        assertEquals("RAM", item1.mediaMetadata.albumTitle?.toString())
        val extras1 = item1.mediaMetadata.extras
        assertNotNull(extras1)
        assertFalse(extras1!!.getBoolean(AutoBrowseTree.EXTRA_IS_DISCOVERY))
        assertTrue(extras1.getBoolean(AutoBrowseTree.EXTRA_IS_STARRED))
        assertTrue(extras1.getBoolean(AutoBrowseTree.EXTRA_IS_OFFLINE_CACHED))
        assertEquals(5, extras1.getInt(AutoBrowseTree.EXTRA_RATING))
        assertEquals("s1", extras1.getString(AutoBrowseTree.EXTRA_TRACK_ID))

        val item2 = items[1]
        assertNull("artworkUri must be omitted when coverArtId is null", item2.mediaMetadata.artworkUri)
        val extras2 = item2.mediaMetadata.extras!!
        assertTrue(extras2.getBoolean(AutoBrowseTree.EXTRA_IS_DISCOVERY))
        assertFalse(extras2.getBoolean(AutoBrowseTree.EXTRA_IS_STARRED))
        assertFalse(extras2.getBoolean(AutoBrowseTree.EXTRA_IS_OFFLINE_CACHED))
        assertEquals(0, extras2.getInt(AutoBrowseTree.EXTRA_RATING))
        assertEquals("s2", extras2.getString(AutoBrowseTree.EXTRA_TRACK_ID))
    }

    @Test
    fun offlineOnlyFiltersOutUncachedLeafTracks() = runTest {
        val core = FakeCoreClient()
        core.childrenByParent["playlist:p1"] = listOf(
            FakeCoreClient.browseNode("track:cached", "Cached", FfiAutoNodeKind.TRACK, isOfflineReady = true),
            FakeCoreClient.browseNode("track:remote", "Remote", FfiAutoNodeKind.TRACK, isOfflineReady = false),
        )

        val items = AutoBrowseTree.loadChildren(
            core = core,
            parentId = "playlist:p1",
            page = 0,
            pageSize = 25,
            artAuthority = authority,
            offlineOnly = true,
        )

        assertEquals(1, items.size)
        assertEquals("track:cached", items[0].mediaId)
    }
}
