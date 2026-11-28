package org.soulbeet.media

import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RadialGradient
import android.graphics.RectF
import android.graphics.Shader
import android.graphics.Typeface
import java.io.File
import java.io.FileOutputStream
import java.util.Locale
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.min
import kotlin.math.sin

/**
 * Pure Android Canvas procedural renderer for Soulbeet's Midnight Studio artwork on Android Auto.
 *
 * Generates crisp 512x512 PNG files (disk-cached under `cacheDir/auto_sleeves_v1/`) with strict
 * allowlisting and bounded cache keys so exported ContentProvider callers cannot fill disk space,
 * and striped synchronization + atomic temp-file renames so concurrent Binder threads never
 * corrupt or observe half-written PNGs.
 */
object AutoArtworkRenderer {

    private const val RENDER_SIZE_PX = 512
    private const val CACHE_DIR_NAME = "auto_sleeves_v1"
    private const val LOCK_STRIPES = 32

    private val renderLocks = Array(LOCK_STRIPES) { Any() }

    private val ALLOWED_FALLBACK_KINDS = setOf("playlist", "album", "artist", "track")

    private val KNOWN_SLEEVE_SLUGS = setOf(
        "drive",
        "weekly_radar",
        "comfort",
        "deep_cuts",
        "triage",
        "starred",
        "offline",
        "shuffle",
        "mood:upbeat",
        "mood:chill",
        "rainy_day",
        "snow_day",
        "storm_night",
        "sunny_day",
        "evening_unwind",
        "friday_night",
        "morning",
        "focus",
        "dinner_groove",
        "late_night",
        "sunday_morning",
        "forgotten_gems",
        "fresh_arrivals",
        "decade_80s",
        "decade_90s",
        "decade_2000s",
        "winter_holiday",
        "new_years_eve",
        "cozy_winter",
        "lunar_new_year",
        "valentines",
        "carnival",
        "celtic_spring",
        "spring",
        "summer",
        "summer_solstice",
        "late_summer_reset",
        "autumn",
        "halloween",
        "fallback",
    )

    private data class SleevePalette(
        val start: Int,
        val mid: Int,
        val end: Int,
        val midStop: Float,
        val accent: Int,
        val accentSoft: Int,
        val motif: String,
        val watermark: String? = null,
    )

    fun renderOrGetCachedSleeve(
        context: Context,
        rawSlug: String,
        genreLabel: String? = null,
    ): File? {
        val normalized = normalizeSlug(rawSlug)
        val isGenre = normalized == "genre" || normalized.startsWith("genre:")
        val effectiveGenre = if (isGenre) {
            (genreLabel ?: normalized.removePrefix("genre:")).trim()
        } else {
            null
        }

        // Strict cache-key bounding: unknown slugs map to one shared `sleeve_fallback.png`;
        // genres map to a bounded `(paletteIndex, 2-char alphanumeric monogram)` key.
        val (canonicalSlug, safeKey) = if (isGenre) {
            val cleanGenre = effectiveGenre.orEmpty()
            val paletteIdx = (cleanGenre.lowercase(Locale.ROOT).hashCode() and 0x7FFFFFFF) % GENRE_PALETTES.size
            val monogram = extractGenreMonogram(cleanGenre)
            "genre:$monogram" to "sleeve_genre_${paletteIdx}_${monogram.lowercase(Locale.ROOT)}"
        } else {
            val allowedSlug = if (normalized in KNOWN_SLEEVE_SLUGS) normalized else "fallback"
            allowedSlug to encodeFileKey("sleeve_$allowedSlug")
        }

        val palette = if (isGenre) {
            val paletteIdx = (effectiveGenre.orEmpty().lowercase(Locale.ROOT).hashCode() and 0x7FFFFFFF) % GENRE_PALETTES.size
            GENRE_PALETTES[paletteIdx]
        } else {
            paletteForSlug(canonicalSlug)
        }

        return renderAndCacheSynchronized(context, safeKey, palette, canonicalSlug)
    }

    fun renderOrGetCachedFallback(context: Context, kind: String, seed: String): File? {
        val rawKind = kind.trim().lowercase(Locale.ROOT)
        val cleanKind = if (rawKind in ALLOWED_FALLBACK_KINDS) rawKind else "playlist"
        val cleanSeed = seed.trim().lowercase(Locale.ROOT).ifEmpty { cleanKind }
        val bucket = (cleanSeed.hashCode() and 0x7FFFFFFF) % FALLBACK_PALETTES.size
        val safeKey = "fallback_${cleanKind}_$bucket"
        val palette = FALLBACK_PALETTES[bucket].copy(motif = "fallback_$cleanKind")
        return renderAndCacheSynchronized(context, safeKey, palette, "$cleanKind:$bucket")
    }

    private fun renderAndCacheSynchronized(
        context: Context,
        safeKey: String,
        palette: SleevePalette,
        slugForDraw: String,
    ): File? {
        val cacheDir = File(context.cacheDir, CACHE_DIR_NAME)
        val target = File(cacheDir, "$safeKey.png")
        if (target.exists() && target.isFile && target.length() > 0L) {
            return target
        }

        val stripe = renderLocks[(safeKey.hashCode() and 0x7FFFFFFF) % LOCK_STRIPES]
        return synchronized(stripe) {
            if (target.exists() && target.isFile && target.length() > 0L) {
                return@synchronized target
            }
            if (!cacheDir.exists()) {
                cacheDir.mkdirs()
            }
            val bitmap = renderSleeveBitmap(palette, slugForDraw) ?: return@synchronized null
            try {
                writePngAtomically(bitmap, target)
            } finally {
                bitmap.recycle()
            }
        }
    }

    internal fun normalizeSlug(raw: String): String {
        val lower = raw.trim().lowercase(Locale.ROOT)
        val stripped = when {
            lower.startsWith("mix.seasonal.") -> lower.removePrefix("mix.seasonal.")
            lower.startsWith("mix.moment.") -> lower.removePrefix("mix.moment.")
            lower.startsWith("mix.rolling.") -> lower.removePrefix("mix.rolling.")
            lower.startsWith("mix.custom.") -> lower.removePrefix("mix.custom.")
            else -> lower
        }
        return when (val norm = stripped.replace('-', '_')) {
            "drive_mix" -> "drive"
            "balanced" -> "weekly_radar"
            "conservative" -> "comfort"
            "adventurous" -> "deep_cuts"
            "noel", "christmas" -> "winter_holiday"
            "nye" -> "new_years_eve"
            else -> norm.ifEmpty { "fallback" }
        }
    }

    private fun extractGenreMonogram(genre: String): String {
        val alnum = genre.filter { it.isLetterOrDigit() }.uppercase(Locale.ROOT)
        return alnum.take(2).ifEmpty { "FM" }
    }

    private fun renderSleeveBitmap(palette: SleevePalette, slug: String): Bitmap? = runCatching {
        val size = RENDER_SIZE_PX
        val s = size.toFloat()
        val bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(bitmap)

        // 1. Midnight Studio directional gradient backdrop
        val bgPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            shader = LinearGradient(
                0f,
                0f,
                s,
                s,
                intArrayOf(palette.start, palette.mid, palette.end),
                floatArrayOf(0f, palette.midStop, 1f),
                Shader.TileMode.CLAMP,
            )
        }
        canvas.drawRect(0f, 0f, s, s, bgPaint)

        // 2. Ambient radial glow anchored slightly off-center for organic studio depth
        val glowPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            shader = RadialGradient(
                s * 0.62f,
                s * 0.36f,
                s * 0.65f,
                intArrayOf(withAlpha(palette.accent, 0x38), withAlpha(palette.accent, 0x10), Color.TRANSPARENT),
                floatArrayOf(0f, 0.55f, 1f),
                Shader.TileMode.CLAMP,
            )
        }
        canvas.drawRect(0f, 0f, s, s, glowPaint)

        // 3. Minimalist geometric motif
        drawMotif(canvas, s, palette, slug)

        // 4. Optional decade watermark
        palette.watermark?.let { mark ->
            val textPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
                color = withAlpha(palette.accent, 0x44)
                textSize = s * 0.24f
                typeface = Typeface.create(Typeface.SANS_SERIF, Typeface.BOLD)
                textAlign = Paint.Align.CENTER
                letterSpacing = -0.04f
            }
            val fm = textPaint.fontMetrics
            val baseline = s * 0.54f - (fm.ascent + fm.descent) / 2f
            canvas.drawText(mark, s * 0.5f, baseline, textPaint)
        }

        // 5. Subtle Midnight Studio inner hairline frame
        val borderPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeWidth = 4f
            color = withAlpha(palette.accent, 0x33)
        }
        canvas.drawRect(2f, 2f, s - 2f, s - 2f, borderPaint)

        bitmap
    }.getOrNull()

    private fun drawMotif(canvas: Canvas, s: Float, p: SleevePalette, slug: String) {
        val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.STROKE
            strokeCap = Paint.Cap.ROUND
            strokeJoin = Paint.Join.ROUND
            color = withAlpha(p.accent, 0xD8)
            strokeWidth = s * 0.018f
        }
        val softStroke = Paint(stroke).apply {
            color = withAlpha(p.accentSoft, 0x66)
            strokeWidth = s * 0.010f
        }
        val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.FILL
            color = withAlpha(p.accent, 0x28)
        }
        val brightFill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            style = Paint.Style.FILL
            color = withAlpha(p.accent, 0xE6)
        }

        val cx = s * 0.5f
        val cy = s * 0.5f

        when (p.motif) {
            "drive" -> {
                val horizonY = s * 0.44f
                val horizonGlow = Paint(Paint.ANTI_ALIAS_FLAG).apply {
                    shader = RadialGradient(
                        cx,
                        horizonY,
                        s * 0.42f,
                        intArrayOf(withAlpha(p.accent, 0x66), withAlpha(p.accent, 0x14), Color.TRANSPARENT),
                        floatArrayOf(0f, 0.55f, 1f),
                        Shader.TileMode.CLAMP,
                    )
                }
                canvas.drawCircle(cx, horizonY, s * 0.42f, horizonGlow)

                for (i in 1..4) {
                    val r = s * (0.14f + i * 0.095f)
                    val arcPaint = Paint(softStroke).apply {
                        color = withAlpha(p.accent, (0x88 - i * 0x16).coerceAtLeast(0x20))
                        strokeWidth = s * 0.011f
                    }
                    canvas.drawArc(
                        RectF(cx - r, horizonY - r, cx + r, horizonY + r),
                        195f,
                        150f,
                        false,
                        arcPaint,
                    )
                }

                canvas.drawLine(s * 0.12f, horizonY, s * 0.88f, horizonY, softStroke)

                val roadPaint = Paint(stroke).apply { strokeWidth = s * 0.016f }
                canvas.drawLine(cx - s * 0.06f, horizonY, s * 0.14f, s * 0.90f, roadPaint)
                canvas.drawLine(cx + s * 0.06f, horizonY, s * 0.86f, s * 0.90f, roadPaint)

                var y = horizonY + s * 0.05f
                var seg = s * 0.045f
                while (y < s * 0.88f) {
                    val yEnd = min(y + seg, s * 0.88f)
                    canvas.drawLine(cx, y, cx, yEnd, roadPaint)
                    y = yEnd + seg * 0.72f
                    seg *= 1.28f
                }
                canvas.drawCircle(cx, horizonY, s * 0.022f, brightFill)
            }

            "weekly_radar" -> {
                for (i in 1..4) {
                    val r = s * (0.095f * i)
                    val ringPaint = Paint(softStroke).apply {
                        color = withAlpha(p.accent, (0xA0 - i * 0x1A).coerceAtLeast(0x24))
                        strokeWidth = s * 0.012f
                    }
                    canvas.drawCircle(cx, cy, r, ringPaint)
                }
                canvas.drawLine(cx - s * 0.39f, cy, cx + s * 0.39f, cy, softStroke)
                canvas.drawLine(cx, cy - s * 0.39f, cx, cy + s * 0.39f, softStroke)
                val wedge = RectF(cx - s * 0.36f, cy - s * 0.36f, cx + s * 0.36f, cy + s * 0.36f)
                canvas.drawArc(wedge, -52f, 44f, true, fill)
                val angle = Math.toRadians(-52.0)
                canvas.drawLine(
                    cx,
                    cy,
                    cx + (cos(angle) * s * 0.36f).toFloat(),
                    cy + (sin(angle) * s * 0.36f).toFloat(),
                    stroke,
                )
                canvas.drawCircle(cx + s * 0.17f, cy - s * 0.14f, s * 0.024f, brightFill)
                canvas.drawCircle(cx - s * 0.21f, cy + s * 0.09f, s * 0.018f, brightFill)
                canvas.drawCircle(cx, cy, s * 0.028f, brightFill)
            }

            "comfort" -> {
                for (i in 0..3) {
                    val r = s * (0.12f + i * 0.085f)
                    val wavePaint = Paint(stroke).apply {
                        color = withAlpha(p.accent, (0xCC - i * 0x26).coerceAtLeast(0x2C))
                        strokeWidth = s * (0.022f - i * 0.003f)
                    }
                    canvas.drawCircle(cx, cy + s * 0.02f * i, r, wavePaint)
                }
                canvas.drawCircle(cx, cy, s * 0.07f, fill)
                canvas.drawCircle(cx, cy, s * 0.026f, brightFill)
            }

            "deep_cuts" -> {
                for (i in 1..4) {
                    val r = s * (0.11f + i * 0.072f)
                    canvas.drawCircle(cx, cy, r, softStroke)
                }
                val diamond = Path().apply {
                    moveTo(cx, cy - s * 0.26f)
                    lineTo(cx + s * 0.12f, cy)
                    lineTo(cx, cy + s * 0.26f)
                    lineTo(cx - s * 0.12f, cy)
                    close()
                }
                canvas.drawPath(diamond, fill)
                canvas.drawPath(diamond, stroke)
                canvas.drawCircle(cx, cy, s * 0.035f, brightFill)
            }

            "triage" -> {
                canvas.drawCircle(cx, cy, s * 0.30f, softStroke)
                val spark = Path().apply {
                    moveTo(cx, cy - s * 0.28f)
                    quadTo(cx, cy, cx + s * 0.28f, cy)
                    quadTo(cx, cy, cx, cy + s * 0.28f)
                    quadTo(cx, cy, cx - s * 0.28f, cy)
                    quadTo(cx, cy, cx, cy - s * 0.28f)
                    close()
                }
                canvas.drawPath(spark, fill)
                canvas.drawPath(spark, stroke)
                canvas.drawCircle(cx + s * 0.21f, cy - s * 0.21f, s * 0.024f, brightFill)
                canvas.drawCircle(cx - s * 0.20f, cy + s * 0.18f, s * 0.016f, brightFill)
            }

            "starred" -> {
                canvas.drawCircle(cx, cy, s * 0.33f, softStroke)
                val star = buildStarPath(cx, cy, s * 0.26f, s * 0.11f, 5)
                canvas.drawPath(star, fill)
                canvas.drawPath(star, stroke)
            }

            "offline" -> {
                canvas.drawCircle(cx, cy, s * 0.29f, fill)
                canvas.drawCircle(cx, cy, s * 0.29f, stroke)
                val arrow = Path().apply {
                    moveTo(cx, cy - s * 0.15f)
                    lineTo(cx, cy + s * 0.10f)
                    moveTo(cx - s * 0.10f, cy + s * 0.01f)
                    lineTo(cx, cy + s * 0.11f)
                    lineTo(cx + s * 0.10f, cy + s * 0.01f)
                }
                canvas.drawPath(arrow, stroke)
                canvas.drawLine(cx - s * 0.12f, cy + s * 0.17f, cx + s * 0.12f, cy + s * 0.17f, stroke)
            }

            "shuffle" -> {
                val wave1 = Path().apply {
                    moveTo(s * 0.18f, s * 0.34f)
                    cubicTo(s * 0.42f, s * 0.34f, s * 0.54f, s * 0.66f, s * 0.80f, s * 0.66f)
                }
                val wave2 = Path().apply {
                    moveTo(s * 0.18f, s * 0.66f)
                    cubicTo(s * 0.42f, s * 0.66f, s * 0.54f, s * 0.34f, s * 0.80f, s * 0.34f)
                }
                val thick = Paint(stroke).apply { strokeWidth = s * 0.026f }
                canvas.drawPath(wave1, thick)
                canvas.drawPath(wave2, Paint(thick).apply { color = withAlpha(p.accentSoft, 0xD0) })
                canvas.drawCircle(s * 0.80f, s * 0.34f, s * 0.028f, brightFill)
                canvas.drawCircle(s * 0.80f, s * 0.66f, s * 0.028f, brightFill)
            }

            "winter_holiday", "snow_day", "cozy_winter" -> {
                for (i in 0 until 6) {
                    val a = Math.toRadians(i * 60.0 - 90.0)
                    val ex = cx + (cos(a) * s * 0.28f).toFloat()
                    val ey = cy + (sin(a) * s * 0.28f).toFloat()
                    canvas.drawLine(cx, cy, ex, ey, stroke)
                    val bx = cx + (cos(a) * s * 0.18f).toFloat()
                    val by = cy + (sin(a) * s * 0.18f).toFloat()
                    for (delta in listOf(-42.0, 42.0)) {
                        val ba = Math.toRadians(i * 60.0 - 90.0 + delta)
                        canvas.drawLine(
                            bx,
                            by,
                            bx + (cos(ba) * s * 0.08f).toFloat(),
                            by + (sin(ba) * s * 0.08f).toFloat(),
                            softStroke,
                        )
                    }
                }
                canvas.drawCircle(cx, cy, s * 0.045f, brightFill)
            }

            "late_night", "storm_night" -> {
                canvas.drawCircle(cx, cy, s * 0.33f, softStroke)
                val moon = Path().apply {
                    addCircle(cx - s * 0.02f, cy, s * 0.21f, Path.Direction.CW)
                }
                val cutout = Path().apply {
                    addCircle(cx + s * 0.09f, cy - s * 0.06f, s * 0.18f, Path.Direction.CW)
                }
                moon.op(cutout, Path.Op.DIFFERENCE)
                canvas.drawPath(moon, fill)
                canvas.drawPath(moon, stroke)
                canvas.drawCircle(cx + s * 0.19f, cy - s * 0.18f, s * 0.020f, brightFill)
                canvas.drawCircle(cx + s * 0.24f, cy - s * 0.06f, s * 0.013f, brightFill)
            }

            "mood_upbeat", "friday_night", "sunny_day", "morning" -> {
                val heights = floatArrayOf(0.18f, 0.34f, 0.46f, 0.28f, 0.38f)
                val barW = s * 0.065f
                val gap = s * 0.042f
                val totalW = heights.size * barW + (heights.size - 1) * gap
                var x = (s - totalW) / 2f
                val barPaint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
                    style = Paint.Style.FILL
                    color = withAlpha(p.accent, 0xCC)
                }
                for (hFrac in heights) {
                    val h = s * hFrac
                    val top = cy - h / 2f
                    canvas.drawRoundRect(
                        RectF(x, top, x + barW, top + h),
                        barW / 2f,
                        barW / 2f,
                        barPaint,
                    )
                    x += barW + gap
                }
            }

            "mood_chill", "rainy_day", "evening_unwind", "focus", "sunday_morning" -> {
                for (layer in -1..1) {
                    val path = Path()
                    val baseY = cy + layer * s * 0.11f
                    val amp = s * 0.045f
                    val startX = s * 0.14f
                    val endX = s * 0.86f
                    val steps = 36
                    for (i in 0..steps) {
                        val t = i.toFloat() / steps
                        val x = startX + (endX - startX) * t
                        val y = baseY + (sin(t * 2.0 * PI + layer * 0.9) * amp).toFloat()
                        if (i == 0) path.moveTo(x, y) else path.lineTo(x, y)
                    }
                    canvas.drawPath(path, if (layer == 0) stroke else softStroke)
                }
            }

            "genre" -> {
                for (i in 1..3) {
                    canvas.drawCircle(cx, cy, s * (0.11f + i * 0.085f), softStroke)
                }
                canvas.drawCircle(cx, cy, s * 0.12f, fill)
                canvas.drawCircle(cx, cy, s * 0.12f, stroke)
                val label = slug.removePrefix("genre:").trim().take(2).uppercase(Locale.ROOT)
                if (label.isNotEmpty()) {
                    val tp = Paint(Paint.ANTI_ALIAS_FLAG).apply {
                        color = withAlpha(p.accent, 0xEC)
                        textSize = s * 0.11f
                        typeface = Typeface.create(Typeface.SANS_SERIF, Typeface.BOLD)
                        textAlign = Paint.Align.CENTER
                    }
                    val fm = tp.fontMetrics
                    canvas.drawText(label, cx, cy - (fm.ascent + fm.descent) / 2f, tp)
                }
            }

            "decade" -> {
                for (i in 1..3) {
                    canvas.drawCircle(cx, cy, s * (0.16f + i * 0.085f), softStroke)
                }
            }

            "fallback_artist" -> {
                canvas.drawCircle(cx, cy, s * 0.30f, softStroke)
                canvas.drawCircle(cx, cy - s * 0.07f, s * 0.09f, fill)
                canvas.drawCircle(cx, cy - s * 0.07f, s * 0.09f, stroke)
                canvas.drawArc(
                    RectF(cx - s * 0.18f, cy + s * 0.04f, cx + s * 0.18f, cy + s * 0.32f),
                    195f,
                    150f,
                    false,
                    stroke,
                )
            }

            "fallback_album", "fallback_track" -> {
                canvas.drawCircle(cx, cy, s * 0.32f, fill)
                canvas.drawCircle(cx, cy, s * 0.32f, stroke)
                canvas.drawCircle(cx, cy, s * 0.22f, softStroke)
                canvas.drawCircle(cx, cy, s * 0.11f, stroke)
                canvas.drawCircle(cx, cy, s * 0.032f, brightFill)
            }

            else -> {
                canvas.drawCircle(cx, cy, s * 0.30f, softStroke)
                val lineX1 = s * 0.28f
                val lineX2 = s * 0.72f
                canvas.drawLine(lineX1, cy - s * 0.11f, lineX2, cy - s * 0.11f, stroke)
                canvas.drawLine(lineX1, cy, lineX2 - s * 0.08f, cy, stroke)
                canvas.drawLine(lineX1, cy + s * 0.11f, lineX2 - s * 0.16f, cy + s * 0.11f, stroke)
                canvas.drawCircle(lineX2 - s * 0.04f, cy + s * 0.11f, s * 0.030f, brightFill)
            }
        }
    }

    private fun buildStarPath(cx: Float, cy: Float, outerR: Float, innerR: Float, points: Int): Path {
        val path = Path()
        val step = PI / points
        for (i in 0 until points * 2) {
            val r = if (i % 2 == 0) outerR else innerR
            val angle = i * step - PI / 2.0
            val x = cx + (cos(angle) * r).toFloat()
            val y = cy + (sin(angle) * r).toFloat()
            if (i == 0) path.moveTo(x, y) else path.lineTo(x, y)
        }
        path.close()
        return path
    }

    private fun paletteForSlug(slug: String): SleevePalette = when (slug) {
        "drive" -> SleevePalette(0xFF121C2E.toInt(), 0xFF1B0E30.toInt(), 0xFF120720.toInt(), 0.58f, 0xFF33FFB0.toInt(), 0xFF93C5FD.toInt(), "drive")
        "weekly_radar" -> SleevePalette(0xFF271238.toInt(), 0xFF190F32.toInt(), 0xFF101628.toInt(), 0.58f, 0xFFF472F4.toInt(), 0xFFC084FC.toInt(), "weekly_radar")
        "comfort" -> SleevePalette(0xFF122826.toInt(), 0xFF16112D.toInt(), 0xFF110820.toInt(), 0.62f, 0xFF33FFB0.toInt(), 0xFF5EEAD4.toInt(), "comfort")
        "deep_cuts" -> SleevePalette(0xFF2F1038.toInt(), 0xFF1C0C34.toInt(), 0xFF110722.toInt(), 0.58f, 0xFFE879F9.toInt(), 0xFFF0ABFC.toInt(), "deep_cuts")
        "triage" -> SleevePalette(0xFF16272B.toInt(), 0xFF24103B.toInt(), 0xFF110720.toInt(), 0.54f, 0xFF33FFB0.toInt(), 0xFFF472F4.toInt(), "triage")
        "starred" -> SleevePalette(0xFF2E1E14.toInt(), 0xFF1E102C.toInt(), 0xFF110720.toInt(), 0.56f, 0xFFFFD166.toInt(), 0xFFFDBA74.toInt(), "starred")
        "offline" -> SleevePalette(0xFF0E2924.toInt(), 0xFF14112C.toInt(), 0xFF0D071B.toInt(), 0.58f, 0xFF33FFB0.toInt(), 0xFF6EE7B7.toInt(), "offline")
        "shuffle" -> SleevePalette(0xFF2A1042.toInt(), 0xFF181134.toInt(), 0xFF0E1B28.toInt(), 0.54f, 0xFFC084FC.toInt(), 0xFF33FFB0.toInt(), "shuffle")
        "mood:upbeat" -> SleevePalette(0xFF381236.toInt(), 0xFF221034.toInt(), 0xFF120820.toInt(), 0.55f, 0xFFF472F4.toInt(), 0xFFFBBF24.toInt(), "mood_upbeat")
        "mood:chill" -> SleevePalette(0xFF0F2436.toInt(), 0xFF161332.toInt(), 0xFF100820.toInt(), 0.56f, 0xFF5EEAD4.toInt(), 0xFF93C5FD.toInt(), "mood_chill")
        "rainy_day" -> SleevePalette(0xFF0E213D.toInt(), 0xFF161836.toInt(), 0xFF110921.toInt(), 0.52f, 0xFF93C5FD.toInt(), 0xFF7DB9FF.toInt(), "rainy_day")
        "snow_day" -> SleevePalette(0xFF15263A.toInt(), 0xFF1C1A38.toInt(), 0xFF130B24.toInt(), 0.56f, 0xFFBAE6FD.toInt(), 0xFFE0F2FE.toInt(), "snow_day")
        "storm_night" -> SleevePalette(0xFF0D0A1F.toInt(), 0xFF19133A.toInt(), 0xFF0E172E.toInt(), 0.48f, 0xFFC4B5FD.toInt(), 0xFFA78BFA.toInt(), "storm_night")
        "sunny_day" -> SleevePalette(0xFF3C1E14.toInt(), 0xFF2A1328.toInt(), 0xFF160922.toInt(), 0.56f, 0xFFFCD34D.toInt(), 0xFFFBBF24.toInt(), "sunny_day")
        "evening_unwind" -> SleevePalette(0xFF2D1426.toInt(), 0xFF1D0D2D.toInt(), 0xFF120720.toInt(), 0.55f, 0xFFFDBA74.toInt(), 0xFFFB923C.toInt(), "evening_unwind")
        "friday_night" -> SleevePalette(0xFF2D0D3D.toInt(), 0xFF1D0C35.toInt(), 0xFF110722.toInt(), 0.55f, 0xFFF472F4.toInt(), 0xFFE879F9.toInt(), "friday_night")
        "morning" -> SleevePalette(0xFF341926.toInt(), 0xFF221030.toInt(), 0xFF120820.toInt(), 0.56f, 0xFFFDA4AF.toInt(), 0xFFFB9278.toInt(), "morning")
        "focus" -> SleevePalette(0xFF0F2034.toInt(), 0xFF151434.toInt(), 0xFF100820.toInt(), 0.55f, 0xFF5EEAD4.toInt(), 0xFF2DD4BF.toInt(), "focus")
        "dinner_groove" -> SleevePalette(0xFF341322.toInt(), 0xFF220E2E.toInt(), 0xFF130820.toInt(), 0.56f, 0xFFFBBF24.toInt(), 0xFFF59E0B.toInt(), "evening_unwind")
        "late_night" -> SleevePalette(0xFF230D38.toInt(), 0xFF160A2C.toInt(), 0xFF0C051A.toInt(), 0.52f, 0xFFD8B4FE.toInt(), 0xFFC084FC.toInt(), "late_night")
        "sunday_morning" -> SleevePalette(0xFF2E1B16.toInt(), 0xFF1E1129.toInt(), 0xFF120820.toInt(), 0.58f, 0xFFFED7AA.toInt(), 0xFFFBBF78.toInt(), "sunday_morning")
        "forgotten_gems" -> SleevePalette(0xFF2D1E17.toInt(), 0xFF1C1028.toInt(), 0xFF140920.toInt(), 0.64f, 0xFFFFD166.toInt(), 0xFFFBBF24.toInt(), "deep_cuts")
        "fresh_arrivals" -> SleevePalette(0xFF0D2825.toInt(), 0xFF15102B.toInt(), 0xFF110820.toInt(), 0.64f, 0xFF33FFB0.toInt(), 0xFF6EE7B7.toInt(), "triage")
        "decade_80s" -> SleevePalette(0xFF32103D.toInt(), 0xFF1A1038.toInt(), 0xFF0F162E.toInt(), 0.56f, 0xFFF472F4.toInt(), 0xFFE879F9.toInt(), "decade", "80s")
        "decade_90s" -> SleevePalette(0xFF2A181E.toInt(), 0xFF1D1228.toInt(), 0xFF13091F.toInt(), 0.62f, 0xFFFDA4AF.toInt(), 0xFFFB7185.toInt(), "decade", "90s")
        "decade_2000s" -> SleevePalette(0xFF13233E.toInt(), 0xFF1A1236.toInt(), 0xFF130922.toInt(), 0.58f, 0xFF7DD3FC.toInt(), 0xFF38BDF8.toInt(), "decade", "00s")
        "winter_holiday" -> SleevePalette(0xFF2C1220.toInt(), 0xFF181A2E.toInt(), 0xFF0F1920.toInt(), 0.55f, 0xFFFFD166.toInt(), 0xFFFDBA74.toInt(), "winter_holiday")
        "new_years_eve" -> SleevePalette(0xFF2A1834.toInt(), 0xFF1A0F32.toInt(), 0xFF110720.toInt(), 0.56f, 0xFFFDE047.toInt(), 0xFFFBBF24.toInt(), "triage")
        "cozy_winter" -> SleevePalette(0xFF2A191A.toInt(), 0xFF1A1530.toInt(), 0xFF110922.toInt(), 0.56f, 0xFFFDBA74.toInt(), 0xFFFBBF92.toInt(), "cozy_winter")
        "lunar_new_year" -> SleevePalette(0xFF3A111C.toInt(), 0xFF240E2A.toInt(), 0xFF13071E.toInt(), 0.58f, 0xFFFDA4AF.toInt(), 0xFFFBBF24.toInt(), "triage")
        "valentines" -> SleevePalette(0xFF381028.toInt(), 0xFF230D2F.toInt(), 0xFF130720.toInt(), 0.56f, 0xFFF9A8D4.toInt(), 0xFFF472B6.toInt(), "comfort")
        "carnival" -> SleevePalette(0xFF2D1438.toInt(), 0xFF1B1838.toInt(), 0xFF0F2128.toInt(), 0.52f, 0xFFFDE047.toInt(), 0xFFF472F4.toInt(), "mood_upbeat")
        "celtic_spring" -> SleevePalette(0xFF0D2B22.toInt(), 0xFF131B30.toInt(), 0xFF100920.toInt(), 0.56f, 0xFF6EE7B7.toInt(), 0xFF34D399.toInt(), "comfort")
        "spring" -> SleevePalette(0xFF172A28.toInt(), 0xFF211433.toInt(), 0xFF130822.toInt(), 0.56f, 0xFF6EE7B7.toInt(), 0xFF33FFB0.toInt(), "comfort")
        "summer" -> SleevePalette(0xFF3A1D16.toInt(), 0xFF26122C.toInt(), 0xFF130922.toInt(), 0.56f, 0xFFFDBA74.toInt(), 0xFFFB923C.toInt(), "sunny_day")
        "summer_solstice" -> SleevePalette(0xFF3A2114.toInt(), 0xFF281332.toInt(), 0xFF140822.toInt(), 0.54f, 0xFFFDE047.toInt(), 0xFFFBBF24.toInt(), "sunny_day")
        "late_summer_reset" -> SleevePalette(0xFF2F1728.toInt(), 0xFF1D1234.toInt(), 0xFF120820.toInt(), 0.56f, 0xFFF0ABFC.toInt(), 0xFFE879F9.toInt(), "evening_unwind")
        "autumn" -> SleevePalette(0xFF331914.toInt(), 0xFF221128.toInt(), 0xFF130820.toInt(), 0.56f, 0xFFFBBF24.toInt(), 0xFFF59E0B.toInt(), "evening_unwind")
        "halloween" -> SleevePalette(0xFF250E33.toInt(), 0xFF1A0B28.toInt(), 0xFF170D16.toInt(), 0.54f, 0xFFFB923C.toInt(), 0xFFF97316.toInt(), "late_night")
        else -> SleevePalette(0xFF24103A.toInt(), 0xFF190C2E.toInt(), 0xFF110720.toInt(), 0.56f, 0xFFF472F4.toInt(), 0xFFD8C2F5.toInt(), "fallback")
    }

    private val GENRE_PALETTES = listOf(
        SleevePalette(0xFF24103A.toInt(), 0xFF170D2F.toInt(), 0xFF0E081F.toInt(), 0.55f, 0xFFD8B4FE.toInt(), 0xFFA78BFA.toInt(), "genre"),
        SleevePalette(0xFF0E2534.toInt(), 0xFF141432.toInt(), 0xFF0E081F.toInt(), 0.55f, 0xFF5EEAD4.toInt(), 0xFF33FFB0.toInt(), "genre"),
        SleevePalette(0xFF321428.toInt(), 0xFF1F0E30.toInt(), 0xFF110720.toInt(), 0.56f, 0xFFF472B6.toInt(), 0xFFFDA4AF.toInt(), "genre"),
        SleevePalette(0xFF2E1E15.toInt(), 0xFF1E102C.toInt(), 0xFF110820.toInt(), 0.56f, 0xFFFBBF24.toInt(), 0xFFFFD166.toInt(), "genre"),
    )

    private val FALLBACK_PALETTES = listOf(
        SleevePalette(0xFF24103A.toInt(), 0xFF180C2E.toInt(), 0xFF0F051A.toInt(), 0.56f, 0xFFC084FC.toInt(), 0xFF33FFB0.toInt(), "fallback"),
        SleevePalette(0xFF132234.toInt(), 0xFF190E30.toInt(), 0xFF10061C.toInt(), 0.56f, 0xFF33FFB0.toInt(), 0xFF93C5FD.toInt(), "fallback"),
        SleevePalette(0xFF2D1233.toInt(), 0xFF1B0C2E.toInt(), 0xFF10061B.toInt(), 0.56f, 0xFFF472F4.toInt(), 0xFFD8B4FE.toInt(), "fallback"),
        SleevePalette(0xFF2B1B18.toInt(), 0xFF1C0F2A.toInt(), 0xFF11071E.toInt(), 0.58f, 0xFFFFD166.toInt(), 0xFFFDBA74.toInt(), "fallback"),
    )

    private fun withAlpha(argb: Int, alpha: Int): Int =
        (argb and 0x00FFFFFF) or ((alpha and 0xFF) shl 24)

    /**
     * Atomically writes [bitmap] to [target] via a unique temporary file + rename.
     * Never overwrites [target] in-place so concurrent Binder readers never see a half-written PNG.
     */
    private fun writePngAtomically(bitmap: Bitmap, target: File): File? = runCatching {
        val parent = target.parentFile ?: return@runCatching null
        if (!parent.exists()) parent.mkdirs()
        val tmp = File.createTempFile("sleeve_", ".tmp", parent)
        try {
            FileOutputStream(tmp).use { out ->
                bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
                out.flush()
            }
            if (tmp.renameTo(target)) {
                target
            } else {
                tmp.delete()
                target.takeIf { it.exists() && it.isFile && it.length() > 0L }
            }
        } catch (e: Exception) {
            tmp.delete()
            throw e
        }
    }.getOrNull()

    private fun encodeFileKey(raw: String): String {
        val sb = StringBuilder(raw.length + 8)
        for (ch in raw) {
            if (ch in 'a'..'z' || ch in '0'..'9' || ch == '_' || ch == '-') {
                sb.append(ch)
            } else {
                sb.append('_')
                sb.append(ch.code.toString(16))
            }
        }
        val encoded = sb.toString()
        if (encoded.length <= 64) return encoded
        val hashHex = (raw.hashCode() and 0x7FFFFFFF).toString(16)
        return "${encoded.take(55)}_$hashHex"
    }
}
