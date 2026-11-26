package org.soulbeet.app

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.soulbeet.app.data.FormattingUtils
import org.soulbeet.app.data.SoulbeetApiClient
import org.soulbeet.app.theme.BeetCanvas
import org.soulbeet.app.theme.BeetCanvasOled
import org.soulbeet.app.theme.soulbeetPaletteFor
import org.soulbeet.core.FfiAudioQualityProfile

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
    fun apiClientExtractsAuthTokenCookieAndNormalizesUrls() {
        val cookies = listOf(
            "other=abc; Path=/",
            "auth_token=tok_secret_123; HttpOnly; SameSite=Lax; Path=/",
        )
        assertEquals("tok_secret_123", SoulbeetApiClient.extractAuthTokenFromCookies(cookies))
        assertNull(SoulbeetApiClient.extractAuthTokenFromCookies(listOf("session=xyz")))

        assertEquals("https://music.example.com", SoulbeetApiClient.normalizeBaseUrl("music.example.com/"))
        assertEquals("http://192.168.1.5:9765", SoulbeetApiClient.normalizeBaseUrl("http://192.168.1.5:9765/"))
    }
}
