package org.soulbeet.app.data

import android.content.Context
import android.os.Build
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import org.soulbeet.core.FfiAudioQualityProfile
import org.soulbeet.core.FfiClientConfig
import org.soulbeet.core.FfiDevicePlatform
import org.soulbeet.core.FfiNavidromeCredentials
import org.soulbeet.core.FfiSoulbeetAuth
import org.soulbeet.core.defaultCachePolicy
import org.soulbeet.media.LibrarySecondaryAction
import org.soulbeet.media.OfflineSyncWorker
import java.io.File
import java.util.UUID

private val Context.soulbeetDataStore: DataStore<Preferences> by preferencesDataStore(
    name = "soulbeet_app_preferences",
)

enum class StoredAuthKind {
    NONE,
    COOKIE,
    BEARER,
}

data class PersistedAppConfig(
    val serverUrl: String = "",
    val authKind: StoredAuthKind = StoredAuthKind.NONE,
    val authToken: String = "",
    val username: String = "",
    val navidrome: FfiNavidromeCredentials? = null,
    val deviceId: String = "",
    val deviceName: String = "",
    val oledMode: Boolean = false,
    val offlineOnly: Boolean = false,
    val librarySecondaryAction: LibrarySecondaryAction = LibrarySecondaryAction.DISLIKE_SKIP,
    val qualityProfileKey: String = OfflineSyncWorker.PROFILE_CELLULAR_OPUS_160,
    val recentSearches: List<String> = emptyList(),
) {
    val isConfigured: Boolean
        get() = serverUrl.isNotBlank() && authKind != StoredAuthKind.NONE && authToken.isNotBlank()

    fun normalizedServerUrl(): String = serverUrl.trim().trimEnd('/')

    fun toSoulbeetAuth(): FfiSoulbeetAuth = when (authKind) {
        StoredAuthKind.BEARER -> if (authToken.isNotBlank()) FfiSoulbeetAuth.Bearer(authToken) else FfiSoulbeetAuth.None
        StoredAuthKind.COOKIE -> if (authToken.isNotBlank()) FfiSoulbeetAuth.SessionCookie(authToken) else FfiSoulbeetAuth.None
        StoredAuthKind.NONE -> FfiSoulbeetAuth.None
    }

    fun toQualityProfile(): FfiAudioQualityProfile =
        parseAudioQualityProfile(qualityProfileKey)

    fun toFfiClientConfig(context: Context): FfiClientConfig {
        val dbDir = File(context.filesDir, "soulbeet_core").apply { mkdirs() }
        val audioCacheDir = File(context.filesDir, "audio_cache").apply { mkdirs() }
        val coverArtDir = File(context.cacheDir, "cover_art").apply { mkdirs() }
        return FfiClientConfig(
            dbPath = File(dbDir, "client_core.db").absolutePath,
            cacheDir = audioCacheDir.absolutePath,
            coverArtDir = coverArtDir.absolutePath,
            soulbeetBaseUrl = normalizedServerUrl().takeIf { it.isNotBlank() },
            soulbeetAuth = toSoulbeetAuth(),
            navidrome = navidrome,
            deviceId = deviceId.ifBlank { UUID.randomUUID().toString() },
            deviceName = deviceName.ifBlank { Build.MODEL.orEmpty().ifBlank { "Android" } },
            platform = FfiDevicePlatform.ANDROID,
            offlineOnly = offlineOnly,
            cachePolicy = defaultCachePolicy(),
        )
    }
}

/**
 * Persistent DataStore-backed preferences for Soulbeet server session, Navidrome bootstrap
 * coordinates, True-Black OLED mode, global Offline Only mode, Android Auto secondary button,
 * audio quality profile, and recent searches.
 */
class AppPreferencesStore(private val context: Context) {

    private val dataStore: DataStore<Preferences> get() = context.soulbeetDataStore

    val configFlow: Flow<PersistedAppConfig> = dataStore.data.map { prefs ->
        val naviUrl = prefs[KEY_NAVI_URL].orEmpty()
        val naviUser = prefs[KEY_NAVI_USER].orEmpty()
        val naviSalt = prefs[KEY_NAVI_SALT].orEmpty()
        val naviToken = prefs[KEY_NAVI_TOKEN].orEmpty()
        val naviCreds = if (naviUrl.isNotBlank() && naviUser.isNotBlank() && naviToken.isNotBlank()) {
            FfiNavidromeCredentials(
                baseUrl = naviUrl,
                username = naviUser,
                subsonicSalt = naviSalt,
                subsonicToken = naviToken,
                apiVersion = prefs[KEY_NAVI_API_VERSION].orEmpty().ifBlank { "1.16.1" },
                clientName = prefs[KEY_NAVI_CLIENT_NAME].orEmpty().ifBlank { "Soulbeet" },
            )
        } else {
            null
        }

        val authKind = when (prefs[KEY_AUTH_KIND]) {
            "bearer" -> StoredAuthKind.BEARER
            "cookie" -> StoredAuthKind.COOKIE
            else -> StoredAuthKind.NONE
        }
        val secondaryAction = when (prefs[KEY_AUTO_SECONDARY_ACTION]) {
            LibrarySecondaryAction.START_RADIO.name -> LibrarySecondaryAction.START_RADIO
            else -> LibrarySecondaryAction.DISLIKE_SKIP
        }
        val recentRaw = prefs[KEY_RECENT_SEARCHES].orEmpty()
        val recentList = if (recentRaw.isBlank()) {
            emptyList()
        } else {
            recentRaw.split("\n").map { it.trim() }.filter { it.isNotEmpty() }.take(MAX_RECENT_SEARCHES)
        }

        PersistedAppConfig(
            serverUrl = prefs[KEY_SERVER_URL].orEmpty(),
            authKind = authKind,
            authToken = prefs[KEY_AUTH_TOKEN].orEmpty(),
            username = prefs[KEY_USERNAME].orEmpty(),
            navidrome = naviCreds,
            deviceId = prefs[KEY_DEVICE_ID].orEmpty(),
            deviceName = prefs[KEY_DEVICE_NAME].orEmpty().ifBlank { defaultDeviceName() },
            oledMode = prefs[KEY_OLED_MODE] ?: false,
            offlineOnly = prefs[KEY_OFFLINE_ONLY] ?: false,
            librarySecondaryAction = secondaryAction,
            qualityProfileKey = prefs[KEY_QUALITY_PROFILE]
                ?: OfflineSyncWorker.PROFILE_CELLULAR_OPUS_160,
            recentSearches = recentList,
        )
    }

    suspend fun getSnapshot(): PersistedAppConfig {
        val current = configFlow.first()
        if (current.deviceId.isNotBlank()) return current
        val newId = "android-${UUID.randomUUID()}"
        val name = defaultDeviceName()
        dataStore.edit { prefs ->
            prefs[KEY_DEVICE_ID] = newId
            if (prefs[KEY_DEVICE_NAME].isNullOrBlank()) {
                prefs[KEY_DEVICE_NAME] = name
            }
        }
        return current.copy(deviceId = newId, deviceName = name)
    }

    suspend fun saveSession(
        serverUrl: String,
        authKind: StoredAuthKind,
        authToken: String,
        username: String,
        navidrome: FfiNavidromeCredentials? = null,
    ) {
        val existing = getSnapshot()
        dataStore.edit { prefs ->
            prefs[KEY_SERVER_URL] = serverUrl.trim().trimEnd('/')
            prefs[KEY_AUTH_KIND] = when (authKind) {
                StoredAuthKind.BEARER -> "bearer"
                StoredAuthKind.COOKIE -> "cookie"
                StoredAuthKind.NONE -> "none"
            }
            prefs[KEY_AUTH_TOKEN] = authToken.trim()
            prefs[KEY_USERNAME] = username.trim()
            if (prefs[KEY_DEVICE_ID].isNullOrBlank()) {
                prefs[KEY_DEVICE_ID] = existing.deviceId
            }
            if (prefs[KEY_DEVICE_NAME].isNullOrBlank()) {
                prefs[KEY_DEVICE_NAME] = existing.deviceName
            }
            if (navidrome != null) {
                prefs[KEY_NAVI_URL] = navidrome.baseUrl
                prefs[KEY_NAVI_USER] = navidrome.username
                prefs[KEY_NAVI_SALT] = navidrome.subsonicSalt
                prefs[KEY_NAVI_TOKEN] = navidrome.subsonicToken
                prefs[KEY_NAVI_API_VERSION] = navidrome.apiVersion
                prefs[KEY_NAVI_CLIENT_NAME] = navidrome.clientName
            }
        }
    }

    suspend fun saveNavidromeCredentials(navidrome: FfiNavidromeCredentials) {
        dataStore.edit { prefs ->
            prefs[KEY_NAVI_URL] = navidrome.baseUrl
            prefs[KEY_NAVI_USER] = navidrome.username
            prefs[KEY_NAVI_SALT] = navidrome.subsonicSalt
            prefs[KEY_NAVI_TOKEN] = navidrome.subsonicToken
            prefs[KEY_NAVI_API_VERSION] = navidrome.apiVersion
            prefs[KEY_NAVI_CLIENT_NAME] = navidrome.clientName
        }
    }

    suspend fun clearSession() {
        dataStore.edit { prefs ->
            prefs.remove(KEY_AUTH_KIND)
            prefs.remove(KEY_AUTH_TOKEN)
            prefs.remove(KEY_USERNAME)
            prefs.remove(KEY_NAVI_URL)
            prefs.remove(KEY_NAVI_USER)
            prefs.remove(KEY_NAVI_SALT)
            prefs.remove(KEY_NAVI_TOKEN)
            prefs.remove(KEY_NAVI_API_VERSION)
            prefs.remove(KEY_NAVI_CLIENT_NAME)
        }
    }

    suspend fun setOledMode(enabled: Boolean) {
        dataStore.edit { it[KEY_OLED_MODE] = enabled }
    }

    suspend fun setOfflineOnly(enabled: Boolean) {
        dataStore.edit { it[KEY_OFFLINE_ONLY] = enabled }
    }

    suspend fun setLibrarySecondaryAction(action: LibrarySecondaryAction) {
        dataStore.edit { it[KEY_AUTO_SECONDARY_ACTION] = action.name }
    }

    suspend fun setQualityProfile(profileKey: String) {
        dataStore.edit { it[KEY_QUALITY_PROFILE] = profileKey }
    }

    suspend fun addRecentSearch(query: String) {
        val clean = query.trim().replace("\n", " ")
        if (clean.length < 2) return
        dataStore.edit { prefs ->
            val current = prefs[KEY_RECENT_SEARCHES].orEmpty()
                .split("\n")
                .map { it.trim() }
                .filter { it.isNotEmpty() && !it.equals(clean, ignoreCase = true) }
            val updated = (listOf(clean) + current).take(MAX_RECENT_SEARCHES)
            prefs[KEY_RECENT_SEARCHES] = updated.joinToString("\n")
        }
    }

    suspend fun clearRecentSearches() {
        dataStore.edit { it.remove(KEY_RECENT_SEARCHES) }
    }

    companion object {
        private const val MAX_RECENT_SEARCHES = 8

        private val KEY_SERVER_URL = stringPreferencesKey("server_url")
        private val KEY_AUTH_KIND = stringPreferencesKey("auth_kind")
        private val KEY_AUTH_TOKEN = stringPreferencesKey("auth_token")
        private val KEY_USERNAME = stringPreferencesKey("username")
        private val KEY_NAVI_URL = stringPreferencesKey("navidrome_url")
        private val KEY_NAVI_USER = stringPreferencesKey("navidrome_user")
        private val KEY_NAVI_SALT = stringPreferencesKey("navidrome_salt")
        private val KEY_NAVI_TOKEN = stringPreferencesKey("navidrome_token")
        private val KEY_NAVI_API_VERSION = stringPreferencesKey("navidrome_api_version")
        private val KEY_NAVI_CLIENT_NAME = stringPreferencesKey("navidrome_client_name")
        private val KEY_DEVICE_ID = stringPreferencesKey("device_id")
        private val KEY_DEVICE_NAME = stringPreferencesKey("device_name")
        private val KEY_OLED_MODE = booleanPreferencesKey("oled_mode")
        private val KEY_OFFLINE_ONLY = booleanPreferencesKey("offline_only")
        private val KEY_AUTO_SECONDARY_ACTION = stringPreferencesKey("auto_secondary_action")
        private val KEY_QUALITY_PROFILE = stringPreferencesKey("quality_profile")
        private val KEY_RECENT_SEARCHES = stringPreferencesKey("recent_searches")

        private fun defaultDeviceName(): String {
            val brand = Build.MANUFACTURER.orEmpty().trim()
            val model = Build.MODEL.orEmpty().trim()
            return when {
                model.isBlank() -> "Android Phone"
                brand.isNotBlank() && !model.startsWith(brand, ignoreCase = true) -> "$brand $model"
                else -> model
            }
        }
    }
}

fun parseAudioQualityProfile(raw: String?): FfiAudioQualityProfile =
    when (raw) {
        OfflineSyncWorker.PROFILE_LOSSLESS_FLAC -> FfiAudioQualityProfile.LosslessFlac
        else -> FfiAudioQualityProfile.CellularOpus160
    }
