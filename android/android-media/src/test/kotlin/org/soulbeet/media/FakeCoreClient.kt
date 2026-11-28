package org.soulbeet.media

import org.soulbeet.core.CoreEventListener
import org.soulbeet.core.FfiAlbum
import org.soulbeet.core.FfiArtist
import org.soulbeet.core.FfiAudioQualityProfile
import org.soulbeet.core.FfiAutoBrowseNode
import org.soulbeet.core.FfiAutoNodeKind
import org.soulbeet.core.FfiCachePolicy
import org.soulbeet.core.FfiCacheStats
import org.soulbeet.core.FfiClientBootstrap
import org.soulbeet.core.FfiConnectCommand
import org.soulbeet.core.FfiConnectState
import org.soulbeet.core.FfiDiscoveryTrack
import org.soulbeet.core.FfiDownloadOutcome
import org.soulbeet.core.FfiDrainReport
import org.soulbeet.core.FfiException
import org.soulbeet.core.FfiLibraryStats
import org.soulbeet.core.FfiNavidromeCredentials
import org.soulbeet.core.FfiOnlineAlbumMatch
import org.soulbeet.core.FfiOutboxMutation
import org.soulbeet.core.FfiOutboxRow
import org.soulbeet.core.FfiOutboxSummary
import org.soulbeet.core.FfiPersistedPlaybackSession
import org.soulbeet.core.FfiPinState
import org.soulbeet.core.FfiPinTarget
import org.soulbeet.core.FfiPlaybackSnapshot
import org.soulbeet.core.FfiPlaybackSource
import org.soulbeet.core.FfiPlaylist
import org.soulbeet.core.FfiRemoteAlbumDetail
import org.soulbeet.core.FfiRemoteArtistDetail
import org.soulbeet.core.FfiRemoteCatalogResults
import org.soulbeet.core.FfiSearchResults
import org.soulbeet.core.FfiSoulbeetAuth
import org.soulbeet.core.FfiSyncReport
import org.soulbeet.core.FfiTrack
import org.soulbeet.core.FfiVoiceMatchResult
import org.soulbeet.corebindings.CoreClient

/**
 * In-memory [CoreClient] test double for JVM unit tests (loads zero native `.so` libraries).
 */
open class FakeCoreClient : CoreClient {
    var offlineOnlyFlag: Boolean = false
    var rootTabs: List<FfiAutoBrowseNode> = defaultFourRootTabs()
    val childrenByParent: MutableMap<String, List<FfiAutoBrowseNode>> = mutableMapOf()
    val tracksById: MutableMap<String, FfiTrack> = mutableMapOf()
    val albumsById: MutableMap<String, FfiAlbum> = mutableMapOf()
    val artistsById: MutableMap<String, FfiArtist> = mutableMapOf()
    val playlists: MutableList<FfiPlaylist> = mutableListOf()
    val offlineTracks: MutableList<FfiDownloadOutcome> = mutableListOf()
    val discoveryTracks: MutableList<FfiDiscoveryTrack> = mutableListOf()
    val coverArtPaths: MutableMap<String, String> = mutableMapOf()
    val enqueuedMutations: MutableList<FfiOutboxMutation> = mutableListOf()
    var persistedSession: FfiPersistedPlaybackSession? = null
    var returnTracksOutOfOrder: Boolean = false
    var enqueueMutationException: Exception? = null
    var onEnqueueMutationCallback: ((FfiOutboxMutation) -> Unit)? = null
    var voiceSearchHandler: (String) -> FfiVoiceMatchResult? = { null }
    var getOrFetchCoverArtHandler: (suspend (String, UInt?) -> String)? = null
    var coverArtLocalPathSyncCalls: Int = 0
    var playbackSourceHandler: (String, FfiAudioQualityProfile) -> FfiPlaybackSource = { _, _ ->
        throw FfiException.Offline("not cached")
    }

    override fun autoRootTabs(): List<FfiAutoBrowseNode> = rootTabs

    override suspend fun autoChildren(parentId: String): List<FfiAutoBrowseNode> =
        hydrateNodeCoverArtPaths(childrenByParent[parentId].orEmpty())

    override suspend fun autoChildrenWithOffline(
        parentId: String,
        offlineOnly: Boolean,
    ): List<FfiAutoBrowseNode> = hydrateNodeCoverArtPaths(childrenByParent[parentId].orEmpty())

    private fun hydrateNodeCoverArtPaths(nodes: List<FfiAutoBrowseNode>): List<FfiAutoBrowseNode> =
        nodes.map { node ->
            if (node.coverArtLocalPath == null && node.coverArtId != null) {
                val localPath = coverArtPaths[node.coverArtId]
                if (localPath != null) node.copy(coverArtLocalPath = localPath) else node
            } else {
                node
            }
        }

    override fun isOfflineOnly(): Boolean = offlineOnlyFlag

    override fun setOfflineOnly(offlineOnly: Boolean) {
        offlineOnlyFlag = offlineOnly
    }

    override fun coverArtLocalPathSync(coverArtId: String): String? {
        coverArtLocalPathSyncCalls++
        return coverArtPaths[coverArtId]
    }

    override suspend fun coverArtLocalPath(coverArtId: String): String? = coverArtPaths[coverArtId]

    override suspend fun getTrack(trackId: String): FfiTrack? = tracksById[trackId]

    override suspend fun getTracksByIds(ids: List<String>): List<FfiTrack> {
        val found = ids.mapNotNull { tracksById[it] }
        return if (returnTracksOutOfOrder) found.reversed() else found
    }

    override suspend fun getAlbum(albumId: String): FfiAlbum? = albumsById[albumId]

    override suspend fun getArtist(artistId: String): FfiArtist? = artistsById[artistId]

    override suspend fun listPlaylists(): List<FfiPlaylist> = playlists.toList()

    override suspend fun listOfflineTracks(): List<FfiDownloadOutcome> = offlineTracks.toList()

    override suspend fun listDiscoveryTriage(pendingOnly: Boolean): List<FfiDiscoveryTrack> =
        if (pendingOnly) discoveryTracks.filter { it.status == "pending" } else discoveryTracks.toList()

    override suspend fun enqueueMutation(mutation: FfiOutboxMutation): Long {
        onEnqueueMutationCallback?.invoke(mutation)
        enqueueMutationException?.let { throw it }
        enqueuedMutations.add(mutation)
        return enqueuedMutations.size.toLong()
    }

    override suspend fun enqueueMutationWithKey(
        mutation: FfiOutboxMutation,
        idempotencyKey: String,
    ): Long = enqueueMutation(mutation)

    override suspend fun savePlaybackSession(session: FfiPersistedPlaybackSession) {
        persistedSession = session
    }

    override suspend fun loadPlaybackSession(): FfiPersistedPlaybackSession? = persistedSession

    override suspend fun voiceSearch(query: String): FfiVoiceMatchResult? = voiceSearchHandler(query)

    override suspend fun resolvePlaybackSource(
        trackId: String,
        profile: FfiAudioQualityProfile,
    ): FfiPlaybackSource = playbackSourceHandler(trackId, profile)

    override fun close() = Unit

    // --- Unused interface stubs ---
    override suspend fun cachePolicy(): FfiCachePolicy = throw UnsupportedOperationException()
    override suspend fun cacheStats(): FfiCacheStats = throw UnsupportedOperationException()
    override fun clearEventListener() = Unit
    override suspend fun clearNavidromeCredentials() = Unit
    override suspend fun connectClaimActive() = Unit
    override suspend fun connectSendCommand(targetDeviceId: String, command: FfiConnectCommand) = Unit
    override suspend fun connectState(): FfiConnectState = throw UnsupportedOperationException()
    override suspend fun connectTransferPlaybackTo(targetDeviceId: String, snapshot: FfiPlaybackSnapshot) = Unit
    override suspend fun connectUpdateState(snapshot: FfiPlaybackSnapshot) = Unit
    override suspend fun connectWs(heartbeatIntervalSecs: ULong) = Unit
    override suspend fun deadLetterOutbox(): List<FfiOutboxRow> = emptyList()
    override suspend fun disconnectWs() = Unit
    override suspend fun downloadTrack(
        trackId: String,
        profile: FfiAudioQualityProfile,
        pinned: Boolean,
        pinSource: String?,
    ): FfiDownloadOutcome = throw UnsupportedOperationException()
    override suspend fun drainOutbox(): FfiDrainReport = FfiDrainReport(0uL, 0uL, 0uL)
    override suspend fun enforceCacheBudget(): ULong = 0uL
    override suspend fun fetchBootstrap(): FfiClientBootstrap? = null
    override suspend fun getOrFetchCoverArt(coverArtId: String, size: UInt?): String =
        getOrFetchCoverArtHandler?.invoke(coverArtId, size) ?: coverArtPaths[coverArtId] ?: ""
    override suspend fun warmCoverArtCache(maxItems: UInt, size: UInt?): UInt = 0u
    override suspend fun libraryStats(): FfiLibraryStats = throw UnsupportedOperationException()
    override suspend fun listAlbums(artistId: String?): List<FfiAlbum> = albumsById.values.toList()
    override suspend fun listArtists(): List<FfiArtist> = artistsById.values.toList()
    override suspend fun listPinnedTargets(): List<FfiPinState> = emptyList()
    override suspend fun listStarredTracks(): List<FfiTrack> = tracksById.values.filter { it.starred }
    override suspend fun listTracksByAlbum(albumId: String): List<FfiTrack> =
        tracksById.values.filter { it.albumId == albumId }
    override suspend fun listTracksByPlaylist(playlistId: String): List<FfiTrack> = emptyList()
    override suspend fun outboxSummary(): FfiOutboxSummary = FfiOutboxSummary(0uL, 0uL, null, null)
    override suspend fun pendingOutbox(): List<FfiOutboxRow> = emptyList()
    override suspend fun pin(target: FfiPinTarget): List<String> = emptyList()
    override suspend fun prefetchQueue(
        queueTrackIds: List<String>,
        currentIndex: UInt,
        profile: FfiAudioQualityProfile,
        isWifi: Boolean,
    ): List<String> = emptyList()
    override suspend fun retryDeadLetters(): ULong = 0uL
    override suspend fun runAutoCacheRules(
        profile: FfiAudioQualityProfile,
        isWifi: Boolean,
    ): List<String> = emptyList()
    override suspend fun search(query: String, limit: UInt): FfiSearchResults =
        throw UnsupportedOperationException()
    override suspend fun searchLocal(query: String, limit: UInt): FfiSearchResults =
        throw UnsupportedOperationException()
    override suspend fun searchRemote(
        query: String,
        provider: String?,
        limit: UInt,
    ): FfiRemoteCatalogResults = throw UnsupportedOperationException()
    override suspend fun getRemoteAlbum(albumId: String): FfiRemoteAlbumDetail =
        throw UnsupportedOperationException()
    override suspend fun getRemoteArtist(
        artistId: String,
        name: String?,
    ): FfiRemoteArtistDetail? = null
    override suspend fun downloadRemoteAlbum(album: FfiOnlineAlbumMatch): String =
        throw UnsupportedOperationException()
    override suspend fun setCachePolicy(policy: FfiCachePolicy) = Unit
    override fun setEventListener(listener: CoreEventListener) = Unit
    override suspend fun setNavidromeCredentials(creds: FfiNavidromeCredentials) = Unit
    override suspend fun setSoulbeetAuth(auth: FfiSoulbeetAuth) = Unit
    override suspend fun shutdown() = Unit
    override suspend fun syncNow(forceFull: Boolean): FfiSyncReport =
        throw UnsupportedOperationException()
    override suspend fun unpin(target: FfiPinTarget): ULong = 0uL

    companion object {
        fun defaultFourRootTabs(): List<FfiAutoBrowseNode> = listOf(
            browseNode(AutoBrowseTree.TAB_FOR_YOU, "For You", FfiAutoNodeKind.TAB, playable = false, browsable = true),
            browseNode(AutoBrowseTree.TAB_PLAYLISTS, "Playlists", FfiAutoNodeKind.TAB, playable = false, browsable = true),
            browseNode(AutoBrowseTree.TAB_LIBRARY, "Library", FfiAutoNodeKind.TAB, playable = false, browsable = true),
            browseNode(AutoBrowseTree.TAB_RADIO, "Radio", FfiAutoNodeKind.TAB, playable = false, browsable = true),
        )

        fun browseNode(
            id: String,
            title: String,
            kind: FfiAutoNodeKind,
            subtitle: String? = null,
            playable: Boolean = kind == FfiAutoNodeKind.TRACK,
            browsable: Boolean = kind != FfiAutoNodeKind.TRACK && kind != FfiAutoNodeKind.RADIO_SEED,
            coverArtId: String? = null,
            isOfflineReady: Boolean = false,
            isPendingDiscovery: Boolean = false,
            groupTitle: String? = null,
            sleeveSlug: String? = null,
        ): FfiAutoBrowseNode = FfiAutoBrowseNode(
            id = id,
            title = title,
            subtitle = subtitle,
            kind = kind,
            playable = playable,
            browsable = browsable,
            coverArtId = coverArtId,
            coverArtLocalPath = null,
            isOfflineReady = isOfflineReady,
            isPendingDiscovery = isPendingDiscovery,
            groupTitle = groupTitle,
            sleeveSlug = sleeveSlug,
        )

        fun sampleTrack(
            id: String,
            title: String = "Track $id",
            artist: String = "Artist $id",
            album: String = "Album $id",
            durationSecs: Long = 200L,
            coverArtId: String? = "art-$id",
            starred: Boolean = false,
            userRating: Long = 0L,
            isDiscovery: Boolean = false,
        ): FfiTrack = FfiTrack(
            id = id,
            title = title,
            artistId = "ar-$id",
            artistName = artist,
            albumId = "al-$id",
            albumName = album,
            trackNumber = 1L,
            discNumber = 1L,
            durationSecs = durationSecs,
            year = 2024L,
            genre = "Electronic",
            bitRate = 320L,
            suffix = "flac",
            contentType = "audio/flac",
            sizeBytes = 10_000_000L,
            coverArtId = coverArtId,
            path = "/music/$id.flac",
            starred = starred,
            userRating = userRating,
            playCount = 0L,
            lastPlayedAt = null,
            musicbrainzId = null,
            isDiscovery = isDiscovery,
            updatedAt = 1_700_000_000L,
        )
    }
}
