package org.soulbeet.app.data

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
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
                val arr = JSONArray(raw)
                if (arr.length() == 0) return@use null

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
                        val text = obj.optNonNullString("text") ?: continue
                        val startMs = if (obj.has("start_ms") && !obj.isNull("start_ms")) {
                            obj.optLong("start_ms")
                        } else {
                            null
                        }
                        add(LyricLine(startMs = startMs, text = text))
                    }
                }
                val finalLines = if (lines.size == 1 && lines[0].startMs == null && lines[0].text.contains("[")) {
                    LyricsSync.parseLrc(lines[0].text)
                } else {
                    lines
                }
                TrackLyricsData(
                    displayArtist = chosen.optNonNullString("display_artist"),
                    displayTitle = chosen.optNonNullString("display_title"),
                    lang = chosen.optNonNullString("lang").orEmpty(),
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
    ): Result<List<ContextualMixUi>> = withContext(Dispatchers.IO) {
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val url = "$base/api/discovery/contextual-mixes".toHttpUrlOrNull()
                ?.newBuilder()
                ?.addQueryParameter("local_hour", localHour.coerceIn(0, 23).toString())
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
                        add(
                            ContextualMixTrackUi(
                                songId = songId,
                                discoveryTrackId = t.optNonNullString("discovery_track_id"),
                                artist = t.optNonNullString("artist").orEmpty(),
                                title = t.optNonNullString("title").orEmpty(),
                                album = t.optNonNullString("album"),
                                durationSecs = dur,
                                owned = t.optBoolean("owned", true),
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
    ): Result<UserCultureProfileUi> = withContext(Dispatchers.IO) {
        runCatching {
            val base = normalizeBaseUrl(config.serverUrl)
            val inner = JSONObject()
                .put("home_country", homeCountry.orEmpty().trim().uppercase())
                .put("local_culture_ratio", localCultureRatio.coerceIn(0f, 1f).toDouble())
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
        val slug = obj.optNonNullString("slug").orEmpty()
        val i18nKey = obj.optNonNullString("i18n_key").orEmpty()
        val customName = obj.optNonNullString("custom_name")
        val customDesc = obj.optNonNullString("custom_description")
        val country = obj.optNonNullString("country_code")
        val localPct = if (obj.has("local_pct") && !obj.isNull("local_pct")) obj.optInt("local_pct") else null
        val (fallbackTitle, fallbackSub) = defaultMixCopy(slug, i18nKey, country)
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

        private fun defaultMixCopy(slug: String, i18nKey: String, country: String?): Pair<String, String> =
            when {
                slug.contains("noel") || slug.contains("christmas") || i18nKey.contains("winter_holiday") -> {
                    val title = when (country?.uppercase()) {
                        "FR" -> "Noël à la Française"
                        "DE", "AT", "CH" -> "Weihnachten Mix"
                        "ES", "MX" -> "Navidad Mix"
                        "IT" -> "Natale Mix"
                        else -> "Christmas Mix"
                    }
                    title to "Christmas favorites · excluded from core taste until kept"
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

internal fun JSONObject.optNonNullString(key: String): String? {
    if (!has(key) || isNull(key)) return null
    return optString(key).trim().takeIf { it.isNotEmpty() }
}
