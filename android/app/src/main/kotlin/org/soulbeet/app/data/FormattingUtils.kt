package org.soulbeet.app.data

import org.soulbeet.core.FfiAudioQualityProfile
import java.time.Instant
import java.util.Locale
import kotlin.math.roundToInt

/**
 * Pure formatting and parsing utilities for durations, cache disk budgets, codec telemetry,
 * Taste DNA summaries, and discovery metadata.
 */
object FormattingUtils {

    fun formatDurationMs(durationMs: Long): String {
        val safeSecs = (durationMs.coerceAtLeast(0L)) / 1_000L
        return formatDurationSecs(safeSecs)
    }

    fun formatDurationSecs(totalSecs: Long): String {
        val clamped = totalSecs.coerceAtLeast(0L)
        val hours = clamped / 3600L
        val minutes = (clamped % 3600L) / 60L
        val seconds = clamped % 60L
        return if (hours > 0L) {
            String.format(Locale.US, "%d:%02d:%02d", hours, minutes, seconds)
        } else {
            String.format(Locale.US, "%02d:%02d", minutes, seconds)
        }
    }

    fun formatBytes(bytes: Long): String {
        val safe = bytes.coerceAtLeast(0L)
        val kb = 1_024.0
        val mb = kb * 1_024.0
        val gb = mb * 1_024.0
        return when {
            safe >= gb -> String.format(Locale.US, "%.1f GB", safe / gb)
            safe >= mb -> String.format(Locale.US, "%.0f MB", safe / mb)
            safe >= kb -> String.format(Locale.US, "%.0f KB", safe / kb)
            else -> "$safe B"
        }
    }

    fun formatCacheBudget(usedBytes: Long, maxBytes: Long): String {
        val usedStr = formatBytes(usedBytes)
        val maxStr = if (maxBytes > 0L) formatBytes(maxBytes) else "∞"
        return "$usedStr / $maxStr"
    }

    fun cacheBudgetFraction(usedBytes: Long, maxBytes: Long): Float {
        if (maxBytes <= 0L) return 0f
        return (usedBytes.coerceAtLeast(0L).toDouble() / maxBytes.toDouble())
            .toFloat()
            .coerceIn(0f, 1f)
    }

    fun formatTasteDnaSummary(weights: List<Pair<String, Float>>, maxItems: Int = 4): String {
        val valid = weights
            .filter { (code, w) -> code.isNotBlank() && w.isFinite() && w > 0f }
            .sortedByDescending { it.second }
            .take(maxItems.coerceAtLeast(1))
        if (valid.isEmpty()) return "No locale signal yet"

        val total = valid.sumOf { it.second.toDouble() }.coerceAtLeast(1e-6)
        return valid.joinToString(" · ") { (code, weight) ->
            val pct = ((weight.toDouble() / total) * 100.0).roundToInt().coerceIn(1, 100)
            "${code.trim().uppercase(Locale.US)} $pct%"
        }
    }

    fun formatCodecBadge(
        suffix: String?,
        bitRateKbps: Long?,
        profile: FfiAudioQualityProfile,
    ): String {
        val cleanSuffix = suffix?.trim()?.uppercase(Locale.US)?.takeIf { it.isNotEmpty() }
        return when (profile) {
            is FfiAudioQualityProfile.LosslessFlac -> {
                if (cleanSuffix != null && cleanSuffix != "FLAC") {
                    if (bitRateKbps != null && bitRateKbps > 0L) "$cleanSuffix $bitRateKbps" else cleanSuffix
                } else {
                    "FLAC"
                }
            }
            is FfiAudioQualityProfile.CellularOpus160 -> "OPUS 160"
            is FfiAudioQualityProfile.CustomTranscode -> {
                val fmt = profile.format.trim().uppercase(Locale.US).ifEmpty { cleanSuffix ?: "STREAM" }
                if (profile.maxBitRate > 0u) "$fmt ${profile.maxBitRate}" else fmt
            }
        }
    }

    fun formatExpiryCountdown(
        expiresAtIso: String?,
        nowEpochMs: Long = System.currentTimeMillis(),
    ): String? {
        val raw = expiresAtIso?.trim()?.takeIf { it.isNotEmpty() } ?: return null
        val instant = runCatching { Instant.parse(raw) }.getOrNull() ?: return null
        val diffMs = instant.toEpochMilli() - nowEpochMs
        if (diffMs <= 0L) return "EXPIRING"
        val days = (diffMs / (24L * 60L * 60L * 1000L)).coerceAtLeast(0L)
        return if (days >= 1L) {
            "${days}D LEFT"
        } else {
            val hours = (diffMs / (60L * 60L * 1000L)).coerceAtLeast(1L)
            "${hours}H LEFT"
        }
    }

    /**
     * Extracts a human-readable recommendation reason pill from `reasonJson` (either JSON object
     * with `summary`/`label`/`reason` fields or a raw string).
     */
    fun parseDiscoveryReason(reasonJson: String?): String? {
        val trimmed = reasonJson?.trim()?.takeIf { it.isNotEmpty() } ?: return null
        if (!trimmed.startsWith("{") && !trimmed.startsWith("[")) {
            return trimmed.trim('"').takeIf { it.isNotBlank() }
        }
        val keyRegex = Regex("""\"(?:summary|label|reason|text|description)\"\s*:\s*\"([^"]+)\"""")
        val match = keyRegex.find(trimmed)
        if (match != null) {
            return match.groupValues[1].trim().takeIf { it.isNotEmpty() }
        }
        val signalRegex = Regex("""\"(?:signal|source)\"\s*:\s*\"([^"]+)\"""")
        val sig = signalRegex.find(trimmed)?.groupValues?.getOrNull(1)
        if (!sig.isNullOrBlank()) {
            return sig.replace('_', ' ').uppercase(Locale.US)
        }
        return null
    }
}
