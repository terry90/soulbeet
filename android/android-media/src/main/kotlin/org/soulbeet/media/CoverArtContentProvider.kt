package org.soulbeet.media

import android.content.ContentProvider
import android.content.ContentValues
import android.content.Context
import android.database.Cursor
import android.net.Uri
import android.os.ParcelFileDescriptor
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.soulbeet.corebindings.CoreClient
import org.soulbeet.corebindings.SoulbeetCoreProvider
import java.io.File
import java.io.FileInputStream
import java.io.FileNotFoundException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Semaphore

/**
 * Read-only [ContentProvider] serving locally cached cover art and procedural Midnight Studio
 * sleeves to Android Auto head units and system media notifications:
 *
 * - `content://<authority>/sleeve/<slug>[?label=<genre>]` -> Midnight Studio Mix Sleeve PNG
 * - `content://<authority>/tile/<kind>[?seed=<seed>]` -> Soulbeet branded fallback PNG
 * - `content://<authority>/art/<coverArtId>[?fallback=<kind>&seed=<seed>]` -> cached real cover art
 *   (rejecting Navidrome placeholders; bounded concurrency on-demand fetch with instant fallback so
 *   Android Auto never starves Binder threads and never renders broken/warning tiles).
 */
class CoverArtContentProvider : ContentProvider() {

    /**
     * Injectable [CoreClient] lookup hook (defaults to [SoulbeetCoreProvider.getOrNull]) for tests.
     */
    internal var coreClientProvider: () -> CoreClient? = { SoulbeetCoreProvider.getOrNull() }

    /**
     * Canonical directory jail for `/art/<id>` lookups. Initialized in [onCreate] to
     * `cacheDir/cover_art` in production; overridable in unit tests.
     */
    internal var allowedCacheRoot: File? = null

    /**
     * Enables bounded on-demand fetch on Binder threads when Android Auto requests an uncached
     * `/art/<id>?fallback=...` URI while online.
     */
    internal var allowBoundedOnlineFetch: Boolean = true

    /**
     * Allows a short cold-start wait when `CoreClient` is still opening SQLite during car ignition.
     */
    internal var allowColdStartAwait: Boolean = true

    override fun onCreate(): Boolean {
        if (allowedCacheRoot == null) {
            context?.cacheDir?.let { allowedCacheRoot = File(it, "cover_art") }
        }
        return true
    }

    override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor {
        if (mode != "r") {
            throw SecurityException("CoverArtContentProvider is read-only; rejected mode '$mode'")
        }
        val file = resolveUriToFile(uri, allowFetch = allowBoundedOnlineFetch)
            ?: throw FileNotFoundException("Cover art not cached locally for URI: $uri")
        return ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY)
    }

    override fun getType(uri: Uri): String? {
        if (parseSleeveSlug(uri) != null || parseTileKind(uri) != null) {
            return "image/png"
        }
        val file = resolveCachedArtFile(uri)
        if (file != null) {
            return detectImageMimeType(file)
        }
        if (parseCoverArtId(uri) != null && !uri.getQueryParameter(QUERY_FALLBACK_KIND).isNullOrBlank()) {
            return "image/png"
        }
        return null
    }

    override fun query(
        uri: Uri,
        projection: Array<out String>?,
        selection: String?,
        selectionArgs: Array<out String>?,
        sortOrder: String?,
    ): Cursor? = null

    override fun insert(uri: Uri, values: ContentValues?): Uri {
        throw UnsupportedOperationException("CoverArtContentProvider is read-only")
    }

    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int {
        throw UnsupportedOperationException("CoverArtContentProvider is read-only")
    }

    override fun update(
        uri: Uri,
        values: ContentValues?,
        selection: String?,
        selectionArgs: Array<out String>?,
    ): Int {
        throw UnsupportedOperationException("CoverArtContentProvider is read-only")
    }

    private fun resolveCoreClient(): CoreClient? {
        coreClientProvider()?.let { return it }
        if (!allowColdStartAwait || SoulbeetCoreProvider.isExplicitlyUnconfigured()) {
            return null
        }
        return runCatching {
            runBlocking(Dispatchers.IO) {
                SoulbeetCoreProvider.awaitOrNull(COLD_START_AWAIT_MS)
            }
        }.getOrNull()
    }

    private fun resolveUriToFile(uri: Uri, allowFetch: Boolean): File? {
        val ctx = context

        // 1. Procedural Midnight Studio Mix Sleeve (`content://<authority>/sleeve/<slug>[?label=...]`)
        parseSleeveSlug(uri)?.let { slug ->
            val appCtx = ctx?.applicationContext ?: return null
            val label = uri.getQueryParameter(QUERY_LABEL)
            return AutoArtworkRenderer.renderOrGetCachedSleeve(appCtx, slug, label)
        }

        // 2. Procedural Branded Fallback Tile (`content://<authority>/tile/<kind>[?seed=...]`)
        parseTileKind(uri)?.let { kind ->
            val appCtx = ctx?.applicationContext ?: return null
            val seed = uri.getQueryParameter(QUERY_FALLBACK_SEED)
                ?: uri.pathSegments.getOrNull(2)
                ?: kind
            return AutoArtworkRenderer.renderOrGetCachedFallback(appCtx, kind, seed)
        }

        // 3. Cached cover art (`content://<authority>/art/<coverArtId>[?fallback=<kind>&seed=<seed>]`)
        val coverArtId = parseCoverArtId(uri) ?: return null
        val fallbackKind = uri.getQueryParameter(QUERY_FALLBACK_KIND)?.trim()?.takeIf { it.isNotEmpty() }
        val fallbackSeed = uri.getQueryParameter(QUERY_FALLBACK_SEED)?.trim().orEmpty().ifEmpty { coverArtId }

        val core = resolveCoreClient()
        if (core != null) {
            resolveCachedArtFileWithCore(core, coverArtId)?.let { return it }

            // If online and this request permits bounded fetch, wait briefly (capped concurrency so
            // Binder threads never starve on large grids or in tunnels).
            if (allowFetch && fallbackKind != null && !core.isOfflineOnly()) {
                val fetched = fetchArtBoundedSingleFlight(core, coverArtId)
                if (fetched != null && isValidNonPlaceholderFile(fetched)) {
                    return fetched
                }
            }
        }

        // Never fail Android Auto artwork loads with FileNotFoundException (which renders a warning/broken icon).
        if (fallbackKind != null && ctx != null) {
            return AutoArtworkRenderer.renderOrGetCachedFallback(ctx.applicationContext, fallbackKind, fallbackSeed)
        }

        return null
    }

    internal fun resolveCachedArtFile(uri: Uri): File? {
        val coverArtId = parseCoverArtId(uri) ?: return null
        val core = coreClientProvider() ?: return null
        return resolveCachedArtFileWithCore(core, coverArtId)
    }

    private fun resolveCachedArtFileWithCore(core: CoreClient, coverArtId: String): File? {
        val rawPath = runCatching { core.coverArtLocalPathSync(coverArtId) }.getOrNull()
            ?.takeIf { it.isNotBlank() }
            ?: return null

        val canonicalFile = runCatching { File(rawPath).canonicalFile }.getOrNull() ?: return null
        if (!isValidNonPlaceholderFile(canonicalFile)) {
            return null
        }
        return canonicalFile
    }

    private fun isValidNonPlaceholderFile(canonicalFile: File): Boolean {
        if (!canonicalFile.exists() || !canonicalFile.isFile || !canonicalFile.canRead()) {
            return false
        }
        val root = allowedCacheRoot?.let { runCatching { it.canonicalFile }.getOrNull() }
        if (root != null) {
            val rootPath = root.path.trimEnd(File.separatorChar) + File.separatorChar
            if (!canonicalFile.path.startsWith(rootPath)) {
                return false
            }
        }
        if (NavidromePlaceholderDetector.isPlaceholder(canonicalFile)) {
            return false
        }
        return true
    }

    private fun fetchArtBoundedSingleFlight(core: CoreClient, coverArtId: String): File? {
        val failedAt = recentPlaceholderOrFailureMs[coverArtId]
        if (failedAt != null && (System.currentTimeMillis() - failedAt) < NEGATIVE_CACHE_TTL_MS) {
            return null
        }

        // Non-blocking semaphore prevents Binder thread-pool starvation when AA opens 16-24 uncached tiles at once.
        if (!onlineFetchPermits.tryAcquire()) {
            return null
        }
        try {
            val stripe = fetchStripes[(coverArtId.hashCode() and 0x7FFFFFFF) % FETCH_LOCK_STRIPES]
            return synchronized(stripe) {
                val existing = runCatching { core.coverArtLocalPathSync(coverArtId) }.getOrNull()
                    ?.takeIf { it.isNotBlank() }
                    ?.let { runCatching { File(it).canonicalFile }.getOrNull() }
                if (existing != null && existing.exists() && existing.isFile) {
                    if (NavidromePlaceholderDetector.isPlaceholder(existing)) {
                        recordNegativeCache(coverArtId)
                        return@synchronized null
                    }
                    return@synchronized existing
                }

                val path = runCatching {
                    runBlocking(Dispatchers.IO) {
                        withTimeoutOrNull(ONLINE_FETCH_TIMEOUT_MS) {
                            core.getOrFetchCoverArt(coverArtId, ON_DEMAND_ART_SIZE_PX)
                        }
                    }
                }.getOrNull()

                val file = path?.takeIf { it.isNotBlank() }?.let { runCatching { File(it).canonicalFile }.getOrNull() }
                if (file == null || NavidromePlaceholderDetector.isPlaceholder(file)) {
                    recordNegativeCache(coverArtId)
                    null
                } else {
                    recentPlaceholderOrFailureMs.remove(coverArtId)
                    file
                }
            }
        } finally {
            onlineFetchPermits.release()
        }
    }

    private fun recordNegativeCache(coverArtId: String) {
        if (recentPlaceholderOrFailureMs.size >= MAX_NEGATIVE_CACHE_ENTRIES) {
            recentPlaceholderOrFailureMs.clear()
        }
        recentPlaceholderOrFailureMs[coverArtId] = System.currentTimeMillis()
    }

    companion object {
        const val ART_PATH_SEGMENT: String = "art"
        const val SLEEVE_PATH_SEGMENT: String = "sleeve"
        const val TILE_PATH_SEGMENT: String = "tile"
        const val QUERY_FALLBACK_KIND: String = "fallback"
        const val QUERY_FALLBACK_SEED: String = "seed"
        const val QUERY_LABEL: String = "label"

        private const val COLD_START_AWAIT_MS: Long = 300L
        private const val ONLINE_FETCH_TIMEOUT_MS: Long = 2_000L
        private const val ON_DEMAND_ART_SIZE_PX: UInt = 512u
        private const val NEGATIVE_CACHE_TTL_MS: Long = 120_000L
        private const val MAX_NEGATIVE_CACHE_ENTRIES: Int = 256
        private const val MAX_CONCURRENT_BINDER_FETCHES: Int = 3
        private const val FETCH_LOCK_STRIPES: Int = 32

        private val onlineFetchPermits = Semaphore(MAX_CONCURRENT_BINDER_FETCHES)
        private val fetchStripes = Array(FETCH_LOCK_STRIPES) { Any() }
        private val recentPlaceholderOrFailureMs = ConcurrentHashMap<String, Long>()

        fun authorityFor(context: Context): String = "${context.packageName}.art"

        fun uriFor(
            authority: String,
            coverArtId: String,
            fallbackKind: String? = null,
            fallbackSeed: String? = null,
        ): Uri =
            Uri.Builder()
                .scheme("content")
                .authority(authority)
                .appendPath(ART_PATH_SEGMENT)
                .appendPath(coverArtId)
                .apply {
                    if (!fallbackKind.isNullOrBlank()) {
                        appendQueryParameter(QUERY_FALLBACK_KIND, fallbackKind)
                    }
                    if (!fallbackSeed.isNullOrBlank()) {
                        appendQueryParameter(QUERY_FALLBACK_SEED, fallbackSeed.take(64))
                    }
                }
                .build()

        fun sleeveUriFor(authority: String, slug: String): Uri {
            val trimmed = slug.trim()
            if (trimmed.startsWith("genre:", ignoreCase = true)) {
                val label = trimmed.substringAfter(':').trim()
                return Uri.Builder()
                    .scheme("content")
                    .authority(authority)
                    .appendPath(SLEEVE_PATH_SEGMENT)
                    .appendPath("genre")
                    .apply {
                        if (label.isNotEmpty()) {
                            appendQueryParameter(QUERY_LABEL, label.take(64))
                        }
                    }
                    .build()
            }
            return Uri.Builder()
                .scheme("content")
                .authority(authority)
                .appendPath(SLEEVE_PATH_SEGMENT)
                .appendPath(trimmed)
                .build()
        }

        fun tileUriFor(authority: String, kind: String, seed: String): Uri =
            Uri.Builder()
                .scheme("content")
                .authority(authority)
                .appendPath(TILE_PATH_SEGMENT)
                .appendPath(kind)
                .apply {
                    val cleanSeed = seed.trim().take(64)
                    if (cleanSeed.isNotEmpty()) {
                        appendQueryParameter(QUERY_FALLBACK_SEED, cleanSeed)
                    }
                }
                .build()

        /**
         * Validates and extracts the coverArtId from `content://<authority>/art/<coverArtId>`.
         * Rejects blank IDs, nested segments, path separators, NUL bytes, and `..` traversal.
         */
        fun parseCoverArtId(uri: Uri): String? {
            if (uri.scheme != "content") return null
            val segments = uri.pathSegments
            if (segments.size != 2 || segments[0] != ART_PATH_SEGMENT) return null
            return sanitizePathSegment(segments[1])
        }

        fun parseSleeveSlug(uri: Uri): String? {
            if (uri.scheme != "content") return null
            val segments = uri.pathSegments
            if (segments.size != 2 || segments[0] != SLEEVE_PATH_SEGMENT) return null
            return sanitizePathSegment(segments[1])
        }

        fun parseTileKind(uri: Uri): String? {
            if (uri.scheme != "content") return null
            val segments = uri.pathSegments
            if (segments.size !in 2..3 || segments[0] != TILE_PATH_SEGMENT) return null
            val kind = sanitizePathSegment(segments[1]) ?: return null
            if (segments.size == 3 && sanitizePathSegment(segments[2]) == null) return null
            return kind
        }

        private fun sanitizePathSegment(raw: String): String? {
            if (raw.isBlank()) return null
            if (raw == "." || raw == ".." || raw.contains("..")) return null
            if (raw.contains('/') || raw.contains('\\') || raw.contains('\u0000')) return null
            return raw
        }

        internal fun detectImageMimeType(file: File): String {
            when (file.extension.lowercase()) {
                "png" -> return "image/png"
                "webp" -> return "image/webp"
                "jpg", "jpeg" -> return "image/jpeg"
            }
            val header = ByteArray(12)
            val bytesRead = runCatching {
                FileInputStream(file).use { it.read(header) }
            }.getOrDefault(0)

            if (bytesRead >= 3 &&
                header[0] == 0xFF.toByte() &&
                header[1] == 0xD8.toByte() &&
                header[2] == 0xFF.toByte()
            ) {
                return "image/jpeg"
            }
            if (bytesRead >= 8 &&
                header[0] == 0x89.toByte() &&
                header[1] == 0x50.toByte() &&
                header[2] == 0x4E.toByte() &&
                header[3] == 0x47.toByte()
            ) {
                return "image/png"
            }
            if (bytesRead >= 12 &&
                header[0] == 'R'.code.toByte() &&
                header[1] == 'I'.code.toByte() &&
                header[2] == 'F'.code.toByte() &&
                header[3] == 'F'.code.toByte() &&
                header[8] == 'W'.code.toByte() &&
                header[9] == 'E'.code.toByte() &&
                header[10] == 'B'.code.toByte() &&
                header[11] == 'P'.code.toByte()
            ) {
                return "image/webp"
            }
            return "image/jpeg"
        }
    }
}
