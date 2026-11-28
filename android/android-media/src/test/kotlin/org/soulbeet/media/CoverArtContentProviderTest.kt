package org.soulbeet.media

import android.content.ContentValues
import android.net.Uri
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.FileNotFoundException

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class CoverArtContentProviderTest {

    @get:Rule
    val tempFolder = TemporaryFolder()

    private val authority = "org.soulbeet.app.art"

    @Test
    fun servesCachedFileReadOnlyAndDetectsMimeType() {
        val pngFile = tempFolder.newFile("al-123.png").apply {
            writeBytes(byteArrayOf(0x89.toByte(), 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A))
        }
        val core = FakeCoreClient().apply {
            coverArtPaths["al-123"] = pngFile.absolutePath
        }
        val provider = CoverArtContentProvider().apply {
            coreClientProvider = { core }
            allowedCacheRoot = tempFolder.root
        }

        val uri = CoverArtContentProvider.uriFor(authority, "al-123")
        assertEquals("image/png", provider.getType(uri))

        val pfd = provider.openFile(uri, "r")
        assertNotNull(pfd)
        pfd.close()
    }

    @Test(expected = FileNotFoundException::class)
    fun throwsFileNotFoundWhenUncached() {
        val core = FakeCoreClient()
        val provider = CoverArtContentProvider().apply {
            coreClientProvider = { core }
        }
        provider.openFile(CoverArtContentProvider.uriFor(authority, "missing"), "r")
    }

    @Test(expected = SecurityException::class)
    fun rejectsWriteModeOnOpenFile() {
        val provider = CoverArtContentProvider()
        provider.openFile(CoverArtContentProvider.uriFor(authority, "any"), "rw")
    }

    @Test
    fun rejectsPathTraversalUris() {
        assertNull(CoverArtContentProvider.parseCoverArtId(Uri.parse("content://$authority/art/..")))
        assertNull(CoverArtContentProvider.parseCoverArtId(Uri.parse("content://$authority/art/..%2Fsecret")))
        assertNull(CoverArtContentProvider.parseCoverArtId(Uri.parse("content://$authority/art/a/b")))
        assertNull(CoverArtContentProvider.parseCoverArtId(Uri.parse("content://$authority/other/123")))
        assertEquals("al-42", CoverArtContentProvider.parseCoverArtId(CoverArtContentProvider.uriFor(authority, "al-42")))
        assertEquals("drive", CoverArtContentProvider.parseSleeveSlug(CoverArtContentProvider.sleeveUriFor(authority, "drive")))
        assertNull(CoverArtContentProvider.parseSleeveSlug(Uri.parse("content://$authority/sleeve/..")))
        assertEquals("playlist", CoverArtContentProvider.parseTileKind(CoverArtContentProvider.tileUriFor(authority, "playlist", "My Mix")))
        assertNull(CoverArtContentProvider.parseTileKind(Uri.parse("content://$authority/tile/playlist/..")))

        // Real music titles and compound genres containing `/` and `..` must parse cleanly via query params.
        val acdcTile = CoverArtContentProvider.tileUriFor(authority, "artist", "AC/DC")
        assertEquals("artist", CoverArtContentProvider.parseTileKind(acdcTile))
        assertEquals("AC/DC", acdcTile.getQueryParameter(CoverArtContentProvider.QUERY_FALLBACK_SEED))

        val justiceTile = CoverArtContentProvider.tileUriFor(authority, "album", "...And Justice for All")
        assertEquals("album", CoverArtContentProvider.parseTileKind(justiceTile))
        assertEquals("...And Justice for All", justiceTile.getQueryParameter(CoverArtContentProvider.QUERY_FALLBACK_SEED))

        val rnbSleeve = CoverArtContentProvider.sleeveUriFor(authority, "genre:R&B/Soul")
        assertEquals("genre", CoverArtContentProvider.parseSleeveSlug(rnbSleeve))
        assertEquals("R&B/Soul", rnbSleeve.getQueryParameter(CoverArtContentProvider.QUERY_LABEL))
    }

    @Test
    fun rendersSleeveAndFallbackTileUrisAndDetectsNavidromePlaceholder() {
        val ctx = androidx.test.core.app.ApplicationProvider.getApplicationContext<android.content.Context>()
        val sleeveFile = AutoArtworkRenderer.renderOrGetCachedSleeve(ctx, "drive")
        assertNotNull(sleeveFile)
        assertEquals(true, sleeveFile!!.exists() && sleeveFile.length() > 0L)

        // Unknown slugs must collapse to the single shared fallback file (no unbounded disk DoS).
        val unknownA = AutoArtworkRenderer.renderOrGetCachedSleeve(ctx, "attacker-slug-1")
        val unknownB = AutoArtworkRenderer.renderOrGetCachedSleeve(ctx, "attacker-slug-2")
        assertEquals(unknownA?.name, unknownB?.name)

        val fallbackFile = AutoArtworkRenderer.renderOrGetCachedFallback(ctx, "playlist", "Road Trip")
        assertNotNull(fallbackFile)
        assertEquals(true, fallbackFile!!.exists() && fallbackFile.length() > 0L)

        // Synthetic Navidrome blue-vinyl bitmap must be detected as a placeholder.
        val blueVinyl = android.graphics.Bitmap.createBitmap(64, 64, android.graphics.Bitmap.Config.ARGB_8888)
        for (y in 0 until 64) {
            for (x in 0 until 64) {
                val dx = x - 32
                val dy = y - 32
                val distSq = dx * dx + dy * dy
                val color = when {
                    distSq <= 10 * 10 -> android.graphics.Color.rgb(34, 33, 29)
                    distSq <= 28 * 28 -> android.graphics.Color.rgb(1, 88, 211)
                    else -> android.graphics.Color.TRANSPARENT
                }
                blueVinyl.setPixel(x, y, color)
            }
        }
        assertEquals(true, NavidromePlaceholderDetector.matchesNavidromeBlueVinyl(blueVinyl))
        blueVinyl.recycle()

        // Synthetic Navidrome gray-star bitmap must be detected as a placeholder.
        val grayStar = android.graphics.Bitmap.createBitmap(64, 64, android.graphics.Bitmap.Config.ARGB_8888)
        for (y in 0 until 64) {
            for (x in 0 until 64) {
                val dx = x - 32
                val dy = y - 32
                val isCenter = dx * dx + dy * dy <= 14 * 14
                val isTopPoint = x in 28..36 && y in 14..32
                val color = if (isCenter || isTopPoint) {
                    android.graphics.Color.WHITE
                } else {
                    android.graphics.Color.rgb(235, 235, 235)
                }
                grayStar.setPixel(x, y, color)
            }
        }
        assertEquals(true, NavidromePlaceholderDetector.matchesNavidromeGrayStar(grayStar))
        grayStar.recycle()

        // Uniform off-white album cover (#F6F6F6) must NOT falsely trigger the gray-star detector.
        val offWhiteCover = android.graphics.Bitmap.createBitmap(64, 64, android.graphics.Bitmap.Config.ARGB_8888)
        for (y in 0 until 64) {
            for (x in 0 until 64) {
                offWhiteCover.setPixel(x, y, android.graphics.Color.rgb(246, 246, 246))
            }
        }
        assertEquals(false, NavidromePlaceholderDetector.matchesNavidromeGrayStar(offWhiteCover))
        offWhiteCover.recycle()
    }

    @Test(expected = UnsupportedOperationException::class)
    fun insertThrowsUnsupportedOperation() {
        CoverArtContentProvider().insert(CoverArtContentProvider.uriFor(authority, "x"), ContentValues())
    }

    @Test(expected = UnsupportedOperationException::class)
    fun updateThrowsUnsupportedOperation() {
        CoverArtContentProvider().update(CoverArtContentProvider.uriFor(authority, "x"), ContentValues(), null, null)
    }

    @Test(expected = UnsupportedOperationException::class)
    fun deleteThrowsUnsupportedOperation() {
        CoverArtContentProvider().delete(CoverArtContentProvider.uriFor(authority, "x"), null, null)
    }
}
