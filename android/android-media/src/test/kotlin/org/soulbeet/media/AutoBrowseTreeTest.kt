package org.soulbeet.media

import androidx.annotation.OptIn
import androidx.media3.common.MediaMetadata
import androidx.media3.common.util.UnstableApi
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
import org.soulbeet.core.FfiPersistedPlaybackSession

@OptIn(UnstableApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class AutoBrowseTreeTest {

    private val authority = "org.soulbeet.app.art"

    @Test
    fun rootTabsExposeExactFourCarValidatorTabsWithStyleHints() = runTest {
        val core = FakeCoreClient()
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

        assertEquals(items, AutoBrowseTree.paginate(items, page = 0, pageSize = Int.MAX_VALUE))
        assertEquals(emptyList<Int>(), AutoBrowseTree.paginate(items, page = 1, pageSize = Int.MAX_VALUE))

        assertEquals(emptyList<Int>(), AutoBrowseTree.paginate(items, page = -1, pageSize = 5))
        assertEquals(emptyList<Int>(), AutoBrowseTree.paginate(items, page = 0, pageSize = 0))
    }

    @Test
    fun trackLeafEncodesParentContextMapsArtworkUriAndOmitsWhenNull() = runTest {
        val core = FakeCoreClient()
        core.tracksById["s1"] = FakeCoreClient.sampleTrack(
            id = "s1",
            title = "Instant Crush",
            artist = "Daft Punk",
            album = "RAM",
            coverArtId = "cov-123",
            starred = true,
            userRating = 5L,
            isDiscovery = false,
        )
        core.tracksById["s2"] = FakeCoreClient.sampleTrack(
            id = "s2",
            title = "Fresh Find",
            artist = "New Artist",
            album = "EP",
            coverArtId = null,
            starred = false,
            userRating = 0L,
            isDiscovery = true,
        )
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
        assertEquals("ctx:playlist:p1|track:s1", item1.mediaId)
        assertEquals(ContextualTrackRef("playlist:p1", "s1"), AutoBrowseTree.parseContextualTrackId(item1.mediaId))
        assertEquals("s1", AutoBrowseTree.extractTrackId(item1.mediaId))
        assertEquals(MediaMetadata.MEDIA_TYPE_MUSIC, item1.mediaMetadata.mediaType)
        assertEquals(
            "content://$authority/art/cov-123?fallback=track&seed=Instant%20Crush",
            item1.mediaMetadata.artworkUri?.toString(),
        )
        assertEquals("RAM", item1.mediaMetadata.albumTitle?.toString())
        val extras1 = item1.mediaMetadata.extras
        assertNotNull(extras1)
        assertFalse(extras1!!.getBoolean(AutoBrowseTree.EXTRA_IS_DISCOVERY))
        assertTrue(extras1.getBoolean(AutoBrowseTree.EXTRA_IS_STARRED))
        assertTrue(extras1.getBoolean(AutoBrowseTree.EXTRA_IS_OFFLINE_CACHED))
        assertEquals(
            AutoBrowseTree.EXTRAS_VALUE_DOWNLOAD_STATUS_DOWNLOADED,
            extras1.getLong(AutoBrowseTree.EXTRAS_KEY_DOWNLOAD_STATUS),
        )
        assertEquals(5, extras1.getInt(AutoBrowseTree.EXTRA_RATING))
        assertEquals("s1", extras1.getString(AutoBrowseTree.EXTRA_TRACK_ID))

        val item2 = items[1]
        assertNull("artworkUri must be omitted for track leaves when coverArtId is null", item2.mediaMetadata.artworkUri)
        val extras2 = item2.mediaMetadata.extras!!
        assertTrue(extras2.getBoolean(AutoBrowseTree.EXTRA_IS_DISCOVERY))
        assertFalse(extras2.getBoolean(AutoBrowseTree.EXTRA_IS_STARRED))
        assertFalse(extras2.getBoolean(AutoBrowseTree.EXTRA_IS_OFFLINE_CACHED))
        assertEquals(0, extras2.getInt(AutoBrowseTree.EXTRA_RATING))
        assertEquals("s2", extras2.getString(AutoBrowseTree.EXTRA_TRACK_ID))
    }

    @Test
    fun mapsGroupTitlesSleevesContinueListeningListHintAndTabIcons() = runTest {
        val core = FakeCoreClient()
        core.childrenByParent[AutoBrowseTree.TAB_FOR_YOU] = listOf(
            FakeCoreClient.browseNode(
                id = AutoBrowseTree.FOR_YOU_DRIVE_MIX,
                title = "Drive Mix",
                subtitle = "Offline-first · 24 tracks",
                kind = FfiAutoNodeKind.PLAYLIST,
                playable = true,
                browsable = true,
                isOfflineReady = true,
                groupTitle = "Right Now",
                sleeveSlug = "drive",
            ),
            FakeCoreClient.browseNode(
                id = "album:al-1",
                title = "Cross",
                subtitle = "Justice · 12 tracks",
                kind = FfiAutoNodeKind.ALBUM,
                playable = true,
                browsable = true,
                coverArtId = "cov-cross",
                groupTitle = AutoBrowseTree.GROUP_CONTINUE_LISTENING,
            ),
            FakeCoreClient.browseNode(
                id = "playlist:user-no-art",
                title = "No Cover List",
                kind = FfiAutoNodeKind.PLAYLIST,
                playable = true,
                browsable = true,
                coverArtId = null,
            ),
        )

        val tabs = AutoBrowseTree.loadChildren(
            core = core,
            parentId = AutoBrowseTree.ROOT_ID,
            page = 0,
            pageSize = 10,
            artAuthority = authority,
            packageName = "org.soulbeet.app",
        )
        tabs.forEach { tab ->
            val uri = tab.mediaMetadata.artworkUri
            assertTrue(
                "Tab ${tab.mediaId} should have named android.resource://<pkg>/drawable/<name> URI (got $uri)",
                uri?.scheme == "android.resource" &&
                    uri.pathSegments.size == 2 &&
                    uri.pathSegments[0] == "drawable" &&
                    uri.pathSegments[1].startsWith("ic_auto_tab_"),
            )
        }

        val items = AutoBrowseTree.loadChildren(
            core = core,
            parentId = AutoBrowseTree.TAB_FOR_YOU,
            page = 0,
            pageSize = 20,
            artAuthority = authority,
            packageName = "org.soulbeet.app",
        )
        assertEquals(3, items.size)

        val drive = items[0]
        assertEquals("content://$authority/sleeve/drive", drive.mediaMetadata.artworkUri?.toString())
        assertEquals(
            "Right Now",
            drive.mediaMetadata.extras?.getString(AutoBrowseTree.EXTRAS_KEY_CONTENT_STYLE_GROUP_TITLE_HINT),
        )
        assertEquals(
            AutoBrowseTree.EXTRAS_VALUE_DOWNLOAD_STATUS_DOWNLOADED,
            drive.mediaMetadata.extras?.getLong(AutoBrowseTree.EXTRAS_KEY_DOWNLOAD_STATUS),
        )

        val cont = items[1]
        assertEquals(
            AutoBrowseTree.GROUP_CONTINUE_LISTENING,
            cont.mediaMetadata.extras?.getString(AutoBrowseTree.EXTRAS_KEY_CONTENT_STYLE_GROUP_TITLE_HINT),
        )
        assertEquals(
            MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM,
            cont.mediaMetadata.extras?.getInt(AutoBrowseTree.EXTRAS_KEY_CONTENT_STYLE_SINGLE_ITEM),
        )

        val fallbackList = items[2]
        assertEquals(
            "content://$authority/tile/playlist?seed=No%20Cover%20List",
            fallbackList.mediaMetadata.artworkUri?.toString(),
        )
    }

    @Test
    fun offlineOnlyFiltersOutUncachedTracksAndUncachedPlayableContainers() = runTest {
        val core = FakeCoreClient()
        core.childrenByParent[AutoBrowseTree.TAB_PLAYLISTS] = listOf(
            FakeCoreClient.browseNode(
                id = "playlist:offline-pl",
                title = "Offline Mix",
                kind = FfiAutoNodeKind.PLAYLIST,
                playable = true,
                browsable = true,
                isOfflineReady = true,
            ),
            FakeCoreClient.browseNode(
                id = "playlist:remote-pl",
                title = "Remote Only Mix",
                kind = FfiAutoNodeKind.PLAYLIST,
                playable = true,
                browsable = true,
                isOfflineReady = false,
            ),
            FakeCoreClient.browseNode(
                id = "az:a-d",
                title = "A–D",
                kind = FfiAutoNodeKind.FOLDER,
                playable = false,
                browsable = true,
                isOfflineReady = false,
            ),
        )

        val items = AutoBrowseTree.loadChildren(
            core = core,
            parentId = AutoBrowseTree.TAB_PLAYLISTS,
            page = 0,
            pageSize = 25,
            artAuthority = authority,
            offlineOnly = true,
        )

        assertEquals(listOf("playlist:offline-pl", "az:a-d"), items.map { it.mediaId })
    }

    @Test
    fun recentRootReturnsSingleLastPlayedTrackOrEmptyWhenNoSession() = runTest {
        val core = FakeCoreClient()

        // No persisted session yet
        val empty = AutoBrowseTree.loadChildren(
            core = core,
            parentId = AutoBrowseTree.RECENT_ROOT_ID,
            page = 0,
            pageSize = 10,
            artAuthority = authority,
        )
        assertTrue(empty.isEmpty())

        // With persisted session
        core.tracksById["t1"] = FakeCoreClient.sampleTrack("t1", title = "First")
        core.tracksById["t2"] = FakeCoreClient.sampleTrack("t2", title = "Resumed Track")
        core.persistedSession = FfiPersistedPlaybackSession(
            queueTrackIds = listOf("t1", "t2"),
            currentIndex = 1u,
            positionMs = 15_000uL,
            isPlaying = false,
            sourceLabel = "Drive Mix",
            updatedAt = 1_700_000_000L,
        )

        val recent = AutoBrowseTree.loadChildren(
            core = core,
            parentId = AutoBrowseTree.RECENT_ROOT_ID,
            page = 0,
            pageSize = 10,
            artAuthority = authority,
        )
        assertEquals(1, recent.size)
        assertEquals("t2", recent[0].mediaId)
        assertEquals("Resumed Track", recent[0].mediaMetadata.title?.toString())
    }

    @Test
    fun artworkUriIncludesCachedQueryParamOnlyWhenCoverArtIsCachedLocally() = runTest {
        val core = FakeCoreClient()
        core.coverArtPaths["cov-cached"] = "/cache/cover_art/cov-cached.jpg"
        core.childrenByParent[AutoBrowseTree.LIB_RECENT] = listOf(
            FakeCoreClient.browseNode(
                id = "album:al-cached",
                title = "Discovery",
                subtitle = "Daft Punk",
                kind = FfiAutoNodeKind.ALBUM,
                playable = true,
                browsable = true,
                coverArtId = "cov-cached",
            ),
            FakeCoreClient.browseNode(
                id = "album:al-cold",
                title = "Homework",
                subtitle = "Daft Punk",
                kind = FfiAutoNodeKind.ALBUM,
                playable = true,
                browsable = true,
                coverArtId = "cov-cold",
            ),
        )

        val initial = AutoBrowseTree.loadChildren(
            core = core,
            parentId = AutoBrowseTree.LIB_RECENT,
            page = 0,
            pageSize = 20,
            artAuthority = authority,
        )
        assertEquals(2, initial.size)
        assertEquals(
            "loadChildren must reuse bulk-loaded node.coverArtLocalPath without N+1 FFI calls",
            0,
            core.coverArtLocalPathSyncCalls,
        )

        val cachedUri = initial[0].mediaMetadata.artworkUri
        assertNotNull(cachedUri)
        assertEquals("1", cachedUri!!.getQueryParameter(CoverArtContentProvider.QUERY_CACHED))

        val coldUriBefore = initial[1].mediaMetadata.artworkUri
        assertNotNull(coldUriBefore)
        assertNull(coldUriBefore!!.getQueryParameter(CoverArtContentProvider.QUERY_CACHED))

        // Simulate background prefetch caching cov-cold and notifyChildrenChanged() reloading the grid.
        core.coverArtPaths["cov-cold"] = "/cache/cover_art/cov-cold.jpg"
        val refreshed = AutoBrowseTree.loadChildren(
            core = core,
            parentId = AutoBrowseTree.LIB_RECENT,
            page = 0,
            pageSize = 20,
            artAuthority = authority,
        )
        val coldUriAfter = refreshed[1].mediaMetadata.artworkUri
        assertNotNull(coldUriAfter)
        assertEquals("1", coldUriAfter!!.getQueryParameter(CoverArtContentProvider.QUERY_CACHED))
        assertFalse(
            "Cached URI must differ from cold URI to bust Android Auto's bitmap cache",
            coldUriBefore.toString() == coldUriAfter.toString(),
        )
    }
}
