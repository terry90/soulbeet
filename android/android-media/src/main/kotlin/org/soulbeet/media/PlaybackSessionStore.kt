package org.soulbeet.media

import android.net.Uri
import androidx.media3.common.MediaItem
import org.soulbeet.core.FfiPersistedPlaybackSession
import org.soulbeet.core.FfiTrack
import org.soulbeet.corebindings.CoreClient

/**
 * Hydrated Media3 playback session restored from `lib/client-core`'s persisted SQLite state
 * for instant car-ignition resumption (`onPlaybackResumption`).
 */
data class RestoredPlaybackSession(
    val items: List<MediaItem>,
    val startIndex: Int,
    val startPositionMs: Long,
    val sourceLabel: String?,
)

/**
 * Persists and restores the active playback queue, index, and position via `lib/client-core`.
 */
object PlaybackSessionStore {

    /**
     * Persists the current playback queue and position into `client-core`'s SQLite store.
     */
    suspend fun saveSession(
        core: CoreClient,
        queueTrackIds: List<String>,
        currentIndex: Int,
        positionMs: Long,
        isPlaying: Boolean,
        sourceLabel: String?,
        nowUnixSec: Long = System.currentTimeMillis() / 1_000L,
    ) {
        val clampedIndex = if (queueTrackIds.isEmpty()) {
            0u
        } else {
            currentIndex.coerceIn(0, queueTrackIds.lastIndex).toUInt()
        }
        val clampedPosition = positionMs.coerceAtLeast(0L).toULong()

        core.savePlaybackSession(
            FfiPersistedPlaybackSession(
                queueTrackIds = queueTrackIds,
                currentIndex = clampedIndex,
                positionMs = clampedPosition,
                isPlaying = isPlaying,
                sourceLabel = sourceLabel,
                updatedAt = nowUnixSec,
            ),
        )
    }

    /**
     * Loads and hydrates the persisted playback session from `client-core`, preserving exact
     * queue order, remapping `currentIndex` if any persisted tracks were deleted, and clamping
     * out-of-range indices and positions safely.
     */
    suspend fun restoreSession(
        core: CoreClient,
        artAuthority: String,
        mediaUriResolver: (FfiTrack) -> Uri? = { SoulbeetDataSourceFactory.trackUri(it.id) },
    ): RestoredPlaybackSession? {
        val persisted = core.loadPlaybackSession() ?: return null
        if (persisted.queueTrackIds.isEmpty()) return null

        val tracksById = core.getTracksByIds(persisted.queueTrackIds).associateBy { it.id }
        if (tracksById.isEmpty()) return null

        val offlineIds = runCatching {
            core.listOfflineTracks().mapTo(HashSet()) { it.trackId }
        }.getOrDefault(emptySet())

        val rawSavedIndexLong = persisted.currentIndex.toLong()
        val hydratedItems = ArrayList<MediaItem>(persisted.queueTrackIds.size)
        val hydratedTracks = ArrayList<FfiTrack>(persisted.queueTrackIds.size)
        var remappedIndex: Int? = null
        var precedingPresentCount = 0

        persisted.queueTrackIds.forEachIndexed { originalIdx, trackId ->
            val track = tracksById[trackId] ?: return@forEachIndexed
            val newIdx = hydratedItems.size
            if (originalIdx.toLong() == rawSavedIndexLong) {
                remappedIndex = newIdx
            }
            if (originalIdx.toLong() < rawSavedIndexLong) {
                precedingPresentCount++
            }
            hydratedTracks.add(track)
            hydratedItems.add(
                AutoBrowseTree.mapTrack(
                    track = track,
                    artAuthority = artAuthority,
                    isOfflineCached = offlineIds.contains(track.id),
                    mediaUri = mediaUriResolver(track),
                    sourceLabel = persisted.sourceLabel,
                    isArtCached = AutoBrowseTree.isArtLocallyCached(core, track.coverArtId),
                ),
            )
        }

        if (hydratedItems.isEmpty()) return null

        val finalIndex = when {
            rawSavedIndexLong <= 0L -> 0
            rawSavedIndexLong >= persisted.queueTrackIds.size.toLong() -> hydratedItems.lastIndex
            remappedIndex != null -> remappedIndex!!
            else -> precedingPresentCount.coerceIn(0, hydratedItems.lastIndex)
        }

        val targetTrack = hydratedTracks[finalIndex]
        val rawPosLong = if (persisted.positionMs > Long.MAX_VALUE.toULong()) {
            Long.MAX_VALUE
        } else {
            persisted.positionMs.toLong()
        }.coerceAtLeast(0L)

        // Reset position to 0 if the originally saved track was deleted and we fell back to another.
        val positionForTrack = if (remappedIndex == null && rawSavedIndexLong in 0L until persisted.queueTrackIds.size.toLong()) {
            0L
        } else if (targetTrack.durationSecs > 0L) {
            val maxPosMs = targetTrack.durationSecs * 1_000L
            rawPosLong.coerceIn(0L, maxPosMs)
        } else {
            rawPosLong
        }

        return RestoredPlaybackSession(
            items = hydratedItems,
            startIndex = finalIndex,
            startPositionMs = positionForTrack,
            sourceLabel = persisted.sourceLabel,
        )
    }
}
