package org.soulbeet.media

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import org.soulbeet.core.FfiAudioQualityProfile

/**
 * Shared immutable UI & session playback snapshot consumed by both [SoulbeetMediaLibraryService]
 * (Android Auto) and PR 4B's Jetpack Compose `:app` (mini-player, full-screen Now Playing, queue).
 */
data class MediaPlaybackUiState(
    val currentTrackId: String? = null,
    val title: String = "",
    val artist: String = "",
    val album: String = "",
    val coverArtId: String? = null,
    val isPlaying: Boolean = false,
    val isBuffering: Boolean = false,
    val positionMs: Long = 0L,
    val durationMs: Long = 0L,
    val queueTrackIds: List<String> = emptyList(),
    val currentIndex: Int = 0,
    val isPendingDiscovery: Boolean = false,
    val isStarred: Boolean = false,
    val userRating: Int = 0,
    val isOfflineCached: Boolean = false,
    val sourceLabel: String? = null,
    val offlineOnly: Boolean = false,
    val librarySecondaryAction: LibrarySecondaryAction = LibrarySecondaryAction.DISLIKE_SKIP,
    val audioQualityProfile: FfiAudioQualityProfile = FfiAudioQualityProfile.CellularOpus160,
    val lastError: String? = null,
) {
    fun toTrackActionContext(): TrackActionContext? {
        val id = currentTrackId?.takeIf { it.isNotBlank() } ?: return null
        return TrackActionContext(
            trackId = id,
            isPendingDiscovery = isPendingDiscovery,
            isStarred = isStarred,
            userRating = userRating,
        )
    }
}

/**
 * Shared state holder injected into both [SoulbeetMediaLibraryService] and the Compose `:app`
 * so Android phone UI and Android Auto observe and mutate one unified playback session.
 */
class SoulbeetMediaRepository {

    private val _playbackState = MutableStateFlow(MediaPlaybackUiState())
    val playbackState: StateFlow<MediaPlaybackUiState> = _playbackState.asStateFlow()

    fun updateActiveTrack(
        trackId: String?,
        title: String,
        artist: String,
        album: String,
        coverArtId: String?,
        durationMs: Long,
        queueTrackIds: List<String>,
        currentIndex: Int,
        isPendingDiscovery: Boolean,
        isStarred: Boolean,
        userRating: Int,
        isOfflineCached: Boolean,
        sourceLabel: String?,
    ) {
        _playbackState.update { prev ->
            prev.copy(
                currentTrackId = trackId,
                title = title,
                artist = artist,
                album = album,
                coverArtId = coverArtId,
                durationMs = durationMs.coerceAtLeast(0L),
                positionMs = if (prev.currentTrackId == trackId) prev.positionMs else 0L,
                queueTrackIds = queueTrackIds,
                currentIndex = currentIndex.coerceAtLeast(0),
                isPendingDiscovery = isPendingDiscovery,
                isStarred = isStarred,
                userRating = userRating,
                isOfflineCached = isOfflineCached,
                sourceLabel = sourceLabel ?: prev.sourceLabel,
                lastError = null,
            )
        }
    }

    fun updatePlaybackStatus(
        isPlaying: Boolean,
        isBuffering: Boolean,
        positionMs: Long,
        durationMs: Long,
    ) {
        _playbackState.update { prev ->
            prev.copy(
                isPlaying = isPlaying,
                isBuffering = isBuffering,
                positionMs = positionMs.coerceAtLeast(0L),
                durationMs = if (durationMs > 0L) durationMs else prev.durationMs,
            )
        }
    }

    fun updateTrackContext(context: TrackActionContext) {
        _playbackState.update { prev ->
            if (prev.currentTrackId != context.trackId) {
                prev
            } else {
                prev.copy(
                    isPendingDiscovery = context.isPendingDiscovery,
                    isStarred = context.isStarred,
                    userRating = context.userRating,
                )
            }
        }
    }

    fun setLibrarySecondaryAction(action: LibrarySecondaryAction) {
        _playbackState.update { it.copy(librarySecondaryAction = action) }
    }

    fun setAudioQualityProfile(profile: FfiAudioQualityProfile) {
        _playbackState.update { it.copy(audioQualityProfile = profile) }
    }

    fun setOfflineOnly(offlineOnly: Boolean) {
        _playbackState.update { it.copy(offlineOnly = offlineOnly) }
    }

    fun setLastError(error: String?) {
        _playbackState.update { it.copy(lastError = error) }
    }

    fun reset() {
        _playbackState.value = MediaPlaybackUiState()
    }

    companion object {
        /**
         * Process-wide default repository shared between [SoulbeetMediaLibraryService] and the
         * upcoming Jetpack Compose `:app` module.
         */
        val shared: SoulbeetMediaRepository = SoulbeetMediaRepository()
    }
}
