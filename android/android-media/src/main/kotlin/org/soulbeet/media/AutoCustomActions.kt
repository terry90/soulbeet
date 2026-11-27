package org.soulbeet.media

import android.os.Bundle
import androidx.annotation.OptIn
import androidx.media3.common.MediaItem
import androidx.media3.common.util.UnstableApi
import androidx.media3.session.CommandButton
import androidx.media3.session.SessionCommand
import androidx.media3.session.SessionError
import androidx.media3.session.SessionResult
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.soulbeet.core.FfiOutboxMutation
import org.soulbeet.corebindings.CoreClient

/**
 * Configures the second custom action slot on Android Auto when playing a permanent Library track.
 *
 * Defaults to [DISLIKE_SKIP] (`1★ & Skip`) per Soulbeet's driving-triage design, and can be
 * switched to [START_RADIO] in mobile Settings.
 */
enum class LibrarySecondaryAction {
    DISLIKE_SKIP,
    START_RADIO,
}

/**
 * Live curation state of the currently playing track used to compute the 2-slot custom action bar.
 */
data class TrackActionContext(
    val trackId: String,
    val isPendingDiscovery: Boolean,
    val isStarred: Boolean,
    val userRating: Int = 0,
    val discoveryTrackId: String? = null,
)

/**
 * Context-aware 2-slot custom action bar for Android Auto (`CAR.VALIDATOR` compliant):
 *
 * - **Pending Discovery track:** Slot 1 = `Keep`, Slot 2 = `Drop & Skip`.
 * - **Permanent Library track:** Slot 1 = `Star` / `Unstar`, Slot 2 = `1★ & Skip` (or `Track Radio`).
 */
@OptIn(UnstableApi::class)
object AutoCustomActions {

    const val ACTION_KEEP_DISCOVERY: String = "org.soulbeet.media.action.KEEP_DISCOVERY"
    const val ACTION_DROP_DISCOVERY: String = "org.soulbeet.media.action.DROP_DISCOVERY"
    const val ACTION_STAR_TOGGLE: String = "org.soulbeet.media.action.STAR_TOGGLE"
    const val ACTION_DISLIKE_SKIP: String = "org.soulbeet.media.action.DISLIKE_SKIP"
    const val ACTION_START_RADIO: String = "org.soulbeet.media.action.START_RADIO"

    val COMMAND_KEEP_DISCOVERY: SessionCommand = SessionCommand(ACTION_KEEP_DISCOVERY, Bundle.EMPTY)
    val COMMAND_DROP_DISCOVERY: SessionCommand = SessionCommand(ACTION_DROP_DISCOVERY, Bundle.EMPTY)
    val COMMAND_STAR_TOGGLE: SessionCommand = SessionCommand(ACTION_STAR_TOGGLE, Bundle.EMPTY)
    val COMMAND_DISLIKE_SKIP: SessionCommand = SessionCommand(ACTION_DISLIKE_SKIP, Bundle.EMPTY)
    val COMMAND_START_RADIO: SessionCommand = SessionCommand(ACTION_START_RADIO, Bundle.EMPTY)

    val ALL_SESSION_COMMANDS: List<SessionCommand> = listOf(
        COMMAND_KEEP_DISCOVERY,
        COMMAND_DROP_DISCOVERY,
        COMMAND_STAR_TOGGLE,
        COMMAND_DISLIKE_SKIP,
        COMMAND_START_RADIO,
    )

    /**
     * Pure builder returning the exact 2-slot custom layout for [trackContext] (or empty when no
     * track is active).
     */
    fun buildCustomLayout(
        trackContext: TrackActionContext?,
        secondaryLibraryAction: LibrarySecondaryAction = LibrarySecondaryAction.DISLIKE_SKIP,
    ): List<CommandButton> {
        if (trackContext == null) return emptyList()

        return if (trackContext.isPendingDiscovery) {
            listOf(
                CommandButton.Builder()
                    .setDisplayName("Keep")
                    .setIconResId(R.drawable.ic_soulbeet_keep)
                    .setSessionCommand(COMMAND_KEEP_DISCOVERY)
                    .setEnabled(true)
                    .build(),
                CommandButton.Builder()
                    .setDisplayName("Drop & Skip")
                    .setIconResId(R.drawable.ic_soulbeet_drop)
                    .setSessionCommand(COMMAND_DROP_DISCOVERY)
                    .setEnabled(true)
                    .build(),
            )
        } else {
            val starButton = CommandButton.Builder()
                .setDisplayName(if (trackContext.isStarred) "Unstar" else "Star")
                .setIconResId(
                    if (trackContext.isStarred) {
                        R.drawable.ic_soulbeet_star_filled
                    } else {
                        R.drawable.ic_soulbeet_star_outline
                    },
                )
                .setSessionCommand(COMMAND_STAR_TOGGLE)
                .setEnabled(true)
                .build()

            val secondaryButton = when (secondaryLibraryAction) {
                LibrarySecondaryAction.DISLIKE_SKIP -> CommandButton.Builder()
                    .setDisplayName("1★ & Skip")
                    .setIconResId(R.drawable.ic_soulbeet_thumb_down)
                    .setSessionCommand(COMMAND_DISLIKE_SKIP)
                    .setEnabled(true)
                    .build()

                LibrarySecondaryAction.START_RADIO -> CommandButton.Builder()
                    .setDisplayName("Track Radio")
                    .setIconResId(R.drawable.ic_soulbeet_radio)
                    .setSessionCommand(COMMAND_START_RADIO)
                    .setEnabled(true)
                    .build()
            }

            listOf(starButton, secondaryButton)
        }
    }

    /**
     * Extracts [TrackActionContext] from a Media3 [MediaItem]'s Soulbeet metadata extras.
     */
    fun contextFromMediaItem(mediaItem: MediaItem?): TrackActionContext? {
        if (mediaItem == null) return null
        val extras = mediaItem.mediaMetadata.extras
        val trackId = extras?.getString(AutoBrowseTree.EXTRA_TRACK_ID)
            ?.takeIf { it.isNotBlank() }
            ?: AutoBrowseTree.extractTrackId(mediaItem.mediaId).takeIf { it.isNotBlank() }
            ?: return null

        return TrackActionContext(
            trackId = trackId,
            isPendingDiscovery = extras?.getBoolean(AutoBrowseTree.EXTRA_IS_DISCOVERY, false) ?: false,
            isStarred = extras?.getBoolean(AutoBrowseTree.EXTRA_IS_STARRED, false) ?: false,
            userRating = extras?.getInt(AutoBrowseTree.EXTRA_RATING, 0) ?: 0,
        )
    }

    /**
     * Executes Android Auto custom actions against [CoreClient]'s offline-first outbox.
     *
     * Threading & consistency guarantees:
     * - Must be invoked on the main/player thread (`Dispatchers.Main.immediate`); ONLY `CoreClient`
     *   and outbox scheduling run on [ioDispatcher].
     * - Mutations are durably enqueued into SQLite **before** mutating in-memory context or
     *   refreshing the MediaSession custom layout, preventing UI desync if persistence fails.
     * - `onSkipToNext` returns `true` if playback advanced to a next item, or `false` if playback
     *   stopped at the end of the queue (in which case the custom layout is cleared cleanly).
     */
    class ActionHandler(
        private val core: CoreClient,
        private val ioDispatcher: CoroutineDispatcher = Dispatchers.IO,
        private val secondaryLibraryActionProvider: () -> LibrarySecondaryAction = {
            LibrarySecondaryAction.DISLIKE_SKIP
        },
        private val currentTrackContextProvider: () -> TrackActionContext?,
        private val onUpdateTrackContext: (TrackActionContext?) -> Unit,
        private val onRefreshCustomLayout: (List<CommandButton>) -> Unit,
        private val onSkipToNext: () -> Boolean,
        private val onStartTrackRadio: suspend (String) -> Unit = {},
        private val onMutationEnqueued: () -> Unit = {},
    ) {
        suspend fun handle(action: String): SessionResult {
            val current = currentTrackContextProvider()
                ?: return SessionResult(SessionError.ERROR_INVALID_STATE)

            return when (action) {
                ACTION_KEEP_DISCOVERY -> {
                    val discoveryId = withContext(ioDispatcher) {
                        resolveDiscoveryTrackId(current)
                    }
                    val enqueued = enqueueSafely(
                        FfiOutboxMutation.Keep(
                            songId = current.trackId,
                            discoveryTrackId = discoveryId,
                            targetFolderId = null,
                        ),
                    )
                    if (!enqueued) return SessionResult(SessionError.ERROR_IO)

                    val updated = current.copy(
                        isPendingDiscovery = false,
                        discoveryTrackId = discoveryId,
                    )
                    if (currentTrackContextProvider()?.trackId == current.trackId) {
                        onUpdateTrackContext(updated)
                        onRefreshCustomLayout(
                            buildCustomLayout(updated, secondaryLibraryActionProvider()),
                        )
                    }
                    notifyMutationEnqueued()
                    SessionResult(SessionResult.RESULT_SUCCESS)
                }

                ACTION_DROP_DISCOVERY -> {
                    val discoveryId = withContext(ioDispatcher) {
                        resolveDiscoveryTrackId(current)
                    }
                    val enqueued = enqueueSafely(
                        FfiOutboxMutation.Drop(
                            songId = current.trackId,
                            discoveryTrackId = discoveryId,
                            confirmDelete = false,
                        ),
                    )
                    if (!enqueued) return SessionResult(SessionError.ERROR_IO)

                    notifyMutationEnqueued()
                    if (currentTrackContextProvider()?.trackId == current.trackId) {
                        val advanced = onSkipToNext()
                        if (!advanced) {
                            onUpdateTrackContext(null)
                            onRefreshCustomLayout(emptyList())
                        }
                    }
                    SessionResult(SessionResult.RESULT_SUCCESS)
                }

                ACTION_STAR_TOGGLE -> {
                    val newStarred = !current.isStarred
                    val discoveryId = if (newStarred && current.isPendingDiscovery) {
                        withContext(ioDispatcher) { resolveDiscoveryTrackId(current) }
                    } else {
                        current.discoveryTrackId
                    }
                    val mutation = if (newStarred) {
                        FfiOutboxMutation.Star(
                            songId = current.trackId,
                            discoveryTrackId = discoveryId,
                        )
                    } else {
                        FfiOutboxMutation.Unstar(
                            songId = current.trackId,
                            discoveryTrackId = discoveryId,
                        )
                    }
                    if (!enqueueSafely(mutation)) return SessionResult(SessionError.ERROR_IO)

                    val updated = current.copy(
                        isStarred = newStarred,
                        isPendingDiscovery = if (newStarred) false else current.isPendingDiscovery,
                        discoveryTrackId = discoveryId,
                    )
                    if (currentTrackContextProvider()?.trackId == current.trackId) {
                        onUpdateTrackContext(updated)
                        onRefreshCustomLayout(
                            buildCustomLayout(updated, secondaryLibraryActionProvider()),
                        )
                    }
                    notifyMutationEnqueued()
                    SessionResult(SessionResult.RESULT_SUCCESS)
                }

                ACTION_DISLIKE_SKIP -> {
                    val enqueued = enqueueSafely(
                        FfiOutboxMutation.Rate(
                            songId = current.trackId,
                            rating = 1u,
                            discoveryTrackId = current.discoveryTrackId,
                        ),
                    )
                    if (!enqueued) return SessionResult(SessionError.ERROR_IO)

                    notifyMutationEnqueued()
                    if (currentTrackContextProvider()?.trackId == current.trackId) {
                        val updated = current.copy(userRating = 1)
                        onUpdateTrackContext(updated)
                        val advanced = onSkipToNext()
                        if (!advanced) {
                            onUpdateTrackContext(null)
                            onRefreshCustomLayout(emptyList())
                        }
                    }
                    SessionResult(SessionResult.RESULT_SUCCESS)
                }

                ACTION_START_RADIO -> {
                    onStartTrackRadio(current.trackId)
                    SessionResult(SessionResult.RESULT_SUCCESS)
                }

                else -> SessionResult(SessionError.ERROR_NOT_SUPPORTED)
            }
        }

        private suspend fun enqueueSafely(mutation: FfiOutboxMutation): Boolean =
            try {
                withContext(ioDispatcher) {
                    core.enqueueMutation(mutation)
                }
                true
            } catch (e: CancellationException) {
                throw e
            } catch (_: Exception) {
                false
            }

        private suspend fun notifyMutationEnqueued() {
            withContext(ioDispatcher) {
                onMutationEnqueued()
            }
        }

        private suspend fun resolveDiscoveryTrackId(context: TrackActionContext): String? {
            if (!context.discoveryTrackId.isNullOrBlank()) return context.discoveryTrackId
            return try {
                core.listDiscoveryTriage(pendingOnly = true)
                    .firstOrNull { it.songId == context.trackId }
                    ?.id
            } catch (e: CancellationException) {
                throw e
            } catch (_: Exception) {
                null
            }
        }
    }
}
