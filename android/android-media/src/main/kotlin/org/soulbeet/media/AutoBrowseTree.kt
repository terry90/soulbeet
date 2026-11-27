package org.soulbeet.media

import android.net.Uri
import android.os.Bundle
import androidx.annotation.OptIn
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.MediaMetadata
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.session.MediaConstants
import org.soulbeet.core.FfiAutoBrowseNode
import org.soulbeet.core.FfiAutoNodeKind
import org.soulbeet.core.FfiTrack
import org.soulbeet.corebindings.CoreClient

/**
 * Parsed contextual track reference (`ctx:<parentId>|track:<trackId>`) allowing Android Auto
 * track taps inside playlists, albums, offline downloads, or discovery triage to hydrate the
 * full sibling queue starting at [trackId].
 */
data class ContextualTrackRef(
    val parentId: String,
    val trackId: String,
)

/**
 * Pure mapper turning `lib/client-core`'s bounded Android Auto browse tree into
 * `CAR.VALIDATOR`-compliant Media3 [MediaItem] hierarchies.
 *
 * Enforces:
 * - Exactly 4 root tabs (`For You`, `Playlists`, `Library`, `Radio`).
 * - Appropriate Grid vs. List content-style hints per container.
 * - Local `content://<authority>/art/<id>` artwork URIs (omitted when `coverArtId == null`).
 * - Overflow-safe pagination even when Android Auto passes `pageSize = Int.MAX_VALUE`.
 */
@OptIn(UnstableApi::class)
object AutoBrowseTree {

    const val ROOT_ID: String = "root"
    const val RECENT_ROOT_ID: String = "root:recent"
    const val TAB_FOR_YOU: String = "tab:for_you"
    const val TAB_PLAYLISTS: String = "tab:playlists"
    const val TAB_LIBRARY: String = "tab:library"
    const val TAB_RADIO: String = "tab:radio"

    const val LIB_OFFLINE: String = "lib:offline"
    const val LIB_RECENT: String = "lib:recent"
    const val FOR_YOU_TRIAGE: String = "for_you:triage"

    const val PREFIX_TRACK: String = "track:"
    const val PREFIX_PLAYLIST: String = "playlist:"
    const val PREFIX_ALBUM: String = "album:"
    const val PREFIX_ARTIST: String = "artist:"
    const val PREFIX_RADIO_ARTIST: String = "radio:artist:"
    const val PREFIX_AZ: String = "az:"

    const val CONTEXT_PREFIX: String = "ctx:"
    const val CONTEXT_SEPARATOR: String = "|track:"

    const val EXTRA_IS_DISCOVERY: String = "org.soulbeet.media.extra.IS_DISCOVERY"
    const val EXTRA_IS_STARRED: String = "org.soulbeet.media.extra.IS_STARRED"
    const val EXTRA_IS_OFFLINE_CACHED: String = "org.soulbeet.media.extra.IS_OFFLINE_CACHED"
    const val EXTRA_RATING: String = "org.soulbeet.media.extra.RATING"
    const val EXTRA_TRACK_ID: String = "org.soulbeet.media.extra.TRACK_ID"
    const val EXTRA_COVER_ART_ID: String = "org.soulbeet.media.extra.COVER_ART_ID"
    const val EXTRA_NODE_KIND: String = "org.soulbeet.media.extra.NODE_KIND"
    const val EXTRA_SOURCE_LABEL: String = "org.soulbeet.media.extra.SOURCE_LABEL"
    const val EXTRA_PARENT_BROWSE_ID: String = "org.soulbeet.media.extra.PARENT_BROWSE_ID"

    const val EXTRAS_KEY_CONTENT_STYLE_SUPPORTED: String =
        "android.media.browse.CONTENT_STYLE_SUPPORTED"
    const val EXTRAS_KEY_SEARCH_SUPPORTED: String =
        "android.media.browse.SEARCH_SUPPORTED"

    private const val MAX_ROOT_TABS: Int = 4

    /**
     * Builds the root [MediaItem] returned by `onGetLibraryRoot`.
     */
    fun rootMediaItem(): MediaItem {
        val extras = Bundle().apply {
            putBoolean(EXTRAS_KEY_CONTENT_STYLE_SUPPORTED, true)
            putInt(
                MediaConstants.EXTRAS_KEY_CONTENT_STYLE_BROWSABLE,
                MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_GRID_ITEM,
            )
            putInt(
                MediaConstants.EXTRAS_KEY_CONTENT_STYLE_PLAYABLE,
                MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM,
            )
        }
        val metadata = MediaMetadata.Builder()
            .setTitle("Soulbeet")
            .setIsBrowsable(true)
            .setIsPlayable(false)
            .setMediaType(MediaMetadata.MEDIA_TYPE_FOLDER_MIXED)
            .setExtras(extras)
            .build()
        return MediaItem.Builder()
            .setMediaId(ROOT_ID)
            .setMediaMetadata(metadata)
            .build()
    }

    /**
     * Builds the dedicated recent-media root [MediaItem] returned when `params.isRecent == true`
     * (used by Android SystemUI / Android Auto media resumption cards).
     */
    fun recentRootMediaItem(): MediaItem {
        val metadata = MediaMetadata.Builder()
            .setTitle("Continue Listening")
            .setIsBrowsable(true)
            .setIsPlayable(false)
            .setMediaType(MediaMetadata.MEDIA_TYPE_FOLDER_MIXED)
            .build()
        return MediaItem.Builder()
            .setMediaId(RECENT_ROOT_ID)
            .setMediaMetadata(metadata)
            .build()
    }

    /**
     * Maps the 4 fixed root tabs (`For You`, `Playlists`, `Library`, `Radio`) returned by
     * `core.autoRootTabs()` into `CAR.VALIDATOR`-compliant root tab [MediaItem]s.
     */
    fun mapRootTabs(
        nodes: List<FfiAutoBrowseNode>,
        artAuthority: String,
    ): List<MediaItem> =
        nodes
            .take(MAX_ROOT_TABS)
            .map { node ->
                val tabMediaType = when (node.id) {
                    TAB_PLAYLISTS -> MediaMetadata.MEDIA_TYPE_FOLDER_PLAYLISTS
                    TAB_RADIO -> MediaMetadata.MEDIA_TYPE_FOLDER_RADIO_STATIONS
                    else -> MediaMetadata.MEDIA_TYPE_FOLDER_MIXED
                }
                val (browsableStyle, playableStyle) = when (node.id) {
                    TAB_FOR_YOU, TAB_PLAYLISTS, TAB_RADIO ->
                        MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_GRID_ITEM to
                            MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_GRID_ITEM

                    TAB_LIBRARY ->
                        MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM to
                            MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM

                    else ->
                        MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM to
                            MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM
                }
                val extras = Bundle().apply {
                    putInt(MediaConstants.EXTRAS_KEY_CONTENT_STYLE_BROWSABLE, browsableStyle)
                    putInt(MediaConstants.EXTRAS_KEY_CONTENT_STYLE_PLAYABLE, playableStyle)
                    putString(EXTRA_NODE_KIND, FfiAutoNodeKind.TAB.name)
                }
                val metadata = MediaMetadata.Builder()
                    .setTitle(node.title)
                    .setSubtitle(node.subtitle)
                    .setIsBrowsable(true)
                    .setIsPlayable(false)
                    .setMediaType(tabMediaType)
                    .setArtworkUri(artworkUriOrNull(artAuthority, node.coverArtId))
                    .setExtras(extras)
                    .build()
                MediaItem.Builder()
                    .setMediaId(node.id)
                    .setMediaMetadata(metadata)
                    .build()
            }

    /**
     * Maps a single [FfiAutoBrowseNode] into a Media3 [MediaItem], optionally enriched with full
     * [FfiTrack] metadata and parent browse context when available.
     */
    fun mapNode(
        node: FfiAutoBrowseNode,
        artAuthority: String,
        trackDetails: FfiTrack? = null,
        parentBrowseId: String? = null,
    ): MediaItem {
        if (node.kind == FfiAutoNodeKind.TAB) {
            return mapRootTabs(listOf(node), artAuthority).first()
        }

        val rawTrackId = if (node.kind == FfiAutoNodeKind.TRACK) {
            extractTrackId(node.id)
        } else {
            null
        }
        val effectiveMediaId = if (rawTrackId != null && !parentBrowseId.isNullOrBlank()) {
            contextualTrackId(parentBrowseId, rawTrackId)
        } else {
            node.id
        }
        val effectiveCoverArtId = trackDetails?.coverArtId ?: node.coverArtId
        val isDiscovery = trackDetails?.isDiscovery ?: node.isPendingDiscovery
        val isStarred = trackDetails?.starred ?: false
        val rating = trackDetails?.userRating?.toInt() ?: 0

        val mediaType = when (node.kind) {
            FfiAutoNodeKind.TAB -> MediaMetadata.MEDIA_TYPE_FOLDER_MIXED
            FfiAutoNodeKind.FOLDER -> when (node.id) {
                LIB_RECENT -> MediaMetadata.MEDIA_TYPE_FOLDER_ALBUMS
                else -> MediaMetadata.MEDIA_TYPE_FOLDER_MIXED
            }
            FfiAutoNodeKind.PLAYLIST -> MediaMetadata.MEDIA_TYPE_PLAYLIST
            FfiAutoNodeKind.ALBUM -> MediaMetadata.MEDIA_TYPE_ALBUM
            FfiAutoNodeKind.ARTIST -> MediaMetadata.MEDIA_TYPE_ARTIST
            FfiAutoNodeKind.TRACK -> MediaMetadata.MEDIA_TYPE_MUSIC
            FfiAutoNodeKind.RADIO_SEED -> MediaMetadata.MEDIA_TYPE_RADIO_STATION
        }

        val extras = Bundle().apply {
            putBoolean(EXTRA_IS_DISCOVERY, isDiscovery)
            putBoolean(EXTRA_IS_STARRED, isStarred)
            putBoolean(EXTRA_IS_OFFLINE_CACHED, node.isOfflineReady)
            putInt(EXTRA_RATING, rating)
            putString(EXTRA_NODE_KIND, node.kind.name)
            if (rawTrackId != null) {
                putString(EXTRA_TRACK_ID, rawTrackId)
            }
            if (!parentBrowseId.isNullOrBlank()) {
                putString(EXTRA_PARENT_BROWSE_ID, parentBrowseId)
            }
            if (!effectiveCoverArtId.isNullOrBlank()) {
                putString(EXTRA_COVER_ART_ID, effectiveCoverArtId)
            }
            applyChildContentStyleHints(this, node)
        }

        val metadataBuilder = MediaMetadata.Builder()
            .setTitle(trackDetails?.title ?: node.title)
            .setSubtitle(node.subtitle)
            .setIsBrowsable(node.browsable)
            .setIsPlayable(node.playable)
            .setMediaType(mediaType)
            .setArtworkUri(artworkUriOrNull(artAuthority, effectiveCoverArtId))
            .setExtras(extras)

        if (node.kind == FfiAutoNodeKind.TRACK) {
            metadataBuilder.setArtist(trackDetails?.artistName ?: node.subtitle)
            if (!trackDetails?.albumName.isNullOrBlank()) {
                metadataBuilder.setAlbumTitle(trackDetails?.albumName)
            }
            if (trackDetails != null && trackDetails.durationSecs > 0L) {
                metadataBuilder.setDurationMs(trackDetails.durationSecs * 1_000L)
            }
        } else if (node.kind == FfiAutoNodeKind.ALBUM && !node.subtitle.isNullOrBlank()) {
            metadataBuilder.setArtist(node.subtitle)
        }

        return MediaItem.Builder()
            .setMediaId(effectiveMediaId)
            .setMediaMetadata(metadataBuilder.build())
            .build()
    }

    /**
     * Maps a full [FfiTrack] into a playable [MediaItem] with optional resolved playback [mediaUri].
     */
    fun mapTrack(
        track: FfiTrack,
        artAuthority: String,
        isOfflineCached: Boolean = false,
        mediaUri: Uri? = null,
        sourceLabel: String? = null,
    ): MediaItem {
        val extras = Bundle().apply {
            putBoolean(EXTRA_IS_DISCOVERY, track.isDiscovery)
            putBoolean(EXTRA_IS_STARRED, track.starred)
            putBoolean(EXTRA_IS_OFFLINE_CACHED, isOfflineCached)
            putInt(EXTRA_RATING, track.userRating.toInt())
            putString(EXTRA_TRACK_ID, track.id)
            putString(EXTRA_NODE_KIND, FfiAutoNodeKind.TRACK.name)
            if (!track.coverArtId.isNullOrBlank()) {
                putString(EXTRA_COVER_ART_ID, track.coverArtId)
            }
            if (!sourceLabel.isNullOrBlank()) {
                putString(EXTRA_SOURCE_LABEL, sourceLabel)
            }
        }

        val metadata = MediaMetadata.Builder()
            .setTitle(track.title)
            .setArtist(track.artistName)
            .setSubtitle(track.artistName)
            .setAlbumTitle(track.albumName)
            .setIsBrowsable(false)
            .setIsPlayable(true)
            .setMediaType(MediaMetadata.MEDIA_TYPE_MUSIC)
            .setArtworkUri(artworkUriOrNull(artAuthority, track.coverArtId))
            .apply {
                if (track.durationSecs > 0L) {
                    setDurationMs(track.durationSecs * 1_000L)
                }
            }
            .setExtras(extras)
            .build()

        val builder = MediaItem.Builder()
            .setMediaId(track.id)
            .setMediaMetadata(metadata)

        if (mediaUri != null) {
            builder.setUri(mediaUri)
        }

        return builder.build()
    }

    /**
     * Fetches and maps paginated children for [parentId] from [core].
     *
     * Always reads purely from local SQLite via `core.autoRootTabs()` / `core.autoChildren()`.
     */
    suspend fun loadChildren(
        core: CoreClient,
        parentId: String,
        page: Int,
        pageSize: Int,
        artAuthority: String,
        offlineOnly: Boolean = core.isOfflineOnly(),
    ): List<MediaItem> {
        if (parentId == ROOT_ID) {
            val rootTabs = mapRootTabs(core.autoRootTabs(), artAuthority)
            return paginate(rootTabs, page, pageSize)
        }

        if (parentId == RECENT_ROOT_ID) {
            val restored = PlaybackSessionStore.restoreSession(core, artAuthority) ?: return emptyList()
            val lastPlayed = restored.items.getOrNull(restored.startIndex) ?: return emptyList()
            return paginate(listOf(lastPlayed), page, pageSize)
        }

        val rawNodes = core.autoChildren(parentId)
        val filteredNodes = if (offlineOnly) {
            rawNodes.filter { node ->
                node.isOfflineReady ||
                    node.kind == FfiAutoNodeKind.TAB ||
                    (node.kind == FfiAutoNodeKind.FOLDER && !node.playable)
            }
        } else {
            rawNodes
        }
        val pagedNodes = paginate(filteredNodes, page, pageSize)

        val trackIds = pagedNodes
            .filter { it.kind == FfiAutoNodeKind.TRACK }
            .map { extractTrackId(it.id) }
        val trackDetailsById = if (trackIds.isNotEmpty()) {
            core.getTracksByIds(trackIds).associateBy { it.id }
        } else {
            emptyMap()
        }

        return pagedNodes.map { node ->
            val details = if (node.kind == FfiAutoNodeKind.TRACK) {
                trackDetailsById[extractTrackId(node.id)]
            } else {
                null
            }
            mapNode(
                node = node,
                artAuthority = artAuthority,
                trackDetails = details,
                parentBrowseId = parentId,
            )
        }
    }

    /**
     * Overflow-safe bounded sublist pagination for Media3 `onGetChildren(parentId, page, pageSize)`.
     *
     * Handles `pageSize == Int.MAX_VALUE` (frequently passed by Android Auto) cleanly without
     * 32-bit multiplication overflow.
     */
    fun <T> paginate(items: List<T>, page: Int, pageSize: Int): List<T> {
        if (page < 0 || pageSize <= 0 || items.isEmpty()) return emptyList()
        val startLong = page.toLong() * pageSize.toLong()
        if (startLong >= items.size.toLong()) return emptyList()
        val start = startLong.toInt()
        val endLong = minOf(startLong + pageSize.toLong(), items.size.toLong())
        return items.subList(start, endLong.toInt())
    }

    fun contextualTrackId(parentId: String, rawTrackId: String): String =
        "$CONTEXT_PREFIX$parentId$CONTEXT_SEPARATOR$rawTrackId"

    fun parseContextualTrackId(mediaId: String): ContextualTrackRef? {
        if (!mediaId.startsWith(CONTEXT_PREFIX)) return null
        val sepIndex = mediaId.indexOf(CONTEXT_SEPARATOR)
        if (sepIndex <= CONTEXT_PREFIX.length) return null
        val parentId = mediaId.substring(CONTEXT_PREFIX.length, sepIndex)
        val trackId = mediaId.substring(sepIndex + CONTEXT_SEPARATOR.length)
        if (parentId.isBlank() || trackId.isBlank()) return null
        return ContextualTrackRef(parentId = parentId, trackId = trackId)
    }

    fun extractTrackId(mediaId: String): String =
        parseContextualTrackId(mediaId)?.trackId ?: mediaId.removePrefix(PREFIX_TRACK)

    /** Resolves the canonical Navidrome track ID from a Media3 [MediaItem]. */
    fun resolveTrackId(item: MediaItem): String =
        item.mediaMetadata.extras?.getString(EXTRA_TRACK_ID)?.takeIf { it.isNotBlank() }
            ?: extractTrackId(item.mediaId)

    /**
     * Removes every occurrence of [trackId] from [player]'s queue.
     *
     * Phase 1 removes all non-current duplicates in descending order so advancing from the
     * active track never lands on another copy of [trackId]. Phase 2 handles the active track:
     * if another distinct track is reachable next (respecting shuffle/repeat, while guarding
     * against single-item `REPEAT_MODE_ALL` self-loops), seeks to it, removes the dropped index,
     * and resumes playback (preparing first if `STATE_IDLE` after an error). Otherwise pauses
     * and removes the dropped index cleanly.
     *
     * Returns `true` if playback advanced to another queued track, `false` otherwise.
     */
    fun removeTrackFromQueue(player: Player, trackId: String): Boolean {
        if (trackId.isBlank()) return false

        val initialCurrent = player.currentMediaItemIndex
        for (i in player.mediaItemCount - 1 downTo 0) {
            if (i != initialCurrent && resolveTrackId(player.getMediaItemAt(i)) == trackId) {
                player.removeMediaItem(i)
            }
        }

        val curIdx = player.currentMediaItemIndex
        if (curIdx !in 0 until player.mediaItemCount ||
            resolveTrackId(player.getMediaItemAt(curIdx)) != trackId
        ) {
            return false
        }

        val nextIdx = player.nextMediaItemIndex
        return if (player.mediaItemCount > 1 && nextIdx != C.INDEX_UNSET && nextIdx != curIdx) {
            val wasPlaying = player.playWhenReady || player.isPlaying
            player.seekToNextMediaItem()
            player.removeMediaItem(curIdx)
            if (wasPlaying) {
                if (player.playbackState == Player.STATE_IDLE) {
                    player.prepare()
                }
                player.play()
            }
            true
        } else {
            player.pause()
            player.removeMediaItem(curIdx)
            false
        }
    }

    private fun artworkUriOrNull(artAuthority: String, coverArtId: String?): Uri? {
        val cleanId = coverArtId?.trim()?.takeIf { it.isNotEmpty() } ?: return null
        return CoverArtContentProvider.uriFor(artAuthority, cleanId)
    }

    private fun applyChildContentStyleHints(extras: Bundle, node: FfiAutoBrowseNode) {
        when (node.kind) {
            FfiAutoNodeKind.ARTIST -> {
                extras.putInt(
                    MediaConstants.EXTRAS_KEY_CONTENT_STYLE_BROWSABLE,
                    MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_GRID_ITEM,
                )
                extras.putInt(
                    MediaConstants.EXTRAS_KEY_CONTENT_STYLE_PLAYABLE,
                    MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM,
                )
            }
            FfiAutoNodeKind.FOLDER -> {
                val browsableStyle = if (node.id == LIB_RECENT) {
                    MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_GRID_ITEM
                } else {
                    MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM
                }
                extras.putInt(MediaConstants.EXTRAS_KEY_CONTENT_STYLE_BROWSABLE, browsableStyle)
                extras.putInt(
                    MediaConstants.EXTRAS_KEY_CONTENT_STYLE_PLAYABLE,
                    MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM,
                )
            }
            FfiAutoNodeKind.PLAYLIST, FfiAutoNodeKind.ALBUM -> {
                extras.putInt(
                    MediaConstants.EXTRAS_KEY_CONTENT_STYLE_PLAYABLE,
                    MediaConstants.EXTRAS_VALUE_CONTENT_STYLE_LIST_ITEM,
                )
            }
            else -> Unit
        }
    }
}
