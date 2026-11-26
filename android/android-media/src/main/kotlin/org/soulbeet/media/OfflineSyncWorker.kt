package org.soulbeet.media

import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import androidx.work.BackoffPolicy
import androidx.work.Constraints
import androidx.work.CoroutineWorker
import androidx.work.Data
import androidx.work.ExistingPeriodicWorkPolicy
import androidx.work.ExistingWorkPolicy
import androidx.work.NetworkType
import androidx.work.OneTimeWorkRequestBuilder
import androidx.work.PeriodicWorkRequestBuilder
import androidx.work.WorkManager
import androidx.work.WorkerParameters
import kotlinx.coroutines.CancellationException
import org.soulbeet.core.FfiAudioQualityProfile
import org.soulbeet.core.FfiException
import org.soulbeet.corebindings.SoulbeetCoreProvider
import java.util.concurrent.TimeUnit

/**
 * Background [CoroutineWorker] synchronizing `lib/client-core`'s local SQLite mirror, draining the
 * offline curation/scrobble outbox, and executing Wi-Fi auto-cache pinning rules.
 */
class OfflineSyncWorker(
    appContext: Context,
    params: WorkerParameters,
) : CoroutineWorker(appContext, params) {

    override suspend fun doWork(): Result {
        val core = SoulbeetCoreProvider.awaitOrNull()
        if (core == null) {
            if (SoulbeetCoreProvider.isExplicitlyUnconfigured()) {
                return Result.success()
            }
            return if (runAttemptCount < MAX_RETRY_ATTEMPTS) Result.retry() else Result.failure()
        }
        if (core.isOfflineOnly()) {
            return Result.success()
        }

        val drainOutbox = inputData.getBoolean(KEY_DRAIN_OUTBOX, true)
        val runSync = inputData.getBoolean(KEY_RUN_SYNC, true)
        val forceFullSync = inputData.getBoolean(KEY_FORCE_FULL_SYNC, false)
        val runAutoCache = inputData.getBoolean(KEY_RUN_AUTO_CACHE, false)
        val profile = parseQualityProfile(inputData.getString(KEY_QUALITY_PROFILE))

        return try {
            if (drainOutbox) {
                core.drainOutbox()
            }
            if (runSync) {
                core.syncNow(forceFull = forceFullSync)
            }
            if (runAutoCache) {
                val isUnmeteredWifi = isUnmeteredConnection(applicationContext)
                core.runAutoCacheRules(profile = profile, isWifi = isUnmeteredWifi)
            }
            Result.success()
        } catch (e: CancellationException) {
            throw e
        } catch (e: FfiException) {
            if (isRetryableError(e) && runAttemptCount < MAX_RETRY_ATTEMPTS) {
                Result.retry()
            } else {
                Result.failure()
            }
        } catch (_: Exception) {
            if (runAttemptCount < MAX_RETRY_ATTEMPTS) {
                Result.retry()
            } else {
                Result.failure()
            }
        }
    }

    companion object {
        const val KEY_DRAIN_OUTBOX: String = "drain_outbox"
        const val KEY_RUN_SYNC: String = "run_sync"
        const val KEY_FORCE_FULL_SYNC: String = "force_full_sync"
        const val KEY_RUN_AUTO_CACHE: String = "run_auto_cache"
        const val KEY_QUALITY_PROFILE: String = "quality_profile"

        const val PROFILE_LOSSLESS_FLAC: String = "lossless_flac"
        const val PROFILE_CELLULAR_OPUS_160: String = "cellular_opus_160"

        internal const val MAX_RETRY_ATTEMPTS: Int = 5

        /**
         * Returns `true` only for transient network/server errors. Permanent client HTTP errors
         * (400/401/403/404) are NOT retried to avoid futile battery-draining loops.
         */
        internal fun isRetryableError(error: FfiException): Boolean =
            when (error) {
                is FfiException.Http,
                is FfiException.WebSocket,
                is FfiException.Io -> true

                is FfiException.ServerHttp -> {
                    val status = error.status.toInt()
                    status == 408 || status == 429 || status >= 500
                }

                else -> false
            }

        internal fun parseQualityProfile(raw: String?): FfiAudioQualityProfile =
            when (raw) {
                PROFILE_LOSSLESS_FLAC -> FfiAudioQualityProfile.LosslessFlac
                else -> FfiAudioQualityProfile.CellularOpus160
            }

        internal fun isUnmeteredConnection(context: Context): Boolean {
            val cm = context.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
                ?: return false
            val activeNetwork = cm.activeNetwork ?: return false
            val caps = cm.getNetworkCapabilities(activeNetwork) ?: return false
            return caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)
        }
    }
}

/**
 * Convenience scheduler for periodic library sync, Wi-Fi auto-cache rules, and immediate outbox
 * drain after offline curation or scrobble mutations.
 */
object SoulbeetWorkScheduler {

    const val WORK_PERIODIC_SYNC: String = "soulbeet_periodic_sync"
    const val WORK_PERIODIC_AUTO_CACHE: String = "soulbeet_periodic_auto_cache"
    const val WORK_IMMEDIATE_OUTBOX_DRAIN: String = "soulbeet_immediate_outbox_drain"

    /**
     * Schedules periodic library sync + outbox drain on any connected network, and periodic
     * auto-cache downloads constrained to [NetworkType.UNMETERED] (Wi-Fi) by default.
     */
    fun schedulePeriodicSync(
        context: Context,
        syncIntervalHours: Long = 6L,
        autoCacheIntervalHours: Long = 12L,
        wifiOnlyAutoCache: Boolean = true,
        qualityProfile: String = OfflineSyncWorker.PROFILE_CELLULAR_OPUS_160,
    ) {
        val workManager = WorkManager.getInstance(context)

        val syncConstraints = Constraints.Builder()
            .setRequiredNetworkType(NetworkType.CONNECTED)
            .build()

        val syncInput = Data.Builder()
            .putBoolean(OfflineSyncWorker.KEY_DRAIN_OUTBOX, true)
            .putBoolean(OfflineSyncWorker.KEY_RUN_SYNC, true)
            .putBoolean(OfflineSyncWorker.KEY_RUN_AUTO_CACHE, false)
            .build()

        val syncRequest = PeriodicWorkRequestBuilder<OfflineSyncWorker>(
            syncIntervalHours.coerceAtLeast(1L),
            TimeUnit.HOURS,
        )
            .setConstraints(syncConstraints)
            .setInputData(syncInput)
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 30L, TimeUnit.SECONDS)
            .build()

        workManager.enqueueUniquePeriodicWork(
            WORK_PERIODIC_SYNC,
            ExistingPeriodicWorkPolicy.UPDATE,
            syncRequest,
        )

        val cacheNetworkType = if (wifiOnlyAutoCache) {
            NetworkType.UNMETERED
        } else {
            NetworkType.CONNECTED
        }
        val cacheConstraints = Constraints.Builder()
            .setRequiredNetworkType(cacheNetworkType)
            .setRequiresBatteryNotLow(true)
            .build()

        val cacheInput = Data.Builder()
            .putBoolean(OfflineSyncWorker.KEY_DRAIN_OUTBOX, true)
            .putBoolean(OfflineSyncWorker.KEY_RUN_SYNC, true)
            .putBoolean(OfflineSyncWorker.KEY_RUN_AUTO_CACHE, true)
            .putString(OfflineSyncWorker.KEY_QUALITY_PROFILE, qualityProfile)
            .build()

        val cacheRequest = PeriodicWorkRequestBuilder<OfflineSyncWorker>(
            autoCacheIntervalHours.coerceAtLeast(1L),
            TimeUnit.HOURS,
        )
            .setConstraints(cacheConstraints)
            .setInputData(cacheInput)
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 1L, TimeUnit.MINUTES)
            .build()

        workManager.enqueueUniquePeriodicWork(
            WORK_PERIODIC_AUTO_CACHE,
            ExistingPeriodicWorkPolicy.UPDATE,
            cacheRequest,
        )
    }

    /**
     * Enqueues a one-time [NetworkType.CONNECTED] job to drain pending outbox mutations as soon as
     * connectivity is available. Uses [ExistingWorkPolicy.APPEND_OR_REPLACE] so rapid mutations
     * never cancel an in-flight drain.
     */
    fun enqueueImmediateOutboxDrain(context: Context) {
        val constraints = Constraints.Builder()
            .setRequiredNetworkType(NetworkType.CONNECTED)
            .build()

        val input = Data.Builder()
            .putBoolean(OfflineSyncWorker.KEY_DRAIN_OUTBOX, true)
            .putBoolean(OfflineSyncWorker.KEY_RUN_SYNC, false)
            .putBoolean(OfflineSyncWorker.KEY_RUN_AUTO_CACHE, false)
            .build()

        val request = OneTimeWorkRequestBuilder<OfflineSyncWorker>()
            .setConstraints(constraints)
            .setInputData(input)
            .setBackoffCriteria(BackoffPolicy.EXPONENTIAL, 15L, TimeUnit.SECONDS)
            .build()

        WorkManager.getInstance(context).enqueueUniqueWork(
            WORK_IMMEDIATE_OUTBOX_DRAIN,
            ExistingWorkPolicy.APPEND_OR_REPLACE,
            request,
        )
    }
}
