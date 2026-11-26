package org.soulbeet.corebindings

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withTimeoutOrNull
import org.soulbeet.core.FfiClientConfig
import org.soulbeet.core.SoulbeetCoreHandle
import org.soulbeet.core.SoulbeetCoreHandleInterface

/**
 * Idiomatic Kotlin facade over UniFFI's [SoulbeetCoreHandleInterface].
 *
 * Production Android code uses [UniFFiCoreClient] backed by the real Rust [SoulbeetCoreHandle];
 * JVM unit tests in `:android-media` and `:app` can supply a lightweight fake implementing
 * [CoreClient] without loading native `.so` libraries.
 */
interface CoreClient : SoulbeetCoreHandleInterface, AutoCloseable

/**
 * Production [CoreClient] backed by a live Rust [SoulbeetCoreHandle].
 */
class UniFFiCoreClient(
    val handle: SoulbeetCoreHandle,
) : CoreClient, SoulbeetCoreHandleInterface by handle {
    override fun close() {
        handle.close()
    }
}

/**
 * Process-wide holder for the shared [CoreClient] instance so `SoulbeetMediaLibraryService`,
 * `CoverArtContentProvider`, `OfflineSyncWorker`, and the Jetpack Compose `:app` UI all share
 * one single SQLite pool, offline cache, and Connect WebSocket handle.
 *
 * Supports cold-start synchronization via [registerInitializer] and [awaitOrNull] so headless
 * Android Auto or WorkManager wakeups cleanly await `Application.onCreate` initialization instead
 * of failing on a transient `null`.
 */
object SoulbeetCoreProvider {
    private val lock = Any()
    private val initMutex = Mutex()

    @Volatile
    private var instance: CoreClient? = null

    @Volatile
    private var initializer: (suspend () -> CoreClient?)? = null

    @Volatile
    private var explicitlyUnconfigured: Boolean = false

    @Volatile
    private var readyDeferred: CompletableDeferred<CoreClient?> = CompletableDeferred()

    /**
     * Returns the initialized [CoreClient], or `null` if the host app has not opened it yet.
     */
    fun getOrNull(): CoreClient? = instance

    /**
     * Returns `true` if the host app explicitly declared that no user/server credentials are
     * configured (e.g., logged out), distinguishing unconfigured state from transient cold start.
     */
    fun isExplicitlyUnconfigured(): Boolean = explicitlyUnconfigured

    /**
     * Registers a lazy cold-start initializer invoked on demand by [awaitOrNull] when no
     * [CoreClient] has been installed yet.
     */
    fun registerInitializer(block: suspend () -> CoreClient?) {
        synchronized(lock) {
            initializer = block
            explicitlyUnconfigured = false
        }
    }

    /**
     * Marks the provider as explicitly unconfigured (e.g., user logged out) and unblocks any
     * callers waiting in [awaitOrNull] with `null`.
     */
    fun markUnconfigured() {
        val deferred = synchronized(lock) {
            explicitlyUnconfigured = true
            readyDeferred
        }
        deferred.complete(null)
    }

    /**
     * Awaits the initialized [CoreClient] up to [timeoutMs].
     *
     * - Returns immediately if already installed.
     * - Invokes any registered [registerInitializer] once (coalescing concurrent callers).
     * - Otherwise awaits completion of the startup readiness signal up to [timeoutMs].
     */
    suspend fun awaitOrNull(timeoutMs: Long = DEFAULT_AWAIT_TIMEOUT_MS): CoreClient? {
        instance?.let { return it }
        if (explicitlyUnconfigured) return null

        val initBlock = initializer
        if (initBlock != null) {
            return withTimeoutOrNull(timeoutMs) {
                initMutex.withLock {
                    instance?.let { return@withLock it }
                    if (explicitlyUnconfigured) return@withLock null
                    val created = initBlock()
                    if (created != null) {
                        install(created)
                        created
                    } else {
                        null
                    }
                }
            }
        }

        val deferred = readyDeferred
        return withTimeoutOrNull(timeoutMs) {
            deferred.await()
        } ?: instance
    }

    /**
     * Returns the initialized [CoreClient], throwing [IllegalStateException] if not initialized.
     */
    fun requireInstance(): CoreClient =
        instance ?: throw IllegalStateException(
            "SoulbeetCoreProvider has not been initialized. Call open(), openSync(), or install() first.",
        )

    /**
     * Synchronously opens and installs the shared [CoreClient] if not already initialized.
     * Safe to invoke from `Application.onCreate` or `ContentProvider` initialization paths.
     */
    fun openSync(config: FfiClientConfig): CoreClient {
        instance?.let { return it }
        val (result, deferred) = synchronized(lock) {
            instance?.let { return it }
            val created = UniFFiCoreClient(SoulbeetCoreHandle.openSync(config))
            instance = created
            explicitlyUnconfigured = false
            created to readyDeferred
        }
        deferred.complete(result)
        return result
    }

    /**
     * Asynchronously opens and installs the shared [CoreClient] if not already initialized.
     */
    suspend fun open(config: FfiClientConfig): CoreClient {
        instance?.let { return it }
        val opened = UniFFiCoreClient(SoulbeetCoreHandle.open(config))
        var discardNew = false
        val (winner, deferred) = synchronized(lock) {
            val existing = instance
            if (existing != null) {
                discardNew = true
                existing to readyDeferred
            } else {
                instance = opened
                explicitlyUnconfigured = false
                opened to readyDeferred
            }
        }
        deferred.complete(winner)
        if (discardNew) {
            runCatching { opened.shutdown() }
            runCatching { opened.close() }
        }
        return winner
    }

    /**
     * Installs an explicit [CoreClient] (used by dependency injection and unit tests).
     */
    fun install(client: CoreClient) {
        val deferred = synchronized(lock) {
            instance = client
            explicitlyUnconfigured = false
            readyDeferred
        }
        deferred.complete(client)
    }

    /**
     * Clears the active [CoreClient] reference, optionally closing it, and resets cold-start state.
     */
    fun clear(closeExisting: Boolean = false) {
        val previous = synchronized(lock) {
            val prev = instance
            instance = null
            initializer = null
            explicitlyUnconfigured = false
            readyDeferred = CompletableDeferred()
            prev
        }
        if (closeExisting && previous != null) {
            runCatching { previous.close() }
        }
    }

    const val DEFAULT_AWAIT_TIMEOUT_MS: Long = 5_000L
}
