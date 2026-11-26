package org.soulbeet.app

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.soulbeet.app.data.AcquireWsEvent
import org.soulbeet.app.data.FormattingUtils
import org.soulbeet.app.data.SoulbeetApiClient
import org.soulbeet.app.data.StoredAuthKind
import org.soulbeet.app.data.optNonNullString
import org.soulbeet.app.theme.BeetCanvas
import org.soulbeet.app.theme.BeetCanvasOled
import org.soulbeet.app.theme.soulbeetPaletteFor
import org.soulbeet.core.FfiAudioQualityProfile

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class FormattingAndThemeTest {

    @Test
    fun formatDurationHandlesZeroMinutesAndHours() {
        assertEquals("00:00", FormattingUtils.formatDurationMs(-500L))
        assertEquals("03:42", FormattingUtils.formatDurationSecs(222L))
        assertEquals("1:05:12", FormattingUtils.formatDurationSecs(3912L))
    }

    @Test
    fun cacheBudgetFormattingAndFractions() {
        assertEquals("0 B", FormattingUtils.formatBytes(0L))
        assertEquals("512 KB", FormattingUtils.formatBytes(512L * 1024L))
        assertEquals("320 MB", FormattingUtils.formatBytes(320L * 1024L * 1024L))
        assertEquals(
            "1.5 GB / 4.0 GB",
            FormattingUtils.formatCacheBudget(
                usedBytes = (1.5 * 1024 * 1024 * 1024).toLong(),
                maxBytes = 4L * 1024L * 1024L * 1024L,
            ),
        )
        assertEquals(0.25f, FormattingUtils.cacheBudgetFraction(250L, 1_000L), 0.001f)
        assertEquals(1.0f, FormattingUtils.cacheBudgetFraction(2_000L, 1_000L), 0.001f)
        assertEquals(0.0f, FormattingUtils.cacheBudgetFraction(500L, 0L), 0.001f)
    }

    @Test
    fun tasteDnaSummaryNormalizesAndFormatsTopLocales() {
        val summary = FormattingUtils.formatTasteDnaSummary(
            listOf("fr" to 0.38f, "us" to 0.50f, "jp" to 0.12f),
        )
        assertEquals("US 50% · FR 38% · JP 12%", summary)
        assertEquals("No locale signal yet", FormattingUtils.formatTasteDnaSummary(emptyList()))
    }

    @Test
    fun codecBadgeReflectsQualityProfile() {
        assertEquals(
            "OPUS 160",
            FormattingUtils.formatCodecBadge("flac", 980L, FfiAudioQualityProfile.CellularOpus160),
        )
        assertEquals(
            "FLAC",
            FormattingUtils.formatCodecBadge("flac", 980L, FfiAudioQualityProfile.LosslessFlac),
        )
        assertEquals(
            "MP3 320",
            FormattingUtils.formatCodecBadge("mp3", 320L, FfiAudioQualityProfile.LosslessFlac),
        )
    }

    @Test
    fun discoveryReasonAndExpiryParsing() {
        assertEquals(
            "Because you starred Daft Punk",
            FormattingUtils.parseDiscoveryReason("""{"summary":"Because you starred Daft Punk"}"""),
        )
        assertEquals(
            "FRENCH ELECTRO",
            FormattingUtils.parseDiscoveryReason("""{"signal":"french_electro"}"""),
        )
        assertNull(FormattingUtils.parseDiscoveryReason("  "))

        val now = 1_700_000_000_000L
        val nineDaysLaterIso = java.time.Instant.ofEpochMilli(now + 9L * 86_400_000L + 3_600_000L).toString()
        assertEquals("9D LEFT", FormattingUtils.formatExpiryCountdown(nineDaysLaterIso, nowEpochMs = now))
    }

    @Test
    fun oledThemeSwitchTogglesCanvasToTrueBlack() {
        val midnight = soulbeetPaletteFor(oledMode = false)
        val oled = soulbeetPaletteFor(oledMode = true)

        assertFalse(midnight.isOled)
        assertEquals(BeetCanvas, midnight.canvas)

        assertTrue(oled.isOled)
        assertEquals(BeetCanvasOled, oled.canvas)
    }

    @Test
    fun apiClientExtractsAuthTokenCookieParsesTokenPrefixesAndNormalizesUrls() {
        val cookies = listOf(
            "other=abc; Path=/",
            "auth_token=tok_secret_123; HttpOnly; SameSite=Lax; Path=/",
        )
        assertEquals("tok_secret_123", SoulbeetApiClient.extractAuthTokenFromCookies(cookies))
        assertNull(SoulbeetApiClient.extractAuthTokenFromCookies(listOf("session=xyz")))

        val parsedBearer = SoulbeetApiClient.parseTokenInput("Bearer my_jwt_99", defaultIsBearer = false)
        assertEquals(StoredAuthKind.BEARER, parsedBearer.authKind)
        assertEquals("my_jwt_99", parsedBearer.token)

        val parsedCookie = SoulbeetApiClient.parseTokenInput("auth_token=cook_77; Path=/", defaultIsBearer = true)
        assertEquals(StoredAuthKind.COOKIE, parsedCookie.authKind)
        assertEquals("cook_77", parsedCookie.token)

        val rawBearer = SoulbeetApiClient.parseTokenInput("raw_tok", defaultIsBearer = true)
        assertEquals(StoredAuthKind.BEARER, rawBearer.authKind)
        assertEquals("raw_tok", rawBearer.token)

        assertEquals("https://music.example.com", SoulbeetApiClient.normalizeBaseUrl("music.example.com/"))
        assertEquals("http://192.168.1.5:9765", SoulbeetApiClient.normalizeBaseUrl("http://192.168.1.5:9765/"))
    }

    @Test
    fun acquireAndPlayWsFrameParserHandlesAllStagesAndFiltersRequestId() {
        val reqId = "req-abc"

        // Non-matching request_id is ignored
        assertNull(
            SoulbeetApiClient.parseAcquireAndPlayWsFrame(
                """{"AcquireAndPlay":{"request_id":"other","stage":{"stage":"importing"}}}""",
                reqId,
            ),
        )

        // Progress stages
        assertEquals(
            AcquireWsEvent.Progress("ACQUIRING…"),
            SoulbeetApiClient.parseAcquireAndPlayWsFrame(
                """{"AcquireAndPlay":{"request_id":"req-abc","stage":{"stage":"acquiring","backend_id":"slskd"}}}""",
                reqId,
            ),
        )
        assertEquals(
            AcquireWsEvent.Progress("IMPORTING…"),
            SoulbeetApiClient.parseAcquireAndPlayWsFrame(
                """{"AcquireAndPlay":{"request_id":"req-abc","stage":{"stage":"importing"}}}""",
                reqId,
            ),
        )
        assertEquals(
            AcquireWsEvent.Progress("INDEXING…"),
            SoulbeetApiClient.parseAcquireAndPlayWsFrame(
                """{"AcquireAndPlay":{"request_id":"req-abc","stage":{"stage":"indexing"}}}""",
                reqId,
            ),
        )

        // Ready
        assertEquals(
            AcquireWsEvent.Ready(songId = "song-999", codec = "opus"),
            SoulbeetApiClient.parseAcquireAndPlayWsFrame(
                """{"AcquireAndPlay":{"request_id":"req-abc","stage":{"stage":"ready","navidrome_song_id":"song-999","codec":"opus"}}}""",
                reqId,
            ),
        )

        // NeedsManualChoice
        assertEquals(
            AcquireWsEvent.NeedsManualChoice(searchId = "srch-1"),
            SoulbeetApiClient.parseAcquireAndPlayWsFrame(
                """{"AcquireAndPlay":{"request_id":"req-abc","stage":{"stage":"needs_manual_choice","search_id":"srch-1"}}}""",
                reqId,
            ),
        )

        // Failed
        assertEquals(
            AcquireWsEvent.Failed("No peer available"),
            SoulbeetApiClient.parseAcquireAndPlayWsFrame(
                """{"AcquireAndPlay":{"request_id":"req-abc","stage":{"stage":"failed","failure":{"message":"No peer available"}}}}""",
                reqId,
            ),
        )
    }

    @Test
    fun optNonNullStringReturnsNullForJsonNullAndBlanks() {
        val obj = JSONObject("""{"present":"hello","nil":null,"blank":"   "}""")
        assertEquals("hello", obj.optNonNullString("present"))
        assertNull(obj.optNonNullString("nil"))
        assertNull(obj.optNonNullString("blank"))
        assertNull(obj.optNonNullString("missing"))
    }

    @Test
    fun parseLyricsResponseSkipsEmptySyncedEntryAndParsesMultiLineLrc() {
        val raw = """
            [
                {
                    "display_artist": "Daft Punk",
                    "display_title": "Instant Crush",
                    "lang": "en",
                    "synced": true,
                    "offset_ms": 0,
                    "lines": []
                },
                {
                    "display_artist": "Daft Punk",
                    "display_title": "Instant Crush",
                    "lang": "en",
                    "synced": false,
                    "offset_ms": 0,
                    "lines": [
                        {"start_ms": null, "text": "[00:12.50]First line"},
                        {"start_ms": null, "text": "[00:18.00]Second line"}
                    ]
                }
            ]
        """.trimIndent()

        val parsed = SoulbeetApiClient.parseLyricsResponse(raw)
        assertTrue(parsed != null)
        assertTrue(parsed!!.synced)
        assertEquals(2, parsed.lines.size)
        assertEquals(12_500L, parsed.lines[0].startMs)
        assertEquals("First line", parsed.lines[0].text)
        assertEquals(18_000L, parsed.lines[1].startMs)
        assertEquals("Second line", parsed.lines[1].text)
    }

    @Test
    fun libraryCategoryOfflineLabelIsCachedAndPlaylistsPartitionCleanly() {
        assertEquals("Cached", LibraryCategory.OFFLINE.label)

        val userPl = org.soulbeet.core.FfiPlaylist(
            id = "pl-user",
            name = "Gym",
            comment = null,
            owner = "alice",
            songCount = 12L,
            durationSecs = 2400L,
            coverArtId = null,
            isSmart = false,
            kind = "user",
            slug = null,
            changedAt = null,
            updatedAt = 0L,
        )
        val seasonalPl = userPl.copy(
            id = "pl-seasonal",
            name = "Winter Holiday",
            isSmart = true,
            kind = "seasonal",
            slug = "winter_holiday",
        )
        val discoveryPl = userPl.copy(
            id = "pl-disc",
            name = "Balanced",
            isSmart = true,
            kind = "discovery",
            slug = null,
        )
        val ctxMix = org.soulbeet.app.data.ContextualMixUi(
            slug = "winter_holiday",
            kind = "seasonal",
            i18nKey = "mix.seasonal.winter_holiday",
            title = "Winter Holiday",
            subtitle = "Seasonal",
            countryCode = "FR",
            localPct = 50,
            ownedPct = 75,
            offlinePriority = false,
            excludeFromCoreTaste = true,
            steering = org.soulbeet.app.data.CultureSteeringOption.BALANCED,
            activeFrom = null,
            activeUntil = null,
            expiresAt = null,
            navidromePlaylistId = "pl-seasonal",
            trackCount = 9,
            targetTracks = 25,
        )
        val state = AppUiState(
            playlists = listOf(userPl, seasonalPl, discoveryPl),
            contextualMixes = listOf(ctxMix),
        )
        assertEquals(listOf("pl-user"), state.userPlaylists.map { it.id })
        assertEquals(listOf("pl-disc"), state.soulbeetMixPlaylists.map { it.id })
        assertEquals(listOf("pl-disc"), state.homeFallbackMixPlaylists.map { it.id })

        // Verify homeFallbackMixPlaylists deduplicates ONLY against seasonalHeroMix, keeping moment mixes visible
        val momentPl = userPl.copy(id = "pl-moment", name = "Drive", isSmart = true, kind = "moment", slug = "drive")
        val momentMix = ctxMix.copy(slug = "drive", kind = "moment", navidromePlaylistId = "pl-moment")
        val stateWithMoment = AppUiState(
            playlists = listOf(seasonalPl, momentPl, discoveryPl),
            contextualMixes = listOf(ctxMix, momentMix),
        )
        assertEquals(listOf("pl-moment", "pl-disc"), stateWithMoment.homeFallbackMixPlaylists.map { it.id })
    }

    @Test
    fun parseLyricsResponsePreservesInstrumentalBreaksAndLeavesSectionHeadersUntouched() {
        val syncedInstrumental = """
            [
                {
                    "synced": true,
                    "lines": [
                        {"start_ms": 1000, "text": "Intro"},
                        {"start_ms": 5000, "text": ""},
                        {"start_ms": 9000, "text": "Verse"}
                    ]
                }
            ]
        """.trimIndent()
        val parsedSynced = SoulbeetApiClient.parseLyricsResponse(syncedInstrumental)!!
        assertEquals(3, parsedSynced.lines.size)
        assertEquals(5000L, parsedSynced.lines[1].startMs)
        assertEquals("", parsedSynced.lines[1].text)

        val plainWithBrackets = """
            [
                {
                    "synced": false,
                    "lines": [
                        {"start_ms": null, "text": "[Chorus: Solo]"},
                        {"start_ms": null, "text": "Sing along now"}
                    ]
                }
            ]
        """.trimIndent()
        val parsedPlain = SoulbeetApiClient.parseLyricsResponse(plainWithBrackets)!!
        assertFalse(parsedPlain.synced)
        assertEquals(2, parsedPlain.lines.size)
        assertEquals("[Chorus: Solo]", parsedPlain.lines[0].text)
        assertEquals("Sing along now", parsedPlain.lines[1].text)
    }
}
