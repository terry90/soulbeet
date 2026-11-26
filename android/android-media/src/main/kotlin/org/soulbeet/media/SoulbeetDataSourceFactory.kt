package org.soulbeet.media

import android.net.Uri
import androidx.annotation.OptIn
import androidx.media3.common.C
import androidx.media3.common.util.UnstableApi
import androidx.media3.datasource.DataSource
import androidx.media3.datasource.DataSpec
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.datasource.FileDataSource
import androidx.media3.datasource.TransferListener
import kotlinx.coroutines.runBlocking
import org.soulbeet.core.FfiAudioQualityProfile
import org.soulbeet.core.FfiPlaybackSource
import org.soulbeet.corebindings.CoreClient
import java.io.File
import java.io.IOException

/**
 * Media3 [DataSource.Factory] backed by `lib/client-core`'s offline-first [CoreClient.resolvePlaybackSource].
 *
 * Track URIs use the internal scheme `soulbeet://track/<trackId>`. When ExoPlayer opens a track on
 * its background loader thread:
 * - [FfiPlaybackSource.LocalFile] streams directly from disk via [FileDataSource] (zero network).
 * - [FfiPlaybackSource.StreamUrl] streams via [DefaultHttpDataSource], injecting every custom HTTP
 *   header returned in [FfiPlaybackSource.StreamUrl.headers].
 */
@OptIn(UnstableApi::class)
class SoulbeetDataSourceFactory(
    private val coreProvider: () -> CoreClient?,
    private val qualityProfileProvider: () -> FfiAudioQualityProfile = {
        FfiAudioQualityProfile.CellularOpus160
    },
    private val httpDataSourceFactory: DefaultHttpDataSource.Factory = DefaultHttpDataSource.Factory()
        .setUserAgent("SoulbeetAndroid/1.0")
        .setAllowCrossProtocolRedirects(true),
) : DataSource.Factory {

    override fun createDataSource(): DataSource =
        SoulbeetDataSource(
            coreProvider = coreProvider,
            qualityProfileProvider = qualityProfileProvider,
            httpDataSourceFactory = httpDataSourceFactory,
        )

    companion object {
        const val SCHEME_SOULBEET: String = "soulbeet"
        const val HOST_TRACK: String = "track"

        fun trackUri(trackId: String): Uri =
            Uri.Builder()
                .scheme(SCHEME_SOULBEET)
                .authority(HOST_TRACK)
                .appendPath(trackId)
                .build()

        fun extractTrackIdFromUri(uri: Uri): String? {
            if (uri.scheme != SCHEME_SOULBEET || uri.authority != HOST_TRACK) return null
            return uri.pathSegments.firstOrNull()?.takeIf { it.isNotBlank() }
        }
    }
}

@OptIn(UnstableApi::class)
internal class SoulbeetDataSource(
    private val coreProvider: () -> CoreClient?,
    private val qualityProfileProvider: () -> FfiAudioQualityProfile,
    private val httpDataSourceFactory: DefaultHttpDataSource.Factory,
) : DataSource {

    private val transferListeners = mutableListOf<TransferListener>()
    private var activeDelegate: DataSource? = null

    override fun addTransferListener(transferListener: TransferListener) {
        transferListeners.add(transferListener)
        activeDelegate?.addTransferListener(transferListener)
    }

    override fun open(dataSpec: DataSpec): Long {
        val uri = dataSpec.uri
        val (delegate, resolvedSpec) = when (uri.scheme) {
            SoulbeetDataSourceFactory.SCHEME_SOULBEET -> resolveSoulbeetSource(dataSpec)
            "file" -> FileDataSource() to dataSpec
            "http", "https" -> httpDataSourceFactory.createDataSource() to dataSpec
            else -> throw IOException("Unsupported playback URI scheme: ${uri.scheme}")
        }

        for (listener in transferListeners) {
            delegate.addTransferListener(listener)
        }
        activeDelegate = delegate
        return delegate.open(resolvedSpec)
    }

    override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
        val delegate = activeDelegate ?: return C.RESULT_END_OF_INPUT
        return delegate.read(buffer, offset, length)
    }

    override fun getUri(): Uri? = activeDelegate?.uri

    override fun getResponseHeaders(): Map<String, List<String>> =
        activeDelegate?.responseHeaders ?: emptyMap()

    override fun close() {
        val delegate = activeDelegate
        activeDelegate = null
        delegate?.close()
    }

    private fun resolveSoulbeetSource(dataSpec: DataSpec): Pair<DataSource, DataSpec> {
        val trackId = SoulbeetDataSourceFactory.extractTrackIdFromUri(dataSpec.uri)
            ?: throw IOException("Invalid Soulbeet track URI: ${dataSpec.uri}")
        val core = coreProvider()
            ?: throw IOException("SoulbeetCoreProvider is not initialized")

        val source = try {
            runBlocking {
                core.resolvePlaybackSource(trackId, qualityProfileProvider())
            }
        } catch (t: Throwable) {
            throw IOException("Failed to resolve playback source for track '$trackId': ${t.message}", t)
        }

        return when (source) {
            is FfiPlaybackSource.LocalFile -> {
                val file = File(source.path)
                val spec = dataSpec.buildUpon()
                    .setUri(Uri.fromFile(file))
                    .build()
                FileDataSource() to spec
            }

            is FfiPlaybackSource.StreamUrl -> {
                val httpSource = httpDataSourceFactory.createDataSource()
                for (header in source.headers) {
                    httpSource.setRequestProperty(header.name, header.value)
                }
                val spec = dataSpec.buildUpon()
                    .setUri(Uri.parse(source.url))
                    .build()
                httpSource to spec
            }
        }
    }
}
