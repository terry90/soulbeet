package org.soulbeet.media

import android.content.ContentValues
import android.graphics.Bitmap
import android.graphics.Color
import android.net.Uri
import kotlinx.coroutines.delay
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.soulbeet.core.FfiException
import java.io.File
import java.io.FileNotFoundException
import java.io.FileOutputStream
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class CoverArtContentProviderTest {

    @get:Rule
    val tempFolder = TemporaryFolder()

    private val authority = "org.soulbeet.app.art"

    @Before
    fun setUp() {
        CoverArtContentProvider.clearAllNegativeCache()
    }

    @After
    fun tearDown() {
        CoverArtContentProvider.clearAllNegativeCache()
    }

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

    @Test
    fun barePhoneUriTriggersOnlineFetchAndServesRealArt() {
        val fetchedPng = createRealPngFile("phone-art-1.png")
        var fetchCalls = 0
        val core = FakeCoreClient().apply {
            getOrFetchCoverArtHandler = { id, _ ->
                fetchCalls++
                assertEquals("phone-art-1", id)
                coverArtPaths[id] = fetchedPng.absolutePath
                fetchedPng.absolutePath
            }
        }
        val provider = CoverArtContentProvider().apply {
            coreClientProvider = { core }
            allowedCacheRoot = tempFolder.root
            allowColdStartAwait = false
        }

        // Bare Phone URI without ?fallback=... must still trigger bounded on-demand fetch.
        val uri = CoverArtContentProvider.uriFor(authority, "phone-art-1")
        assertNull(uri.getQueryParameter(CoverArtContentProvider.QUERY_FALLBACK_KIND))

        val pfd = provider.openFile(uri, "r")
        assertNotNull(pfd)
        pfd.close()
        assertEquals(1, fetchCalls)
    }

    @Test
    fun transientErrorAndTimeoutDoNotEnterNegativeCacheWhilePlaceholderDoes() {
        val realPng = createRealPngFile("transient-real.png")
        var ioAttempt = 0
        val ioCore = FakeCoreClient().apply {
            getOrFetchCoverArtHandler = { id, _ ->
                ioAttempt++
                if (ioAttempt == 1) {
                    throw FfiException.Io("temporary network reset")
                }
                coverArtPaths[id] = realPng.absolutePath
                realPng.absolutePath
            }
        }
        val ioProvider = CoverArtContentProvider().apply {
            coreClientProvider = { ioCore }
            allowedCacheRoot = tempFolder.root
            allowColdStartAwait = false
        }
        val ioUri = CoverArtContentProvider.uriFor(authority, "transient-io")
        try {
            ioProvider.openFile(ioUri, "r")
            fail("Expected FileNotFoundException on transient IO error")
        } catch (_: FileNotFoundException) {
            // Expected on first attempt
        }
        // Immediate retry must NOT be blocked by the 120s negative cache.
        val pfdAfterIo = ioProvider.openFile(ioUri, "r")
        assertNotNull(pfdAfterIo)
        pfdAfterIo.close()
        assertEquals(2, ioAttempt)

        // Timeout must also NOT enter the 120s negative cache.
        val timeoutRealPng = createRealPngFile("timeout-real.png")
        var timeoutAttempt = 0
        val timeoutCore = FakeCoreClient().apply {
            getOrFetchCoverArtHandler = { id, _ ->
                timeoutAttempt++
                if (timeoutAttempt == 1) {
                    delay(400L)
                }
                coverArtPaths[id] = timeoutRealPng.absolutePath
                timeoutRealPng.absolutePath
            }
        }
        val timeoutProvider = CoverArtContentProvider().apply {
            coreClientProvider = { timeoutCore }
            allowedCacheRoot = tempFolder.root
            allowColdStartAwait = false
            phoneFetchTimeoutMs = 50L
        }
        val timeoutUri = CoverArtContentProvider.uriFor(authority, "transient-timeout")
        try {
            timeoutProvider.openFile(timeoutUri, "r")
            fail("Expected FileNotFoundException on fetch timeout")
        } catch (_: FileNotFoundException) {
            // Expected on timed-out first attempt
        }
        val pfdAfterTimeout = timeoutProvider.openFile(timeoutUri, "r")
        assertNotNull(pfdAfterTimeout)
        pfdAfterTimeout.close()
        assertEquals(2, timeoutAttempt)

        // Navidrome blue-vinyl placeholder MUST enter the 120s negative cache.
        val placeholderPng = createNavidromeBlueVinylPngFile("placeholder-vinyl.png")
        var placeholderCalls = 0
        val placeholderCore = FakeCoreClient().apply {
            getOrFetchCoverArtHandler = { _, _ ->
                placeholderCalls++
                placeholderPng.absolutePath
            }
        }
        val placeholderProvider = CoverArtContentProvider().apply {
            coreClientProvider = { placeholderCore }
            allowedCacheRoot = tempFolder.root
            allowColdStartAwait = false
        }
        val placeholderUri = CoverArtContentProvider.uriFor(authority, "placeholder-id")
        try {
            placeholderProvider.openFile(placeholderUri, "r")
            fail("Expected FileNotFoundException when server returns Navidrome placeholder")
        } catch (_: FileNotFoundException) {
            // Expected
        }
        try {
            placeholderProvider.openFile(placeholderUri, "r")
            fail("Expected FileNotFoundException from negative cache on second attempt")
        } catch (_: FileNotFoundException) {
            // Expected
        }
        assertEquals(
            "Placeholder must be negatively cached so the second call skips network fetch",
            1,
            placeholderCalls,
        )

        // Clearing negative cache allows subsequent fetches (e.g. after prefetch succeeds).
        CoverArtContentProvider.clearNegativeCache("placeholder-id")
        try {
            placeholderProvider.openFile(placeholderUri, "r")
            fail("Expected FileNotFoundException after cache clear while still returning placeholder")
        } catch (_: FileNotFoundException) {
            // Expected
        }
        assertEquals(2, placeholderCalls)
    }

    @Test
    fun uriForWithIsCachedAppendsQueryParamAndServesCachedFile() {
        val pngFile = createRealPngFile("al-99.png")
        val core = FakeCoreClient().apply {
            coverArtPaths["al-99"] = pngFile.absolutePath
        }
        val provider = CoverArtContentProvider().apply {
            coreClientProvider = { core }
            allowedCacheRoot = tempFolder.root
            allowColdStartAwait = false
        }

        val uncachedUri = CoverArtContentProvider.uriFor(
            authority = authority,
            coverArtId = "al-99",
            fallbackKind = "album",
            fallbackSeed = "Cross",
            isCached = false,
        )
        assertNull(uncachedUri.getQueryParameter(CoverArtContentProvider.QUERY_CACHED))

        val cachedUri = CoverArtContentProvider.uriFor(
            authority = authority,
            coverArtId = "al-99",
            fallbackKind = "album",
            fallbackSeed = "Cross",
            isCached = true,
        )
        assertEquals("1", cachedUri.getQueryParameter(CoverArtContentProvider.QUERY_CACHED))
        assertEquals("al-99", CoverArtContentProvider.parseCoverArtId(cachedUri))
        assertEquals("image/png", provider.getType(cachedUri))

        val pfd = provider.openFile(cachedUri, "r")
        assertNotNull(pfd)
        pfd.close()
    }

    @Test
    fun rustRejectedPlaceholderInvalidInputAndSubsonic70EnterNegativeCache() {
        var invalidInputCalls = 0
        var subsonic70Calls = 0
        var subsonicOtherCalls = 0
        val recoveredPng = createRealPngFile("subsonic-recovered.png")

        val core = FakeCoreClient().apply {
            getOrFetchCoverArtHandler = { id, _ ->
                when (id) {
                    "al-1" -> {
                        invalidInputCalls++
                        throw FfiException.InvalidInput("navidrome placeholder artwork for al-1")
                    }
                    "al-missing" -> {
                        subsonic70Calls++
                        throw FfiException.Subsonic(70L, "Data not found")
                    }
                    "al-transient" -> {
                        subsonicOtherCalls++
                        if (subsonicOtherCalls == 1) {
                            throw FfiException.Subsonic(0L, "Temporary backend error")
                        }
                        coverArtPaths[id] = recoveredPng.absolutePath
                        recoveredPng.absolutePath
                    }
                    else -> ""
                }
            }
        }
        val provider = CoverArtContentProvider().apply {
            coreClientProvider = { core }
            allowedCacheRoot = tempFolder.root
            allowColdStartAwait = false
        }

        val invalidUri = CoverArtContentProvider.uriFor(authority, "al-1")
        repeat(2) {
            try {
                provider.openFile(invalidUri, "r")
                fail("Expected FileNotFoundException for Rust-rejected placeholder")
            } catch (_: FileNotFoundException) {
                // Expected
            }
        }
        assertEquals(
            "FfiException.InvalidInput must enter the 120s negative cache",
            1,
            invalidInputCalls,
        )
        assertEquals(true, CoverArtContentProvider.isNegativelyCached("al-1"))

        val subsonic70Uri = CoverArtContentProvider.uriFor(authority, "al-missing")
        repeat(2) {
            try {
                provider.openFile(subsonic70Uri, "r")
                fail("Expected FileNotFoundException for Subsonic code 70")
            } catch (_: FileNotFoundException) {
                // Expected
            }
        }
        assertEquals(
            "FfiException.Subsonic(code=70) must enter the 120s negative cache",
            1,
            subsonic70Calls,
        )
        assertEquals(true, CoverArtContentProvider.isNegativelyCached("al-missing"))

        // Other Subsonic error codes remain transient and retry immediately.
        val transientUri = CoverArtContentProvider.uriFor(authority, "al-transient")
        try {
            provider.openFile(transientUri, "r")
            fail("Expected FileNotFoundException on transient Subsonic error")
        } catch (_: FileNotFoundException) {
            // Expected
        }
        assertEquals(false, CoverArtContentProvider.isNegativelyCached("al-transient"))
        val pfd = provider.openFile(transientUri, "r")
        assertNotNull(pfd)
        pfd.close()
        assertEquals(2, subsonicOtherCalls)
    }

    @Test
    fun concurrentRequestsForSameCoverDeduplicateBeforePermitsWithoutStarvingDistinctCovers() {
        val filesById = mapOf(
            "shared-album" to createRealPngFile("shared-album.png"),
            "distinct-1" to createRealPngFile("distinct-1.png"),
            "distinct-2" to createRealPngFile("distinct-2.png"),
            "distinct-3" to createRealPngFile("distinct-3.png"),
        )
        val fetchCounts = ConcurrentHashMap<String, AtomicInteger>()
        val sharedStarted = CountDownLatch(1)
        val distinctStarted = CountDownLatch(3)
        val releaseShared = CountDownLatch(1)

        val core = FakeCoreClient().apply {
            getOrFetchCoverArtHandler = { id, _ ->
                fetchCounts.computeIfAbsent(id) { AtomicInteger(0) }.incrementAndGet()
                if (id == "shared-album") {
                    sharedStarted.countDown()
                    // Stay in-flight until all 3 distinct covers have acquired permits concurrently.
                    releaseShared.await(3L, TimeUnit.SECONDS)
                } else {
                    distinctStarted.countDown()
                }
                val path = filesById.getValue(id).absolutePath
                coverArtPaths[id] = path
                path
            }
        }
        val provider = CoverArtContentProvider().apply {
            coreClientProvider = { core }
            allowedCacheRoot = tempFolder.root
            allowColdStartAwait = false
            phonePermitWaitMs = 1_500L
            phoneFetchTimeoutMs = 4_000L
        }

        val executor = Executors.newFixedThreadPool(9)
        try {
            val sharedFutures = (1..6).map {
                executor.submit<Boolean> {
                    val pfd = provider.openFile(CoverArtContentProvider.uriFor(authority, "shared-album"), "r")
                    pfd.close()
                    true
                }
            }
            assertEquals("Shared leader must start fetching", true, sharedStarted.await(2L, TimeUnit.SECONDS))

            // While 6 threads are waiting on "shared-album", 3 distinct covers must still acquire
            // the remaining 3 of 4 phone permits immediately without starvation.
            val distinctFutures = listOf("distinct-1", "distinct-2", "distinct-3").map { id ->
                executor.submit<Boolean> {
                    val pfd = provider.openFile(CoverArtContentProvider.uriFor(authority, id), "r")
                    pfd.close()
                    true
                }
            }
            assertEquals(
                "3 distinct covers must not be starved by 6 concurrent requests for the same coverArtId",
                true,
                distinctStarted.await(2L, TimeUnit.SECONDS),
            )
            releaseShared.countDown()

            (sharedFutures + distinctFutures).forEach { fut ->
                assertEquals(true, fut.get(4L, TimeUnit.SECONDS))
            }

            assertEquals(1, fetchCounts["shared-album"]?.get())
            assertEquals(1, fetchCounts["distinct-1"]?.get())
            assertEquals(1, fetchCounts["distinct-2"]?.get())
            assertEquals(1, fetchCounts["distinct-3"]?.get())
        } finally {
            releaseShared.countDown()
            executor.shutdownNow()
        }
    }

    private fun createRealPngFile(name: String): File {
        val bmp = Bitmap.createBitmap(32, 32, Bitmap.Config.ARGB_8888)
        for (y in 0 until 32) {
            for (x in 0 until 32) {
                bmp.setPixel(x, y, Color.rgb(180, 40, 210))
            }
        }
        val file = tempFolder.newFile(name)
        FileOutputStream(file).use { out ->
            bmp.compress(Bitmap.CompressFormat.PNG, 100, out)
        }
        bmp.recycle()
        return file
    }

    private fun createNavidromeBlueVinylPngFile(name: String): File {
        val bmp = Bitmap.createBitmap(64, 64, Bitmap.Config.ARGB_8888)
        for (y in 0 until 64) {
            for (x in 0 until 64) {
                val dx = x - 32
                val dy = y - 32
                val distSq = dx * dx + dy * dy
                val color = when {
                    distSq <= 10 * 10 -> Color.rgb(34, 33, 29)
                    distSq <= 28 * 28 -> Color.rgb(1, 88, 211)
                    else -> Color.TRANSPARENT
                }
                bmp.setPixel(x, y, color)
            }
        }
        val file = tempFolder.newFile(name)
        FileOutputStream(file).use { out ->
            bmp.compress(Bitmap.CompressFormat.PNG, 100, out)
        }
        bmp.recycle()
        return file
    }
}
