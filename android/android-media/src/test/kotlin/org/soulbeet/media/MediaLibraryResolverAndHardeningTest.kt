package org.soulbeet.media

import androidx.annotation.OptIn
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DataSpec
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.datasource.HttpDataSource
import kotlinx.coroutines.async
import kotlinx.coroutines.delay
import kotlinx.coroutines.test.runTest
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.soulbeet.core.FfiAlbum
import org.soulbeet.core.FfiArtist
import org.soulbeet.core.FfiAudioQualityProfile
import org.soulbeet.core.FfiAutoNodeKind
import org.soulbeet.core.FfiException
import org.soulbeet.core.FfiHttpHeader
import org.soulbeet.core.FfiPersistedPlaybackSession
import org.soulbeet.core.FfiPlaybackSource
import org.soulbeet.core.FfiPlaylist
import org.soulbeet.core.FfiVoiceMatchKind
import org.soulbeet.core.FfiVoiceMatchResult
import org.soulbeet.corebindings.SoulbeetCoreProvider

@OptIn(UnstableApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class MediaLibraryResolverAndHardeningTest {

    private val authority = "org.soulbeet.app.art"

    @After
    fun tearDown() {
        SoulbeetCoreProvider.clear()
    }

    @Test
    fun googleAssistantVoiceSearchQueryHydratesMatchedTracks() = runTest {
        val core = FakeCoreClient()
        core.tracksById["n1"] = FakeCoreClient.sampleTrack("n1", title = "Petit Papa Noël")
        core.tracksById["n2"] = FakeCoreClient.sampleTrack("n2", title = "Vive le Vent")
        core.voiceSearchHandler = { query ->
            if (query.contains("Noël", ignoreCase = true)) {
                FfiVoiceMatchResult(
                    kind = FfiVoiceMatchKind.PLAYLIST,
                    targetId = "pl-noel",
                    label = "Noël à la Française",
                    trackIds = listOf("n1", "n2"),
                    elapsedMs = 4uL,
                )
            } else {
                null
            }
        }

        val voiceRequestItem = MediaItem.Builder()
            .setRequestMetadata(
                MediaItem.RequestMetadata.Builder()
                    .setSearchQuery("Play Noël mix")
                    .build(),
            )
            .build()

        val result = MediaLibraryResolver.resolveSetMediaItems(
            core = core,
            requestedItems = listOf(voiceRequestItem),
            startIndex = C.INDEX_UNSET,
            startPositionMs = C.TIME_UNSET,
            artAuthority = authority,
        )

        assertEquals(listOf("n1", "n2"), result.items.map { it.mediaId })
        assertEquals(0, result.startIndex)
        assertEquals(
            "Noël à la Française",
            result.items[0].mediaMetadata.extras?.getString(AutoBrowseTree.EXTRA_SOURCE_LABEL),
        )
    }

    @Test
    fun genericPlayMusicRestoresPersistedSessionOrFallsBackToForYou() = runTest {
        val core = FakeCoreClient()
        core.tracksById["s1"] = FakeCoreClient.sampleTrack("s1")
        core.tracksById["s2"] = FakeCoreClient.sampleTrack("s2")
        core.persistedSession = FfiPersistedPlaybackSession(
            queueTrackIds = listOf("s1", "s2"),
            currentIndex = 1u,
            positionMs = 22_000uL,
            isPlaying = false,
            sourceLabel = "Drive Mix",
            updatedAt = 1_700_000_000L,
        )

        val emptyPlayRequest = MediaItem.Builder().build()
        val resumed = MediaLibraryResolver.resolveSetMediaItems(
            core = core,
            requestedItems = listOf(emptyPlayRequest),
            startIndex = C.INDEX_UNSET,
            startPositionMs = C.TIME_UNSET,
            artAuthority = authority,
        )

        assertEquals(listOf("s1", "s2"), resumed.items.map { it.mediaId })
        assertEquals(1, resumed.startIndex)
        assertEquals(22_000L, resumed.startPositionMs)

        // Without persisted session, falls back to first playable For You container
        core.persistedSession = null
        core.childrenByParent[AutoBrowseTree.TAB_FOR_YOU] = listOf(
            FakeCoreClient.browseNode("playlist:radar", "Weekly Radar", FfiAutoNodeKind.PLAYLIST, playable = true),
        )
        core.childrenByParent["playlist:radar"] = listOf(
            FakeCoreClient.browseNode("track:s1", "Track s1", FfiAutoNodeKind.TRACK),
            FakeCoreClient.browseNode("track:s2", "Track s2", FfiAutoNodeKind.TRACK),
        )

        val fallback = MediaLibraryResolver.resolveSetMediaItems(
            core = core,
            requestedItems = listOf(emptyPlayRequest),
            startIndex = C.INDEX_UNSET,
            startPositionMs = C.TIME_UNSET,
            artAuthority = authority,
        )
        assertEquals(listOf("s1", "s2"), fallback.items.map { it.mediaId })
        assertEquals(0, fallback.startIndex)
    }

    @Test
    fun tappingContextualTrackExpandsSiblingQueueAtTappedIndexAndResolvesSourceLabel() = runTest {
        val core = FakeCoreClient()
        core.playlists.add(
            FfiPlaylist(
                id = "p1",
                name = "Night Drive",
                comment = null,
                owner = "doc",
                songCount = 3L,
                durationSecs = 600L,
                coverArtId = null,
                isSmart = false,
                kind = "user",
                slug = null,
                changedAt = null,
                updatedAt = 1_700_000_000L,
            ),
        )
        core.tracksById["t1"] = FakeCoreClient.sampleTrack("t1")
        core.tracksById["t2"] = FakeCoreClient.sampleTrack("t2")
        core.tracksById["t3"] = FakeCoreClient.sampleTrack("t3")
        core.childrenByParent["playlist:p1"] = listOf(
            FakeCoreClient.browseNode("track:t1", "T1", FfiAutoNodeKind.TRACK),
            FakeCoreClient.browseNode("track:t2", "T2", FfiAutoNodeKind.TRACK),
            FakeCoreClient.browseNode("track:t3", "T3", FfiAutoNodeKind.TRACK),
        )

        // Simulate Android Auto sending ONLY mediaId (title metadata stripped over IPC)
        val tapped = MediaItem.Builder()
            .setMediaId(AutoBrowseTree.contextualTrackId("playlist:p1", "t2"))
            .build()

        val result = MediaLibraryResolver.resolveSetMediaItems(
            core = core,
            requestedItems = listOf(tapped),
            startIndex = C.INDEX_UNSET,
            startPositionMs = C.TIME_UNSET,
            artAuthority = authority,
        )

        assertEquals(listOf("t1", "t2", "t3"), result.items.map { it.mediaId })
        assertEquals(1, result.startIndex)
        assertEquals(
            "Night Drive",
            result.items[1].mediaMetadata.extras?.getString(AutoBrowseTree.EXTRA_SOURCE_LABEL),
        )
    }

    @Test
    fun resolveItemByIdSupportsContainersAndTracks() = runTest {
        val core = FakeCoreClient()
        core.playlists.add(
            FfiPlaylist("p1", "Starred Mix", null, "doc", 10L, 1800L, "art-pl", false, "user", null, null, 1L),
        )
        core.albumsById["al1"] = FfiAlbum(
            "al1", "Discovery", "ar1", "Daft Punk", 2001L, "Electronic", 14L, 3600L, "art-al", true, null, 1L,
        )
        core.artistsById["ar1"] = FfiArtist("ar1", "Daft Punk", 4L, "art-ar", true, null, 1L)
        core.tracksById["s1"] = FakeCoreClient.sampleTrack("s1", title = "One More Time")

        val plItem = MediaLibraryResolver.resolveItemById(core, "playlist:p1", authority)
        assertNotNull(plItem)
        assertEquals(MediaMetadata.MEDIA_TYPE_PLAYLIST, plItem!!.mediaMetadata.mediaType)
        assertEquals("Starred Mix", plItem.mediaMetadata.title?.toString())

        val alItem = MediaLibraryResolver.resolveItemById(core, "album:al1", authority)
        assertNotNull(alItem)
        assertEquals(MediaMetadata.MEDIA_TYPE_ALBUM, alItem!!.mediaMetadata.mediaType)
        assertEquals("Discovery", alItem.mediaMetadata.title?.toString())

        val radioItem = MediaLibraryResolver.resolveItemById(core, "radio:artist:ar1", authority)
        assertNotNull(radioItem)
        assertEquals(MediaMetadata.MEDIA_TYPE_RADIO_STATION, radioItem!!.mediaMetadata.mediaType)
        assertEquals("Daft Punk Radio", radioItem.mediaMetadata.title?.toString())

        val trackItem = MediaLibraryResolver.resolveItemById(core, "ctx:playlist:p1|track:s1", authority)
        assertNotNull(trackItem)
        assertEquals("One More Time", trackItem!!.mediaMetadata.title?.toString())
    }

    @Test
    fun coreProviderColdStartInitializerCoalescesAndUnconfiguredReturnsImmediately() = runTest {
        SoulbeetCoreProvider.clear()
        val fake = FakeCoreClient()
        var initCount = 0
        SoulbeetCoreProvider.registerInitializer {
            initCount++
            delay(20L)
            fake
        }

        val first = async { SoulbeetCoreProvider.awaitOrNull(1_000L) }
        val second = async { SoulbeetCoreProvider.awaitOrNull(1_000L) }
        assertSame(fake, first.await())
        assertSame(fake, second.await())
        assertEquals(1, initCount)

        SoulbeetCoreProvider.clear()
        SoulbeetCoreProvider.markUnconfigured()
        assertTrue(SoulbeetCoreProvider.isExplicitlyUnconfigured())
        assertNull(SoulbeetCoreProvider.awaitOrNull(1_000L))
    }

    @Test
    fun offlineSyncWorkerRetriesOnlyTransientHttpStatuses() {
        assertTrue(OfflineSyncWorker.isRetryableError(FfiException.Http("timeout")))
        assertTrue(OfflineSyncWorker.isRetryableError(FfiException.Io("reset")))
        assertTrue(OfflineSyncWorker.isRetryableError(FfiException.WebSocket("closed")))
        assertTrue(OfflineSyncWorker.isRetryableError(FfiException.ServerHttp(408u, "timeout")))
        assertTrue(OfflineSyncWorker.isRetryableError(FfiException.ServerHttp(429u, "rate limited")))
        assertTrue(OfflineSyncWorker.isRetryableError(FfiException.ServerHttp(500u, "internal")))
        assertTrue(OfflineSyncWorker.isRetryableError(FfiException.ServerHttp(503u, "unavailable")))

        assertFalse(OfflineSyncWorker.isRetryableError(FfiException.ServerHttp(400u, "bad request")))
        assertFalse(OfflineSyncWorker.isRetryableError(FfiException.ServerHttp(401u, "unauthorized")))
        assertFalse(OfflineSyncWorker.isRetryableError(FfiException.ServerHttp(403u, "forbidden")))
        assertFalse(OfflineSyncWorker.isRetryableError(FfiException.ServerHttp(404u, "not found")))
        assertFalse(OfflineSyncWorker.isRetryableError(FfiException.Offline("offline")))
        assertFalse(OfflineSyncWorker.isRetryableError(FfiException.InvalidInput("bad")))
    }

    @Test(expected = OfflineTrackUnavailableException::class)
    fun dataSourceThrowsDedicatedOfflineExceptionOnFfiOffline() {
        val core = FakeCoreClient().apply {
            playbackSourceHandler = { _, _ -> throw FfiException.Offline("track not cached") }
        }
        val factory = SoulbeetDataSourceFactory(coreProvider = { core })
        val ds = factory.createDataSource()
        ds.open(DataSpec(SoulbeetDataSourceFactory.trackUri("uncached-1")))
    }

    @Test
    fun dataSourceUsesAuthenticatedHttpFactoryWhenHeadersPresent() {
        val core = FakeCoreClient().apply {
            playbackSourceHandler = { id, _ ->
                if (id == "with-auth") {
                    FfiPlaybackSource.StreamUrl(
                        url = "https://music.example.com/stream/1",
                        format = "opus",
                        bitRate = 160u,
                        headers = listOf(FfiHttpHeader("Authorization", "Bearer secret")),
                    )
                } else {
                    FfiPlaybackSource.StreamUrl(
                        url = "https://music.example.com/stream/2",
                        format = "opus",
                        bitRate = 160u,
                        headers = emptyList(),
                    )
                }
            }
        }
        var plainFactoryUsed = 0
        var authFactoryUsed = 0
        val plainDelegate = DefaultHttpDataSource.Factory().setAllowCrossProtocolRedirects(true)
        val authDelegate = DefaultHttpDataSource.Factory().setAllowCrossProtocolRedirects(false)
        val plainFactory = object : HttpDataSource.Factory {
            override fun createDataSource(): HttpDataSource {
                plainFactoryUsed++
                return plainDelegate.createDataSource()
            }
            override fun setDefaultRequestProperties(defaultRequestProperties: MutableMap<String, String>): HttpDataSource.Factory = this
        }
        val authFactory = object : HttpDataSource.Factory {
            override fun createDataSource(): HttpDataSource {
                authFactoryUsed++
                return authDelegate.createDataSource()
            }
            override fun setDefaultRequestProperties(defaultRequestProperties: MutableMap<String, String>): HttpDataSource.Factory = this
        }

        val ds = SoulbeetDataSource(
            coreProvider = { core },
            qualityProfileProvider = { FfiAudioQualityProfile.CellularOpus160 },
            httpDataSourceFactory = plainFactory,
            authenticatedHttpDataSourceFactory = authFactory,
        )

        ds.resolveSoulbeetSource(DataSpec(SoulbeetDataSourceFactory.trackUri("with-auth")))
        assertEquals(1, authFactoryUsed)
        assertEquals(0, plainFactoryUsed)

        ds.resolveSoulbeetSource(DataSpec(SoulbeetDataSourceFactory.trackUri("public")))
        assertEquals(1, authFactoryUsed)
        assertEquals(1, plainFactoryUsed)
    }

    @Test
    fun boundedSearchCacheEnforcesLruCapAndOfflineKeySeparation() {
        val cache = BoundedSearchCache(maxEntries = 3)
        val itemA = listOf(MediaItem.Builder().setMediaId("a").build())
        val itemB = listOf(MediaItem.Builder().setMediaId("b").build())
        val itemC = listOf(MediaItem.Builder().setMediaId("c").build())
        val itemD = listOf(MediaItem.Builder().setMediaId("d").build())

        cache.put("daft", offlineOnly = false, items = itemA)
        cache.put("daft", offlineOnly = true, items = itemB)
        cache.put("air", offlineOnly = false, items = itemC)
        // Touch ("daft", false) so ("daft", true) becomes least-recently-used
        assertEquals(itemA, cache.get("daft", offlineOnly = false))

        cache.put("justice", offlineOnly = false, items = itemD)
        assertEquals(3, cache.size())
        assertNull("LRU entry must be evicted", cache.get("daft", offlineOnly = true))
        assertEquals(itemA, cache.get("daft", offlineOnly = false))
        assertEquals(itemD, cache.get("justice", offlineOnly = false))

        cache.clear()
        assertEquals(0, cache.size())
    }

    @Test
    fun repositoryClearActiveTrackPreservesUserPreferences() {
        val repo = SoulbeetMediaRepository()
        repo.setLibrarySecondaryAction(LibrarySecondaryAction.START_RADIO)
        repo.setOfflineOnly(true)
        repo.setAudioQualityProfile(FfiAudioQualityProfile.LosslessFlac)
        repo.updateActiveTrack(
            trackId = "t1",
            title = "Song",
            artist = "Artist",
            album = "Album",
            coverArtId = "cov1",
            durationMs = 200_000L,
            queueTrackIds = listOf("t1"),
            currentIndex = 0,
            isPendingDiscovery = true,
            isStarred = true,
            userRating = 5,
            isOfflineCached = true,
            sourceLabel = "Drive Mix",
        )

        repo.clearActiveTrack()

        val state = repo.playbackState.value
        assertNull(state.currentTrackId)
        assertEquals("", state.title)
        assertFalse(state.isPlaying)
        assertTrue(state.queueTrackIds.isEmpty())
        assertEquals(LibrarySecondaryAction.START_RADIO, state.librarySecondaryAction)
        assertTrue(state.offlineOnly)
        assertEquals(FfiAudioQualityProfile.LosslessFlac, state.audioQualityProfile)
    }

    @Test
    fun resolvesDriveMixStarredShuffleMoodAndGenreRadioContainers() = runTest {
        val core = FakeCoreClient()
        core.tracksById["t1"] = FakeCoreClient.sampleTrack("t1", title = "Nightcall", artist = "Kavinsky")
        core.tracksById["t2"] = FakeCoreClient.sampleTrack("t2", title = "Instant Crush", artist = "Daft Punk")
        val nodes = listOf(
            FakeCoreClient.browseNode("track:t1", "Nightcall", FfiAutoNodeKind.TRACK),
            FakeCoreClient.browseNode("track:t2", "Instant Crush", FfiAutoNodeKind.TRACK),
        )
        core.childrenByParent[AutoBrowseTree.FOR_YOU_DRIVE_MIX] = nodes
        core.childrenByParent[AutoBrowseTree.PLAYLIST_STARRED] = nodes
        core.childrenByParent[AutoBrowseTree.RADIO_SHUFFLE] = nodes
        core.childrenByParent["radio:mood:upbeat"] = nodes
        core.childrenByParent["radio:genre:Synthwave"] = nodes

        for ((containerId, expectedLabel) in listOf(
            AutoBrowseTree.FOR_YOU_DRIVE_MIX to "Drive Mix",
            AutoBrowseTree.PLAYLIST_STARRED to "Starred Favorites",
            AutoBrowseTree.RADIO_SHUFFLE to "Library Shuffle",
            "radio:mood:upbeat" to "Upbeat Radio",
            "radio:genre:Synthwave" to "Synthwave Radio",
        )) {
            val res = MediaLibraryResolver.resolveSetMediaItems(
                core = core,
                requestedItems = listOf(MediaItem.Builder().setMediaId(containerId).build()),
                startIndex = 0,
                startPositionMs = 0L,
                artAuthority = "org.soulbeet.app.art",
            )
            assertEquals("Expected 2 tracks for $containerId", 2, res.items.size)
            assertEquals(
                expectedLabel,
                res.items[0].mediaMetadata.extras?.getString(AutoBrowseTree.EXTRA_SOURCE_LABEL),
            )
        }
    }
}
