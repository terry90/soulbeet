package org.soulbeet.app.data

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.io.IOException
import java.util.concurrent.TimeUnit

data class LoginResult(
    val authKind: StoredAuthKind,
    val token: String,
    val username: String,
)

enum class CultureSteeringOption(val wireValue: String, val shortLabel: String) {
    MORE_LOCAL("more_local", "More Local"),
    BALANCED("balanced", "Balanced"),
    MORE_INTERNATIONAL("more_international", "More Intl"),
    ;

    companion object {
        fun fromWire(raw: String?): CultureSteeringOption = when (raw?.trim()?.lowercase()) {
            "more_local", "local" -> MORE_LOCAL
            "more_international", "international", "intl" -> MORE_INTERNATIONAL
            else -> BALANCED
        }
    }
}

data class ContextualMixUi(
    val slug: String,
    val kind: String,
    val i18nKey: String,
    val title: String,
    val subtitle: String,
    val countryCode: String?,
    val localPct: Int?,
    val ownedPct: Int,
    val offlinePriority: Boolean,
    val excludeFromCoreTaste: Boolean,
    val steering: CultureSteeringOption,
    val activeFrom: String?,
    val activeUntil: String?,
    val expiresAt: String?,
    val navidromePlaylistId: String?,
    val trackCount: Int,
    val targetTracks: Int,
)

data class UserCultureProfileUi(
    val homeCountry: String?,
    val preferredLanguages: List<String>,
    val localCultureRatio: Float,
    val inferredAreaWeights: List<Pair<String, Float>>,
    val inferredLocalTags: List<String>,
)

data class ListenerProtectionUi(
    val kind: String,
    val summary: String,
)

data class DeleteGuardPreviewUi(
    val songId: String,
    val trackTitle: String,
    val trackArtist: String,
    val isPendingDiscovery: Boolean,
    val discoveryTrackId: String?,
    val canDelete: Boolean,
    val soleOwner: Boolean,
    val otherListeners: List<ListenerProtectionUi>,
    val ownPlaylists: List<String>,
    val blockReason: String?,
    val offlineFallback: Boolean = false,
)

sealed interface AcquireAndPlayOutcome {
    data class AlreadyOwned(val songId: String, val codec: String?) : AcquireAndPlayOutcome
    data class Queued(val requestId: String, val stage: String) : AcquireAndPlayOutcome
    data class Failed(val message: String) : AcquireAndPlayOutcome
}

/**
 * Lightweight OkHttp client for Soulbeet REST endpoints that complement `lib/client-core`'s
 * local-first UniFFI engine: authentication login, structured lyrics, Multi-Listener Delete Guard
 * preview, 1-tap Acquire & Play, contextual/seasonal mix steering, and Taste DNA culture profile.
 */
class SoulbeetApiClient(
    private val httpClient: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(12, TimeUnit.SECONDS)
        .readTimeout(25, TimeUnit.SECONDS)
        .build(),
) {
    private val jsonMediaType = "application/json; charset=utf-8".toMediaType()

    suspend fun login(
        baseUrl: String,
        username: String,
        password: String,
    ): Result<LoginResult> = withContext(Dispatchers.IO) {
        runCatching {
            val normalizedBase = normalizeBaseUrl(baseUrl)
            val bodyJson = JSONObject()
                .put("username", username.trim())
                .put("password", password)
                .toString()

            val request = Request.Builder()
                .url("$normalizedBase/api/auth/login")
                .post(bodyJson.toRequestBody(jsonMediaType))
                .header("Accept", "application/json")
                .build()

            httpClient.newCall(request).execute().use { response ->
                val rawBody = response.body?.string().orEmpty()
                if (!response.isSuccessful) {
                    val detail = extractErrorMessage(rawBody) ?: "HTTP ${response.code}"
                    throw IOException("Sign-in failed: $detail")
                }

                val setCookies = response.headers("Set-Cookie")
                val authCookie = extractAuthTokenFromCookies(setCookies)
                    ?: throw IOException("Server succeeded but did not issue an auth_token session cookie.")

                val parsedUser = runCatching {
                    JSONObject(rawBody).optString("username").takeIf { it.isNotBlank() }
                }.getOrNull() ?: username.trim()

                LoginResult(
                    authKind = StoredAuthKind.COOKIE,
                    token = authCookie,
                    username = parsedUser,
                )
            }
        }
    }

    suspend fun fetchLyrics(
        config: PersistedAppConfig,
        songId: String,
        artist: String?,
        title: String?,
    ): Result<TrackLyricsData?> = withContext(Dispatchers.IO) {
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val urlBuilder = "$base/api/client/lyrics".toHttpUrlOrNull()?.newBuilder()
                ?: throw IOException("Invalid Soulbeet server URL")
            urlBuilder.addQueryParameter("song_id", songId)
            if (!artist.isNullOrBlank()) urlBuilder.addQueryParameter("artist", artist)
            if (!title.isNullOrBlank()) urlBuilder.addQueryParameter("title", title)

            val request = Request.Builder()
                .url(urlBuilder.build())
                .get()
                .applyAuth(config)
                .build()

            httpClient.newCall(request).execute().use { response ->
                val raw = response.body?.string().orEmpty()
                if (!response.isSuccessful) {
                    throw IOException(extractErrorMessage(raw) ?: "Lyrics HTTP ${response.code}")
                }
                val arr = JSONArray(raw)
                if (arr.length() == 0) return@use null

                // Prefer synced lyrics over unsynced if multiple tracks returned.
                var chosen: JSONObject = arr.getJSONObject(0)
                for (i in 0 until arr.length()) {
                    val candidate = arr.getJSONObject(i)
                    if (candidate.optBoolean("synced", false)) {
                        chosen = candidate
                        break
                    }
                }
                val linesArr = chosen.optJSONArray("lines") ?: JSONArray()
                val lines = buildList {
                    for (i in 0 until linesArr.length()) {
                        val obj = linesArr.optJSONObject(i) ?: continue
                        val text = obj.optString("text", "").trim()
                        if (text.isEmpty()) continue
                        val startMs = if (obj.has("start_ms") && !obj.isNull("start_ms")) {
                            obj.optLong("start_ms")
                        } else {
                            null
                        }
                        add(LyricLine(startMs = startMs, text = text))
                    }
                }
                // If backend returned a single raw LRC block, parse timestamps cleanly.
                val finalLines = if (lines.size == 1 && lines[0].startMs == null && lines[0].text.contains("[")) {
                    LyricsSync.parseLrc(lines[0].text)
                } else {
                    lines
                }
                TrackLyricsData(
                    displayArtist = chosen.optString("display_artist").takeIf { it.isNotBlank() },
                    displayTitle = chosen.optString("display_title").takeIf { it.isNotBlank() },
                    lang = chosen.optString("lang", ""),
                    synced = chosen.optBoolean("synced", false) || finalLines.any { it.startMs != null },
                    offsetMs = chosen.optLong("offset_ms", 0L),
                    lines = finalLines,
                )
            }
        }
    }

    suspend fun fetchGuardPreview(
        config: PersistedAppConfig,
        songId: String,
        trackTitle: String,
        trackArtist: String,
    ): Result<DeleteGuardPreviewUi> = withContext(Dispatchers.IO) {
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val url = "$base/api/curation/guard-preview/$songId"
            val request = Request.Builder()
                .url(url)
                .get()
                .applyAuth(config)
                .build()

            httpClient.newCall(request).execute().use { response ->
                val raw = response.body?.string().orEmpty()
                if (!response.isSuccessful) {
                    throw IOException(extractErrorMessage(raw) ?: "Guard preview HTTP ${response.code}")
                }
                val obj = JSONObject(raw)
                val listeners = mutableListOf<ListenerProtectionUi>()
                val listenersArr = obj.optJSONArray("other_listeners") ?: JSONArray()
                for (i in 0 until listenersArr.length()) {
                    val item = listenersArr.optJSONObject(i) ?: continue
                    listeners.add(
                        ListenerProtectionUi(
                            kind = item.optString("kind", "other"),
                            summary = item.optString("summary", "Shared listener"),
                        ),
                    )
                }
                val ownPlaylists = mutableListOf<String>()
                val plArr = obj.optJSONArray("own_playlists") ?: JSONArray()
                for (i in 0 until plArr.length()) {
                    plArr.optString(i).takeIf { it.isNotBlank() }?.let { ownPlaylists.add(it) }
                }
                DeleteGuardPreviewUi(
                    songId = obj.optString("song_id", songId),
                    trackTitle = trackTitle,
                    trackArtist = trackArtist,
                    isPendingDiscovery = obj.optBoolean("is_pending_discovery", false),
                    discoveryTrackId = obj.optString("discovery_track_id").takeIf { it.isNotBlank() && it != "null" },
                    canDelete = obj.optBoolean("can_delete", false),
                    soleOwner = obj.optBoolean("sole_owner", false),
                    otherListeners = listeners,
                    ownPlaylists = ownPlaylists,
                    blockReason = obj.optString("block_reason").takeIf { it.isNotBlank() && it != "null" },
                )
            }
        }
    }

    suspend fun acquireAndPlay(
        config: PersistedAppConfig,
        artist: String,
        title: String,
        album: String?,
        recordingMbid: String?,
        durationSecs: Long?,
    ): AcquireAndPlayOutcome = withContext(Dispatchers.IO) {
        try {
            val base = normalizeBaseUrl(config.serverUrl)
            val inner = JSONObject()
                .put("artist", artist)
                .put("title", title)
                .put("prefer_fast", true)
            if (!album.isNullOrBlank()) inner.put("album", album)
            if (!recordingMbid.isNullOrBlank()) inner.put("recording_mbid", recordingMbid)
            if (durationSecs != null && durationSecs > 0L) inner.put("duration_s", durationSecs)

            val payload = JSONObject().put("req", inner).toString()
            val request = Request.Builder()
                .url("$base/api/acquire-and-play")
                .post(payload.toRequestBody(jsonMediaType))
                .applyAuth(config)
                .build()

            httpClient.newCall(request).execute().use { response ->
                val raw = response.body?.string().orEmpty()
                if (!response.isSuccessful) {
                    return@use AcquireAndPlayOutcome.Failed(
                        extractErrorMessage(raw) ?: "Acquire failed (HTTP ${response.code})",
                    )
                }
                val obj = JSONObject(raw)
                val requestId = obj.optString("request_id", "")
                val statusObj = obj.optJSONObject("status")
                val kind = statusObj?.optString("kind").orEmpty()
                if (kind == "already_owned") {
                    val songId = statusObj?.optString("navidrome_song_id").orEmpty()
                    val codec = statusObj?.optString("codec")?.takeIf { it.isNotBlank() && it != "null" }
                    if (songId.isNotBlank()) {
                        return@use AcquireAndPlayOutcome.AlreadyOwned(songId = songId, codec = codec)
                    }
                }
                AcquireAndPlayOutcome.Queued(
                    requestId = requestId,
                    stage = kind.ifBlank { "acquiring" },
                )
            }
        } catch (e: Exception) {
            AcquireAndPlayOutcome.Failed(e.localizedMessage ?: "Acquisition request failed")
        }
    }

    suspend fun fetchContextualMixes(
        config: PersistedAppConfig,
        localHour: Int,
    ): Result<List<ContextualMixUi>> = withContext(Dispatchers.IO) {
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val url = "$base/api/discovery/contextual-mixes?local_hour=${localHour.coerceIn(0, 23)}"
            val request = Request.Builder()
                .url(url)
                .get()
                .applyAuth(config)
                .build()

            httpClient.newCall(request).execute().use { response ->
                val raw = response.body?.string().orEmpty()
                if (!response.isSuccessful) {
                    throw IOException(extractErrorMessage(raw) ?: "Mixes HTTP ${response.code}")
                }
                val arr = JSONArray(raw)
                buildList {
                    for (i in 0 until arr.length()) {
                        arr.optJSONObject(i)?.let { add(parseContextualMix(it)) }
                    }
                }
            }
        }
    }

    suspend fun steerSeasonalPack(
        config: PersistedAppConfig,
        slug: String,
        steering: CultureSteeringOption,
    ): Result<ContextualMixUi> = withContext(Dispatchers.IO) {
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val inner = JSONObject()
                .put("slug", slug)
                .put("steering", steering.wireValue)
            val payload = JSONObject().put("req", inner).toString()

            val request = Request.Builder()
                .url("$base/api/discovery/seasonal/steer")
                .post(payload.toRequestBody(jsonMediaType))
                .applyAuth(config)
                .build()

            httpClient.newCall(request).execute().use { response ->
                val raw = response.body?.string().orEmpty()
                if (!response.isSuccessful) {
                    throw IOException(extractErrorMessage(raw) ?: "Steer HTTP ${response.code}")
                }
                parseContextualMix(JSONObject(raw))
            }
        }
    }

    suspend fun fetchCultureProfile(config: PersistedAppConfig): Result<UserCultureProfileUi> =
        withContext(Dispatchers.IO) {
            runCatching {
                val base = normalizeBaseUrl(config.serverUrl)
                val request = Request.Builder()
                    .url("$base/api/discovery/culture-profile")
                    .get()
                    .applyAuth(config)
                    .build()

                httpClient.newCall(request).execute().use { response ->
                    val raw = response.body?.string().orEmpty()
                    if (!response.isSuccessful) {
                        throw IOException(extractErrorMessage(raw) ?: "Profile HTTP ${response.code}")
                    }
                    parseCultureProfile(JSONObject(raw))
                }
            }
        }

    suspend fun updateCultureProfile(
        config: PersistedAppConfig,
        homeCountry: String?,
        localCultureRatio: Float,
    ): Result<UserCultureProfileUi> = withContext(Dispatchers.IO) {
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val inner = JSONObject()
                .put("local_culture_ratio", localCultureRatio.coerceIn(0f, 1f).toDouble())
            if (!homeCountry.isNullOrBlank()) {
                inner.put("home_country", homeCountry.trim().uppercase())
            }
            val payload = JSONObject().put("req", inner).toString()

            val request = Request.Builder()
                .url("$base/api/discovery/culture-profile")
                .post(payload.toRequestBody(jsonMediaType))
                .applyAuth(config)
                .build()

            httpClient.newCall(request).execute().use { response ->
                val raw = response.body?.string().orEmpty()
                if (!response.isSuccessful) {
                    throw IOException(extractErrorMessage(raw) ?: "Update profile HTTP ${response.code}")
                }
                parseCultureProfile(JSONObject(raw))
            }
        }
    }

    private fun Request.Builder.applyAuth(config: PersistedAppConfig): Request.Builder {
        header("Accept", "application/json")
        when (config.authKind) {
            StoredAuthKind.BEARER -> if (config.authToken.isNotBlank()) {
                header("Authorization", "Bearer ${config.authToken}")
            }
            StoredAuthKind.COOKIE -> if (config.authToken.isNotBlank()) {
                header("Cookie", "auth_token=${config.authToken}")
            }
            StoredAuthKind.NONE -> Unit
        }
        return this
    }

    private fun parseContextualMix(obj: JSONObject): ContextualMixUi {
        val slug = obj.optString("slug", "")
        val i18nKey = obj.optString("i18n_key", "")
        val customName = obj.optString("custom_name").takeIf { it.isNotBlank() && it != "null" }
        val customDesc = obj.optString("custom_description").takeIf { it.isNotBlank() && it != "null" }
        val country = obj.optString("country_code").takeIf { it.isNotBlank() && it != "null" }
        val localPct = if (obj.has("local_pct") && !obj.isNull("local_pct")) obj.optInt("local_pct") else null
        val (fallbackTitle, fallbackSub) = defaultMixCopy(slug, i18nKey, country)
        return ContextualMixUi(
            slug = slug,
            kind = obj.optString("kind", "rolling"),
            i18nKey = i18nKey,
            title = customName ?: fallbackTitle,
            subtitle = customDesc ?: fallbackSub,
            countryCode = country,
            localPct = localPct,
            ownedPct = obj.optInt("owned_pct", 75),
            offlinePriority = obj.optBoolean("offline_priority", false),
            excludeFromCoreTaste = obj.optBoolean("exclude_from_core_taste", false),
            steering = CultureSteeringOption.fromWire(obj.optString("steering")),
            activeFrom = obj.optString("active_from").takeIf { it.isNotBlank() && it != "null" },
            activeUntil = obj.optString("active_until").takeIf { it.isNotBlank() && it != "null" },
            expiresAt = obj.optString("expires_at").takeIf { it.isNotBlank() && it != "null" },
            navidromePlaylistId = obj.optString("navidrome_playlist_id").takeIf { it.isNotBlank() && it != "null" },
            trackCount = obj.optInt("track_count", 0),
            targetTracks = obj.optInt("target_tracks", 25),
        )
    }

    private fun parseCultureProfile(obj: JSONObject): UserCultureProfileUi {
        val langs = mutableListOf<String>()
        val langsArr = obj.optJSONArray("preferred_languages") ?: JSONArray()
        for (i in 0 until langsArr.length()) {
            langsArr.optString(i).takeIf { it.isNotBlank() }?.let { langs.add(it) }
        }
        val weights = mutableListOf<Pair<String, Float>>()
        val wArr = obj.optJSONArray("inferred_area_weights") ?: JSONArray()
        for (i in 0 until wArr.length()) {
            val pair = wArr.optJSONArray(i) ?: continue
            if (pair.length() >= 2) {
                val code = pair.optString(0).trim()
                val weight = pair.optDouble(1, 0.0).toFloat()
                if (code.isNotBlank() && weight > 0f) weights.add(code to weight)
            }
        }
        val tags = mutableListOf<String>()
        val tagsArr = obj.optJSONArray("inferred_local_tags") ?: JSONArray()
        for (i in 0 until tagsArr.length()) {
            tagsArr.optString(i).takeIf { it.isNotBlank() }?.let { tags.add(it) }
        }
        return UserCultureProfileUi(
            homeCountry = obj.optString("home_country").takeIf { it.isNotBlank() && it != "null" },
            preferredLanguages = langs,
            localCultureRatio = obj.optDouble("local_culture_ratio", 0.45).toFloat().coerceIn(0f, 1f),
            inferredAreaWeights = weights,
            inferredLocalTags = tags,
        )
    }

    companion object {
        fun normalizeBaseUrl(raw: String): String {
            val trimmed = raw.trim().trimEnd('/')
            if (trimmed.isEmpty()) throw IOException("Server URL is required")
            val withScheme = if (trimmed.startsWith("http://", ignoreCase = true) ||
                trimmed.startsWith("https://", ignoreCase = true)
            ) {
                trimmed
            } else {
                "https://$trimmed"
            }
            return withScheme.trimEnd('/')
        }

        internal fun extractAuthTokenFromCookies(setCookieHeaders: List<String>): String? {
            for (header in setCookieHeaders) {
                val firstPart = header.substringBefore(';').trim()
                val eq = firstPart.indexOf('=')
                if (eq > 0) {
                    val name = firstPart.substring(0, eq).trim()
                    val value = firstPart.substring(eq + 1).trim()
                    if (name == "auth_token" && value.isNotEmpty()) {
                        return value
                    }
                }
            }
            return null
        }

        private fun extractErrorMessage(body: String): String? {
            val trimmed = body.trim()
            if (trimmed.isEmpty()) return null
            return runCatching {
                val obj = JSONObject(trimmed)
                obj.optString("message").takeIf { it.isNotBlank() }
                    ?: obj.optString("error").takeIf { it.isNotBlank() }
                    ?: obj.optString("detail").takeIf { it.isNotBlank() }
            }.getOrNull() ?: trimmed.take(160)
        }

        private fun defaultMixCopy(slug: String, i18nKey: String, country: String?): Pair<String, String> =
            when {
                slug.contains("noel") || slug.contains("christmas") || i18nKey.contains("winter_holiday") -> {
                    val title = if (country.equals("FR", ignoreCase = true)) {
                        "Noël à la Française"
                    } else {
                        "Winter Holiday Mix"
                    }
                    title to "Seasonal pack · excluded from core taste until kept"
                }
                slug == "comfort" || i18nKey.contains("comfort") ->
                    "Comfort Mix" to "Your highest-rated favorites & warm repeats"
                slug == "weekly_radar" || i18nKey.contains("weekly") ->
                    "Weekly Radar" to "Fresh discoveries tuned to your recent listening"
                slug == "deep_cuts" || i18nKey.contains("deep") ->
                    "Deep Cuts" to "Hidden gems from artists in your orbit"
                slug == "drive" ->
                    "Drive Mix" to "Upbeat momentum built for the road · offline ready"
                slug == "morning" ->
                    "Morning Lift" to "Bright openers to ease into the day"
                slug == "focus" ->
                    "Deep Focus" to "Steady ambient, jazz, and downtempo flow"
                slug == "late_night" ->
                    "Late Night Studio" to "Nocturnal trip-hop, dream pop & smooth cuts"
                else ->
                    slug.replace('_', ' ').replace('-', ' ')
                        .split(' ')
                        .joinToString(" ") { part -> part.replaceFirstChar { it.uppercase() } }
                        .ifBlank { "Curated Mix" } to "Contextual Soulbeet mix"
            }
    }
}
