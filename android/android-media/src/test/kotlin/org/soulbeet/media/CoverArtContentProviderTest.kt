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
