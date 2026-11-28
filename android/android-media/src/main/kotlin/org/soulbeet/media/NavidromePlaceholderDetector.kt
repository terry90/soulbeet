package org.soulbeet.media

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Color
import java.io.File
import java.io.FileInputStream
import kotlin.math.abs

/**
 * Detects Navidrome's built-in default placeholder images (`album-placeholder.webp` blue vinyl
 * and `artist-placeholder.webp` gray star) both by exact raw byte fingerprint and by visual
 * pixel sampling (catching resized/re-encoded placeholders from `/rest/getCoverArt?size=...`).
 *
 * Results are memoized in a bounded LRU map keyed by `(canonicalPath, lastModified, length)` so
 * repeated `getType()` + `openFile()` IPCs decode each image file at most once.
 */
object NavidromePlaceholderDetector {

    private const val NAVIDROME_ALBUM_PLACEHOLDER_LEN = 69_228L
    private const val NAVIDROME_ALBUM_PLACEHOLDER_FNV1A = -0x412f45e8056f69bdL // 0xbed0ba17fa909643
    private const val NAVIDROME_ARTIST_PLACEHOLDER_LEN = 546L
    private const val NAVIDROME_ARTIST_PLACEHOLDER_FNV1A = -0x5fd8a285e8f7cfd1L // 0xa0275d7a1708302f
    private const val MAX_CACHE_ENTRIES = 512

    private data class CacheKey(val path: String, val lastModified: Long, val length: Long)

    private val memoCache = object : LinkedHashMap<CacheKey, Boolean>(MAX_CACHE_ENTRIES, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<CacheKey, Boolean>?): Boolean =
            size > MAX_CACHE_ENTRIES
    }

    fun isPlaceholder(file: File): Boolean {
        if (!file.exists() || !file.isFile || !file.canRead()) return false

        val len = file.length()
        val key = CacheKey(file.path, file.lastModified(), len)
        synchronized(memoCache) {
            memoCache[key]?.let { return it }
        }

        val result = evaluatePlaceholder(file, len)
        synchronized(memoCache) {
            memoCache[key] = result
        }
        return result
    }

    private fun evaluatePlaceholder(file: File, len: Long): Boolean {
        if (len == NAVIDROME_ALBUM_PLACEHOLDER_LEN || len == NAVIDROME_ARTIST_PLACEHOLDER_LEN) {
            if (matchesRawPlaceholderHash(file, len)) return true
        }

        val bitmap = decodeSmallSample(file) ?: return false
        return try {
            matchesNavidromeBlueVinyl(bitmap) || matchesNavidromeGrayStar(bitmap)
        } finally {
            bitmap.recycle()
        }
    }

    internal fun matchesRawPlaceholderHash(file: File, expectedLen: Long): Boolean {
        val bytes = runCatching {
            FileInputStream(file).use { it.readBytes() }
        }.getOrNull() ?: return false
        if (bytes.size.toLong() != expectedLen) return false
        var hash = -0x340d631b7bdddcdbL // 0xcbf29ce484222325
        for (b in bytes) {
            hash = hash xor (b.toLong() and 0xFFL)
            hash *= 0x100000001b3L
        }
        return (expectedLen == NAVIDROME_ALBUM_PLACEHOLDER_LEN && hash == NAVIDROME_ALBUM_PLACEHOLDER_FNV1A) ||
            (expectedLen == NAVIDROME_ARTIST_PLACEHOLDER_LEN && hash == NAVIDROME_ARTIST_PLACEHOLDER_FNV1A)
    }

    private fun decodeSmallSample(file: File): Bitmap? = runCatching {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeFile(file.path, bounds)
        val w = bounds.outWidth
        val h = bounds.outHeight
        if (w <= 0 || h <= 0) return null

        var sample = 1
        while (w / (sample * 2) >= 64 && h / (sample * 2) >= 64) {
            sample *= 2
        }
        val opts = BitmapFactory.Options().apply {
            inSampleSize = sample
            inPreferredConfig = Bitmap.Config.ARGB_8888
        }
        BitmapFactory.decodeFile(file.path, opts)
    }.getOrNull()

    internal fun matchesNavidromeBlueVinyl(bmp: Bitmap): Boolean {
        val w = bmp.width
        val h = bmp.height
        if (w < 16 || h < 16) return false
        if (abs(w - h) > maxOf(w, h) / 10) return false

        fun sample(fx: Float, fy: Float): Int =
            bmp.getPixel(
                (w * fx).toInt().coerceIn(0, w - 1),
                (h * fy).toInt().coerceIn(0, h - 1),
            )

        fun isNavidromeBlue(px: Int): Boolean {
            val a = Color.alpha(px)
            val r = Color.red(px)
            val g = Color.green(px)
            val b = Color.blue(px)
            return a >= 235 && r <= 32 && g in 55..130 && b >= 170 && (b - g) >= 75
        }

        // All cardinal + outer vinyl points must match Navidrome's electric blue.
        val ringPoints = listOf(
            sample(0.50f, 0.14f),
            sample(0.50f, 0.22f),
            sample(0.20f, 0.50f),
            sample(0.80f, 0.50f),
            sample(0.50f, 0.80f),
        )
        if (!ringPoints.all(::isNavidromeBlue)) return false

        // Center spindle and inner label area must be dark charcoal.
        val centerPoints = listOf(
            sample(0.50f, 0.50f),
            sample(0.50f, 0.60f),
        )
        for (center in centerPoints) {
            if (Color.alpha(center) < 235 ||
                Color.red(center) > 70 ||
                Color.green(center) > 70 ||
                Color.blue(center) > 70
            ) {
                return false
            }
        }

        // All 4 corners must be transparent, pure black, or pure white (consistent matte).
        val corners = listOf(
            sample(0.02f, 0.02f),
            sample(0.98f, 0.02f),
            sample(0.02f, 0.98f),
            sample(0.98f, 0.98f),
        )
        val allTransparent = corners.all { Color.alpha(it) < 25 }
        val allBlack = corners.all {
            Color.alpha(it) >= 235 && Color.red(it) <= 18 && Color.green(it) <= 18 && Color.blue(it) <= 18
        }
        val allWhite = corners.all {
            Color.alpha(it) >= 235 && Color.red(it) >= 238 && Color.green(it) >= 238 && Color.blue(it) >= 238
        }
        return allTransparent || allBlack || allWhite
    }

    internal fun matchesNavidromeGrayStar(bmp: Bitmap): Boolean {
        val w = bmp.width
        val h = bmp.height
        if (w < 16 || h < 16) return false
        if (abs(w - h) > maxOf(w, h) / 10) return false

        fun sample(fx: Float, fy: Float): Int =
            bmp.getPixel(
                (w * fx).toInt().coerceIn(0, w - 1),
                (h * fy).toInt().coerceIn(0, h - 1),
            )

        fun luminance(px: Int): Int = (Color.red(px) + Color.green(px) + Color.blue(px)) / 3

        fun isUniformLightGray(px: Int): Boolean {
            val a = Color.alpha(px)
            val r = Color.red(px)
            val g = Color.green(px)
            val b = Color.blue(px)
            return a >= 235 &&
                r in 224..242 &&
                g in 224..242 &&
                b in 224..242 &&
                abs(r - g) <= 5 &&
                abs(g - b) <= 5
        }

        fun isNearWhite(px: Int): Boolean =
            Color.alpha(px) >= 235 &&
                Color.red(px) >= 248 &&
                Color.green(px) >= 248 &&
                Color.blue(px) >= 248

        // Corners, outer edges, and concave star valleys must be flat light gray.
        val bgPoints = listOf(
            sample(0.04f, 0.04f),
            sample(0.96f, 0.04f),
            sample(0.04f, 0.96f),
            sample(0.96f, 0.96f),
            sample(0.28f, 0.25f),
            sample(0.72f, 0.25f),
            sample(0.18f, 0.72f),
            sample(0.82f, 0.72f),
            sample(0.50f, 0.88f),
        )
        if (!bgPoints.all(::isUniformLightGray)) return false

        val center = sample(0.50f, 0.50f)
        val topPoint = sample(0.50f, 0.28f)
        if (!isNearWhite(center) || !isNearWhite(topPoint)) return false

        // Require genuine foreground/background contrast (>= 10 levels) to exclude flat off-white covers.
        val avgBg = bgPoints.map(::luminance).average()
        return luminance(center) - avgBg >= 10.0
    }
}
