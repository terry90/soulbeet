package org.soulbeet.app

import java.util.Calendar
import java.util.Locale
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
import org.soulbeet.app.data.CoarseLocationProvider
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

    @Test
    fun defaultMixCopyUsesNeutralCanonicalTitlesAcrossAllBuiltins() {
        val cases = listOf(
            Triple("winter_holiday", "mix.seasonal.winter_holiday", "Christmas Mix"),
            Triple("new_years_eve", "mix.seasonal.new_years_eve", "New Year's Eve"),
            Triple("cozy_winter", "mix.seasonal.cozy_winter", "Cozy Winter"),
            Triple("lunar_new_year", "mix.seasonal.lunar_new_year", "Lunar New Year"),
            Triple("valentines", "mix.seasonal.valentines", "Date Night"),
            Triple("carnival", "mix.seasonal.carnival", "Carnival Beats"),
            Triple("celtic_spring", "mix.seasonal.celtic_spring", "Celtic Folk"),
            Triple("spring", "mix.seasonal.spring", "Spring Awakening"),
            Triple("summer", "mix.seasonal.summer", "Summer Anthems"),
            Triple("summer_solstice", "mix.seasonal.summer_solstice", "Midsummer Solstice"),
            Triple("late_summer_reset", "mix.seasonal.late_summer_reset", "Late Summer Reset"),
            Triple("autumn", "mix.seasonal.autumn", "Autumn Sessions"),
            Triple("halloween", "mix.seasonal.halloween", "Darkwave Halloween"),
            Triple("drive", "mix.moment.drive", "Drive Mix"),
            Triple("morning", "mix.moment.morning", "Morning Lift"),
            Triple("focus", "mix.moment.focus", "Deep Focus"),
            Triple("evening_unwind", "mix.moment.evening_unwind", "Evening Unwind"),
            Triple("dinner_groove", "mix.moment.dinner_groove", "Dinner Groove"),
            Triple("late_night", "mix.moment.late_night", "Late Night"),
            Triple("rainy_day", "mix.moment.rainy_day", "Rainy Day"),
            Triple("snow_day", "mix.moment.snow_day", "Snow Day"),
            Triple("storm_night", "mix.moment.storm_night", "Storm & Thunder"),
            Triple("sunny_day", "mix.moment.sunny_day", "Sunny Vibes"),
            Triple("friday_night", "mix.moment.friday_night", "Friday Night Warmup"),
            Triple("sunday_morning", "mix.moment.sunday_morning", "Sunday Morning Coffee"),
            Triple("forgotten_gems", "mix.moment.forgotten_gems", "Forgotten Gems"),
            Triple("fresh_arrivals", "mix.moment.fresh_arrivals", "Fresh Arrivals"),
            Triple("decade_80s", "mix.moment.decade_80s", "80s Wave"),
            Triple("decade_90s", "mix.moment.decade_90s", "90s Mix"),
            Triple("decade_2000s", "mix.moment.decade_2000s", "2000s Mix"),
            Triple("conservative", "mix.rolling.conservative", "Comfort Zone"),
            Triple("comfort", "mix.rolling.comfort", "Comfort Zone"),
            Triple("balanced", "mix.rolling.balanced", "Fresh Picks"),
            Triple("weekly_radar", "mix.rolling.weekly_radar", "Fresh Picks"),
            Triple("adventurous", "mix.rolling.adventurous", "Deep Cuts"),
            Triple("deep_cuts", "mix.rolling.deep_cuts", "Deep Cuts"),
            Triple("late_coding", "mix.custom.late_coding", "Late Coding"),
            Triple("WINTER_HOLIDAY", "MIX.SEASONAL.WINTER_HOLIDAY", "Christmas Mix"),
            Triple("foo_bar", "custom_vibes", "Custom Vibes"),
            Triple("  late_coding  ", "", "Late Coding"),
            Triple("___", "...", "Curated Mix"),
        )
        for ((slug, key, expected) in cases) {
            val (title, subtitle) = SoulbeetApiClient.defaultMixCopy(slug, key)
            assertEquals("slug=$slug key=$key", expected, title)
            assertTrue("slug=$slug subtitle should be non-blank", subtitle.isNotBlank())
        }

        // Removed diwali mix falls back to humanized title and generic subtitle.
        val (diwaliTitle, diwaliSub) = SoulbeetApiClient.defaultMixCopy("diwali", "mix.seasonal.diwali")
        assertEquals("Diwali", diwaliTitle)
        assertEquals("Contextual Soulbeet mix", diwaliSub)

        // Weather-reactive mix subtitles match Rust UI i18n copy verbatim.
        assertEquals(
            "Soft acoustic folk, piano, and warm reflective melodies for rainy weather.",
            SoulbeetApiClient.defaultMixCopy("rainy_day", "mix.moment.rainy_day").second,
        )
        assertEquals(
            "Hushed Nordic folk, gentle piano, and ambient warmth for snowy skies.",
            SoulbeetApiClient.defaultMixCopy("snow_day", "mix.moment.snow_day").second,
        )
        assertEquals(
            "Brooding post-rock, shoegaze, and moody trip hop built for stormy weather.",
            SoulbeetApiClient.defaultMixCopy("storm_night", "mix.moment.storm_night").second,
        )
        assertEquals(
            "Bright funk, soul, indie pop, and sun-drenched grooves for clear skies.",
            SoulbeetApiClient.defaultMixCopy("sunny_day", "mix.moment.sunny_day").second,
        )
    }

    @Test
    fun weatherBadgeFormattingAndParsingCoverAllConditions() {
        assertEquals(
            "RAINY · 11°C",
            SoulbeetApiClient.formatWeatherBadge(
                JSONObject("""{"condition":"rainy","temperature_c":11.4,"weather_code":61,"is_day":true}"""),
            ),
        )
        assertEquals(
            "SNOW · -2°C",
            SoulbeetApiClient.formatWeatherBadge(
                JSONObject("""{"condition":"snowy","temperature_c":-1.5,"weather_code":73,"is_day":false}"""),
            ),
        )
        assertEquals(
            "STORM · 16°C",
            SoulbeetApiClient.formatWeatherBadge(
                JSONObject("""{"condition":"stormy","temperature_c":16.0,"weather_code":95,"is_day":false}"""),
            ),
        )
        assertEquals(
            "SUNNY · 25°C",
            SoulbeetApiClient.formatWeatherBadge(
                JSONObject("""{"condition":"sunny","temperature_c":24.6,"weather_code":0,"is_day":true}"""),
            ),
        )
        assertNull(SoulbeetApiClient.formatWeatherBadge(null))
        assertNull(SoulbeetApiClient.formatWeatherBadge(JSONObject("""{"condition":"cloudy","temperature_c":14.0}""")))

        val client = SoulbeetApiClient()
        val parsed = client.parseContextualMix(
            JSONObject(
                """
                {
                    "slug": "snow_day",
                    "kind": "moment",
                    "i18n_key": "mix.moment.snow_day",
                    "owned_pct": 75,
                    "exclude_from_core_taste": false,
                    "track_count": 20,
                    "target_tracks": 25,
                    "weather_badge": {
                        "condition": "snowy",
                        "temperature_c": -2.1,
                        "weather_code": 75,
                        "is_day": true,
                        "location_label": "Oslo, NO"
                    }
                }
                """.trimIndent(),
            ),
        )
        assertEquals("Snow Day", parsed.title)
        assertEquals("SNOW · -2°C", parsed.weatherBadge)
    }

    @Test
    fun calendarDayOfWeekMapsToIsoWeekdayOneThroughSeven() {
        assertEquals(1, SoulbeetApiClient.calendarDayOfWeekToIso(Calendar.MONDAY))
        assertEquals(2, SoulbeetApiClient.calendarDayOfWeekToIso(Calendar.TUESDAY))
        assertEquals(3, SoulbeetApiClient.calendarDayOfWeekToIso(Calendar.WEDNESDAY))
        assertEquals(4, SoulbeetApiClient.calendarDayOfWeekToIso(Calendar.THURSDAY))
        assertEquals(5, SoulbeetApiClient.calendarDayOfWeekToIso(Calendar.FRIDAY))
        assertEquals(6, SoulbeetApiClient.calendarDayOfWeekToIso(Calendar.SATURDAY))
        assertEquals(7, SoulbeetApiClient.calendarDayOfWeekToIso(Calendar.SUNDAY))
    }

    @Test
    fun coarseLocationRoundingAndSanitizationHandleEdgeCases() {
        assertEquals(45.8, CoarseLocationProvider.roundToTenthDegree(45.764), 0.0)
        assertEquals(45.7, CoarseLocationProvider.roundToTenthDegree(45.74), 0.0)
        assertEquals(-33.9, CoarseLocationProvider.roundToTenthDegree(-33.8688), 0.0)
        assertEquals(0.0, CoarseLocationProvider.roundToTenthDegree(0.0), 0.0)

        // Ensure -0.0 and tiny negative values normalize to positive +0.0 (never "-0.0").
        val negZero = CoarseLocationProvider.roundToTenthDegree(-0.0)
        assertEquals(0L, java.lang.Double.doubleToRawLongBits(negZero))
        val tinyNeg = CoarseLocationProvider.roundToTenthDegree(-0.04)
        assertEquals(0L, java.lang.Double.doubleToRawLongBits(tinyNeg))

        // Valid coordinates are rounded to 0.1 degree.
        assertEquals(45.8 to 4.8, CoarseLocationProvider.sanitizeCoarseCoordinates(45.764, 4.8357))
        assertEquals(-33.9 to 151.2, CoarseLocationProvider.sanitizeCoarseCoordinates(-33.8688, 151.2093))
        assertEquals(0.0 to 0.1, CoarseLocationProvider.sanitizeCoarseCoordinates(-0.02, 0.09))

        // Invalid, out-of-range, and exact (0.0, 0.0) null-island fixes are rejected.
        assertNull(CoarseLocationProvider.sanitizeCoarseCoordinates(0.0, 0.0))
        assertNull(CoarseLocationProvider.sanitizeCoarseCoordinates(Double.NaN, 4.8))
        assertNull(CoarseLocationProvider.sanitizeCoarseCoordinates(45.8, Double.NaN))
        assertNull(CoarseLocationProvider.sanitizeCoarseCoordinates(Double.POSITIVE_INFINITY, 4.8))
        assertNull(CoarseLocationProvider.sanitizeCoarseCoordinates(45.8, Double.NEGATIVE_INFINITY))
        assertNull(CoarseLocationProvider.sanitizeCoarseCoordinates(90.1, 4.8))
        assertNull(CoarseLocationProvider.sanitizeCoarseCoordinates(-90.1, 4.8))
        assertNull(CoarseLocationProvider.sanitizeCoarseCoordinates(45.8, 180.1))
        assertNull(CoarseLocationProvider.sanitizeCoarseCoordinates(45.8, -180.1))
    }

    @Test
    fun contextualMixesUrlFormatsCoarseCoordsWithRootLocaleAndOmitsInvalidOnes() {
        val originalLocale = Locale.getDefault()
        try {
            // French locale uses comma as decimal separator; URL query params must still use dots.
            Locale.setDefault(Locale.FRANCE)

            val withCoords = SoulbeetApiClient.buildContextualMixesUrl(
                serverUrl = "https://music.example.com/",
                localHour = 14,
                localWeekday = 5,
                lat = 45.764,
                lon = -0.04,
            )
            assertEquals("14", withCoords.queryParameter("local_hour"))
            assertEquals("5", withCoords.queryParameter("local_weekday"))
            assertEquals("45.8", withCoords.queryParameter("lat"))
            assertEquals("0.0", withCoords.queryParameter("lon"))

            val negativeCoords = SoulbeetApiClient.buildContextualMixesUrl(
                serverUrl = "https://music.example.com",
                localHour = 8,
                localWeekday = 1,
                lat = -33.8688,
                lon = -70.6693,
            )
            assertEquals("-33.9", negativeCoords.queryParameter("lat"))
            assertEquals("-70.7", negativeCoords.queryParameter("lon"))

            val withoutCoords = SoulbeetApiClient.buildContextualMixesUrl(
                serverUrl = "https://music.example.com",
                localHour = 21,
                localWeekday = 7,
            )
            assertNull(withoutCoords.queryParameter("lat"))
            assertNull(withoutCoords.queryParameter("lon"))

            val partialCoords = SoulbeetApiClient.buildContextualMixesUrl(
                serverUrl = "https://music.example.com",
                localHour = 21,
                localWeekday = 7,
                lat = 45.8,
                lon = null,
            )
            assertNull(partialCoords.queryParameter("lat"))
            assertNull(partialCoords.queryParameter("lon"))

            val outOfRangeCoords = SoulbeetApiClient.buildContextualMixesUrl(
                serverUrl = "https://music.example.com",
                localHour = 21,
                localWeekday = 7,
                lat = 95.0,
                lon = 4.8,
            )
            assertNull(outOfRangeCoords.queryParameter("lat"))
            assertNull(outOfRangeCoords.queryParameter("lon"))

            val nanCoords = SoulbeetApiClient.buildContextualMixesUrl(
                serverUrl = "https://music.example.com",
                localHour = 21,
                localWeekday = 7,
                lat = Double.NaN,
                lon = 4.8,
            )
            assertNull(nanCoords.queryParameter("lat"))
            assertNull(nanCoords.queryParameter("lon"))
        } finally {
            Locale.setDefault(originalLocale)
        }
    }
}
