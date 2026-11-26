package org.soulbeet.app

import android.app.Application
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import org.soulbeet.app.data.AppPreferencesStore
import org.soulbeet.corebindings.SoulbeetCoreProvider
import org.soulbeet.media.SoulbeetMediaRepository
import org.soulbeet.media.SoulbeetWorkScheduler

/**
 * Application entry point wiring the single shared [SoulbeetCoreProvider] instance, Media3
 * repository preferences, and periodic WorkManager sync across the Compose phone UI, Android Auto,
 * and background sync workers.
 */
class SoulbeetApplication : Application() {

    private val appScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    lateinit var preferencesStore: AppPreferencesStore
        private set

    override fun onCreate() {
        super.onCreate()
        preferencesStore = AppPreferencesStore(this)

        SoulbeetCoreProvider.registerInitializer {
            initializeCoreFromPreferences()
        }

        // Warm up initialization off the main thread so first screen paint is instant and
        // headless Android Auto / WorkManager wakeups reuse the exact same CoreClient.
        appScope.launch {
            SoulbeetCoreProvider.awaitOrNull()
        }
    }

    private suspend fun initializeCoreFromPreferences() = runCatching {
        val snapshot = preferencesStore.getSnapshot()
        SoulbeetMediaRepository.shared.setOfflineOnly(snapshot.offlineOnly)
        SoulbeetMediaRepository.shared.setLibrarySecondaryAction(snapshot.librarySecondaryAction)
        SoulbeetMediaRepository.shared.setAudioQualityProfile(snapshot.toQualityProfile())

        if (!snapshot.isConfigured) {
            SoulbeetCoreProvider.markUnconfigured()
            return@runCatching null
        }

        val config = snapshot.toFfiClientConfig(this)
        val client = SoulbeetCoreProvider.open(config)
        client.setOfflineOnly(snapshot.offlineOnly)

        runCatching {
            SoulbeetWorkScheduler.schedulePeriodicSync(
                context = this,
                qualityProfile = snapshot.qualityProfileKey,
            )
        }
        client
    }.getOrNull()
}
