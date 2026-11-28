package org.soulbeet.app.ui.components

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material3.Icon
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.CacheDrawScope
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.draw.drawWithCache
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.graphics.PathOperation
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.DrawScope
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.rotate
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.em
import androidx.compose.ui.unit.sp
import java.util.Locale
import org.soulbeet.app.data.ContextualMixUi
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.theme.SpaceGroteskFamily
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.sin

enum class MixShelfBucket {
    RIGHT_NOW,
    COLLECTION,
}

enum class MixSleeveLayout {
    WIDE,
    SQUARE,
}

enum class MixMotif {
    // Weather (4)
    RAINY_DAY,
    SNOW_DAY,
    STORM_NIGHT,
    SUNNY_DAY,

    // Moments & Weekdays (8)
    DRIVE,
    EVENING_UNWIND,
    FRIDAY_NIGHT,
    MORNING,
    FOCUS,
    DINNER_GROOVE,
    LATE_NIGHT,
    SUNDAY_MORNING,

    // Collection & Decades (5)
    FORGOTTEN_GEMS,
    FRESH_ARRIVALS,
    DECADE_80S,
    DECADE_90S,
    DECADE_2000S,

    // Rolling (3)
    ROLLING_COMFORT,
    ROLLING_RADAR,
    ROLLING_DEEP_CUTS,

    // Seasonals (13)
    SEASONAL_WINTER_HOLIDAY,
    SEASONAL_NEW_YEARS_EVE,
    SEASONAL_COZY_WINTER,
    SEASONAL_LUNAR_NEW_YEAR,
    SEASONAL_VALENTINES,
    SEASONAL_CARNIVAL,
    SEASONAL_CELTIC_SPRING,
    SEASONAL_SPRING,
    SEASONAL_SUMMER,
    SEASONAL_SUMMER_SOLSTICE,
    SEASONAL_LATE_SUMMER_RESET,
    SEASONAL_AUTUMN,
    SEASONAL_HALLOWEEN,

    // Fallback
    FALLBACK,
}

@Immutable
data class MixSleeveSpec(
    val motif: MixMotif,
    val gradientStart: Color,
    val gradientMid: Color,
    val gradientEnd: Color,
    val gradientMidStop: Float,
    val borderColor: Color,
    val pillBackground: Color,
    val pillForeground: Color,
    val decadeWatermark: String? = null,
    val watermarkColor: Color = Color.Transparent,
)

private val COLLECTION_SLUGS = setOf(
    "forgotten_gems",
    "fresh_arrivals",
    "decade_80s",
    "decade_90s",
    "decade_2000s",
)

internal fun normalizeMixSlug(slug: String): String {
    val lowered = slug.trim().lowercase(Locale.ROOT)
    val stripped = when {
        lowered.startsWith("mix.seasonal.") -> lowered.removePrefix("mix.seasonal.")
        lowered.startsWith("mix.moment.") -> lowered.removePrefix("mix.moment.")
        lowered.startsWith("mix.rolling.") -> lowered.removePrefix("mix.rolling.")
        lowered.startsWith("mix.custom.") -> lowered.removePrefix("mix.custom.")
        else -> lowered
    }
    return stripped.replace('-', '_')
}

internal fun classifyMixShelf(mix: ContextualMixUi): MixShelfBucket {
    val slug = normalizeMixSlug(mix.slug)
    val kind = mix.kind.trim().lowercase(Locale.ROOT)
    return if (slug in COLLECTION_SLUGS || kind == "rolling") {
        MixShelfBucket.COLLECTION
    } else {
        MixShelfBucket.RIGHT_NOW
    }
}

internal fun formatMixSleeveTag(mix: ContextualMixUi): String {
    val slug = normalizeMixSlug(mix.slug)
    val kind = mix.kind.trim().lowercase(Locale.ROOT)
    return when {
        slug == "decade_80s" -> "1980S"
        slug == "decade_90s" -> "1990S"
        slug == "decade_2000s" -> "2000S"
        slug == "forgotten_gems" -> "VAULT"
        slug == "fresh_arrivals" -> "NEW"
        slug == "friday_night" -> "FRIDAY"
        slug == "sunday_morning" -> "SUNDAY"
        mix.offlinePriority -> "OFFLINE PRIORITY"
        kind == "seasonal" -> "SEASONAL"
        kind == "rolling" -> "ROLLING"
        kind == "moment" -> "MOMENT"
        else -> "MIX"
    }
}

internal fun mixSleeveSpecFor(slug: String): MixSleeveSpec = when (normalizeMixSlug(slug)) {
    // --- Weather (4) ---
    "rainy_day" -> MixSleeveSpec(
        motif = MixMotif.RAINY_DAY,
        gradientStart = Color(0xFF0E213D),
        gradientMid = Color(0xFF161836),
        gradientEnd = Color(0xFF110921),
        gradientMidStop = 0.52f,
        borderColor = Color(0x427DB9FF),
        pillBackground = Color(0x247DB9FF),
        pillForeground = Color(0xFF93C5FD),
    )

    "snow_day" -> MixSleeveSpec(
        motif = MixMotif.SNOW_DAY,
        gradientStart = Color(0xFF15263A),
        gradientMid = Color(0xFF1C1A38),
        gradientEnd = Color(0xFF130B24),
        gradientMidStop = 0.56f,
        borderColor = Color(0x47BAE6FD),
        pillBackground = Color(0x24E0F2FE),
        pillForeground = Color(0xFFBAE6FD),
    )

    "storm_night" -> MixSleeveSpec(
        motif = MixMotif.STORM_NIGHT,
        gradientStart = Color(0xFF0D0A1F),
        gradientMid = Color(0xFF19133A),
        gradientEnd = Color(0xFF0E172E),
        gradientMidStop = 0.48f,
        borderColor = Color(0x4DA78BFA),
        pillBackground = Color(0x29A78BFA),
        pillForeground = Color(0xFFC4B5FD),
    )

    "sunny_day" -> MixSleeveSpec(
        motif = MixMotif.SUNNY_DAY,
        gradientStart = Color(0xFF3C1E14),
        gradientMid = Color(0xFF2A1328),
        gradientEnd = Color(0xFF160922),
        gradientMidStop = 0.56f,
        borderColor = Color(0x4DFBBF24),
        pillBackground = Color(0x26FBBF24),
        pillForeground = Color(0xFFFCD34D),
    )

    // --- Moments & Weekdays (8) ---
    "drive", "drive_mix" -> MixSleeveSpec(
        motif = MixMotif.DRIVE,
        gradientStart = Color(0xFF121C2E),
        gradientMid = Color(0xFF1B0E30),
        gradientEnd = Color(0xFF120720),
        gradientMidStop = 0.58f,
        borderColor = Color(0x3833FFB0),
        pillBackground = Color(0x1F00FF9D),
        pillForeground = Color(0xFF33FFB0),
    )

    "evening_unwind" -> MixSleeveSpec(
        motif = MixMotif.EVENING_UNWIND,
        gradientStart = Color(0xFF2D1426),
        gradientMid = Color(0xFF1D0D2D),
        gradientEnd = Color(0xFF120720),
        gradientMidStop = 0.55f,
        borderColor = Color(0x3DFB923C),
        pillBackground = Color(0x24FB923C),
        pillForeground = Color(0xFFFDBA74),
    )

    "friday_night" -> MixSleeveSpec(
        motif = MixMotif.FRIDAY_NIGHT,
        gradientStart = Color(0xFF2D0D3D),
        gradientMid = Color(0xFF1D0C35),
        gradientEnd = Color(0xFF110722),
        gradientMidStop = 0.55f,
        borderColor = Color(0x47F472F4),
        pillBackground = Color(0x24FF00FF),
        pillForeground = Color(0xFFF472F4),
    )

    "morning" -> MixSleeveSpec(
        motif = MixMotif.MORNING,
        gradientStart = Color(0xFF341926),
        gradientMid = Color(0xFF221030),
        gradientEnd = Color(0xFF120820),
        gradientMidStop = 0.56f,
        borderColor = Color(0x42FDA4AF),
        pillBackground = Color(0x24FB9278),
        pillForeground = Color(0xFFFDA4AF),
    )

    "focus" -> MixSleeveSpec(
        motif = MixMotif.FOCUS,
        gradientStart = Color(0xFF0F2034),
        gradientMid = Color(0xFF151434),
        gradientEnd = Color(0xFF100820),
        gradientMidStop = 0.55f,
        borderColor = Color(0x3D2DD4BF),
        pillBackground = Color(0x212DD4BF),
        pillForeground = Color(0xFF5EEAD4),
    )

    "dinner_groove" -> MixSleeveSpec(
        motif = MixMotif.DINNER_GROOVE,
        gradientStart = Color(0xFF341322),
        gradientMid = Color(0xFF220E2E),
        gradientEnd = Color(0xFF130820),
        gradientMidStop = 0.56f,
        borderColor = Color(0x42F59E0B),
        pillBackground = Color(0x24F59E0B),
        pillForeground = Color(0xFFFBBF24),
    )

    "late_night" -> MixSleeveSpec(
        motif = MixMotif.LATE_NIGHT,
        gradientStart = Color(0xFF230D38),
        gradientMid = Color(0xFF160A2C),
        gradientEnd = Color(0xFF0C051A),
        gradientMidStop = 0.52f,
        borderColor = Color(0x42C084FC),
        pillBackground = Color(0x24C084FC),
        pillForeground = Color(0xFFD8B4FE),
    )

    "sunday_morning" -> MixSleeveSpec(
        motif = MixMotif.SUNDAY_MORNING,
        gradientStart = Color(0xFF2E1B16),
        gradientMid = Color(0xFF1E1129),
        gradientEnd = Color(0xFF120820),
        gradientMidStop = 0.58f,
        borderColor = Color(0x40FBBF78),
        pillBackground = Color(0x24FBBF78),
        pillForeground = Color(0xFFFED7AA),
    )

    // --- Collection & Decades (5) ---
    "forgotten_gems" -> MixSleeveSpec(
        motif = MixMotif.FORGOTTEN_GEMS,
        gradientStart = Color(0xFF2D1E17),
        gradientMid = Color(0xFF1C1028),
        gradientEnd = Color(0xFF140920),
        gradientMidStop = 0.64f,
        borderColor = Color(0x42FFD166),
        pillBackground = Color(0x24FFD166),
        pillForeground = Color(0xFFFFD166),
    )

    "fresh_arrivals" -> MixSleeveSpec(
        motif = MixMotif.FRESH_ARRIVALS,
        gradientStart = Color(0xFF0D2825),
        gradientMid = Color(0xFF15102B),
        gradientEnd = Color(0xFF110820),
        gradientMidStop = 0.64f,
        borderColor = Color(0x4233FFB0),
        pillBackground = Color(0x2400FF9D),
        pillForeground = Color(0xFF33FFB0),
    )

    "decade_80s" -> MixSleeveSpec(
        motif = MixMotif.DECADE_80S,
        gradientStart = Color(0xFF32103D),
        gradientMid = Color(0xFF1A1038),
        gradientEnd = Color(0xFF0F162E),
        gradientMidStop = 0.56f,
        borderColor = Color(0x42F472F4),
        pillBackground = Color(0x24F472F4),
        pillForeground = Color(0xFFF472F4),
        decadeWatermark = "80s",
        watermarkColor = Color(0x33F472F4),
    )

    "decade_90s" -> MixSleeveSpec(
        motif = MixMotif.DECADE_90S,
        gradientStart = Color(0xFF2A181E),
        gradientMid = Color(0xFF1D1228),
        gradientEnd = Color(0xFF13091F),
        gradientMidStop = 0.62f,
        borderColor = Color(0x42FB7185),
        pillBackground = Color(0x24FB7185),
        pillForeground = Color(0xFFFDA4AF),
        decadeWatermark = "90s",
        watermarkColor = Color(0x33FDA4AF),
    )

    "decade_2000s" -> MixSleeveSpec(
        motif = MixMotif.DECADE_2000S,
        gradientStart = Color(0xFF13233E),
        gradientMid = Color(0xFF1A1236),
        gradientEnd = Color(0xFF130922),
        gradientMidStop = 0.58f,
        borderColor = Color(0x427DD3FC),
        pillBackground = Color(0x2438BDF8),
        pillForeground = Color(0xFF7DD3FC),
        decadeWatermark = "00s",
        watermarkColor = Color(0x337DD3FC),
    )

    // --- Rolling (3) ---
    "conservative", "comfort" -> MixSleeveSpec(
        motif = MixMotif.ROLLING_COMFORT,
        gradientStart = Color(0xFF122826),
        gradientMid = Color(0xFF16112D),
        gradientEnd = Color(0xFF110820),
        gradientMidStop = 0.62f,
        borderColor = Color(0x4033FFB0),
        pillBackground = Color(0x2100FF9D),
        pillForeground = Color(0xFF33FFB0),
    )

    "balanced", "weekly_radar" -> MixSleeveSpec(
        motif = MixMotif.ROLLING_RADAR,
        gradientStart = Color(0xFF271238),
        gradientMid = Color(0xFF190F32),
        gradientEnd = Color(0xFF101628),
        gradientMidStop = 0.58f,
        borderColor = Color(0x40F472F4),
        pillBackground = Color(0x24F472F4),
        pillForeground = Color(0xFFF472F4),
    )

    "adventurous", "deep_cuts" -> MixSleeveSpec(
        motif = MixMotif.ROLLING_DEEP_CUTS,
        gradientStart = Color(0xFF2F1038),
        gradientMid = Color(0xFF1C0C34),
        gradientEnd = Color(0xFF110722),
        gradientMidStop = 0.58f,
        borderColor = Color(0x47E879F9),
        pillBackground = Color(0x26E879F9),
        pillForeground = Color(0xFFF0ABFC),
    )

    // --- Seasonals (13) ---
    "winter_holiday", "noel", "christmas" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_WINTER_HOLIDAY,
        gradientStart = Color(0xFF2C1220),
        gradientMid = Color(0xFF181A2E),
        gradientEnd = Color(0xFF0F1920),
        gradientMidStop = 0.55f,
        borderColor = Color(0x47FFD166),
        pillBackground = Color(0x24FFD166),
        pillForeground = Color(0xFFFFD166),
    )

    "new_years_eve", "nye" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_NEW_YEARS_EVE,
        gradientStart = Color(0xFF2A1834),
        gradientMid = Color(0xFF1A0F32),
        gradientEnd = Color(0xFF110720),
        gradientMidStop = 0.56f,
        borderColor = Color(0x47FDE047),
        pillBackground = Color(0x24FDE047),
        pillForeground = Color(0xFFFDE047),
    )

    "cozy_winter" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_COZY_WINTER,
        gradientStart = Color(0xFF2A191A),
        gradientMid = Color(0xFF1A1530),
        gradientEnd = Color(0xFF110922),
        gradientMidStop = 0.56f,
        borderColor = Color(0x42FBBF92),
        pillBackground = Color(0x24FBBF92),
        pillForeground = Color(0xFFFDBA74),
    )

    "lunar_new_year" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_LUNAR_NEW_YEAR,
        gradientStart = Color(0xFF3A111C),
        gradientMid = Color(0xFF240E2A),
        gradientEnd = Color(0xFF13071E),
        gradientMidStop = 0.58f,
        borderColor = Color(0x4DFBBF24),
        pillBackground = Color(0x29FB7185),
        pillForeground = Color(0xFFFDA4AF),
    )

    "valentines" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_VALENTINES,
        gradientStart = Color(0xFF381028),
        gradientMid = Color(0xFF230D2F),
        gradientEnd = Color(0xFF130720),
        gradientMidStop = 0.56f,
        borderColor = Color(0x47F472B6),
        pillBackground = Color(0x24F472B6),
        pillForeground = Color(0xFFF9A8D4),
    )

    "carnival" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_CARNIVAL,
        gradientStart = Color(0xFF2D1438),
        gradientMid = Color(0xFF1B1838),
        gradientEnd = Color(0xFF0F2128),
        gradientMidStop = 0.52f,
        borderColor = Color(0x47FACC15),
        pillBackground = Color(0x24FACC15),
        pillForeground = Color(0xFFFDE047),
    )

    "celtic_spring" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_CELTIC_SPRING,
        gradientStart = Color(0xFF0D2B22),
        gradientMid = Color(0xFF131B30),
        gradientEnd = Color(0xFF100920),
        gradientMidStop = 0.56f,
        borderColor = Color(0x4734D399),
        pillBackground = Color(0x2434D399),
        pillForeground = Color(0xFF6EE7B7),
    )

    "spring" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_SPRING,
        gradientStart = Color(0xFF172A28),
        gradientMid = Color(0xFF211433),
        gradientEnd = Color(0xFF130822),
        gradientMidStop = 0.56f,
        borderColor = Color(0x426EE7B7),
        pillBackground = Color(0x246EE7B7),
        pillForeground = Color(0xFF6EE7B7),
    )

    "summer" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_SUMMER,
        gradientStart = Color(0xFF3A1D16),
        gradientMid = Color(0xFF26122C),
        gradientEnd = Color(0xFF130922),
        gradientMidStop = 0.56f,
        borderColor = Color(0x47FB923C),
        pillBackground = Color(0x26FB923C),
        pillForeground = Color(0xFFFDBA74),
    )

    "summer_solstice" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_SUMMER_SOLSTICE,
        gradientStart = Color(0xFF3A2114),
        gradientMid = Color(0xFF281332),
        gradientEnd = Color(0xFF140822),
        gradientMidStop = 0.54f,
        borderColor = Color(0x47FDE047),
        pillBackground = Color(0x24FDE047),
        pillForeground = Color(0xFFFDE047),
    )

    "late_summer_reset" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_LATE_SUMMER_RESET,
        gradientStart = Color(0xFF2F1728),
        gradientMid = Color(0xFF1D1234),
        gradientEnd = Color(0xFF120820),
        gradientMidStop = 0.56f,
        borderColor = Color(0x40F472B6),
        pillBackground = Color(0x24E879F9),
        pillForeground = Color(0xFFF0ABFC),
    )

    "autumn" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_AUTUMN,
        gradientStart = Color(0xFF331914),
        gradientMid = Color(0xFF221128),
        gradientEnd = Color(0xFF130820),
        gradientMidStop = 0.56f,
        borderColor = Color(0x47F59E0B),
        pillBackground = Color(0x24F59E0B),
        pillForeground = Color(0xFFFBBF24),
    )

    "halloween" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_HALLOWEEN,
        gradientStart = Color(0xFF250E33),
        gradientMid = Color(0xFF1A0B28),
        gradientEnd = Color(0xFF170D16),
        gradientMidStop = 0.54f,
        borderColor = Color(0x47F97316),
        pillBackground = Color(0x26F97316),
        pillForeground = Color(0xFFFB923C),
    )

    // --- Fallback (Midnight Studio violet-accent wash) ---
    else -> MixSleeveSpec(
        motif = MixMotif.FALLBACK,
        gradientStart = Color(0xFF24103A),
        gradientMid = Color(0xFF190C2E),
        gradientEnd = Color(0xFF110720),
        gradientMidStop = 0.56f,
        borderColor = Color(0x38D8C2F5),
        pillBackground = Color(0x24F472F4),
        pillForeground = Color(0xFFF472F4),
    )
}

@Composable
fun MixSleeveBackdrop(
    slug: String,
    modifier: Modifier = Modifier,
) {
    val spec = remember(slug) { mixSleeveSpecFor(slug) }

    Box(modifier = modifier.clipToBounds()) {
        Spacer(
            modifier = Modifier
                .fillMaxSize()
                .drawWithCache {
                    if (size.width <= 0f || size.height <= 0f) {
                        return@drawWithCache onDrawBehind {}
                    }
                    val baseBrush = Brush.linearGradient(
                        colorStops = arrayOf(
                            0f to spec.gradientStart,
                            spec.gradientMidStop to spec.gradientMid,
                            1f to spec.gradientEnd,
                        ),
                        start = Offset.Zero,
                        end = Offset(size.width, size.height),
                    )
                    val drawMotif = buildMotifDrawer(spec.motif)
                    onDrawBehind {
                        drawRect(brush = baseBrush)
                        drawMotif()
                    }
                },
        )

        spec.decadeWatermark?.let { label ->
            Text(
                text = label,
                style = TextStyle(
                    fontFamily = SpaceGroteskFamily,
                    fontWeight = FontWeight.Bold,
                    fontSize = 52.sp,
                    lineHeight = 52.sp,
                    letterSpacing = (-0.045).em,
                    fontFeatureSettings = "lnum, tnum",
                ),
                color = spec.watermarkColor,
                maxLines = 1,
                softWrap = false,
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .padding(top = 46.dp, end = 12.dp)
                    .clearAndSetSemantics {},
            )
        }
    }
}

@Composable
fun MixSleeveCard(
    mix: ContextualMixUi,
    layout: MixSleeveLayout,
    onOpenDetail: () -> Unit,
    onPlayClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val spec = remember(mix.slug) { mixSleeveSpecFor(mix.slug) }
    val tag = remember(mix.slug, mix.kind, mix.offlinePriority) { formatMixSleeveTag(mix) }
    val metaLine = remember(mix.kind, mix.countryCode, mix.localPct, mix.trackCount, mix.ownedPct, layout) {
        formatMixSleeveMeta(mix, compact = layout == MixSleeveLayout.SQUARE)
    }
    val scrimBrush = remember(layout) {
        if (layout == MixSleeveLayout.WIDE) {
            Brush.horizontalGradient(
                colorStops = arrayOf(
                    0.0f to Color(0x8F0B0414),
                    0.56f to Color(0x3D0B0414),
                    1.0f to Color(0x0A0B0414),
                ),
            )
        } else {
            Brush.verticalGradient(
                colorStops = arrayOf(
                    0.0f to Color(0x140B0414),
                    0.48f to Color(0x380B0414),
                    1.0f to Color(0xAD0B0414),
                ),
            )
        }
    }
    val cardShape = RoundedCornerShape(14.dp)
    val sizeModifier = when (layout) {
        MixSleeveLayout.WIDE -> Modifier
            .width(272.dp)
            .height(168.dp)

        MixSleeveLayout.SQUARE -> Modifier
            .width(172.dp)
            .height(188.dp)
    }
    val trackWord = if (mix.trackCount == 1) "track" else "tracks"

    Surface(
        onClick = onOpenDetail,
        shape = cardShape,
        color = spec.gradientEnd,
        border = BorderStroke(1.dp, spec.borderColor),
        modifier = modifier
            .then(sizeModifier)
            .clearAndSetSemantics {
                role = Role.Button
                contentDescription = "${mix.title}, $tag, ${mix.subtitle}, ${mix.trackCount} $trackWord. Open mix details."
            },
    ) {
        Box(modifier = Modifier.fillMaxSize()) {
            MixSleeveBackdrop(
                slug = mix.slug,
                modifier = Modifier.matchParentSize(),
            )

            // Subtle directional scrim to guarantee crisp text contrast over every palette
            Box(
                modifier = Modifier
                    .matchParentSize()
                    .background(scrimBrush),
            )

            when (layout) {
                MixSleeveLayout.WIDE -> {
                    Column(
                        modifier = Modifier
                            .fillMaxSize()
                            .padding(16.dp),
                        verticalArrangement = Arrangement.SpaceBetween,
                    ) {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Box(
                                modifier = Modifier.weight(1f),
                                contentAlignment = Alignment.CenterStart,
                            ) {
                                MixSleeveTagPill(
                                    text = tag,
                                    background = spec.pillBackground,
                                    foreground = spec.pillForeground,
                                    border = spec.borderColor,
                                )
                            }
                            Surface(
                                onClick = onPlayClick,
                                shape = CircleShape,
                                color = colors.leafSolid,
                                modifier = Modifier.size(36.dp),
                            ) {
                                Box(contentAlignment = Alignment.Center) {
                                    Icon(
                                        imageVector = Icons.Filled.PlayArrow,
                                        contentDescription = "Play ${mix.title}",
                                        tint = colors.leafInk,
                                        modifier = Modifier.size(20.dp),
                                    )
                                }
                            }
                        }

                        Column(
                            modifier = Modifier.fillMaxWidth(0.68f),
                            verticalArrangement = Arrangement.spacedBy(3.dp),
                        ) {
                            Text(
                                text = mix.title,
                                style = type.titleMd.copy(fontWeight = FontWeight.Bold),
                                color = colors.textPrimary,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                            Text(
                                text = mix.subtitle,
                                style = type.bodySm.copy(lineHeight = 16.5.sp),
                                color = Color(0xCCEBE4F6),
                                minLines = 2,
                                maxLines = 2,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }

                        Text(
                            text = metaLine,
                            style = type.monoXs,
                            color = Color(0xB3D8C2F5),
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }

                MixSleeveLayout.SQUARE -> {
                    Column(
                        modifier = Modifier
                            .fillMaxSize()
                            .padding(14.dp),
                        verticalArrangement = Arrangement.SpaceBetween,
                    ) {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Box(
                                modifier = Modifier.weight(1f),
                                contentAlignment = Alignment.CenterStart,
                            ) {
                                MixSleeveTagPill(
                                    text = tag,
                                    background = spec.pillBackground,
                                    foreground = spec.pillForeground,
                                    border = spec.borderColor,
                                )
                            }
                            Surface(
                                onClick = onPlayClick,
                                shape = CircleShape,
                                color = colors.leafSolid,
                                modifier = Modifier.size(32.dp),
                            ) {
                                Box(contentAlignment = Alignment.Center) {
                                    Icon(
                                        imageVector = Icons.Filled.PlayArrow,
                                        contentDescription = "Play ${mix.title}",
                                        tint = colors.leafInk,
                                        modifier = Modifier.size(18.dp),
                                    )
                                }
                            }
                        }

                        Spacer(modifier = Modifier.weight(1f))

                        Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
                            Text(
                                text = mix.title,
                                style = TextStyle(
                                    fontFamily = SpaceGroteskFamily,
                                    fontWeight = FontWeight.Bold,
                                    fontSize = 16.sp,
                                    lineHeight = 19.sp,
                                    letterSpacing = (-0.02).em,
                                ),
                                color = colors.textPrimary,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                            Text(
                                text = mix.subtitle,
                                style = TextStyle(
                                    fontFamily = SpaceGroteskFamily,
                                    fontWeight = FontWeight.Normal,
                                    fontSize = 11.5.sp,
                                    lineHeight = 15.sp,
                                ),
                                color = Color(0xCCEBE4F6),
                                minLines = 2,
                                maxLines = 2,
                                overflow = TextOverflow.Ellipsis,
                            )
                            Spacer(modifier = Modifier.height(3.dp))
                            Text(
                                text = metaLine,
                                style = type.monoXs.copy(fontSize = 10.sp),
                                color = Color(0xB3D8C2F5),
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun MixSleeveTagPill(
    text: String,
    background: Color,
    foreground: Color,
    border: Color,
    modifier: Modifier = Modifier,
) {
    val type = LocalSoulbeetTypography.current
    Surface(
        modifier = modifier,
        color = background,
        shape = SoulbeetShapes.full,
        border = BorderStroke(1.dp, border),
    ) {
        Text(
            text = text,
            style = type.monoXs.copy(fontSize = 10.sp, lineHeight = 12.sp),
            color = foreground,
            maxLines = 1,
            softWrap = false,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(horizontal = 8.dp, vertical = 3.dp),
        )
    }
}

internal fun formatMixSleeveMeta(mix: ContextualMixUi, compact: Boolean): String {
    val trackWord = if (mix.trackCount == 1) "TRACK" else "TRACKS"
    val tracks = "${mix.trackCount} $trackWord"
    if (compact) return tracks
    return if (mix.kind.trim().lowercase(Locale.ROOT) == "seasonal") {
        val cc = mix.countryCode?.trim()?.takeIf { it.isNotEmpty() }?.uppercase(Locale.ROOT)
        val localPct = mix.localPct
        if (cc != null && localPct != null) {
            "$tracks · $localPct% $cc"
        } else {
            "$tracks · SEASONAL"
        }
    } else if (mix.ownedPct in 1..100) {
        "$tracks · ${mix.ownedPct}% LIBRARY"
    } else {
        tracks
    }
}

@Composable
fun MixSleeveShelf(
    title: String,
    eyebrow: String,
    mixes: List<ContextualMixUi>,
    layout: MixSleeveLayout,
    onOpenMix: (ContextualMixUi) -> Unit,
    onPlayMix: (ContextualMixUi) -> Unit,
    modifier: Modifier = Modifier,
) {
    if (mixes.isEmpty()) return
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    Column(
        modifier = modifier.fillMaxWidth(),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = title,
                style = type.titleMd,
                color = colors.textPrimary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            Text(
                text = eyebrow,
                style = type.monoXs,
                color = colors.textMuted,
                maxLines = 1,
                softWrap = false,
            )
        }
        LazyRow(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            itemsIndexed(mixes, key = { idx, mix -> "${idx}_${mix.slug}" }) { _, mix ->
                MixSleeveCard(
                    mix = mix,
                    layout = layout,
                    onOpenDetail = { onOpenMix(mix) },
                    onPlayClick = { onPlayMix(mix) },
                )
            }
        }
    }
}

fun LazyListScope.mixSleeveShelves(
    rightNowMixes: List<ContextualMixUi>,
    collectionMixes: List<ContextualMixUi>,
    keyPrefix: String,
    onOpenMix: (ContextualMixUi) -> Unit,
    onPlayMix: (ContextualMixUi) -> Unit,
) {
    if (rightNowMixes.isNotEmpty()) {
        item(key = "${keyPrefix}_right_now_mixes") {
            MixSleeveShelf(
                title = "Right Now",
                eyebrow = "LIVE WEATHER & TIME",
                mixes = rightNowMixes,
                layout = MixSleeveLayout.WIDE,
                onOpenMix = onOpenMix,
                onPlayMix = onPlayMix,
            )
        }
    }
    if (collectionMixes.isNotEmpty()) {
        item(key = "${keyPrefix}_collection_mixes") {
            MixSleeveShelf(
                title = "From Your Collection",
                eyebrow = "LIBRARY & DECADES",
                mixes = collectionMixes,
                layout = MixSleeveLayout.SQUARE,
                onOpenMix = onOpenMix,
                onPlayMix = onPlayMix,
            )
        }
    }
}

internal enum class ViewBoxScaleMode {
    SLICE,
    MEET,
    STRETCH,
}

internal data class MotifViewBoxSpec(
    val width: Float,
    val height: Float,
    val scaleMode: ViewBoxScaleMode,
)

internal fun motifViewBoxSpecFor(motif: MixMotif): MotifViewBoxSpec = when (motif) {
    // Weather (4)
    MixMotif.RAINY_DAY -> MotifViewBoxSpec(400f, 200f, ViewBoxScaleMode.SLICE)
    MixMotif.SNOW_DAY,
    MixMotif.STORM_NIGHT,
    MixMotif.SUNNY_DAY,
    -> MotifViewBoxSpec(320f, 180f, ViewBoxScaleMode.SLICE)

    // Existing landscape moments (3)
    MixMotif.DRIVE -> MotifViewBoxSpec(400f, 220f, ViewBoxScaleMode.STRETCH)
    MixMotif.EVENING_UNWIND -> MotifViewBoxSpec(400f, 200f, ViewBoxScaleMode.SLICE)
    MixMotif.FRIDAY_NIGHT -> MotifViewBoxSpec(400f, 184f, ViewBoxScaleMode.SLICE)

    // Collection & Decades (5) + Rolling (3): 260x260 MEET
    MixMotif.FORGOTTEN_GEMS,
    MixMotif.FRESH_ARRIVALS,
    MixMotif.DECADE_80S,
    MixMotif.DECADE_90S,
    MixMotif.DECADE_2000S,
    MixMotif.ROLLING_COMFORT,
    MixMotif.ROLLING_RADAR,
    MixMotif.ROLLING_DEEP_CUTS,
    -> MotifViewBoxSpec(260f, 260f, ViewBoxScaleMode.MEET)

    // Redesigned wide moments (5), seasonals (13), and fallback (1): 240x200 MEET so hero/wide never vertically crops
    MixMotif.MORNING,
    MixMotif.FOCUS,
    MixMotif.DINNER_GROOVE,
    MixMotif.LATE_NIGHT,
    MixMotif.SUNDAY_MORNING,
    MixMotif.SEASONAL_WINTER_HOLIDAY,
    MixMotif.SEASONAL_NEW_YEARS_EVE,
    MixMotif.SEASONAL_COZY_WINTER,
    MixMotif.SEASONAL_LUNAR_NEW_YEAR,
    MixMotif.SEASONAL_VALENTINES,
    MixMotif.SEASONAL_CARNIVAL,
    MixMotif.SEASONAL_CELTIC_SPRING,
    MixMotif.SEASONAL_SPRING,
    MixMotif.SEASONAL_SUMMER,
    MixMotif.SEASONAL_SUMMER_SOLSTICE,
    MixMotif.SEASONAL_LATE_SUMMER_RESET,
    MixMotif.SEASONAL_AUTUMN,
    MixMotif.SEASONAL_HALLOWEEN,
    MixMotif.FALLBACK,
    -> MotifViewBoxSpec(240f, 200f, ViewBoxScaleMode.MEET)
}

/**
 * Maps virtual SVG viewBox coordinates onto the Canvas using:
 * - [ViewBoxScaleMode.SLICE] (`xMaxYMid slice`) for full-bleed landscape motifs
 * - [ViewBoxScaleMode.MEET] (`xMaxYMid meet`) for 240x200 and 260x260 motifs so nothing clips vertically
 * - [ViewBoxScaleMode.STRETCH] for wireframe perspective grids
 */
private class ViewBoxMapper(
    val canvasW: Float,
    val canvasH: Float,
    val viewW: Float,
    val viewH: Float,
    mode: ViewBoxScaleMode = ViewBoxScaleMode.SLICE,
) {
    private val sx: Float = when (mode) {
        ViewBoxScaleMode.STRETCH -> canvasW / viewW
        ViewBoxScaleMode.MEET -> minOf(canvasW / viewW, canvasH / viewH)
        ViewBoxScaleMode.SLICE -> maxOf(canvasW / viewW, canvasH / viewH)
    }
    private val sy: Float = if (mode == ViewBoxScaleMode.STRETCH) canvasH / viewH else sx
    private val dx: Float = if (mode == ViewBoxScaleMode.STRETCH) 0f else canvasW - viewW * sx
    private val dy: Float = if (mode == ViewBoxScaleMode.STRETCH) 0f else (canvasH - viewH * sy) * 0.5f

    fun x(vx: Float): Float = dx + vx * sx
    fun y(vy: Float): Float = dy + vy * sy
    fun pt(vx: Float, vy: Float): Offset = Offset(x(vx), y(vy))
    fun s(v: Float): Float = v * sx
    fun sw(v: Float): Float = v * sx
    fun sh(v: Float): Float = v * sy
}

private fun CacheDrawScope.mapperFor(motif: MixMotif): ViewBoxMapper {
    val spec = motifViewBoxSpecFor(motif)
    return ViewBoxMapper(size.width, size.height, spec.width, spec.height, mode = spec.scaleMode)
}

// --- Static motif geometry constants (hoisted to avoid per-frame allocations) ---

private val RAIN_STREAKS = listOf(
    floatArrayOf(252f, 8f, 228f, 74f, 1.4f, 0.55f),
    floatArrayOf(286f, 22f, 256f, 108f, 1.2f, 0.48f),
    floatArrayOf(316f, -4f, 292f, 68f, 1.6f, 0.62f),
    floatArrayOf(348f, 30f, 318f, 120f, 1.3f, 0.55f),
    floatArrayOf(378f, 10f, 352f, 86f, 1.5f, 0.52f),
    floatArrayOf(226f, 88f, 206f, 148f, 1.0f, 0.35f),
    floatArrayOf(298f, 98f, 274f, 168f, 1.2f, 0.45f),
    floatArrayOf(364f, 92f, 338f, 168f, 1.3f, 0.42f),
)

private val SNOW_DOTS = listOf(
    floatArrayOf(212f, 30f, 2.2f, 0.42f),
    floatArrayOf(268f, 28f, 1.6f, 0.35f),
    floatArrayOf(294f, 68f, 2.8f, 0.28f),
    floatArrayOf(246f, 96f, 1.8f, 0.32f),
    floatArrayOf(256f, 144f, 2.4f, 0.22f),
    floatArrayOf(192f, 76f, 1.5f, 0.24f),
)

private val SUNNY_RAYS = listOf(
    Pair(listOf(236f to 64f, 128f to 18f, 144f to 4f, 238f to 60f), 0.26f),
    Pair(listOf(236f to 66f, 112f to 72f, 116f to 54f, 236f to 63f), 0.34f),
    Pair(listOf(236f to 68f, 134f to 126f, 124f to 108f, 235f to 66f), 0.36f),
    Pair(listOf(238f to 70f, 176f to 176f, 158f to 164f, 236f to 68f), 0.32f),
    Pair(listOf(240f to 70f, 232f to 180f, 212f to 180f, 238f to 69f), 0.24f),
    Pair(listOf(242f to 68f, 286f to 180f, 266f to 180f, 240f to 68f), 0.20f),
)

private val EVENING_BANDS = listOf(
    floatArrayOf(210f, 150f, 170f, 1.0f),
    floatArrayOf(242f, 164f, 138f, 0.72f),
    floatArrayOf(276f, 178f, 104f, 0.48f),
)

private val MORNING_BARS = listOf(
    floatArrayOf(42f, 114f, 178f, 1.2f, 0.6f, 1.0f),
    floatArrayOf(86f, 128f, 130f, 1.1f, 0.5f, 0.78f),
    floatArrayOf(112f, 142f, 92f, 1.0f, 0.5f, 0.52f),
    floatArrayOf(134f, 156f, 54f, 0.9f, 0.5f, 0.32f),
)

private val FOCUS_CALIBRATION_DOTS = listOf(
    102f to 52f,
    126f to 52f,
    174f to 52f,
    198f to 52f,
    102f to 76f,
    198f to 76f,
    102f to 124f,
    198f to 124f,
    102f to 148f,
    126f to 148f,
    174f to 148f,
    198f to 148f,
)

private val DINNER_PILLARS = listOf(
    // x, y, w, h, rx, warm(1f=warm, 0f=cool), opacity
    floatArrayOf(108f, 88f, 10f, 66f, 5f, 0f, 1.0f),
    floatArrayOf(128f, 62f, 12f, 92f, 6f, 1f, 1.0f),
    floatArrayOf(150f, 48f, 14f, 106f, 7f, 1f, 1.0f),
    floatArrayOf(174f, 72f, 11f, 82f, 5.5f, 0f, 1.0f),
    floatArrayOf(194f, 98f, 9f, 56f, 4.5f, 0f, 0.75f),
)

private val SUNDAY_SLATS = listOf(
    Pair(listOf(152f to 0f, 178f to 0f, 82f to 200f, 56f to 200f), 0.42f),
    Pair(listOf(186f to 0f, 208f to 0f, 112f to 200f, 90f to 200f), 0.68f),
    Pair(listOf(216f to 0f, 232f to 0f, 140f to 200f, 124f to 200f), 0.52f),
    Pair(listOf(240f to 18f, 240f to 56f, 174f to 200f, 156f to 200f), 0.34f),
)

private val FORGOTTEN_GEMS_GROOVES = listOf(
    floatArrayOf(24f, 0.26f, 1.2f),
    floatArrayOf(40f, 0.20f, 1.0f),
    floatArrayOf(56f, 0.16f, 1.0f),
    floatArrayOf(72f, 0.12f, 0.9f),
    floatArrayOf(88f, 0.08f, 0.9f),
)

private val FRESH_ARRIVALS_BARS = listOf(
    floatArrayOf(132f, 78f, 22f),
    floatArrayOf(142f, 66f, 46f),
    floatArrayOf(152f, 82f, 14f),
    floatArrayOf(162f, 56f, 66f),
    floatArrayOf(172f, 70f, 38f),
    floatArrayOf(182f, 52f, 74f),
    floatArrayOf(192f, 68f, 42f),
    floatArrayOf(202f, 60f, 58f),
    floatArrayOf(212f, 74f, 30f),
    floatArrayOf(222f, 80f, 18f),
)

private val DECADE_80S_LINE_YS = floatArrayOf(196f, 216f, 238f)

private val NYE_RAYS = listOf(
    // x1, y1, x2, y2, strokeWidth, opacity
    floatArrayOf(152f, 84f, 152f, 42f, 1.2f, 0.56f),
    floatArrayOf(160f, 87f, 184f, 54f, 1.1f, 0.46f),
    floatArrayOf(165f, 94f, 206f, 80f, 1.2f, 0.52f),
    floatArrayOf(165f, 103f, 200f, 118f, 1.0f, 0.40f),
    floatArrayOf(159f, 110f, 178f, 144f, 1.1f, 0.44f),
    floatArrayOf(148f, 112f, 140f, 150f, 1.0f, 0.38f),
    floatArrayOf(140f, 106f, 106f, 128f, 1.2f, 0.48f),
    floatArrayOf(138f, 97f, 98f, 94f, 1.1f, 0.42f),
    floatArrayOf(143f, 88f, 116f, 62f, 1.0f, 0.44f),
)

private val COZY_WINTER_BANDS = listOf(
    // x, y, w, h, rx, opacity
    floatArrayOf(66f, 112f, 148f, 10f, 5f, 0.28f),
    floatArrayOf(52f, 130f, 166f, 12f, 6f, 0.48f),
    floatArrayOf(78f, 150f, 136f, 8f, 4f, 0.34f),
    floatArrayOf(104f, 166f, 92f, 2f, 1f, 0.52f),
)

private val SUMMER_HORIZON_BARS = listOf(
    // x, y, w, h, rx
    Pair(floatArrayOf(66f, 128f, 156f, 1.3f, 0.6f), Color(0x70FED7AA)),
    Pair(floatArrayOf(94f, 140f, 120f, 1.3f, 0.6f), Color(0x57FDBA74)),
    Pair(floatArrayOf(112f, 151f, 84f, 1.2f, 0.6f), Color(0x42FDBA74)),
    Pair(floatArrayOf(128f, 162f, 52f, 1.1f, 0.5f), Color(0x2EFDE047)),
    Pair(floatArrayOf(140f, 173f, 28f, 1.0f, 0.5f), Color(0x1FFDE047)),
)

private val SPRING_PRIMARY_ANGLES = doubleArrayOf(0.0, 120.0, 240.0)
private val SPRING_ALT_ANGLES = doubleArrayOf(60.0, 180.0, 300.0)

// --- Cached geometry containers ---

private class CachedLine(
    val start: Offset,
    val end: Offset,
    val strokeWidth: Float,
    val color: Color = Color.Unspecified,
    val brush: Brush? = null,
)

private class CachedRect(
    val topLeft: Offset,
    val size: Size,
    val color: Color = Color.Unspecified,
    val brush: Brush? = null,
    val cornerRadius: CornerRadius = CornerRadius.Zero,
)

private class CachedCircle(
    val center: Offset,
    val radius: Float,
    val color: Color = Color.Unspecified,
    val brush: Brush? = null,
    val stroke: Stroke? = null,
)

private class CachedColoredPath(
    val path: Path,
    val color: Color = Color.Unspecified,
    val brush: Brush? = null,
    val stroke: Stroke? = null,
    val strokeColor: Color = Color.Unspecified,
)

private fun buildSmoothFourPointStarPath(
    center: Offset,
    outerR: Float,
    waist: Float,
): Path = Path().apply {
    moveTo(center.x, center.y - outerR)
    quadraticTo(center.x + waist, center.y - waist, center.x + outerR, center.y)
    quadraticTo(center.x + waist, center.y + waist, center.x, center.y + outerR)
    quadraticTo(center.x - waist, center.y + waist, center.x - outerR, center.y)
    quadraticTo(center.x - waist, center.y - waist, center.x, center.y - outerR)
    close()
}

private fun rotatedOffset(
    cx: Float,
    cy: Float,
    lx: Float,
    ly: Float,
    degrees: Double,
    scale: Float = 1f,
): Pair<Float, Float> {
    val rad = degrees * PI / 180.0
    val c = cos(rad).toFloat()
    val s = sin(rad).toFloat()
    val sx = lx * scale
    val sy = ly * scale
    return (cx + sx * c - sy * s) to (cy + sx * s + sy * c)
}

private fun CacheDrawScope.radialGlowBrush(
    cxFrac: Float,
    cyFrac: Float,
    radiusFrac: Float,
    vararg stops: Pair<Float, Color>,
): Brush {
    val radius = (maxOf(size.width, size.height) * radiusFrac).coerceAtLeast(1f)
    return Brush.radialGradient(
        colorStops = stops,
        center = Offset(size.width * cxFrac, size.height * cyFrac),
        radius = radius,
    )
}

private fun buildFourPointStarPath(
    center: Offset,
    outerR: Float,
    innerR: Float,
): Path = Path().apply {
    moveTo(center.x, center.y - outerR)
    lineTo(center.x + innerR, center.y - innerR)
    lineTo(center.x + outerR, center.y)
    lineTo(center.x + innerR, center.y + innerR)
    lineTo(center.x, center.y + outerR)
    lineTo(center.x - innerR, center.y + innerR)
    lineTo(center.x - outerR, center.y)
    lineTo(center.x - innerR, center.y - innerR)
    close()
}

private fun CacheDrawScope.buildMotifDrawer(motif: MixMotif): DrawScope.() -> Unit = when (motif) {
    MixMotif.RAINY_DAY -> buildRainyDay()
    MixMotif.SNOW_DAY -> buildSnowDay()
    MixMotif.STORM_NIGHT -> buildStormNight()
    MixMotif.SUNNY_DAY -> buildSunnyDay()
    MixMotif.DRIVE -> buildDriveGrid()
    MixMotif.EVENING_UNWIND -> buildEveningUnwind()
    MixMotif.FRIDAY_NIGHT -> buildFridayStarlight()
    MixMotif.MORNING -> buildMorningSunrise()
    MixMotif.FOCUS -> buildFocusGrid()
    MixMotif.DINNER_GROOVE -> buildDinnerGroove()
    MixMotif.LATE_NIGHT -> buildLateNightSkyline()
    MixMotif.SUNDAY_MORNING -> buildSundaySteam()
    MixMotif.FORGOTTEN_GEMS -> buildForgottenGemsVinyl()
    MixMotif.FRESH_ARRIVALS -> buildFreshArrivalsWaveform()
    MixMotif.DECADE_80S -> buildDecade80s()
    MixMotif.DECADE_90S -> buildDecade90s()
    MixMotif.DECADE_2000S -> buildDecade2000s()
    MixMotif.ROLLING_COMFORT -> buildRollingComfort()
    MixMotif.ROLLING_RADAR -> buildRollingRadar()
    MixMotif.ROLLING_DEEP_CUTS -> buildRollingDeepCuts()
    MixMotif.SEASONAL_WINTER_HOLIDAY -> buildSeasonalWinterHoliday()
    MixMotif.SEASONAL_NEW_YEARS_EVE -> buildSeasonalNewYearsEve()
    MixMotif.SEASONAL_COZY_WINTER -> buildSeasonalCozyWinter()
    MixMotif.SEASONAL_LUNAR_NEW_YEAR -> buildSeasonalLunarNewYear()
    MixMotif.SEASONAL_VALENTINES -> buildSeasonalValentines()
    MixMotif.SEASONAL_CARNIVAL -> buildSeasonalCarnival()
    MixMotif.SEASONAL_CELTIC_SPRING -> buildSeasonalCelticSpring()
    MixMotif.SEASONAL_SPRING -> buildSeasonalSpring()
    MixMotif.SEASONAL_SUMMER -> buildSeasonalSummer()
    MixMotif.SEASONAL_SUMMER_SOLSTICE -> buildSeasonalSummerSolstice()
    MixMotif.SEASONAL_LATE_SUMMER_RESET -> buildSeasonalLateSummerReset()
    MixMotif.SEASONAL_AUTUMN -> buildSeasonalAutumn()
    MixMotif.SEASONAL_HALLOWEEN -> buildSeasonalHalloween()
    MixMotif.FALLBACK -> buildFallbackWash()
}

// --- Weather Motifs (4) ---

private fun CacheDrawScope.buildRainyDay(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.RAINY_DAY)
    val glow = radialGlowBrush(
        0.76f, 0.22f, 0.60f,
        0.0f to Color(0x3D60A5FA),
        0.60f to Color(0x0F38BDF8),
        1.0f to Color.Transparent,
    )
    val rippleStroke = Stroke(width = m.s(1f))
    val outerRippleTopLeft = m.pt(322f - 44f, 178f - 7f)
    val outerRippleSize = Size(m.sw(88f), m.sh(14f))
    val innerRippleTopLeft = m.pt(322f - 22f, 178f - 3.5f)
    val innerRippleSize = Size(m.sw(44f), m.sh(7f))

    val cachedStreaks = RAIN_STREAKS.map { s ->
        val start = m.pt(s[0], s[1])
        val end = m.pt(s[2], s[3])
        val alpha = s[5]
        CachedLine(
            start = start,
            end = end,
            strokeWidth = m.s(s[4]),
            brush = Brush.linearGradient(
                colorStops = arrayOf(
                    0.0f to Color(0x00BAE6FD),
                    0.55f to Color(0xFF93C5FD).copy(alpha = 0.52f * alpha),
                    1.0f to Color(0xFFE0F2FE).copy(alpha = 0.85f * alpha),
                ),
                start = start,
                end = end,
            ),
        )
    }

    return {
        drawRect(brush = glow)
        drawOval(
            color = Color(0x2993C5FD),
            topLeft = outerRippleTopLeft,
            size = outerRippleSize,
            style = rippleStroke,
        )
        drawOval(
            color = Color(0x38BAE6FD),
            topLeft = innerRippleTopLeft,
            size = innerRippleSize,
            style = rippleStroke,
        )
        for (line in cachedStreaks) {
            drawLine(
                brush = line.brush!!,
                start = line.start,
                end = line.end,
                strokeWidth = line.strokeWidth,
                cap = StrokeCap.Round,
            )
        }
    }
}

private fun CacheDrawScope.buildSnowDay(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SNOW_DAY)
    val glow = radialGlowBrush(
        0.76f, 0.24f, 0.62f,
        0.0f to Color(0x3DE0F2FE),
        0.55f to Color(0x147DD3FC),
        1.0f to Color.Transparent,
    )
    val crystalLines = mutableListOf<CachedLine>()
    val chevronPaths = mutableListOf<CachedColoredPath>()
    val chevStyle = Stroke(width = m.s(1.0f), cap = StrokeCap.Round, join = StrokeJoin.Round)

    fun addCrystal(cx: Float, cy: Float, r: Float, strokeW: Float, alpha: Float, withChevrons: Boolean) {
        val color = Color(0xFFE0F2FE).copy(alpha = alpha)
        val dx = r * 0.866f
        val dy = r * 0.5f
        val sw = m.s(strokeW)
        crystalLines.add(CachedLine(m.pt(cx, cy - r), m.pt(cx, cy + r), sw, color))
        crystalLines.add(CachedLine(m.pt(cx - dx, cy - dy), m.pt(cx + dx, cy + dy), sw, color))
        crystalLines.add(CachedLine(m.pt(cx - dx, cy + dy), m.pt(cx + dx, cy - dy), sw, color))
        if (withChevrons) {
            chevronPaths.add(
                CachedColoredPath(
                    path = Path().apply {
                        moveTo(m.x(cx - 2.85f), m.y(cy - 9.5f))
                        lineTo(m.x(cx), m.y(cy - 6.65f))
                        lineTo(m.x(cx + 2.85f), m.y(cy - 9.5f))
                    },
                    color = color,
                ),
            )
            chevronPaths.add(
                CachedColoredPath(
                    path = Path().apply {
                        moveTo(m.x(cx - 2.85f), m.y(cy + 9.5f))
                        lineTo(m.x(cx), m.y(cy + 6.65f))
                        lineTo(m.x(cx + 2.85f), m.y(cy + 9.5f))
                    },
                    color = color,
                ),
            )
        }
    }

    addCrystal(238f, 54f, 13.3f, 1.2f, 0.42f, withChevrons = true)
    addCrystal(278f, 108f, 8.8f, 1.0f, 0.34f, withChevrons = false)
    addCrystal(206f, 118f, 5.4f, 0.9f, 0.26f, withChevrons = false)

    val dots = SNOW_DOTS.map { d ->
        CachedCircle(
            center = m.pt(d[0], d[1]),
            radius = m.s(d[2]),
            color = Color(0xFFF0F9FF).copy(alpha = d[3]),
        )
    }

    return {
        drawRect(brush = glow)
        for (line in crystalLines) {
            drawLine(line.color, line.start, line.end, strokeWidth = line.strokeWidth, cap = StrokeCap.Round)
        }
        for (chev in chevronPaths) {
            drawPath(chev.path, color = chev.color, style = chevStyle)
        }
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
    }
}

private fun CacheDrawScope.buildStormNight(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.STORM_NIGHT)
    val glowPrimary = radialGlowBrush(
        0.72f, 0.38f, 0.54f,
        0.0f to Color(0x578B5CF6),
        0.50f to Color(0x1F38BDF8),
        1.0f to Color.Transparent,
    )
    val glowDark = radialGlowBrush(
        0.88f, 0.08f, 0.48f,
        0.0f to Color(0xB8090514),
        1.0f to Color.Transparent,
    )
    val bolt = Path().apply {
        moveTo(m.x(242f), m.y(20f))
        lineTo(m.x(220f), m.y(74f))
        lineTo(m.x(240f), m.y(74f))
        lineTo(m.x(210f), m.y(154f))
    }
    val outerStroke = Stroke(width = m.s(11f), cap = StrokeCap.Round, join = StrokeJoin.Round)
    val midStroke = Stroke(width = m.s(4.5f), cap = StrokeCap.Round, join = StrokeJoin.Round)
    val coreStroke = Stroke(width = m.s(1.8f), cap = StrokeCap.Round, join = StrokeJoin.Round)
    val boltBrush = Brush.linearGradient(
        colorStops = arrayOf(
            0.0f to Color(0x1FE0E7FF),
            0.22f to Color(0xC7F5F3FF),
            0.72f to Color(0x7AA78BFA),
            1.0f to Color(0x0038BDF8),
        ),
        start = m.pt(242f, 20f),
        end = m.pt(210f, 154f),
    )

    return {
        drawRect(brush = glowPrimary)
        drawRect(brush = glowDark)
        drawPath(path = bolt, color = Color(0x1CA78BFA), style = outerStroke)
        drawPath(path = bolt, color = Color(0x33C4B5FD), style = midStroke)
        drawPath(path = bolt, brush = boltBrush, style = coreStroke)
    }
}

private fun CacheDrawScope.buildSunnyDay(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SUNNY_DAY)
    val glow = radialGlowBrush(
        0.76f, 0.38f, 0.64f,
        0.0f to Color(0x66FDE68A),
        0.30f to Color(0x38FBBF24),
        0.65f to Color(0x17FB923C),
        1.0f to Color.Transparent,
    )
    val rayStart = m.pt(238f, 66f)
    val rayEnd = m.pt(120f, 176f)
    val cachedRays = SUNNY_RAYS.map { (pts, alpha) ->
        val poly = Path().apply {
            moveTo(m.x(pts[0].first), m.y(pts[0].second))
            for (i in 1 until pts.size) {
                lineTo(m.x(pts[i].first), m.y(pts[i].second))
            }
            close()
        }
        val brush = Brush.linearGradient(
            colorStops = arrayOf(
                0.0f to Color(0xFFFEF08A).copy(alpha = 0.46f * alpha),
                1.0f to Color(0x00FB923C),
            ),
            start = rayStart,
            end = rayEnd,
        )
        CachedColoredPath(path = poly, brush = brush)
    }
    val sunCenter = m.pt(238f, 66f)
    val outerSunR = m.s(18f)
    val innerSunR = m.s(10f)

    return {
        drawRect(brush = glow)
        for (ray in cachedRays) {
            drawPath(path = ray.path, brush = ray.brush!!)
        }
        drawCircle(color = Color(0x33FEF08A), radius = outerSunR, center = sunCenter)
        drawCircle(color = Color(0x57FDE68A), radius = innerSunR, center = sunCenter)
    }
}

// --- Moments & Weekdays (8) ---

private fun CacheDrawScope.buildDriveGrid(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.DRIVE)
    val glow = radialGlowBrush(
        0.78f, 0.48f, 0.55f,
        0.0f to Color(0x2400FF9D),
        0.50f to Color(0x14FF00FF),
        1.0f to Color.Transparent,
    )
    val strokeW = (size.width / 400f).coerceAtLeast(1f)
    val lineColor = Color(0x2E33FFB0)
    val lines = listOf(
        CachedLine(m.pt(180f, 125f), m.pt(400f, 125f), strokeW, lineColor),
        CachedLine(m.pt(295f, 125f), m.pt(195f, 220f), strokeW, lineColor),
        CachedLine(m.pt(295f, 125f), m.pt(265f, 220f), strokeW, lineColor),
        CachedLine(m.pt(295f, 125f), m.pt(345f, 220f), strokeW, lineColor),
        CachedLine(m.pt(295f, 125f), m.pt(420f, 220f), strokeW, lineColor),
        CachedLine(m.pt(230f, 152f), m.pt(400f, 152f), strokeW, lineColor.copy(alpha = 0.18f * 0.55f)),
        CachedLine(m.pt(205f, 184f), m.pt(400f, 184f), strokeW, lineColor.copy(alpha = 0.18f * 0.40f)),
    )

    return {
        drawRect(brush = glow)
        for (line in lines) {
            drawLine(line.color, line.start, line.end, strokeWidth = line.strokeWidth)
        }
    }
}

private fun CacheDrawScope.buildEveningUnwind(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.EVENING_UNWIND)
    val glow = radialGlowBrush(
        0.78f, 0.46f, 0.58f,
        0.0f to Color(0x3DF97316),
        0.48f to Color(0x17E879F9),
        1.0f to Color.Transparent,
    )

    // Clean thin twilight crescent moon via circle subtraction (cached per size)
    val outer = Path().apply {
        addOval(Rect(center = m.pt(320f, 84f), radius = m.s(22f)))
    }
    val cutout = Path().apply {
        addOval(Rect(center = m.pt(329f, 76f), radius = m.s(19f)))
    }
    val crescent = Path().apply {
        op(outer, cutout, PathOperation.Difference)
    }

    val starPath = buildFourPointStarPath(m.pt(250f, 90.8f), m.s(4.8f), m.s(1.4f))
    val dots = listOf(
        CachedCircle(m.pt(268f, 54f), m.s(1.4f), Color(0x73FDE68A)),
        CachedCircle(m.pt(352f, 118f), m.s(1.2f), Color(0x59FDE68A)),
        CachedCircle(m.pt(284f, 126f), m.s(1.1f), Color(0x4DFDE68A)),
    )

    val bandH = m.sh(1.4f).coerceAtLeast(1f)
    val bandCorner = CornerRadius(m.s(1f), m.s(1f))
    val bands = EVENING_BANDS.map { b ->
        val x0 = m.x(b[0])
        val x1 = m.x(b[0] + b[2])
        val y0 = m.y(b[1])
        val alpha = b[3]
        CachedRect(
            topLeft = Offset(x0, y0),
            size = Size(x1 - x0, bandH),
            brush = Brush.horizontalGradient(
                colorStops = arrayOf(
                    0.0f to Color.Transparent,
                    0.55f to Color(0xFFFDBA74).copy(alpha = 0.22f * alpha),
                    1.0f to Color(0xFFF472B6).copy(alpha = 0.06f * alpha),
                ),
                startX = x0,
                endX = x1,
            ),
        )
    }

    return {
        drawRect(brush = glow)
        drawPath(path = crescent, color = Color(0x47FED7AA))
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
        drawPath(path = starPath, color = Color(0x61FDE68A))
        for (band in bands) {
            drawRoundRect(
                brush = band.brush!!,
                topLeft = band.topLeft,
                size = band.size,
                cornerRadius = bandCorner,
            )
        }
    }
}

private fun CacheDrawScope.buildFridayStarlight(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.FRIDAY_NIGHT)
    val glow = radialGlowBrush(
        0.84f, 0.32f, 0.60f,
        0.0f to Color(0x4DF472F4),
        0.52f to Color(0x1A38BDF8),
        1.0f to Color.Transparent,
    )
    val stars = listOf(
        CachedColoredPath(buildFourPointStarPath(m.pt(318f, 87f), m.s(11.0f), m.s(2.8f)), Color(0x9EF5D0FE)),
        CachedColoredPath(buildFourPointStarPath(m.pt(276f, 119f), m.s(7.0f), m.s(1.8f)), Color(0x7533FFB0)),
        CachedColoredPath(buildFourPointStarPath(m.pt(352f, 130f), m.s(6.0f), m.s(1.6f)), Color(0x85F472F4)),
        CachedColoredPath(buildFourPointStarPath(m.pt(286f, 62.1f), m.s(4.1f), m.s(1.1f)), Color(0x707DD3FC)),
    )
    val dots = listOf(
        CachedCircle(m.pt(338f, 60f), m.s(1.4f), Color(0x73FFFFFF)),
        CachedCircle(m.pt(308f, 142f), m.s(1.3f), Color(0x61F5D0FE)),
        CachedCircle(m.pt(252f, 86f), m.s(1.2f), Color(0x57F472F4)),
    )

    return {
        drawRect(brush = glow)
        for (star in stars) {
            drawPath(path = star.path, color = star.color)
        }
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
    }
}

private fun CacheDrawScope.buildLateNightSkyline(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.LATE_NIGHT)
    val glow = radialGlowBrush(
        0.78f, 0.48f, 0.58f,
        0.0f to Color(0x47A855F7),
        0.52f to Color(0x1A38BDF8),
        1.0f to Color.Transparent,
    )
    val baseStart = m.pt(48f, 106f)
    val baseEnd = m.pt(226f, 106f)
    val baseWidth = m.s(0.9f)

    val primaryWave = Path().apply {
        moveTo(m.x(24f), m.y(106f))
        cubicTo(m.x(64f), m.y(62f), m.x(108f), m.y(148f), m.x(152f), m.y(94f))
        cubicTo(m.x(182f), m.y(58f), m.x(208f), m.y(82f), m.x(232f), m.y(98f))
    }
    val secondaryWave = Path().apply {
        moveTo(m.x(38f), m.y(118f))
        cubicTo(m.x(80f), m.y(86f), m.x(122f), m.y(152f), m.x(168f), m.y(112f))
        cubicTo(m.x(196f), m.y(86f), m.x(214f), m.y(96f), m.x(232f), m.y(108f))
    }
    val haloStroke = Stroke(width = m.s(8f), cap = StrokeCap.Round)
    val primaryStroke = Stroke(width = m.s(1.6f), cap = StrokeCap.Round)
    val secondaryStroke = Stroke(width = m.s(1.0f), cap = StrokeCap.Round)
    val primaryBrush = Brush.horizontalGradient(
        colorStops = arrayOf(
            0.0f to Color(0x00C084FC),
            0.30f to Color(0xADD8B4FE),
            0.76f to Color(0x947DD3FC),
            1.0f to Color(0x2438BDF8),
        ),
        startX = m.x(24f),
        endX = m.x(232f),
    )
    val secondaryBrush = Brush.horizontalGradient(
        colorStops = arrayOf(
            0.0f to Color(0x00C084FC),
            0.30f to Color(0xFFD8B4FE).copy(alpha = 0.68f * 0.42f),
            0.76f to Color(0xFF7DD3FC).copy(alpha = 0.58f * 0.42f),
            1.0f to Color(0xFF38BDF8).copy(alpha = 0.14f * 0.42f),
        ),
        startX = m.x(38f),
        endX = m.x(232f),
    )
    val dots = listOf(
        CachedCircle(m.pt(116f, 56f), m.s(1.4f), Color(0x7AE9D5FF)),
        CachedCircle(m.pt(176f, 46f), m.s(1.8f), Color(0x8ABAE6FD)),
        CachedCircle(m.pt(204f, 138f), m.s(1.3f), Color(0x61D8B4FE)),
    )

    return {
        drawRect(brush = glow)
        drawLine(Color(0x1FD8B4FE), baseStart, baseEnd, strokeWidth = baseWidth)
        drawPath(path = primaryWave, color = Color(0x1FC084FC), style = haloStroke)
        drawPath(path = primaryWave, brush = primaryBrush, style = primaryStroke)
        drawPath(path = secondaryWave, brush = secondaryBrush, style = secondaryStroke)
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
    }
}

private fun CacheDrawScope.buildMorningSunrise(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.MORNING)
    val glow = radialGlowBrush(
        0.80f, 0.62f, 0.58f,
        0.0f to Color(0x47FB923C),
        0.48f to Color(0x1FFDA4AF),
        1.0f to Color.Transparent,
    )
    val sunTopLeft = m.pt(152f - 42f, 114f - 42f)
    val sunSize = Size(m.sw(84f), m.sh(84f))
    val sunBrush = Brush.verticalGradient(
        colorStops = arrayOf(
            0.0f to Color(0x61FDE68A),
            1.0f to Color(0x14FDA4AF),
        ),
        startY = m.y(72f),
        endY = m.y(114f),
    )
    val outerArcTopLeft = m.pt(152f - 56f, 114f - 56f)
    val outerArcSize = Size(m.sw(112f), m.sh(112f))
    val outerArcStroke = Stroke(width = m.s(1f))

    val bars = MORNING_BARS.map { b ->
        val x0 = m.x(b[0])
        val x1 = m.x(b[0] + b[2])
        val op = b[5]
        val cr = m.s(b[4])
        CachedRect(
            topLeft = Offset(x0, m.y(b[1])),
            size = Size(x1 - x0, m.sh(b[3]).coerceAtLeast(1f)),
            brush = Brush.horizontalGradient(
                colorStops = arrayOf(
                    0.0f to Color(0x00FDA4AF),
                    0.50f to Color(0xFFFED7AA).copy(alpha = 0.46f * op),
                    1.0f to Color(0xFFFDA4AF).copy(alpha = 0.08f * op),
                ),
                startX = x0,
                endX = x1,
            ),
            cornerRadius = CornerRadius(cr, cr),
        )
    }

    return {
        drawRect(brush = glow)
        drawArc(
            brush = sunBrush,
            startAngle = 180f,
            sweepAngle = 180f,
            useCenter = true,
            topLeft = sunTopLeft,
            size = sunSize,
        )
        drawArc(
            color = Color(0x3DFDA4AF),
            startAngle = 180f,
            sweepAngle = 180f,
            useCenter = false,
            topLeft = outerArcTopLeft,
            size = outerArcSize,
            style = outerArcStroke,
        )
        for (bar in bars) {
            drawRoundRect(
                brush = bar.brush!!,
                topLeft = bar.topLeft,
                size = bar.size,
                cornerRadius = bar.cornerRadius,
            )
        }
    }
}

private fun CacheDrawScope.buildFocusGrid(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.FOCUS)
    val glow = radialGlowBrush(
        0.78f, 0.50f, 0.56f,
        0.0f to Color(0x3D2DD4BF),
        0.55f to Color(0x1438BDF8),
        1.0f to Color.Transparent,
    )
    val dotR = m.s(1.1f)
    val dots = FOCUS_CALIBRATION_DOTS.map { (vx, vy) -> m.pt(vx, vy) }
    val center = m.pt(150f, 100f)
    val outerR = m.s(48f)
    val innerR = m.s(26f)
    val outerStroke = Stroke(width = m.s(1f))
    val innerStroke = Stroke(width = m.s(1.1f))
    val tickW = m.s(1f)
    val ticks = listOf(
        CachedLine(m.pt(150f, 38f), m.pt(150f, 46f), tickW, Color(0x575EEAD4)),
        CachedLine(m.pt(150f, 154f), m.pt(150f, 162f), tickW, Color(0x575EEAD4)),
        CachedLine(m.pt(88f, 100f), m.pt(96f, 100f), tickW, Color(0x575EEAD4)),
        CachedLine(m.pt(204f, 100f), m.pt(212f, 100f), tickW, Color(0x575EEAD4)),
    )
    val coreR = m.s(4.2f)

    return {
        drawRect(brush = glow)
        for (pt in dots) {
            drawCircle(color = Color(0x2E5EEAD4), radius = dotR, center = pt)
        }
        drawCircle(color = Color(0x385EEAD4), radius = outerR, center = center, style = outerStroke)
        drawCircle(color = Color(0x5C5EEAD4), radius = innerR, center = center, style = innerStroke)
        for (tick in ticks) {
            drawLine(tick.color, tick.start, tick.end, strokeWidth = tick.strokeWidth, cap = StrokeCap.Round)
        }
        drawCircle(color = Color(0xBD5EEAD4), radius = coreR, center = center)
    }
}

private fun CacheDrawScope.buildDinnerGroove(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.DINNER_GROOVE)
    val glow = radialGlowBrush(
        0.80f, 0.58f, 0.58f,
        0.0f to Color(0x47F59E0B),
        0.52f to Color(0x1CFB7185),
        1.0f to Color.Transparent,
    )
    val pillars = DINNER_PILLARS.map { p ->
        val topLeft = m.pt(p[0], p[1])
        val sz = Size(m.sw(p[2]), m.sh(p[3]))
        val cr = m.s(p[4])
        val isWarm = p[5] > 0.5f
        val op = p[6]
        val brush = if (isWarm) {
            Brush.verticalGradient(
                colorStops = arrayOf(
                    0.0f to Color(0x00FBBF24),
                    0.25f to Color(0xFFFBBF24).copy(alpha = 0.14f * op),
                    0.70f to Color(0xFFFDE68A).copy(alpha = 0.36f * op),
                    1.0f to Color(0xFFFB923C).copy(alpha = 0.04f * op),
                ),
                startY = topLeft.y,
                endY = topLeft.y + sz.height,
            )
        } else {
            Brush.verticalGradient(
                colorStops = arrayOf(
                    0.0f to Color(0x00FDA4AF),
                    0.28f to Color(0xFFFDA4AF).copy(alpha = 0.14f * op),
                    0.74f to Color(0xFFFBBF24).copy(alpha = 0.28f * op),
                    1.0f to Color(0xFFF59E0B).copy(alpha = 0.02f * op),
                ),
                startY = topLeft.y,
                endY = topLeft.y + sz.height,
            )
        }
        CachedRect(topLeft = topLeft, size = sz, brush = brush, cornerRadius = CornerRadius(cr, cr))
    }
    val flames = listOf(
        CachedCircle(m.pt(134f, 78f), m.s(2.0f), Color(0x9EFEF3C7)),
        CachedCircle(m.pt(157f, 64f), m.s(2.4f), Color(0xC2FEF3C7)),
        CachedCircle(m.pt(179.5f, 86f), m.s(1.6f), Color(0x8CFDE68A)),
    )
    val tableStart = m.pt(86f, 156f)
    val tableEnd = m.pt(218f, 156f)
    val tableWidth = m.s(1f)

    return {
        drawRect(brush = glow)
        for (pillar in pillars) {
            drawRoundRect(
                brush = pillar.brush!!,
                topLeft = pillar.topLeft,
                size = pillar.size,
                cornerRadius = pillar.cornerRadius,
            )
        }
        for (flame in flames) {
            drawCircle(color = flame.color, radius = flame.radius, center = flame.center)
        }
        drawLine(Color(0x38FBBF24), tableStart, tableEnd, strokeWidth = tableWidth, cap = StrokeCap.Round)
    }
}

private fun CacheDrawScope.buildSundaySteam(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SUNDAY_MORNING)
    val glow = radialGlowBrush(
        0.80f, 0.38f, 0.60f,
        0.0f to Color(0x42F59E0B),
        0.55f to Color(0x1AFED7AA),
        1.0f to Color.Transparent,
    )
    val slats = SUNDAY_SLATS.map { (pts, op) ->
        var minX = Float.POSITIVE_INFINITY
        var maxX = Float.NEGATIVE_INFINITY
        var minY = Float.POSITIVE_INFINITY
        var maxY = Float.NEGATIVE_INFINITY
        val poly = Path().apply {
            pts.forEachIndexed { idx, (vx, vy) ->
                val px = m.x(vx)
                val py = m.y(vy)
                if (px < minX) minX = px
                if (px > maxX) maxX = px
                if (py < minY) minY = py
                if (py > maxY) maxY = py
                if (idx == 0) moveTo(px, py) else lineTo(px, py)
            }
            close()
        }
        val brush = Brush.linearGradient(
            colorStops = arrayOf(
                0.0f to Color(0xFFFEF3C7).copy(alpha = 0.34f * op),
                0.55f to Color(0xFFFDBA74).copy(alpha = 0.14f * op),
                1.0f to Color(0x00FDBA74),
            ),
            start = Offset(maxX, minY),
            end = Offset(minX, maxY),
        )
        CachedColoredPath(path = poly, brush = brush)
    }
    val motes = listOf(
        CachedCircle(m.pt(142f, 72f), m.s(1.8f), Color(0x8FFEF3C7)),
        CachedCircle(m.pt(174f, 108f), m.s(1.4f), Color(0x75FED7AA)),
        CachedCircle(m.pt(122f, 132f), m.s(1.2f), Color(0x61FDE68A)),
    )

    return {
        drawRect(brush = glow)
        for (slat in slats) {
            drawPath(path = slat.path, brush = slat.brush!!)
        }
        for (mote in motes) {
            drawCircle(color = mote.color, radius = mote.radius, center = mote.center)
        }
    }
}

// --- Collection & Decades (5, 260x260 MEET mode + full-bleed wash) ---

private fun CacheDrawScope.buildForgottenGemsVinyl(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.FORGOTTEN_GEMS)
    val glow = radialGlowBrush(
        0.84f, 0.32f, 0.64f,
        0.0f to Color(0x38FFD166),
        1.0f to Color.Transparent,
    )
    val center = m.pt(222f, 86f)
    val grooves = FORGOTTEN_GEMS_GROOVES.map { g ->
        CachedCircle(
            center = center,
            radius = m.s(g[0]),
            color = Color(0xFFFFD166).copy(alpha = g[1]),
            stroke = Stroke(width = m.s(g[2])),
        )
    }
    val labelR = m.s(10f)
    val labelStroke = Stroke(width = m.s(1f))
    val spindleR = m.s(2.8f)

    return {
        drawRect(brush = glow)
        for (groove in grooves) {
            drawCircle(color = groove.color, radius = groove.radius, center = groove.center, style = groove.stroke!!)
        }
        drawCircle(color = Color(0x38FFD166), radius = labelR, center = center)
        drawCircle(color = Color(0x59FFD166), radius = labelR, center = center, style = labelStroke)
        drawCircle(color = Color(0xFF1C1028), radius = spindleR, center = center)
    }
}

private fun CacheDrawScope.buildFreshArrivalsWaveform(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.FRESH_ARRIVALS)
    val glow = radialGlowBrush(
        0.80f, 0.32f, 0.64f,
        0.0f to Color(0x3800FF9D),
        1.0f to Color.Transparent,
    )
    val barColor = Color(0x4733FFB0)
    val cr = CornerRadius(m.s(2f), m.s(2f))
    val barW = m.sw(4f)
    val bars = FRESH_ARRIVALS_BARS.map { b ->
        CachedRect(m.pt(b[0], b[1]), Size(barW, m.sh(b[2])), color = barColor)
    }

    return {
        drawRect(brush = glow)
        for (bar in bars) {
            drawRoundRect(color = bar.color, topLeft = bar.topLeft, size = bar.size, cornerRadius = cr)
        }
    }
}

private fun CacheDrawScope.buildDecade80s(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.DECADE_80S)
    val wash = Brush.linearGradient(
        colors = listOf(Color(0x38FF00FF), Color(0x2922D3EE)),
        start = Offset.Zero,
        end = Offset(size.width, size.height),
    )
    val lineColor = Color(0x3822D3EE)
    val strokeW = m.s(1f)
    val lines = DECADE_80S_LINE_YS.map { ly ->
        CachedLine(m.pt(0f, ly), m.pt(260f, ly), strokeW, lineColor)
    }

    return {
        drawRect(brush = wash)
        for (line in lines) {
            drawLine(line.color, line.start, line.end, strokeWidth = line.strokeWidth)
        }
    }
}

private fun CacheDrawScope.buildDecade90s(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.DECADE_90S)
    val dotColor = Color(0x29FDA4AF)
    val r = m.s(1.15f)
    val dots = buildList {
        var vx = 100f
        while (vx <= 258f) {
            var vy = 4f
            while (vy <= 152f) {
                add(m.pt(vx, vy))
                vy += 11f
            }
            vx += 11f
        }
    }

    return {
        for (pt in dots) {
            drawCircle(color = dotColor, radius = r, center = pt)
        }
    }
}

private fun CacheDrawScope.buildDecade2000s(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.DECADE_2000S)
    val triangle = Path().apply {
        moveTo(m.x(88f), m.y(0f))
        lineTo(m.x(260f), m.y(0f))
        lineTo(m.x(260f), m.y(172f))
        close()
    }
    val brush = Brush.linearGradient(
        colorStops = arrayOf(
            0.0f to Color(0x3D38BDF8),
            0.52f to Color(0x1FC084FC),
            1.0f to Color.Transparent,
        ),
        start = m.pt(88f, 0f),
        end = m.pt(260f, 172f),
    )

    return {
        drawPath(path = triangle, brush = brush)
    }
}

// --- Rolling (3, 260x260 MEET mode + full-bleed wash) ---

private fun CacheDrawScope.buildRollingComfort(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.ROLLING_COMFORT)
    val glow = radialGlowBrush(
        0.80f, 0.34f, 0.62f,
        0.0f to Color(0x3D33FFB0),
        1.0f to Color.Transparent,
    )
    val c = m.pt(194f, 94f)
    val ringR = m.s(26f)
    val ringStroke = Stroke(width = m.s(1f))
    val coreR = m.s(3.5f)
    val waves = listOf(
        CachedColoredPath(
            path = Path().apply {
                moveTo(m.x(96f), m.y(86f))
                quadraticTo(m.x(146f), m.y(60f), m.x(196f), m.y(86f))
                quadraticTo(m.x(246f), m.y(112f), m.x(260f), m.y(86f))
            },
            strokeColor = Color(0x5733FFB0),
            stroke = Stroke(width = m.s(1.3f), cap = StrokeCap.Round),
        ),
        CachedColoredPath(
            path = Path().apply {
                moveTo(m.x(84f), m.y(102f))
                quadraticTo(m.x(140f), m.y(76f), m.x(196f), m.y(102f))
                quadraticTo(m.x(252f), m.y(128f), m.x(260f), m.y(102f))
            },
            strokeColor = Color(0x3D33FFB0),
            stroke = Stroke(width = m.s(1.1f), cap = StrokeCap.Round),
        ),
        CachedColoredPath(
            path = Path().apply {
                moveTo(m.x(108f), m.y(118f))
                quadraticTo(m.x(158f), m.y(94f), m.x(208f), m.y(118f))
                quadraticTo(m.x(258f), m.y(142f), m.x(260f), m.y(118f))
            },
            strokeColor = Color(0x2633FFB0),
            stroke = Stroke(width = m.s(1.0f), cap = StrokeCap.Round),
        ),
    )

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x2E33FFB0), radius = ringR, center = c, style = ringStroke)
        for (wave in waves) {
            drawPath(path = wave.path, color = wave.strokeColor, style = wave.stroke!!)
        }
        drawCircle(color = Color(0xAD33FFB0), radius = coreR, center = c)
    }
}

private fun CacheDrawScope.buildRollingRadar(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.ROLLING_RADAR)
    val glow = radialGlowBrush(
        0.78f, 0.34f, 0.62f,
        0.0f to Color(0x3DF472F4),
        0.55f to Color(0x1A33FFB0),
        1.0f to Color.Transparent,
    )
    val c = m.pt(204f, 94f)
    val wedgeTopLeft = m.pt(204f - 46f, 94f - 46f)
    val wedgeSize = Size(m.sw(92f), m.sh(92f))
    val rings = listOf(
        CachedCircle(c, m.s(22f), Color(0x4DF472F4), stroke = Stroke(width = m.s(1.1f))),
        CachedCircle(
            c,
            m.s(42f),
            Color(0x33F472F4),
            stroke = Stroke(
                width = m.s(1f),
                pathEffect = PathEffect.dashPathEffect(floatArrayOf(m.s(18f), m.s(10f))),
            ),
        ),
        CachedCircle(c, m.s(64f), Color(0x21F472F4), stroke = Stroke(width = m.s(0.9f))),
    )
    val sweepEnd = m.pt(242f, 68f)
    val sweepWidth = m.s(1.2f)
    val coreR = m.s(3.2f)
    val blipCenter = m.pt(234f, 74f)
    val blipR = m.s(2.0f)

    return {
        drawRect(brush = glow)
        drawArc(
            color = Color(0x1433FFB0),
            startAngle = -34.4f,
            sweepAngle = 49.7f,
            useCenter = true,
            topLeft = wedgeTopLeft,
            size = wedgeSize,
        )
        for (ring in rings) {
            drawCircle(color = ring.color, radius = ring.radius, center = ring.center, style = ring.stroke!!)
        }
        drawLine(Color(0x7533FFB0), c, sweepEnd, strokeWidth = sweepWidth, cap = StrokeCap.Round)
        drawCircle(color = Color(0xB333FFB0), radius = coreR, center = c)
        drawCircle(color = Color(0x94F472F4), radius = blipR, center = blipCenter)
    }
}

private fun CacheDrawScope.buildRollingDeepCuts(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.ROLLING_DEEP_CUTS)
    val glow = radialGlowBrush(
        0.80f, 0.34f, 0.60f,
        0.0f to Color(0x42E879F9),
        0.55f to Color(0x1A6366F1),
        1.0f to Color.Transparent,
    )
    val c = m.pt(202f, 94f)
    val outerR = m.s(48f)
    val innerR = m.s(28f)
    val outerStroke = Stroke(
        width = m.s(1f),
        pathEffect = PathEffect.dashPathEffect(floatArrayOf(m.s(6f), m.s(6f))),
    )
    val innerStroke = Stroke(width = m.s(1f))
    val tickW = m.s(1f)
    val ticks = listOf(
        CachedLine(m.pt(202f, 38f), m.pt(202f, 44f), tickW, Color(0x57F0ABFC)),
        CachedLine(m.pt(202f, 144f), m.pt(202f, 150f), tickW, Color(0x57F0ABFC)),
        CachedLine(m.pt(146f, 94f), m.pt(152f, 94f), tickW, Color(0x57F0ABFC)),
        CachedLine(m.pt(252f, 94f), m.pt(258f, 94f), tickW, Color(0x57F0ABFC)),
    )
    val starPath = buildSmoothFourPointStarPath(c, m.s(30f), m.s(2f))

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x33F0ABFC), radius = outerR, center = c, style = outerStroke)
        drawCircle(color = Color(0x47F0ABFC), radius = innerR, center = c, style = innerStroke)
        for (tick in ticks) {
            drawLine(tick.color, tick.start, tick.end, strokeWidth = tick.strokeWidth, cap = StrokeCap.Round)
        }
        drawPath(path = starPath, color = Color(0x6BF0ABFC))
    }
}

// --- Seasonals (13, 240x200 MEET mode + full-bleed glow) ---

private fun CacheDrawScope.buildSeasonalWinterHoliday(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_WINTER_HOLIDAY)
    val glow = radialGlowBrush(
        0.78f, 0.44f, 0.58f,
        0.0f to Color(0x47FFD166),
        0.52f to Color(0x1C34D399),
        1.0f to Color.Transparent,
    )
    fun bokehCircle(vx: Float, vy: Float, vr: Float, op: Float): CachedCircle {
        val center = m.pt(vx, vy)
        val radius = m.s(vr)
        val brush = Brush.radialGradient(
            colorStops = arrayOf(
                0.0f to Color(0xFFFEF08A).copy(alpha = 0.34f * op),
                0.60f to Color(0xFFFFD166).copy(alpha = 0.12f * op),
                1.0f to Color(0x00FFD166),
            ),
            center = center,
            radius = radius.coerceAtLeast(1f),
        )
        return CachedCircle(center = center, radius = radius, brush = brush)
    }
    val bokehs = listOf(
        bokehCircle(110f, 126f, 22f, 0.62f),
        bokehCircle(194f, 118f, 15f, 0.52f),
        bokehCircle(156f, 82f, 34f, 0.85f),
    )
    val vTopLeft = m.pt(155.3f, 26f)
    val vSize = Size(m.sw(1.4f).coerceAtLeast(1f), m.sh(112f))
    val vCr = CornerRadius(m.s(0.7f), m.s(0.7f))
    val vBrush = Brush.verticalGradient(
        colorStops = arrayOf(
            0.0f to Color(0x00FEF08A),
            0.50f to Color(0xD9FFFBEB),
            1.0f to Color(0x00FEF08A),
        ),
        startY = vTopLeft.y,
        endY = vTopLeft.y + vSize.height,
    )
    val hTopLeft = m.pt(106f, 81.3f)
    val hSize = Size(m.sw(100f), m.sh(1.4f).coerceAtLeast(1f))
    val hBrush = Brush.horizontalGradient(
        colorStops = arrayOf(
            0.0f to Color(0x00FEF08A),
            0.50f to Color(0xD9FFFBEB),
            1.0f to Color(0x00FEF08A),
        ),
        startX = hTopLeft.x,
        endX = hTopLeft.x + hSize.width,
    )
    val diagW = m.s(0.9f)
    val diagAStart = m.pt(136f, 62f)
    val diagAEnd = m.pt(176f, 102f)
    val diagBStart = m.pt(176f, 62f)
    val diagBEnd = m.pt(136f, 102f)
    val dots = listOf(
        CachedCircle(m.pt(156f, 82f), m.s(3.2f), Color(0xF0FFFBEB)),
        CachedCircle(m.pt(102f, 54f), m.s(1.5f), Color(0x856EE7B7)),
        CachedCircle(m.pt(204f, 140f), m.s(1.6f), Color(0x8AFFD166)),
    )

    return {
        drawRect(brush = glow)
        for (bk in bokehs) {
            drawCircle(brush = bk.brush!!, radius = bk.radius, center = bk.center)
        }
        drawRoundRect(brush = vBrush, topLeft = vTopLeft, size = vSize, cornerRadius = vCr)
        drawRoundRect(brush = hBrush, topLeft = hTopLeft, size = hSize, cornerRadius = vCr)
        drawLine(Color(0x3DFEF08A), diagAStart, diagAEnd, strokeWidth = diagW, cap = StrokeCap.Round)
        drawLine(Color(0x3DFEF08A), diagBStart, diagBEnd, strokeWidth = diagW, cap = StrokeCap.Round)
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
    }
}

private fun CacheDrawScope.buildSeasonalNewYearsEve(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_NEW_YEARS_EVE)
    val glow = radialGlowBrush(
        0.78f, 0.48f, 0.58f,
        0.0f to Color(0x47FDE047),
        0.52f to Color(0x1FF472F4),
        1.0f to Color.Transparent,
    )
    fun bokehCircle(vx: Float, vy: Float, vr: Float, op: Float): CachedCircle {
        val center = m.pt(vx, vy)
        val radius = m.s(vr)
        val brush = Brush.radialGradient(
            colorStops = arrayOf(
                0.0f to Color(0xFFFEF08A).copy(alpha = 0.42f * op),
                0.65f to Color(0xFFFDE047).copy(alpha = 0.12f * op),
                1.0f to Color(0x00FDE047),
            ),
            center = center,
            radius = radius.coerceAtLeast(1f),
        )
        return CachedCircle(center = center, radius = radius, brush = brush)
    }
    val bokehs = listOf(
        bokehCircle(152f, 98f, 34f, 0.75f),
        bokehCircle(106f, 136f, 14f, 0.52f),
        bokehCircle(194f, 138f, 10f, 0.42f),
    )
    val rays = NYE_RAYS.map { r ->
        CachedLine(
            start = m.pt(r[0], r[1]),
            end = m.pt(r[2], r[3]),
            strokeWidth = m.s(r[4]),
            color = Color(0xFFFEF08A).copy(alpha = r[5]),
        )
    }
    val dots = listOf(
        CachedCircle(m.pt(152f, 98f), m.s(3.4f), Color(0xE0FFFBEB)),
        CachedCircle(m.pt(152f, 40f), m.s(1.6f), Color(0xB3FDE047)),
        CachedCircle(m.pt(208f, 79f), m.s(1.6f), Color(0xA8F472F4)),
        CachedCircle(m.pt(104f, 129f), m.s(1.5f), Color(0xA3FEF08A)),
        CachedCircle(m.pt(114f, 60f), m.s(1.4f), Color(0x94F472F4)),
    )

    return {
        drawRect(brush = glow)
        for (bk in bokehs) {
            drawCircle(brush = bk.brush!!, radius = bk.radius, center = bk.center)
        }
        for (ray in rays) {
            drawLine(ray.color, ray.start, ray.end, strokeWidth = ray.strokeWidth, cap = StrokeCap.Round)
        }
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
    }
}

private fun CacheDrawScope.buildSeasonalCozyWinter(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_COZY_WINTER)
    val glow = radialGlowBrush(
        0.78f, 0.66f, 0.60f,
        0.0f to Color(0x4DFB923C),
        0.50f to Color(0x1A93C5FD),
        1.0f to Color.Transparent,
    )
    val bands = COZY_WINTER_BANDS.map { b ->
        val x0 = m.x(b[0])
        val x1 = m.x(b[0] + b[2])
        val op = b[5]
        val cr = m.s(b[4])
        CachedRect(
            topLeft = Offset(x0, m.y(b[1])),
            size = Size(x1 - x0, m.sh(b[3]).coerceAtLeast(1f)),
            brush = Brush.horizontalGradient(
                colorStops = arrayOf(
                    0.0f to Color(0x00FB923C),
                    0.52f to Color(0xFFFDBA74).copy(alpha = 0.46f * op),
                    1.0f to Color(0xFFFDE68A).copy(alpha = 0.06f * op),
                ),
                startX = x0,
                endX = x1,
            ),
            cornerRadius = CornerRadius(cr, cr),
        )
    }
    val specks = listOf(
        CachedCircle(m.pt(116f, 62f), m.s(1.6f), Color(0x75BAE6FD)),
        CachedCircle(m.pt(152f, 48f), m.s(2.2f), Color(0x6BE0F2FE)),
        CachedCircle(m.pt(184f, 70f), m.s(1.4f), Color(0x7ABAE6FD)),
        CachedCircle(m.pt(138f, 86f), m.s(1.3f), Color(0x70FED7AA)),
    )

    return {
        drawRect(brush = glow)
        for (band in bands) {
            drawRoundRect(
                brush = band.brush!!,
                topLeft = band.topLeft,
                size = band.size,
                cornerRadius = band.cornerRadius,
            )
        }
        for (speck in specks) {
            drawCircle(color = speck.color, radius = speck.radius, center = speck.center)
        }
    }
}

private fun CacheDrawScope.buildSeasonalLunarNewYear(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_LUNAR_NEW_YEAR)
    val glow = radialGlowBrush(
        0.78f, 0.50f, 0.58f,
        0.0f to Color(0x47FB7185),
        0.52f to Color(0x24FBBF24),
        1.0f to Color.Transparent,
    )
    val c = m.pt(154f, 100f)
    val discR = m.s(34f)
    val discBrush = Brush.radialGradient(
        colorStops = arrayOf(
            0.0f to Color(0x6BFDE68A),
            0.72f to Color(0x29FBBF24),
            1.0f to Color(0x0AFB7185),
        ),
        center = c,
        radius = discR.coerceAtLeast(1f),
    )
    val gateR = m.s(48f)
    val gateStroke = Stroke(width = m.s(1.2f))
    val sideTopLeft = m.pt(154f - 64f, 100f - 64f)
    val sideSize = Size(m.sw(128f), m.sh(128f))
    val sideStroke = Stroke(width = m.s(1f), cap = StrokeCap.Round)
    val topCordStart = m.pt(154f, 22f)
    val topCordEnd = m.pt(154f, 42f)
    val botCordStart = m.pt(154f, 158f)
    val botCordEnd = m.pt(154f, 178f)
    val cordWidth = m.s(1.1f)
    val tasselR = m.s(2.0f)

    return {
        drawRect(brush = glow)
        drawCircle(brush = discBrush, radius = discR, center = c)
        drawCircle(color = Color(0x61FBBF24), radius = gateR, center = c, style = gateStroke)
        drawArc(
            color = Color(0x38FBBF24),
            startAngle = 136.3f,
            sweepAngle = 87.4f,
            useCenter = false,
            topLeft = sideTopLeft,
            size = sideSize,
            style = sideStroke,
        )
        drawArc(
            color = Color(0x38FBBF24),
            startAngle = -43.7f,
            sweepAngle = 87.4f,
            useCenter = false,
            topLeft = sideTopLeft,
            size = sideSize,
            style = sideStroke,
        )
        drawLine(Color(0x61FBBF24), topCordStart, topCordEnd, strokeWidth = cordWidth, cap = StrokeCap.Round)
        drawLine(Color(0x6BFDA4AF), botCordStart, botCordEnd, strokeWidth = cordWidth, cap = StrokeCap.Round)
        drawCircle(color = Color(0xADFDA4AF), radius = tasselR, center = botCordEnd)
    }
}

private fun CacheDrawScope.buildSeasonalValentines(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_VALENTINES)
    val glow = radialGlowBrush(
        0.78f, 0.50f, 0.56f,
        0.0f to Color(0x42F472B6),
        0.54f to Color(0x1AC084FC),
        1.0f to Color.Transparent,
    )
    val orbACenter = m.pt(136f, 108f)
    val orbAR = m.s(48f)
    val orbABrush = Brush.radialGradient(
        colorStops = arrayOf(
            0.0f to Color(0x70FBCFE8),
            0.58f to Color(0x38F472B6),
            1.0f to Color(0x00F472B6),
        ),
        center = m.pt(124.48f, 96.48f),
        radius = m.s(59.52f).coerceAtLeast(1f),
    )
    val orbBCenter = m.pt(174f, 92f)
    val orbBR = m.s(42f)
    val orbBBrush = Brush.radialGradient(
        colorStops = arrayOf(
            0.0f to Color(0x57E9D5FF),
            0.58f to Color(0x29C084FC),
            1.0f to Color(0x00C084FC),
        ),
        center = m.pt(184.08f, 100.4f),
        radius = m.s(52.08f).coerceAtLeast(1f),
    )
    val rimTopLeft = m.pt(136.14f - 38f, 107.54f - 38f)
    val rimSize = Size(m.sw(76f), m.sh(76f))
    val rimStroke = Stroke(width = m.s(1.1f), cap = StrokeCap.Round)
    val dots = listOf(
        CachedCircle(m.pt(198f, 136f), m.s(1.6f), Color(0x85FBCFE8)),
        CachedCircle(m.pt(98f, 62f), m.s(1.4f), Color(0x75F472B6)),
    )

    return {
        drawRect(brush = glow)
        drawCircle(brush = orbABrush, radius = orbAR, center = orbACenter)
        drawCircle(brush = orbBBrush, radius = orbBR, center = orbBCenter)
        drawArc(
            color = Color(0x5CFBCFE8),
            startAngle = -137.1f,
            sweepAngle = 75.0f,
            useCenter = false,
            topLeft = rimTopLeft,
            size = rimSize,
            style = rimStroke,
        )
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
    }
}

private fun CacheDrawScope.buildSeasonalCarnival(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_CARNIVAL)
    val glow = radialGlowBrush(
        0.80f, 0.46f, 0.58f,
        0.0f to Color(0x3DFDE047),
        0.48f to Color(0x1F33FFB0),
        1.0f to Color.Transparent,
    )
    val ribbons = listOf(
        CachedColoredPath(
            path = Path().apply {
                moveTo(m.x(48f), m.y(154f))
                cubicTo(m.x(98f), m.y(102f), m.x(152f), m.y(128f), m.x(226f), m.y(48f))
            },
            strokeColor = Color(0x70FDE047),
            stroke = Stroke(width = m.s(1.5f), cap = StrokeCap.Round),
        ),
        CachedColoredPath(
            path = Path().apply {
                moveTo(m.x(62f), m.y(168f))
                cubicTo(m.x(112f), m.y(118f), m.x(166f), m.y(142f), m.x(232f), m.y(68f))
            },
            strokeColor = Color(0x5C33FFB0),
            stroke = Stroke(width = m.s(1.3f), cap = StrokeCap.Round),
        ),
        CachedColoredPath(
            path = Path().apply {
                moveTo(m.x(78f), m.y(180f))
                cubicTo(m.x(126f), m.y(134f), m.x(178f), m.y(156f), m.x(234f), m.y(90f))
            },
            strokeColor = Color(0x47F472F4),
            stroke = Stroke(width = m.s(1.1f), cap = StrokeCap.Round),
        ),
    )
    fun diamond(cx: Float, cy: Float, rx: Float, ry: Float, col: Color) = CachedColoredPath(
        path = Path().apply {
            moveTo(m.x(cx), m.y(cy - ry))
            lineTo(m.x(cx + rx), m.y(cy))
            lineTo(m.x(cx), m.y(cy + ry))
            lineTo(m.x(cx - rx), m.y(cy))
            close()
        },
        color = col,
    )
    val diamonds = listOf(
        diamond(142f, 64f, 7f, 8f, Color(0x8FFDE047)),
        diamond(106f, 102f, 5f, 6f, Color(0x85F472F4)),
        diamond(192f, 133f, 6f, 7f, Color(0x8033FFB0)),
    )

    return {
        drawRect(brush = glow)
        for (ribbon in ribbons) {
            drawPath(path = ribbon.path, color = ribbon.strokeColor, style = ribbon.stroke!!)
        }
        for (d in diamonds) {
            drawPath(path = d.path, color = d.color)
        }
    }
}

private fun CacheDrawScope.buildSeasonalCelticSpring(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_CELTIC_SPRING)
    val glow = radialGlowBrush(
        0.78f, 0.50f, 0.56f,
        0.0f to Color(0x4234D399),
        0.55f to Color(0x1410B981),
        1.0f to Color.Transparent,
    )
    val c = m.pt(154f, 100f)
    val outerR = m.s(56f)
    val innerR = m.s(26f)
    val outerStroke = Stroke(width = m.s(1f))
    val innerStroke = Stroke(width = m.s(1.1f))
    val leafStroke = Stroke(width = m.s(1.35f), join = StrokeJoin.Round)
    val leaves = listOf(
        Path().apply {
            moveTo(m.x(154f), m.y(100f))
            quadraticTo(m.x(130f), m.y(74f), m.x(154f), m.y(48f))
            quadraticTo(m.x(178f), m.y(74f), m.x(154f), m.y(100f))
            close()
        },
        Path().apply {
            moveTo(m.x(154f), m.y(100f))
            quadraticTo(m.x(188.5f), m.y(92.2f), m.x(199f), m.y(126f))
            quadraticTo(m.x(164.5f), m.y(133.8f), m.x(154f), m.y(100f))
            close()
        },
        Path().apply {
            moveTo(m.x(154f), m.y(100f))
            quadraticTo(m.x(143.5f), m.y(133.8f), m.x(109f), m.y(126f))
            quadraticTo(m.x(119.5f), m.y(92.2f), m.x(154f), m.y(100f))
            close()
        },
    )
    val coreR = m.s(3.0f)

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x2E6EE7B7), radius = outerR, center = c, style = outerStroke)
        drawCircle(color = Color(0x4234D399), radius = innerR, center = c, style = innerStroke)
        for (leaf in leaves) {
            drawPath(path = leaf, color = Color(0x0F34D399))
            drawPath(path = leaf, color = Color(0x756EE7B7), style = leafStroke)
        }
        drawCircle(color = Color(0xB8A7F3D0), radius = coreR, center = c)
    }
}

private fun CacheDrawScope.buildSeasonalSpring(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_SPRING)
    val glow = radialGlowBrush(
        0.78f, 0.48f, 0.58f,
        0.0f to Color(0x3D6EE7B7),
        0.52f to Color(0x1CF9A8D4),
        1.0f to Color.Transparent,
    )
    fun buildPetal(deg: Double, ctrlX: Float, ctrlY: Float, tipY: Float): Path {
        val (x0, y0) = rotatedOffset(154f, 100f, 0f, -8f, deg)
        val (lx, ly) = rotatedOffset(154f, 100f, -ctrlX, ctrlY, deg)
        val (tx, ty) = rotatedOffset(154f, 100f, 0f, tipY, deg)
        val (rx, ry) = rotatedOffset(154f, 100f, ctrlX, ctrlY, deg)
        return Path().apply {
            moveTo(m.x(x0), m.y(y0))
            quadraticTo(m.x(lx), m.y(ly), m.x(tx), m.y(ty))
            quadraticTo(m.x(rx), m.y(ry), m.x(x0), m.y(y0))
            close()
        }
    }
    val primaryPetals = SPRING_PRIMARY_ANGLES.map { deg -> buildPetal(deg, 12f, -26f, -46f) }
    val altPetals = SPRING_ALT_ANGLES.map { deg -> buildPetal(deg, 11f, -24f, -42f) }
    val primaryStroke = Stroke(width = m.s(1.1f), join = StrokeJoin.Round)
    val altStroke = Stroke(width = m.s(1.0f), join = StrokeJoin.Round)
    val dots = listOf(
        CachedCircle(m.pt(154f, 100f), m.s(4.0f), Color(0xD1FBCFE8)),
        CachedCircle(m.pt(104f, 64f), m.s(1.4f), Color(0x75F9A8D4)),
        CachedCircle(m.pt(204f, 136f), m.s(1.5f), Color(0x756EE7B7)),
    )

    return {
        drawRect(brush = glow)
        for (petal in primaryPetals) {
            drawPath(path = petal, color = Color(0x146EE7B7))
            drawPath(path = petal, color = Color(0x61A7F3D0), style = primaryStroke)
        }
        for (petal in altPetals) {
            drawPath(path = petal, color = Color(0x12F9A8D4))
            drawPath(path = petal, color = Color(0x52F9A8D4), style = altStroke)
        }
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
    }
}

private fun CacheDrawScope.buildSeasonalSummer(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_SUMMER)
    val glow = radialGlowBrush(
        0.78f, 0.44f, 0.58f,
        0.0f to Color(0x4DFB923C),
        0.50f to Color(0x1FFDE047),
        1.0f to Color.Transparent,
    )
    val sunCenter = m.pt(154f, 82f)
    val sunR = m.s(34f)
    val sunBrush = Brush.verticalGradient(
        colorStops = arrayOf(
            0.0f to Color(0x85FEF08A),
            0.65f to Color(0x3DFB923C),
            1.0f to Color(0x0FF472B6),
        ),
        startY = m.y(48f),
        endY = m.y(116f),
    )
    val haloR = m.s(43f)
    val haloStroke = Stroke(width = m.s(1f))
    val bars = SUMMER_HORIZON_BARS.map { (coords, col) ->
        val cr = m.s(coords[4])
        CachedRect(
            topLeft = m.pt(coords[0], coords[1]),
            size = Size(m.sw(coords[2]), m.sh(coords[3]).coerceAtLeast(1f)),
            color = col,
            cornerRadius = CornerRadius(cr, cr),
        )
    }

    return {
        drawRect(brush = glow)
        drawCircle(brush = sunBrush, radius = sunR, center = sunCenter)
        drawCircle(color = Color(0x33FED7AA), radius = haloR, center = sunCenter, style = haloStroke)
        for (bar in bars) {
            drawRoundRect(
                color = bar.color,
                topLeft = bar.topLeft,
                size = bar.size,
                cornerRadius = bar.cornerRadius,
            )
        }
    }
}

private fun CacheDrawScope.buildSeasonalSummerSolstice(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_SUMMER_SOLSTICE)
    val glow = radialGlowBrush(
        0.78f, 0.48f, 0.58f,
        0.0f to Color(0x47FDE047),
        0.48f to Color(0x1FF472B6),
        1.0f to Color.Transparent,
    )
    val c = m.pt(154f, 100f)
    val coronaR = m.s(48f)
    val coronaStroke = Stroke(
        width = m.s(2.2f),
        pathEffect = PathEffect.dashPathEffect(floatArrayOf(m.s(1.8f), m.s(10.76f))),
    )
    val innerRingR = m.s(28f)
    val innerRingStroke = Stroke(width = m.s(1f))
    val orbitTopLeft = m.pt(154f - 68f, 100f - 20f)
    val orbitSize = Size(m.sw(136f), m.sh(40f))
    val orbitStroke = Stroke(width = m.s(1.1f))
    val coreGlowR = m.s(13f)
    val coreR = m.s(5.2f)
    val planetCenter = m.pt(217f, 75f)
    val planetR = m.s(2.4f)

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x5CFDE047), radius = coronaR, center = c, style = coronaStroke)
        drawCircle(color = Color(0x47FEF08A), radius = innerRingR, center = c, style = innerRingStroke)
        rotate(degrees = -22f, pivot = c) {
            drawOval(color = Color(0x5CF9A8D4), topLeft = orbitTopLeft, size = orbitSize, style = orbitStroke)
        }
        drawCircle(color = Color(0x4DFEF08A), radius = coreGlowR, center = c)
        drawCircle(color = Color(0xC7FFFBEB), radius = coreR, center = c)
        drawCircle(color = Color(0xC7FDE047), radius = planetR, center = planetCenter)
    }
}

private fun CacheDrawScope.buildSeasonalLateSummerReset(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_LATE_SUMMER_RESET)
    val glow = radialGlowBrush(
        0.78f, 0.48f, 0.56f,
        0.0f to Color(0x42F0ABFC),
        0.50f to Color(0x1FFB923C),
        1.0f to Color.Transparent,
    )
    val domeTopLeft = m.pt(154f - 36f, 106f - 36f)
    val domeSize = Size(m.sw(72f), m.sh(72f))
    val duskBrush = Brush.verticalGradient(
        colorStops = arrayOf(
            0.0f to Color(0x61F0ABFC),
            1.0f to Color(0x24FB923C),
        ),
        startY = m.y(70f),
        endY = m.y(106f),
    )
    val reflectStroke = Stroke(
        width = m.s(1.1f),
        pathEffect = PathEffect.dashPathEffect(floatArrayOf(m.s(4f), m.s(5f))),
    )
    val lines = listOf(
        CachedLine(m.pt(58f, 106f), m.pt(222f, 106f), m.s(1.1f), Color(0x66F0ABFC)),
        CachedLine(m.pt(94f, 152f), m.pt(214f, 152f), m.s(1.0f), Color(0x2EF0ABFC)),
        CachedLine(m.pt(118f, 166f), m.pt(190f, 166f), m.s(1.0f), Color(0x24FDBA74)),
    )

    return {
        drawRect(brush = glow)
        drawArc(
            brush = duskBrush,
            startAngle = 180f,
            sweepAngle = 180f,
            useCenter = true,
            topLeft = domeTopLeft,
            size = domeSize,
        )
        drawArc(
            color = Color(0x47FDBA74),
            startAngle = 0f,
            sweepAngle = 180f,
            useCenter = false,
            topLeft = domeTopLeft,
            size = domeSize,
            style = reflectStroke,
        )
        for (line in lines) {
            drawLine(line.color, line.start, line.end, strokeWidth = line.strokeWidth, cap = StrokeCap.Round)
        }
    }
}

private fun CacheDrawScope.buildSeasonalAutumn(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_AUTUMN)
    val glow = radialGlowBrush(
        0.78f, 0.48f, 0.58f,
        0.0f to Color(0x42F59E0B),
        0.52f to Color(0x1AEA580C),
        1.0f to Color.Transparent,
    )
    val breeze = Path().apply {
        moveTo(m.x(48f), m.y(156f))
        quadraticTo(m.x(132f), m.y(126f), m.x(226f), m.y(62f))
    }
    val breezeStroke = Stroke(width = m.s(1f), cap = StrokeCap.Round)

    fun buildFacet(
        cx: Float,
        cy: Float,
        deg: Double,
        scale: Float,
        fillCol: Color,
        strokeCol: Color,
        spineCol: Color,
    ): Pair<CachedColoredPath, CachedLine> {
        val (tx, ty) = rotatedOffset(cx, cy, 0f, -24f, deg, scale)
        val (rx, ry) = rotatedOffset(cx, cy, 13f, 0f, deg, scale)
        val (bx, by) = rotatedOffset(cx, cy, 0f, 24f, deg, scale)
        val (lx, ly) = rotatedOffset(cx, cy, -13f, 0f, deg, scale)
        val poly = Path().apply {
            moveTo(m.x(tx), m.y(ty))
            lineTo(m.x(rx), m.y(ry))
            lineTo(m.x(bx), m.y(by))
            lineTo(m.x(lx), m.y(ly))
            close()
        }
        val facet = CachedColoredPath(
            path = poly,
            color = fillCol,
            strokeColor = strokeCol,
            stroke = Stroke(width = m.s(1.1f * scale)),
        )
        val spine = CachedLine(
            start = m.pt(tx, ty),
            end = m.pt(bx, by),
            strokeWidth = m.s(0.9f * scale),
            color = spineCol,
        )
        return facet to spine
    }

    val facets = listOf(
        buildFacet(168f, 78f, -24.0, 1.0f, Color(0x21FBBF24), Color(0x75FBBF24), Color(0x57FDE68A)),
        buildFacet(122f, 116f, 16.0, 0.76f, Color(0x1CFB923C), Color(0x5CFDBA74), Color(0x47FED7AA)),
        buildFacet(202f, 128f, -38.0, 0.56f, Color(0x1AF59E0B), Color(0x47FBBF24), Color(0x38FDE68A)),
    )

    return {
        drawRect(brush = glow)
        drawPath(path = breeze, color = Color(0x33FBBF24), style = breezeStroke)
        for ((facet, spine) in facets) {
            drawPath(path = facet.path, color = facet.color)
            drawPath(path = facet.path, color = facet.strokeColor, style = facet.stroke!!)
            drawLine(spine.color, spine.start, spine.end, strokeWidth = spine.strokeWidth)
        }
    }
}

private fun CacheDrawScope.buildSeasonalHalloween(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.SEASONAL_HALLOWEEN)
    val glow = radialGlowBrush(
        0.78f, 0.48f, 0.56f,
        0.0f to Color(0x47F97316),
        0.52f to Color(0x24A855F7),
        1.0f to Color.Transparent,
    )
    val c = m.pt(156f, 98f)
    val coronaR = m.s(56f)
    val coronaBrush = Brush.radialGradient(
        colorStops = arrayOf(
            0.0f to Color(0x85FB923C),
            0.50f to Color(0x85FB923C),
            0.72f to Color(0x38F97316),
            1.0f to Color.Transparent,
        ),
        center = c,
        radius = coronaR.coerceAtLeast(1f),
    )
    val outerRimR = m.s(34f)
    val outerRimStroke = Stroke(width = m.s(4f))
    val innerRimR = m.s(32f)
    val innerRimStroke = Stroke(width = m.s(1.5f))
    val moonCenter = m.pt(161f, 95f)
    val moonR = m.s(30f)
    val dots = listOf(
        CachedCircle(m.pt(104f, 60f), m.s(1.5f), Color(0x7AC084FC)),
        CachedCircle(m.pt(206f, 138f), m.s(1.4f), Color(0x80FB923C)),
    )

    return {
        drawRect(brush = glow)
        drawCircle(brush = coronaBrush, radius = coronaR, center = c)
        drawCircle(color = Color(0x3DF97316), radius = outerRimR, center = c, style = outerRimStroke)
        drawCircle(color = Color(0xB8FB923C), radius = innerRimR, center = c, style = innerRimStroke)
        drawCircle(color = Color(0xFF160822), radius = moonR, center = moonCenter)
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
    }
}

private fun CacheDrawScope.buildFallbackWash(): DrawScope.() -> Unit {
    val m = mapperFor(MixMotif.FALLBACK)
    val glow = radialGlowBrush(
        0.78f, 0.46f, 0.58f,
        0.0f to Color(0x3DF472F4),
        0.52f to Color(0x1AA855F7),
        1.0f to Color.Transparent,
    )
    val c = m.pt(154f, 100f)
    val rings = listOf(
        CachedCircle(c, m.s(26f), Color(0x47F472F4), stroke = Stroke(width = m.s(1.1f))),
        CachedCircle(
            c,
            m.s(46f),
            Color(0x2EF472F4),
            stroke = Stroke(
                width = m.s(1f),
                pathEffect = PathEffect.dashPathEffect(floatArrayOf(m.s(8f), m.s(6f))),
            ),
        ),
        CachedCircle(c, m.s(66f), Color(0x1CF472F4), stroke = Stroke(width = m.s(0.9f))),
    )
    val sparkPath = buildSmoothFourPointStarPath(c, m.s(16f), m.s(1.5f))
    val accentCenter = m.pt(186f, 67f)
    val accentR = m.s(2.2f)

    return {
        drawRect(brush = glow)
        for (ring in rings) {
            drawCircle(color = ring.color, radius = ring.radius, center = ring.center, style = ring.stroke!!)
        }
        drawPath(path = sparkPath, color = Color(0x8FF5D0FE))
        drawCircle(color = Color(0x9E33FFB0), radius = accentR, center = accentCenter)
    }
}
