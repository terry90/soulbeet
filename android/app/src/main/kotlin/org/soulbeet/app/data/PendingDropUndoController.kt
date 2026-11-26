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
 * Snapshot of a deferred Drop/Remove action currently inside the 5-second non-blocking Undo window.
 */
data class PendingDropAction(
    val trackId: String,
    val title: String,
    val artist: String,
    val discoveryTrackId: String?,
    val confirmDelete: Boolean,
    val sourceLabel: String?,
    val createdAtMs: Long,
    val durationMs: Long = DEFAULT_UNDO_WINDOW_MS,
) {
    fun toMutation(): FfiOutboxMutation = FfiOutboxMutation.Drop(
        songId = trackId,
        discoveryTrackId = discoveryTrackId,
        confirmDelete = confirmDelete,
    )

    companion object {
        const val DEFAULT_UNDO_WINDOW_MS: Long = 5_000L
    }
}

/**
 * Coordinates Soulbeet's 5-second non-blocking Undo buffer before committing destructive
 * [FfiOutboxMutation.Drop] mutations.
 *
 * Guarantees:
 * - Undoing within [undoWindowMs] cancels the countdown and commits **zero** mutation.
 * - Letting the countdown expire commits the drop mutation via [onCommit].
 * - Triggering a second drop while a previous drop is still pending immediately commits the
 *   displaced first drop so rapid triage swipes never lose mutations.
 */
class PendingDropUndoController(
    private val scope: CoroutineScope,
    private val onCommit: suspend (PendingDropAction) -> Unit,
    private val onUndo: (PendingDropAction) -> Unit = {},
    private val undoWindowMs: Long = PendingDropAction.DEFAULT_UNDO_WINDOW_MS,
    private val clockMs: () -> Long = { System.currentTimeMillis() },
) {
    private val _pending = MutableStateFlow<PendingDropAction?>(null)
    val pending: StateFlow<PendingDropAction?> = _pending.asStateFlow()

    private var timerJob: Job? = null
    private var generation: Long = 0L

    /**
     * Schedules [action] for commit after [undoWindowMs]. If an earlier drop is still pending,
     * commits the displaced action immediately before starting the new countdown.
     */
    fun scheduleDrop(action: PendingDropAction) {
        val previous = _pending.value
        timerJob?.cancel()
        timerJob = null

        if (previous != null) {
            scope.launch {
                onCommit(previous)
            }
        }

        val scheduled = action.copy(
            createdAtMs = clockMs(),
            durationMs = undoWindowMs,
        )
        val token = ++generation
        _pending.value = scheduled

        timerJob = scope.launch {
            delay(undoWindowMs)
            if (generation == token && _pending.value?.trackId == scheduled.trackId) {
                _pending.value = null
                timerJob = null
                onCommit(scheduled)
            }
        }
    }

    /**
     * Cancels the currently pending drop (if any), restores UI via [onUndo], and commits nothing.
     * Returns `true` if a pending action was undone.
     */
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
     * Immediately commits any pending drop (e.g., on screen/app teardown or user dismiss).
     */
    suspend fun flushNow() {
        val current = _pending.value ?: return
        generation++
        timerJob?.cancel()
        timerJob = null
        _pending.value = null
        onCommit(current)
    }
}
