package org.soulbeet.corebindings

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
 */
object SoulbeetCoreProvider {
    private val lock = Any()

    @Volatile
    private var instance: CoreClient? = null

    /**
     * Returns the initialized [CoreClient], or `null` if the host app has not opened it yet.
     */
    fun getOrNull(): CoreClient? = instance

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
        synchronized(lock) {
            instance?.let { return it }
            val created = UniFFiCoreClient(SoulbeetCoreHandle.openSync(config))
            instance = created
            return created
        }
    }

    /**
     * Asynchronously opens and installs the shared [CoreClient] if not already initialized.
     */
    suspend fun open(config: FfiClientConfig): CoreClient {
        instance?.let { return it }
        val opened = UniFFiCoreClient(SoulbeetCoreHandle.open(config))
        var discardNew = false
        val winner = synchronized(lock) {
            val existing = instance
            if (existing != null) {
                discardNew = true
                existing
            } else {
                instance = opened
                opened
            }
        }
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
        synchronized(lock) {
            instance = client
        }
    }

    /**
     * Clears the active [CoreClient] reference, optionally closing it.
     */
    fun clear(closeExisting: Boolean = false) {
        val previous = synchronized(lock) {
            val prev = instance
            instance = null
            prev
        }
        if (closeExisting && previous != null) {
            runCatching { previous.close() }
        }
    }
}
