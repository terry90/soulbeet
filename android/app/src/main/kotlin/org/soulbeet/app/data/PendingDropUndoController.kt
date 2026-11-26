package org.soulbeet.app.data

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import org.soulbeet.core.FfiOutboxMutation

/**
 * Explicit curation outcome committed when the 5-second non-blocking Undo window expires.
 */
sealed class PendingDropMode {
    /** Removes a staged Pending Discovery track (never touches permanent library files). */
    data object DropDiscovery : PendingDropMode()

    /** Deletes a sole-owner permanent library track after Multi-Listener Delete Guard verification. */
    data object DeleteFromLibrary : PendingDropMode()

    /**
     * Safe non-destructive hide for shared/protected/offline-fallback permanent library tracks:
     * unstars and rates 1★ without ever attempting file deletion or hitting 409 ConfirmationRequired.
     */
    data object UnstarAndHide : PendingDropMode()
}

/**
 * Snapshot of a deferred Drop/Remove action currently inside the 5-second non-blocking Undo window.
 */
data class PendingDropAction(
    val trackId: String,
    val title: String,
    val artist: String,
    val discoveryTrackId: String?,
    val mode: PendingDropMode,
    val sourceLabel: String?,
    val createdAtMs: Long,
    val durationMs: Long = DEFAULT_UNDO_WINDOW_MS,
) {
    /**
     * Exact ordered outbox mutations committed when this action expires or flushes.
     */
    fun toMutations(): List<FfiOutboxMutation> = when (mode) {
        PendingDropMode.DropDiscovery -> listOf(
            FfiOutboxMutation.Drop(
                songId = trackId,
                discoveryTrackId = discoveryTrackId,
                confirmDelete = false,
            ),
        )

        PendingDropMode.DeleteFromLibrary -> listOf(
            FfiOutboxMutation.Drop(
                songId = trackId,
                discoveryTrackId = null,
                confirmDelete = true,
            ),
        )

        PendingDropMode.UnstarAndHide -> listOf(
            FfiOutboxMutation.Unstar(
                songId = trackId,
                discoveryTrackId = null,
            ),
            FfiOutboxMutation.Rate(
                songId = trackId,
                rating = 1u,
                discoveryTrackId = null,
            ),
        )
    }

    companion object {
        const val DEFAULT_UNDO_WINDOW_MS: Long = 5_000L
    }
}

/**
 * Coordinates Soulbeet's 5-second non-blocking Undo buffer before committing destructive or
 * library-hiding curation mutations.
 *
 * Guarantees:
 * - Undoing within [undoWindowMs] cancels the countdown and commits **zero** mutations.
 * - Letting the countdown expire commits [PendingDropAction.toMutations] via [onCommit] on
 *   [commitScope] (application-scoped in production so rapid swipes and teardown never cancel).
 * - Triggering a second drop while a previous drop is still pending immediately commits the
 *   displaced first drop on [commitScope] so rapid triage swipes never lose mutations.
 */
class PendingDropUndoController(
    private val timerScope: CoroutineScope,
    private val commitScope: CoroutineScope = timerScope,
    private val onCommit: suspend (PendingDropAction) -> Unit,
    private val onUndo: (PendingDropAction) -> Unit = {},
    private val undoWindowMs: Long = PendingDropAction.DEFAULT_UNDO_WINDOW_MS,
    private val clockMs: () -> Long = { System.currentTimeMillis() },
) {
    private val _pending = MutableStateFlow<PendingDropAction?>(null)
    val pending: StateFlow<PendingDropAction?> = _pending.asStateFlow()

    private var timerJob: Job? = null
    private var generation: Long = 0L

    fun scheduleDrop(action: PendingDropAction) {
        val previous = _pending.value
        timerJob?.cancel()
        timerJob = null

        if (previous != null) {
            commitScope.launch {
                onCommit(previous)
            }
        }

        val scheduled = action.copy(
            createdAtMs = clockMs(),
            durationMs = undoWindowMs,
        )
        val token = ++generation
        _pending.value = scheduled

        timerJob = timerScope.launch {
            delay(undoWindowMs)
            if (generation == token && _pending.value?.trackId == scheduled.trackId) {
                _pending.value = null
                timerJob = null
                commitScope.launch {
                    onCommit(scheduled)
                }
            }
        }
    }

    fun undo(): Boolean {
        val current = _pending.value ?: return false
        generation++
        timerJob?.cancel()
        timerJob = null
        _pending.value = null
        onUndo(current)
        return true
    }

    /**
     * Immediately commits any pending drop on [commitScope] (safe to invoke from `onCleared`).
     */
    fun flushAsync(): Job? {
        val current = _pending.value ?: return null
        generation++
        timerJob?.cancel()
        timerJob = null
        _pending.value = null
        return commitScope.launch {
            onCommit(current)
        }
    }

    suspend fun flushNow() {
        val current = _pending.value ?: return
        generation++
        timerJob?.cancel()
        timerJob = null
        _pending.value = null
        onCommit(current)
    }
}
