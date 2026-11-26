package org.soulbeet.media

import android.content.ContentProvider
import android.content.ContentValues
import android.content.Context
import android.database.Cursor
import android.net.Uri
import android.os.ParcelFileDescriptor
import org.soulbeet.corebindings.CoreClient
import org.soulbeet.corebindings.SoulbeetCoreProvider
import java.io.File
import java.io.FileInputStream
import java.io.FileNotFoundException

/**
 * Read-only [ContentProvider] serving locally cached cover art to Android Auto head units and
 * system media notifications via `content://<authority>/art/<coverArtId>`.
 *
 * Strictly performs ZERO network calls, ZERO coroutines, and ZERO SQLite writes so it is safe
 * to invoke synchronously on Android Auto Binder threads.
 */
class CoverArtContentProvider : ContentProvider() {

    /**
     * Injectable [CoreClient] lookup hook (defaults to [SoulbeetCoreProvider.getOrNull]) for tests.
     */
    internal var coreClientProvider: () -> CoreClient? = { SoulbeetCoreProvider.getOrNull() }

    /**
     * Optional canonical directory jail. When non-null, resolved files must reside inside this root.
     */
    internal var allowedCacheRoot: File? = null

    override fun onCreate(): Boolean = true

    override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor {
        if (mode != "r") {
            throw SecurityException("CoverArtContentProvider is read-only; rejected mode '$mode'")
        }
        val file = resolveCachedArtFile(uri)
            ?: throw FileNotFoundException("Cover art not cached locally for URI: $uri")
        return ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY)
    }

    override fun getType(uri: Uri): String? {
        val file = resolveCachedArtFile(uri) ?: return null
        return detectImageMimeType(file)
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

    internal fun resolveCachedArtFile(uri: Uri): File? {
        val coverArtId = parseCoverArtId(uri) ?: return null
        val core = coreClientProvider() ?: return null
        val rawPath = runCatching { core.coverArtLocalPathSync(coverArtId) }.getOrNull()
            ?.takeIf { it.isNotBlank() }
            ?: return null

        val canonicalFile = runCatching { File(rawPath).canonicalFile }.getOrNull() ?: return null
        if (!canonicalFile.exists() || !canonicalFile.isFile || !canonicalFile.canRead()) {
            return null
        }

        val root = allowedCacheRoot?.let { runCatching { it.canonicalFile }.getOrNull() }
        if (root != null) {
            val rootPath = root.path.trimEnd(File.separatorChar) + File.separatorChar
            if (!canonicalFile.path.startsWith(rootPath)) {
                return null
            }
        }
        return canonicalFile
    }

    companion object {
        const val ART_PATH_SEGMENT: String = "art"

        fun authorityFor(context: Context): String = "${context.packageName}.art"

        fun uriFor(authority: String, coverArtId: String): Uri =
            Uri.Builder()
                .scheme("content")
                .authority(authority)
                .appendPath(ART_PATH_SEGMENT)
                .appendPath(coverArtId)
                .build()

        /**
         * Validates and extracts the coverArtId from `content://<authority>/art/<coverArtId>`.
         * Rejects blank IDs, nested segments, path separators, NUL bytes, and `..` traversal.
         */
        fun parseCoverArtId(uri: Uri): String? {
            if (uri.scheme != "content") return null
            val segments = uri.pathSegments
            if (segments.size != 2 || segments[0] != ART_PATH_SEGMENT) return null
            val id = segments[1]
            if (id.isBlank()) return null
            if (id == "." || id == ".." || id.contains("..")) return null
            if (id.contains('/') || id.contains('\\') || id.contains('\u0000')) return null
            return id
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
