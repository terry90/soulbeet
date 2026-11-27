package org.soulbeet.app.data

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONArray
import org.json.JSONObject
import java.io.IOException
import java.util.Calendar
import java.util.Locale
import java.util.concurrent.TimeUnit
import kotlin.coroutines.resume

data class LoginResult(
    val authKind: StoredAuthKind,
    val token: String,
    val username: String,
    val passwordChangeRequired: Boolean = false,
)

data class ParsedTokenInput(
    val authKind: StoredAuthKind,
    val token: String,
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

data class ContextualMixTrackUi(
    val songId: String,
    val discoveryTrackId: String?,
    val artist: String,
    val title: String,
    val album: String?,
    val durationSecs: Long?,
    val owned: Boolean,
    val rating: Int = 0,
    val starred: Boolean = false,
)

data class UserCultureProfileUi(
    val homeCountry: String?,
    val preferredLanguages: List<String>,
    val localCultureRatio: Float,
    val inferredAreaWeights: List<Pair<String, Float>>,
    val inferredLocalTags: List<String>,
    val weatherCity: String? = null,
)

data class ListenerProtectionUi(
    val kind: String,
    val summary: String,
) {
    val isHouseholdListenerProtection: Boolean
        get() = kind == "other_listener_mark" || kind == "other_listener_playlist"
}

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
) {
    val hasHouseholdListenerProtection: Boolean
        get() = otherListeners.any { it.isHouseholdListenerProtection }

    fun toPendingDropMode(): PendingDropMode = when {
        isPendingDiscovery -> PendingDropMode.DropDiscovery
        canDelete && soleOwner && !offlineFallback -> PendingDropMode.DeleteFromLibrary
        else -> PendingDropMode.UnstarAndHide
    }
}

sealed interface AcquireAndPlayOutcome {
    data class AlreadyOwned(val songId: String, val codec: String?) : AcquireAndPlayOutcome
    data class Queued(val requestId: String, val stage: String) : AcquireAndPlayOutcome
    data class Failed(val message: String) : AcquireAndPlayOutcome
}

sealed interface AcquireWsEvent {
    data class Progress(val stageLabel: String) : AcquireWsEvent
    data class Ready(val songId: String?, val codec: String?) : AcquireWsEvent
    data class NeedsManualChoice(val searchId: String) : AcquireWsEvent
    data class Failed(val message: String) : AcquireWsEvent
}

/**
 * Lightweight OkHttp client for Soulbeet REST & WebSocket endpoints complementing `lib/client-core`:
 * auth login/logout, structured lyrics, Multi-Listener Delete Guard preview, 1-tap Acquire & Play
 * (with live `/api/downloads/updates` WebSocket stage tracking), contextual mix playlist resolution,
 * seasonal culture steering, and Taste DNA culture profile.
 */
class SoulbeetApiClient(
    private val httpClient: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(12, TimeUnit.SECONDS)
        .readTimeout(25, TimeUnit.SECONDS)
        .pingInterval(20, TimeUnit.SECONDS)
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

                val parsedObj = runCatching { JSONObject(rawBody) }.getOrNull()
                val passwordChangeRequired = parsedObj?.optBoolean("password_change_required", false) ?: false
                if (passwordChangeRequired) {
                    throw IOException(
                        "Password change required — sign in on the Soulbeet web app to update your password first.",
                    )
                }

                val setCookies = response.headers("Set-Cookie")
                val authCookie = extractAuthTokenFromCookies(setCookies)
                    ?: throw IOException("Server succeeded but did not issue an auth_token session cookie.")

                val parsedUser = parsedObj?.optNonNullString("username") ?: username.trim()

                LoginResult(
                    authKind = StoredAuthKind.COOKIE,
                    token = authCookie,
                    username = parsedUser,
                    passwordChangeRequired = false,
                )
            }
        }
    }

    suspend fun logout(config: PersistedAppConfig): Unit = withContext(Dispatchers.IO) {
        if (!config.isConfigured) return@withContext
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val request = Request.Builder()
                .url("$base/api/auth/logout")
                .post("{}".toRequestBody(jsonMediaType))
                .applyAuth(config)
                .build()
            httpClient.newCall(request).execute().close()
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
                parseLyricsResponse(raw)
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
            val url = "$base/api/curation/guard-preview".toHttpUrlOrNull()
                ?.newBuilder()
                ?.addPathSegment(songId)
                ?.build()
                ?: throw IOException("Invalid Soulbeet server URL")

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
                            kind = item.optNonNullString("kind") ?: "other",
                            summary = item.optNonNullString("summary") ?: "Shared listener",
                        ),
                    )
                }
                val ownPlaylists = mutableListOf<String>()
                val plArr = obj.optJSONArray("own_playlists") ?: JSONArray()
                for (i in 0 until plArr.length()) {
                    if (!plArr.isNull(i)) {
                        plArr.optString(i).trim().takeIf { it.isNotEmpty() }?.let { ownPlaylists.add(it) }
                    }
                }
                DeleteGuardPreviewUi(
                    songId = obj.optNonNullString("song_id") ?: songId,
                    trackTitle = trackTitle,
                    trackArtist = trackArtist,
                    isPendingDiscovery = obj.optBoolean("is_pending_discovery", false),
                    discoveryTrackId = obj.optNonNullString("discovery_track_id"),
                    canDelete = obj.optBoolean("can_delete", false),
                    soleOwner = obj.optBoolean("sole_owner", false),
                    otherListeners = listeners,
                    ownPlaylists = ownPlaylists,
                    blockReason = obj.optNonNullString("block_reason"),
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
                val requestId = obj.optNonNullString("request_id").orEmpty()
                val statusObj = obj.optJSONObject("status")
                val kind = statusObj?.optNonNullString("kind").orEmpty()
                if (kind == "already_owned") {
                    val songId = statusObj?.optNonNullString("navidrome_song_id").orEmpty()
                    val codec = statusObj?.optNonNullString("codec")
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

    /**
     * Subscribes to `/api/downloads/updates` over WebSocket and streams live
     * `DownloadEvent::AcquireAndPlay` stages for [requestId] until a terminal event
     * (`Ready`, `NeedsManualChoice`, or `Failed`) arrives or [timeoutMs] elapses.
     */
    suspend fun observeAcquireAndPlay(
        config: PersistedAppConfig,
        requestId: String,
        timeoutMs: Long = 90_000L,
        onStage: (String) -> Unit,
    ): AcquireWsEvent = withTimeoutOrNull(timeoutMs) {
        suspendCancellableCoroutine { cont ->
            val base = runCatching { normalizeBaseUrl(config.serverUrl) }.getOrNull()
            val wsUrl = base?.let {
                val httpUrl = "$it/api/downloads/updates".toHttpUrlOrNull()?.newBuilder()
                    ?.addQueryParameter("build_frame", "false")
                    ?.addQueryParameter("snapshot", "false")
                    ?.build()
                    ?.toString()
                httpUrl?.replaceFirst("https://", "wss://")
                    ?.replaceFirst("http://", "ws://")
            }

            if (wsUrl == null) {
                cont.resume(AcquireWsEvent.Failed("Invalid server URL"))
                return@suspendCancellableCoroutine
            }

            val req = Request.Builder()
                .url(wsUrl)
                .applyAuth(config)
                .build()

            val terminal = CompletableDeferred<AcquireWsEvent>()
            val socket = httpClient.newWebSocket(
                req,
                object : WebSocketListener() {
                    override fun onMessage(webSocket: WebSocket, text: String) {
                        val ev = parseAcquireAndPlayWsFrame(text, requestId) ?: return
                        when (ev) {
                            is AcquireWsEvent.Progress -> onStage(ev.stageLabel)
                            is AcquireWsEvent.Ready,
                            is AcquireWsEvent.NeedsManualChoice,
                            is AcquireWsEvent.Failed -> {
                                if (terminal.complete(ev)) {
                                    webSocket.close(1000, "done")
                                    if (cont.isActive) cont.resume(ev)
                                }
                            }
                        }
                    }

                    override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                        val err = AcquireWsEvent.Failed(t.localizedMessage ?: "Download updates socket closed")
                        if (terminal.complete(err) && cont.isActive) {
                            cont.resume(err)
                        }
                    }

                    override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                        if (!terminal.isCompleted && cont.isActive) {
                            val err = AcquireWsEvent.Failed("Acquisition stream closed before ready")
                            if (terminal.complete(err)) cont.resume(err)
                        }
                    }
                },
            )

            cont.invokeOnCancellation {
                socket.cancel()
            }
        }
    } ?: AcquireWsEvent.Failed("Acquisition timed out — check Activity on server")

    suspend fun fetchContextualMixes(
        config: PersistedAppConfig,
        localHour: Int,
        localWeekday: Int = currentIsoWeekday(),
        lat: Double? = null,
        lon: Double? = null,
    ): Result<List<ContextualMixUi>> = withContext(Dispatchers.IO) {
        runCatching {
            val url = buildContextualMixesUrl(
                serverUrl = config.serverUrl,
                localHour = localHour,
                localWeekday = localWeekday,
                lat = lat,
                lon = lon,
            )

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

    suspend fun fetchContextualMixPlaylist(
        config: PersistedAppConfig,
        slug: String,
    ): Result<List<ContextualMixTrackUi>> = withContext(Dispatchers.IO) {
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val url = "$base/api/discovery/contextual-mixes/playlist".toHttpUrlOrNull()
                ?.newBuilder()
                ?.addQueryParameter("slug", slug)
                ?.build()
                ?: throw IOException("Invalid Soulbeet server URL")

            val request = Request.Builder()
                .url(url)
                .get()
                .applyAuth(config)
                .build()

            httpClient.newCall(request).execute().use { response ->
                val raw = response.body?.string().orEmpty()
                if (!response.isSuccessful) {
                    throw IOException(extractErrorMessage(raw) ?: "Mix playlist HTTP ${response.code}")
                }
                val obj = JSONObject(raw)
                val tracksArr = obj.optJSONArray("tracks") ?: JSONArray()
                buildList {
                    for (i in 0 until tracksArr.length()) {
                        val t = tracksArr.optJSONObject(i) ?: continue
                        val songId = t.optNonNullString("song_id") ?: continue
                        val dur = if (t.has("duration_s") && !t.isNull("duration_s")) {
                            t.optLong("duration_s")
                        } else {
                            null
                        }
                        val rating = if (t.has("rating") && !t.isNull("rating")) {
                            t.optInt("rating", 0).coerceIn(0, 5)
                        } else {
                            0
                        }
                        val starred = t.optBoolean("starred", false)
                        add(
                            ContextualMixTrackUi(
                                songId = songId,
                                discoveryTrackId = t.optNonNullString("discovery_track_id"),
                                artist = t.optNonNullString("artist").orEmpty(),
                                title = t.optNonNullString("title").orEmpty(),
                                album = t.optNonNullString("album"),
                                durationSecs = dur,
                                owned = t.optBoolean("owned", true),
                                rating = rating,
                                starred = starred,
                            ),
                        )
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
        weatherCity: String? = null,
    ): Result<UserCultureProfileUi> = withContext(Dispatchers.IO) {
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val inner = JSONObject()
                .put("home_country", homeCountry.orEmpty().trim().uppercase())
                .put("local_culture_ratio", localCultureRatio.coerceIn(0f, 1f).toDouble())
            if (weatherCity != null) {
                inner.put("weather_city", weatherCity.trim())
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

    internal fun parseContextualMix(obj: JSONObject): ContextualMixUi {
        val slug = obj.optNonNullString("slug").orEmpty()
        val i18nKey = obj.optNonNullString("i18n_key").orEmpty()
        val customName = obj.optNonNullString("custom_name")
        val customDesc = obj.optNonNullString("custom_description")
        val country = obj.optNonNullString("country_code")
        val localPct = if (obj.has("local_pct") && !obj.isNull("local_pct")) obj.optInt("local_pct") else null
        val (fallbackTitle, fallbackSub) = defaultMixCopy(slug, i18nKey)
        return ContextualMixUi(
            slug = slug,
            kind = obj.optNonNullString("kind") ?: "rolling",
            i18nKey = i18nKey,
            title = customName ?: fallbackTitle,
            subtitle = customDesc ?: fallbackSub,
            countryCode = country,
            localPct = localPct,
            ownedPct = obj.optInt("owned_pct", 75),
            offlinePriority = obj.optBoolean("offline_priority", false),
            excludeFromCoreTaste = obj.optBoolean("exclude_from_core_taste", false),
            steering = CultureSteeringOption.fromWire(obj.optNonNullString("steering")),
            activeFrom = obj.optNonNullString("active_from"),
            activeUntil = obj.optNonNullString("active_until"),
            expiresAt = obj.optNonNullString("expires_at"),
            navidromePlaylistId = obj.optNonNullString("navidrome_playlist_id"),
            trackCount = obj.optInt("track_count", 0),
            targetTracks = obj.optInt("target_tracks", 25),
        )
    }

    private fun parseCultureProfile(obj: JSONObject): UserCultureProfileUi {
        val langs = mutableListOf<String>()
        val langsArr = obj.optJSONArray("preferred_languages") ?: JSONArray()
        for (i in 0 until langsArr.length()) {
            if (!langsArr.isNull(i)) {
                langsArr.optString(i).trim().takeIf { it.isNotEmpty() }?.let { langs.add(it) }
            }
        }
        val weights = mutableListOf<Pair<String, Float>>()
        val wArr = obj.optJSONArray("inferred_area_weights") ?: JSONArray()
        for (i in 0 until wArr.length()) {
            val pair = wArr.optJSONArray(i) ?: continue
            if (pair.length() >= 2 && !pair.isNull(0)) {
                val code = pair.optString(0).trim()
                val weight = pair.optDouble(1, 0.0).toFloat()
                if (code.isNotBlank() && weight > 0f) weights.add(code to weight)
            }
        }
        val tags = mutableListOf<String>()
        val tagsArr = obj.optJSONArray("inferred_local_tags") ?: JSONArray()
        for (i in 0 until tagsArr.length()) {
            if (!tagsArr.isNull(i)) {
                tagsArr.optString(i).trim().takeIf { it.isNotEmpty() }?.let { tags.add(it) }
            }
        }
        return UserCultureProfileUi(
            homeCountry = obj.optNonNullString("home_country"),
            preferredLanguages = langs,
            localCultureRatio = obj.optDouble("local_culture_ratio", 0.45).toFloat().coerceIn(0f, 1f),
            inferredAreaWeights = weights,
            inferredLocalTags = tags,
            weatherCity = obj.optNonNullString("weather_city"),
        )
    }

    companion object {
        private val LRC_TIMESTAMP_REGEX = Regex("""\[\d{1,2}:\d{2}(?:[.:]\d{1,3})?]""")

        fun parseLyricsResponse(raw: String): TrackLyricsData? {
            val arr = JSONArray(raw)
            if (arr.length() == 0) return null

            var chosen: JSONObject? = null
            for (i in 0 until arr.length()) {
                val candidate = arr.optJSONObject(i) ?: continue
                val lineCount = candidate.optJSONArray("lines")?.length() ?: 0
                if (candidate.optBoolean("synced", false) && lineCount > 0) {
                    chosen = candidate
                    break
                }
            }
            if (chosen == null) {
                for (i in 0 until arr.length()) {
                    val candidate = arr.optJSONObject(i) ?: continue
                    val lineCount = candidate.optJSONArray("lines")?.length() ?: 0
                    if (lineCount > 0) {
                        chosen = candidate
                        break
                    }
                }
            }
            val selected = chosen ?: arr.optJSONObject(0) ?: return null
            val linesArr = selected.optJSONArray("lines") ?: JSONArray()
            val lines = buildList {
                for (i in 0 until linesArr.length()) {
                    val obj = linesArr.optJSONObject(i) ?: continue
                    val startMs = if (obj.has("start_ms") && !obj.isNull("start_ms")) {
                        obj.optLong("start_ms")
                    } else {
                        null
                    }
                    val rawText = if (obj.has("text") && !obj.isNull("text")) {
                        obj.optString("text", "")
                    } else {
                        ""
                    }
                    val text = rawText.trim()
                    if (text.isEmpty() && startMs == null) continue
                    add(LyricLine(startMs = startMs, text = text))
                }
            }
            val isWireSynced = selected.optBoolean("synced", false)
            val finalLines = if (
                !isWireSynced &&
                lines.isNotEmpty() &&
                lines.all { it.startMs == null } &&
                lines.any { LRC_TIMESTAMP_REGEX.containsMatchIn(it.text) }
            ) {
                LyricsSync.parseLrc(lines.joinToString("\n") { it.text })
            } else {
                lines
            }
            return TrackLyricsData(
                displayArtist = selected.optNonNullString("display_artist"),
                displayTitle = selected.optNonNullString("display_title"),
                lang = selected.optNonNullString("lang").orEmpty(),
                synced = isWireSynced || finalLines.any { it.startMs != null },
                offsetMs = selected.optLong("offset_ms", 0L),
                lines = finalLines,
            )
        }

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

        /**
         * Cleans user-pasted token strings, recognizing `Bearer <tok>` and `auth_token=<tok>` prefixes.
         */
        fun parseTokenInput(rawInput: String, defaultIsBearer: Boolean): ParsedTokenInput {
            val trimmed = rawInput.trim()
            return when {
                trimmed.startsWith("Bearer ", ignoreCase = true) -> ParsedTokenInput(
                    authKind = StoredAuthKind.BEARER,
                    token = trimmed.substring(7).trim(),
                )
                trimmed.startsWith("auth_token=", ignoreCase = true) -> ParsedTokenInput(
                    authKind = StoredAuthKind.COOKIE,
                    token = trimmed.substring(11).substringBefore(';').trim(),
                )
                else -> ParsedTokenInput(
                    authKind = if (defaultIsBearer) StoredAuthKind.BEARER else StoredAuthKind.COOKIE,
                    token = trimmed,
                )
            }
        }

        /**
         * Parses a single JSON text frame from `/api/downloads/updates` matching [expectedRequestId].
         */
        fun parseAcquireAndPlayWsFrame(text: String, expectedRequestId: String): AcquireWsEvent? {
            val root = runCatching { JSONObject(text) }.getOrNull() ?: return null
            val ev = root.optJSONObject("AcquireAndPlay") ?: return null
            val reqId = ev.optNonNullString("request_id") ?: return null
            if (reqId != expectedRequestId) return null
            val stageObj = ev.optJSONObject("stage") ?: return null
            return when (stageObj.optNonNullString("stage")) {
                "acquiring" -> AcquireWsEvent.Progress("ACQUIRING…")
                "importing" -> AcquireWsEvent.Progress("IMPORTING…")
                "indexing" -> AcquireWsEvent.Progress("INDEXING…")
                "ready" -> AcquireWsEvent.Ready(
                    songId = stageObj.optNonNullString("navidrome_song_id"),
                    codec = stageObj.optNonNullString("codec"),
                )
                "needs_manual_choice" -> AcquireWsEvent.NeedsManualChoice(
                    searchId = stageObj.optNonNullString("search_id").orEmpty(),
                )
                "failed" -> {
                    val failureObj = stageObj.optJSONObject("failure")
                    val msg = failureObj?.optNonNullString("message")
                        ?: failureObj?.optNonNullString("summary")
                        ?: failureObj?.optNonNullString("detail")
                        ?: "Acquisition failed"
                    AcquireWsEvent.Failed(msg)
                }
                else -> null
            }
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
                obj.optNonNullString("message")
                    ?: obj.optNonNullString("error")
                    ?: obj.optNonNullString("detail")
            }.getOrNull() ?: trimmed.take(160)
        }

        internal fun buildContextualMixesUrl(
            serverUrl: String,
            localHour: Int,
            localWeekday: Int = currentIsoWeekday(),
            lat: Double? = null,
            lon: Double? = null,
        ): HttpUrl {
            val base = normalizeBaseUrl(serverUrl)
            val builder = "$base/api/discovery/contextual-mixes".toHttpUrlOrNull()
                ?.newBuilder()
                ?.addQueryParameter("local_hour", localHour.coerceIn(0, 23).toString())
                ?.addQueryParameter("local_weekday", localWeekday.coerceIn(1, 7).toString())
                ?: throw IOException("Invalid Soulbeet server URL")
            if (lat != null && lon != null) {
                val sanitized = CoarseLocationProvider.sanitizeCoarseCoordinates(lat, lon)
                if (sanitized != null) {
                    builder.addQueryParameter("lat", String.format(Locale.ROOT, "%.1f", sanitized.first))
                    builder.addQueryParameter("lon", String.format(Locale.ROOT, "%.1f", sanitized.second))
                }
            }
            return builder.build()
        }

        internal fun calendarDayOfWeekToIso(dayOfWeek: Int): Int = when (dayOfWeek) {
            Calendar.MONDAY -> 1
            Calendar.TUESDAY -> 2
            Calendar.WEDNESDAY -> 3
            Calendar.THURSDAY -> 4
            Calendar.FRIDAY -> 5
            Calendar.SATURDAY -> 6
            Calendar.SUNDAY -> 7
            else -> 1
        }

        internal fun currentIsoWeekday(): Int =
            calendarDayOfWeekToIso(Calendar.getInstance().get(Calendar.DAY_OF_WEEK))

        internal fun defaultMixCopy(slug: String, i18nKey: String): Pair<String, String> {
            fun lookup(raw: String): Pair<String, String>? =
                when (raw.trim().lowercase(Locale.ROOT)) {
                    "mix.seasonal.winter_holiday", "winter_holiday", "noel", "christmas" ->
                        "Christmas Mix" to "Christmas favorites and festive classics, blended to your taste."
                    "mix.seasonal.new_years_eve", "new_years_eve", "nye" ->
                        "New Year's Eve" to "High-energy dance floor staples and regional party picks to close the year."
                    "mix.seasonal.cozy_winter", "cozy_winter" ->
                        "Cozy Winter" to "Fireside folk, gentle ambient, and acoustic warmth for cold winter days."
                    "mix.seasonal.lunar_new_year", "lunar_new_year" ->
                        "Lunar New Year" to "Celebratory regional pop and fresh festive picks for the new lunar cycle."
                    "mix.seasonal.valentines", "valentines" ->
                        "Date Night" to "Smooth soul, jazz, R&B, and romantic classics set for an intimate evening."
                    "mix.seasonal.carnival", "carnival" ->
                        "Carnival Beats" to "Percussive rhythms, samba, funk, and vibrant carnival energy."
                    "mix.seasonal.celtic_spring", "celtic_spring" ->
                        "Celtic Folk" to "Spirited Irish trad, Celtic folk, and lively acoustic pub anthems."
                    "mix.seasonal.spring", "spring" ->
                        "Spring Awakening" to "Fresh indie, acoustic warmth, and bright new-season discoveries."
                    "mix.seasonal.summer", "summer" ->
                        "Summer Anthems" to "Sun-soaked grooves, upbeat pop, and warm-weather classics."
                    "mix.seasonal.summer_solstice", "summer_solstice" ->
                        "Midsummer Solstice" to "Golden-hour indie, festival electro, and long-evening summer sounds."
                    "mix.seasonal.late_summer_reset", "late_summer_reset" ->
                        "Late Summer Reset" to "Hazy dream pop, indie grooves, and bittersweet September transitions."
                    "mix.seasonal.autumn", "autumn" ->
                        "Autumn Sessions" to "Cozy acoustic textures, folk, jazz, and mellow indie cuts."
                    "mix.seasonal.halloween", "halloween" ->
                        "Darkwave Halloween" to "Atmospheric darkwave, post-punk, and nocturnal synth textures."
                    "mix.moment.drive", "drive" ->
                        "Drive Mix" to "Offline-prioritized favorites and upbeat momentum built for the road."
                    "mix.moment.morning", "morning" ->
                        "Morning Lift" to "Bright grooves and familiar lifts to ease into the day."
                    "mix.moment.focus", "focus" ->
                        "Deep Focus" to "Steady ambient, instrumental, and downtempo cuts for uninterrupted flow."
                    "mix.moment.evening_unwind", "evening_unwind" ->
                        "Evening Unwind" to "Warm soul, acoustic folk, and mellow cuts to ease out of the day."
                    "mix.moment.dinner_groove", "dinner_groove" ->
                        "Dinner Groove" to "Feel-good funk, bossa nova, and nu-disco grooves for cooking and company."
                    "mix.moment.late_night", "late_night" ->
                        "Late Night" to "Low-lit dream pop, trip hop, and midnight library favorites."
                    "mix.moment.rainy_day", "rainy_day" ->
                        "Rainy Day" to "Soft acoustic folk, piano, and warm reflective melodies for rainy weather."
                    "mix.moment.snow_day", "snow_day" ->
                        "Snow Day" to "Hushed Nordic folk, gentle piano, and ambient warmth for snowy skies."
                    "mix.moment.storm_night", "storm_night" ->
                        "Storm & Thunder" to "Brooding post-rock, shoegaze, and moody trip hop built for stormy weather."
                    "mix.moment.sunny_day", "sunny_day" ->
                        "Sunny Vibes" to "Bright funk, soul, indie pop, and sun-drenched grooves for clear skies."
                    "mix.moment.friday_night", "friday_night" ->
                        "Friday Night Warmup" to "Upbeat electro, disco, and indie energy to kick off the weekend."
                    "mix.moment.sunday_morning", "sunday_morning" ->
                        "Sunday Morning Coffee" to "Unrushed jazz, warm soul, and acoustic favorites for a slow start."
                    "mix.moment.forgotten_gems", "forgotten_gems" ->
                        "Forgotten Gems" to "Top-rated favorites from your library that you have not spun in months."
                    "mix.moment.fresh_arrivals", "fresh_arrivals" ->
                        "Fresh Arrivals" to "Recently added tracks in your library waiting for their first proper listen."
                    "mix.moment.decade_80s", "decade_80s" ->
                        "80s Wave" to "Your library's finest 1980s synth, new wave, and classic cuts."
                    "mix.moment.decade_90s", "decade_90s" ->
                        "90s Mix" to "Standout 1990s records and golden-era favorites from your collection."
                    "mix.moment.decade_2000s", "decade_2000s" ->
                        "2000s Mix" to "Essential 2000s indie, electronic, and throwback tracks from your library."
                    "mix.rolling.conservative", "mix.rolling.comfort", "conservative", "comfort" ->
                        "Comfort Zone" to "High-rotation favorites and trusted genres from your library."
                    "mix.rolling.balanced", "mix.rolling.weekly_radar", "balanced", "weekly_radar" ->
                        "Fresh Picks" to "Fresh recommendations tuned to your recent listening momentum."
                    "mix.rolling.adventurous", "mix.rolling.deep_cuts", "adventurous", "deep_cuts" ->
                        "Deep Cuts" to "Hidden gems and adventurous tracks beyond your usual rotation."
                    else -> null
                }

            val trimmedKey = i18nKey.trim()
            if (trimmedKey.isNotEmpty()) {
                lookup(trimmedKey)?.let { return it }
            }
            lookup(slug)?.let { return it }

            val fallbackSource = trimmedKey
                .substringAfterLast('.')
                .trim()
                .ifBlank { slug }
            val title = fallbackSource
                .split('_', '-', '.')
                .map { it.trim() }
                .filter { it.isNotEmpty() }
                .joinToString(" ") { part -> part.replaceFirstChar { it.titlecase(Locale.ROOT) } }
                .ifBlank { "Curated Mix" }
            return title to "Contextual Soulbeet mix"
        }
    }
}

internal fun JSONObject.optNonNullString(key: String): String? {
    if (!has(key) || isNull(key)) return null
    return optString(key).trim().takeIf { it.isNotEmpty() }
}
