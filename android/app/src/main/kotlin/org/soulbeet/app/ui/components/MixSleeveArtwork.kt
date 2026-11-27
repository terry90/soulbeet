package org.soulbeet.app.ui.components

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
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
    return when {
        lowered.startsWith("mix.seasonal.") -> lowered.removePrefix("mix.seasonal.")
        lowered.startsWith("mix.moment.") -> lowered.removePrefix("mix.moment.")
        lowered.startsWith("mix.rolling.") -> lowered.removePrefix("mix.rolling.")
        else -> lowered
    }
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
    "drive" -> MixSleeveSpec(
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
        gradientStart = Color(0xFF341A1D),
        gradientMid = Color(0xFF24112B),
        gradientEnd = Color(0xFF140921),
        gradientMidStop = 0.55f,
        borderColor = Color(0x42FBBF24),
        pillBackground = Color(0x24FBBF24),
        pillForeground = Color(0xFFFDE68A),
    )

    "focus" -> MixSleeveSpec(
        motif = MixMotif.FOCUS,
        gradientStart = Color(0xFF101932),
        gradientMid = Color(0xFF161233),
        gradientEnd = Color(0xFF100820),
        gradientMidStop = 0.54f,
        borderColor = Color(0x4260A5FA),
        pillBackground = Color(0x2460A5FA),
        pillForeground = Color(0xFF93C5FD),
    )

    "dinner_groove" -> MixSleeveSpec(
        motif = MixMotif.DINNER_GROOVE,
        gradientStart = Color(0xFF311520),
        gradientMid = Color(0xFF22102A),
        gradientEnd = Color(0xFF13081F),
        gradientMidStop = 0.55f,
        borderColor = Color(0x42FB923C),
        pillBackground = Color(0x24FB923C),
        pillForeground = Color(0xFFFDBA74),
    )

    "late_night" -> MixSleeveSpec(
        motif = MixMotif.LATE_NIGHT,
        gradientStart = Color(0xFF2B0E3C),
        gradientMid = Color(0xFF190C30),
        gradientEnd = Color(0xFF0F071F),
        gradientMidStop = 0.52f,
        borderColor = Color(0x45A78BFA),
        pillBackground = Color(0x24A78BFA),
        pillForeground = Color(0xFFC4B5FD),
    )

    "sunday_morning" -> MixSleeveSpec(
        motif = MixMotif.SUNDAY_MORNING,
        gradientStart = Color(0xFF2D1B18),
        gradientMid = Color(0xFF1F1128),
        gradientEnd = Color(0xFF13091F),
        gradientMidStop = 0.56f,
        borderColor = Color(0x42FDBA74),
        pillBackground = Color(0x24FDBA74),
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
        gradientStart = Color(0xFF271A2E),
        gradientMid = Color(0xFF1B102C),
        gradientEnd = Color(0xFF120820),
        gradientMidStop = 0.58f,
        borderColor = Color(0x42FFD166),
        pillBackground = Color(0x24FFD166),
        pillForeground = Color(0xFFFFD166),
    )

    "balanced", "weekly_radar" -> MixSleeveSpec(
        motif = MixMotif.ROLLING_RADAR,
        gradientStart = Color(0xFF112430),
        gradientMid = Color(0xFF19102F),
        gradientEnd = Color(0xFF110820),
        gradientMidStop = 0.58f,
        borderColor = Color(0x4233FFB0),
        pillBackground = Color(0x2400FF9D),
        pillForeground = Color(0xFF33FFB0),
    )

    "adventurous", "deep_cuts" -> MixSleeveSpec(
        motif = MixMotif.ROLLING_DEEP_CUTS,
        gradientStart = Color(0xFF2B0F3A),
        gradientMid = Color(0xFF1B0C31),
        gradientEnd = Color(0xFF10061E),
        gradientMidStop = 0.56f,
        borderColor = Color(0x45F472F4),
        pillBackground = Color(0x24FF00FF),
        pillForeground = Color(0xFFF472F4),
    )

    // --- Seasonals (13) ---
    "winter_holiday", "noel", "christmas" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_WINTER_HOLIDAY,
        gradientStart = Color(0xFF102A24),
        gradientMid = Color(0xFF1D122C),
        gradientEnd = Color(0xFF14081E),
        gradientMidStop = 0.56f,
        borderColor = Color(0x47FFD166),
        pillBackground = Color(0x24FFD166),
        pillForeground = Color(0xFFFFD166),
    )

    "new_years_eve", "nye" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_NEW_YEARS_EVE,
        gradientStart = Color(0xFF2B133D),
        gradientMid = Color(0xFF1E1133),
        gradientEnd = Color(0xFF11081F),
        gradientMidStop = 0.54f,
        borderColor = Color(0x4DFDE68A),
        pillBackground = Color(0x26FDE68A),
        pillForeground = Color(0xFFFDE68A),
    )

    "cozy_winter" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_COZY_WINTER,
        gradientStart = Color(0xFF1A2538),
        gradientMid = Color(0xFF23152E),
        gradientEnd = Color(0xFF13091F),
        gradientMidStop = 0.56f,
        borderColor = Color(0x42FDBA74),
        pillBackground = Color(0x24FDBA74),
        pillForeground = Color(0xFFFED7AA),
    )

    "lunar_new_year" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_LUNAR_NEW_YEAR,
        gradientStart = Color(0xFF3B111C),
        gradientMid = Color(0xFF261028),
        gradientEnd = Color(0xFF14081E),
        gradientMidStop = 0.56f,
        borderColor = Color(0x4DFFD166),
        pillBackground = Color(0x26FF668C),
        pillForeground = Color(0xFFFFD166),
    )

    "valentines" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_VALENTINES,
        gradientStart = Color(0xFF3A102A),
        gradientMid = Color(0xFF240F30),
        gradientEnd = Color(0xFF13071F),
        gradientMidStop = 0.56f,
        borderColor = Color(0x47FB7185),
        pillBackground = Color(0x24FB7185),
        pillForeground = Color(0xFFFDA4AF),
    )

    "carnival" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_CARNIVAL,
        gradientStart = Color(0xFF31143B),
        gradientMid = Color(0xFF1E1336),
        gradientEnd = Color(0xFF111A2B),
        gradientMidStop = 0.52f,
        borderColor = Color(0x47FBBF24),
        pillBackground = Color(0x24FBBF24),
        pillForeground = Color(0xFFFCD34D),
    )

    "celtic_spring" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_CELTIC_SPRING,
        gradientStart = Color(0xFF0E2C22),
        gradientMid = Color(0xFF161A30),
        gradientEnd = Color(0xFF110920),
        gradientMidStop = 0.56f,
        borderColor = Color(0x4233FFB0),
        pillBackground = Color(0x2400FF9D),
        pillForeground = Color(0xFF33FFB0),
    )

    "spring" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_SPRING,
        gradientStart = Color(0xFF152928),
        gradientMid = Color(0xFF231532),
        gradientEnd = Color(0xFF130920),
        gradientMidStop = 0.56f,
        borderColor = Color(0x426EE7B7),
        pillBackground = Color(0x246EE7B7),
        pillForeground = Color(0xFFA7F3D0),
    )

    "summer" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_SUMMER,
        gradientStart = Color(0xFF3A1D1B),
        gradientMid = Color(0xFF27132D),
        gradientEnd = Color(0xFF140922),
        gradientMidStop = 0.56f,
        borderColor = Color(0x47FB923C),
        pillBackground = Color(0x24FB923C),
        pillForeground = Color(0xFFFDBA74),
    )

    "summer_solstice" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_SUMMER_SOLSTICE,
        gradientStart = Color(0xFF3E2212),
        gradientMid = Color(0xFF29142B),
        gradientEnd = Color(0xFF150921),
        gradientMidStop = 0.55f,
        borderColor = Color(0x4DFDE047),
        pillBackground = Color(0x26FDE047),
        pillForeground = Color(0xFFFEF08A),
    )

    "late_summer_reset" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_LATE_SUMMER_RESET,
        gradientStart = Color(0xFF152830),
        gradientMid = Color(0xFF221430),
        gradientEnd = Color(0xFF130920),
        gradientMidStop = 0.56f,
        borderColor = Color(0x425EEAD4),
        pillBackground = Color(0x245EEAD4),
        pillForeground = Color(0xFF99F6E4),
    )

    "autumn" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_AUTUMN,
        gradientStart = Color(0xFF351B14),
        gradientMid = Color(0xFF241126),
        gradientEnd = Color(0xFF14081E),
        gradientMidStop = 0.56f,
        borderColor = Color(0x47F59E0B),
        pillBackground = Color(0x24F59E0B),
        pillForeground = Color(0xFFFCD34D),
    )

    "halloween" -> MixSleeveSpec(
        motif = MixMotif.SEASONAL_HALLOWEEN,
        gradientStart = Color(0xFF2B1033),
        gradientMid = Color(0xFF1E0D2B),
        gradientEnd = Color(0xFF11061A),
        gradientMidStop = 0.54f,
        borderColor = Color(0x4DF97316),
        pillBackground = Color(0x26F97316),
        pillForeground = Color(0xFFFB923C),
    )

    // --- Fallback (Midnight Studio violet-accent wash) ---
    else -> MixSleeveSpec(
        motif = MixMotif.FALLBACK,
        gradientStart = Color(0xFF24103B),
        gradientMid = Color(0xFF190B2C),
        gradientEnd = Color(0xFF11061E),
        gradientMidStop = 0.56f,
        borderColor = Color(0x2ED8C2F5),
        pillBackground = Color(0x1FFF00FF),
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
    onClick: () -> Unit,
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
                    0.0f to Color(0x700B0414),
                    0.58f to Color(0x300B0414),
                    1.0f to Color.Transparent,
                ),
            )
        } else {
            Brush.verticalGradient(
                colorStops = arrayOf(
                    0.0f to Color.Transparent,
                    0.46f to Color(0x180B0414),
                    1.0f to Color(0xA60B0414),
                ),
            )
        }
    }
    val cardShape = RoundedCornerShape(14.dp)
    val sizeModifier = when (layout) {
        MixSleeveLayout.WIDE -> Modifier
            .width(272.dp)
            .height(164.dp)

        MixSleeveLayout.SQUARE -> Modifier
            .width(172.dp)
            .height(188.dp)
    }
    val trackWord = if (mix.trackCount == 1) "track" else "tracks"

    Surface(
        shape = cardShape,
        color = spec.gradientEnd,
        border = BorderStroke(1.dp, spec.borderColor),
        modifier = modifier
            .then(sizeModifier)
            .clip(cardShape)
            .clickable(onClick = onClick)
            .clearAndSetSemantics {
                role = Role.Button
                contentDescription = "${mix.title}, $tag, ${mix.subtitle}, ${mix.trackCount} $trackWord. Play mix."
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
                            Box(
                                modifier = Modifier
                                    .size(32.dp)
                                    .clip(CircleShape)
                                    .background(colors.leafSolid),
                                contentAlignment = Alignment.Center,
                            ) {
                                Icon(
                                    imageVector = Icons.Filled.PlayArrow,
                                    contentDescription = null,
                                    tint = colors.leafInk,
                                    modifier = Modifier.size(18.dp),
                                )
                            }
                        }

                        Column(
                            modifier = Modifier.fillMaxWidth(0.84f),
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
                            Box(
                                modifier = Modifier
                                    .size(28.dp)
                                    .clip(CircleShape)
                                    .background(colors.leafSolid),
                                contentAlignment = Alignment.Center,
                            ) {
                                Icon(
                                    imageVector = Icons.Filled.PlayArrow,
                                    contentDescription = null,
                                    tint = colors.leafInk,
                                    modifier = Modifier.size(16.dp),
                                )
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

private enum class ViewBoxScaleMode {
    SLICE,
    MEET,
    STRETCH,
}

/**
 * Maps virtual SVG viewBox coordinates onto the Canvas using:
 * - [ViewBoxScaleMode.SLICE] (`xMaxYMid slice`) for wide landscape motifs
 * - [ViewBoxScaleMode.MEET] (`xMaxYMid meet`) for 260x260 square motifs so nothing clips vertically
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

private val LATE_NIGHT_BACK_TOWERS = listOf(
    floatArrayOf(330f, 106f, 24f, 78f),
    floatArrayOf(386f, 76f, 28f, 108f),
    floatArrayOf(442f, 64f, 32f, 120f),
    floatArrayOf(498f, 88f, 26f, 96f),
    floatArrayOf(546f, 72f, 34f, 112f),
)

private val LATE_NIGHT_FRONT_TOWERS = listOf(
    floatArrayOf(352f, 122f, 28f, 62f),
    floatArrayOf(404f, 92f, 34f, 92f),
    floatArrayOf(466f, 78f, 36f, 106f),
    floatArrayOf(518f, 104f, 30f, 80f),
)

private val LATE_NIGHT_WINDOWS = listOf(
    Pair(floatArrayOf(411f, 102f, 5f, 3f), Color(0xA6FDE68A)),
    Pair(floatArrayOf(421f, 102f, 5f, 3f), Color(0x8CF472F4)),
    Pair(floatArrayOf(411f, 112f, 5f, 3f), Color(0x73FDE68A)),
    Pair(floatArrayOf(421f, 122f, 5f, 3f), Color(0xA633FFB0)),
    Pair(floatArrayOf(411f, 136f, 5f, 3f), Color(0x8CFDE68A)),
    Pair(floatArrayOf(474f, 90f, 6f, 3f), Color(0xA633FFB0)),
    Pair(floatArrayOf(486f, 90f, 6f, 3f), Color(0x8CFDE68A)),
    Pair(floatArrayOf(474f, 102f, 6f, 3f), Color(0xA6F472F4)),
    Pair(floatArrayOf(486f, 114f, 6f, 3f), Color(0xB3FDE68A)),
    Pair(floatArrayOf(474f, 128f, 6f, 3f), Color(0x8C38BDF8)),
    Pair(floatArrayOf(486f, 142f, 6f, 3f), Color(0x80F472F4)),
    Pair(floatArrayOf(525f, 114f, 5f, 3f), Color(0x99FDE68A)),
    Pair(floatArrayOf(535f, 126f, 5f, 3f), Color(0x8C33FFB0)),
    Pair(floatArrayOf(360f, 134f, 5f, 3f), Color(0x80F472F4)),
    Pair(floatArrayOf(360f, 148f, 5f, 3f), Color(0x80FDE68A)),
)

private val MORNING_RAY_ANGLES = doubleArrayOf(-160.0, -135.0, -110.0, -85.0, -55.0, -25.0)
private val FOCUS_GRID_XS = floatArrayOf(228f, 264f, 300f, 336f, 372f)
private val FOCUS_GRID_YS = floatArrayOf(48f, 84f, 120f, 156f)
private val SUNDAY_STEAM_XS = floatArrayOf(282f, 312f, 342f)

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

private val ROLLING_COMFORT_ARCS = listOf(
    22f to 0.30f,
    42f to 0.22f,
    64f to 0.14f,
    88f to 0.08f,
)

private val ROLLING_RADAR_RINGS = listOf(
    24f to 0.28f,
    46f to 0.20f,
    68f to 0.13f,
)

private val ROLLING_DEEP_CUTS_ORBITS = listOf(
    floatArrayOf(52f, 20f, 0.34f),
    floatArrayOf(76f, 34f, 0.24f),
    floatArrayOf(102f, 50f, 0.14f),
)

private val WINTER_DIAGONAL_ANGLES = doubleArrayOf(45.0, 135.0)

private val CARNIVAL_RIBBONS = listOf(
    0f to Color(0x52FCD34D),
    20f to Color(0x4733FFB0),
    40f to Color(0x4DF472F4),
)

private val AUTUMN_LEAVES = listOf(
    Pair(floatArrayOf(326f, 82f, 11f, 6f), Color(0x52F59E0B)),
    Pair(floatArrayOf(284f, 114f, 9f, 5f), Color(0x47FB923C)),
    Pair(floatArrayOf(354f, 128f, 8f, 4.5f), Color(0x3DFCD34D)),
)

private val HALLOWEEN_WEB_ANGLES = doubleArrayOf(105.0, 125.0, 145.0, 165.0)
private val HALLOWEEN_WEB_RADII = floatArrayOf(38f, 70f, 102f)

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
)

private class CachedCircle(
    val center: Offset,
    val radius: Float,
    val color: Color,
    val stroke: Stroke? = null,
)

private class CachedColoredPath(
    val path: Path,
    val color: Color = Color.Unspecified,
    val brush: Brush? = null,
)

private class CachedArc(
    val topLeft: Offset,
    val size: Size,
    val color: Color,
)

private class CachedOval(
    val topLeft: Offset,
    val size: Size,
    val color: Color,
)

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
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
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
    val m = ViewBoxMapper(size.width, size.height, 320f, 180f)
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
    val m = ViewBoxMapper(size.width, size.height, 320f, 180f)
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
    val m = ViewBoxMapper(size.width, size.height, 320f, 180f)
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
    val m = ViewBoxMapper(size.width, size.height, 400f, 220f, mode = ViewBoxScaleMode.STRETCH)
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
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
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
    val m = ViewBoxMapper(size.width, size.height, 400f, 184f)
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
    val m = ViewBoxMapper(size.width, size.height, 600f, 184f)
    val glow = radialGlowBrush(
        0.76f, 0.82f, 0.55f,
        0.0f to Color(0x57FF00FF),
        0.48f to Color(0x1F38BDF8),
        1.0f to Color.Transparent,
    )
    val backColor = Color(0xA6220F3B)
    val backRects = LATE_NIGHT_BACK_TOWERS.map { t ->
        CachedRect(m.pt(t[0], t[1]), Size(m.sw(t[2]), m.sh(t[3])), color = backColor)
    }
    val frontRects = LATE_NIGHT_FRONT_TOWERS.map { t ->
        val topLeft = m.pt(t[0], t[1])
        val rectSize = Size(m.sw(t[2]), m.sh(t[3]))
        CachedRect(
            topLeft = topLeft,
            size = rectSize,
            brush = Brush.verticalGradient(
                colors = listOf(Color(0xE02A1246), Color(0xF2110621)),
                startY = topLeft.y,
                endY = topLeft.y + rectSize.height,
            ),
        )
    }
    val rimStroke = Stroke(width = m.s(0.9f))
    val spireStart = m.pt(484f, 54f)
    val spireEnd = m.pt(484f, 78f)
    val spireWidth = m.s(1.2f)
    val beaconRadius = m.s(2.0f)

    val windowRects = LATE_NIGHT_WINDOWS.map { (w, col) ->
        CachedRect(m.pt(w[0], w[1]), Size(m.sw(w[2]), m.sh(w[3])), color = col)
    }

    return {
        drawRect(brush = glow)
        for (r in backRects) {
            drawRect(color = r.color, topLeft = r.topLeft, size = r.size)
        }
        for (r in frontRects) {
            drawRect(brush = r.brush!!, topLeft = r.topLeft, size = r.size)
            drawRect(color = Color(0x38F472F4), topLeft = r.topLeft, size = r.size, style = rimStroke)
        }
        drawLine(color = Color(0x73F472F4), start = spireStart, end = spireEnd, strokeWidth = spireWidth)
        drawCircle(color = Color(0xCCFF00FF), radius = beaconRadius, center = spireStart)
        for (w in windowRects) {
            drawRect(color = w.color, topLeft = w.topLeft, size = w.size)
        }
    }
}

private fun CacheDrawScope.buildMorningSunrise(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.78f, 0.62f,
        0.0f to Color(0x52FBBF24),
        0.48f to Color(0x1FFB7185),
        1.0f to Color.Transparent,
    )
    val center = m.pt(312f, 152f)
    val horizonStart = m.pt(208f, 152f)
    val horizonEnd = m.pt(394f, 152f)
    val horizonWidth = m.s(1.1f)
    val innerArcTopLeft = m.pt(312f - 28f, 152f - 28f)
    val innerArcSize = Size(m.sw(56f), m.sh(56f))
    val outerArcTopLeft = m.pt(312f - 46f, 152f - 46f)
    val outerArcSize = Size(m.sw(92f), m.sh(92f))
    val outerArcStroke = Stroke(width = m.s(1.1f))
    val rayWidth = m.s(1.3f)

    val rays = MORNING_RAY_ANGLES.map { deg ->
        val rad = deg * PI / 180.0
        val inner = Offset(center.x + (cos(rad) * m.s(36f)).toFloat(), center.y + (sin(rad) * m.s(36f)).toFloat())
        val outer = Offset(center.x + (cos(rad) * m.s(76f)).toFloat(), center.y + (sin(rad) * m.s(76f)).toFloat())
        CachedLine(
            start = inner,
            end = outer,
            strokeWidth = rayWidth,
            brush = Brush.linearGradient(
                colors = listOf(Color(0x52FDE68A), Color.Transparent),
                start = inner,
                end = outer,
            ),
        )
    }

    return {
        drawRect(brush = glow)
        drawLine(Color(0x38FDE68A), horizonStart, horizonEnd, strokeWidth = horizonWidth)
        drawArc(
            color = Color(0x47FDE68A),
            startAngle = 180f,
            sweepAngle = 180f,
            useCenter = true,
            topLeft = innerArcTopLeft,
            size = innerArcSize,
        )
        drawArc(
            color = Color(0x33FBBF24),
            startAngle = 180f,
            sweepAngle = 180f,
            useCenter = false,
            topLeft = outerArcTopLeft,
            size = outerArcSize,
            style = outerArcStroke,
        )
        for (ray in rays) {
            drawLine(
                brush = ray.brush!!,
                start = ray.start,
                end = ray.end,
                strokeWidth = ray.strokeWidth,
                cap = StrokeCap.Round,
            )
        }
    }
}

private fun CacheDrawScope.buildFocusGrid(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.48f, 0.56f,
        0.0f to Color(0x3360A5FA),
        0.55f to Color(0x14818CF8),
        1.0f to Color.Transparent,
    )
    val gridColor = Color(0x1F93C5FD)
    val gridStrokeW = m.s(0.9f)
    val gridLines = buildList {
        for (gx in FOCUS_GRID_XS) {
            add(CachedLine(m.pt(gx, 16f), m.pt(gx, 184f), gridStrokeW, gridColor))
        }
        for (gy in FOCUS_GRID_YS) {
            add(CachedLine(m.pt(210f, gy), m.pt(390f, gy), gridStrokeW, gridColor))
        }
    }
    val center = m.pt(300f, 102f)
    val innerR = m.s(34f)
    val outerR = m.s(54f)
    val dotR = m.s(3.2f)
    val innerStroke = Stroke(width = m.s(1.2f))
    val outerStroke = Stroke(width = m.s(0.9f))

    return {
        drawRect(brush = glow)
        for (line in gridLines) {
            drawLine(line.color, line.start, line.end, strokeWidth = line.strokeWidth)
        }
        drawCircle(color = Color(0x4793C5FD), radius = innerR, center = center, style = innerStroke)
        drawCircle(color = Color(0x2E60A5FA), radius = outerR, center = center, style = outerStroke)
        drawCircle(color = Color(0x80BAE6FD), radius = dotR, center = center)
    }
}

private fun CacheDrawScope.buildDinnerGroove(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.76f, 0.50f, 0.58f,
        0.0f to Color(0x40FB923C),
        0.52f to Color(0x17F43F5E),
        1.0f to Color.Transparent,
    )
    val curveStroke = Stroke(width = m.s(1.3f), cap = StrokeCap.Round)
    val curves = (0..3).map { i ->
        val yOffset = i * 16f
        val path = Path().apply {
            moveTo(m.x(210f), m.y(118f + yOffset))
            cubicTo(
                m.x(258f), m.y(78f + yOffset),
                m.x(316f), m.y(144f + yOffset),
                m.x(392f), m.y(86f + yOffset),
            )
        }
        CachedColoredPath(path = path, color = Color(0xFFFDBA74).copy(alpha = 0.30f - i * 0.055f))
    }
    val candleCenter = m.pt(332f, 68f)
    val flameR = m.s(4f)
    val haloR = m.s(11f)

    return {
        drawRect(brush = glow)
        for (curve in curves) {
            drawPath(path = curve.path, color = curve.color, style = curveStroke)
        }
        drawCircle(color = Color(0x52FDE68A), radius = flameR, center = candleCenter)
        drawCircle(color = Color(0x33FB923C), radius = haloR, center = candleCenter)
    }
}

private fun CacheDrawScope.buildSundaySteam(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.52f, 0.58f,
        0.0f to Color(0x3DFDBA74),
        0.55f to Color(0x14F59E0B),
        1.0f to Color.Transparent,
    )
    val steamStroke = Stroke(width = m.s(1.6f), cap = StrokeCap.Round)
    val wisps = SUNDAY_STEAM_XS.mapIndexed { idx, baseX ->
        val topY = 44f + (idx % 2) * 12f
        val path = Path().apply {
            moveTo(m.x(baseX), m.y(146f))
            cubicTo(
                m.x(baseX - 14f), m.y(116f),
                m.x(baseX + 16f), m.y(86f),
                m.x(baseX - 4f), m.y(topY),
            )
        }
        val brush = Brush.verticalGradient(
            colorStops = arrayOf(
                0.0f to Color.Transparent,
                0.5f to Color(0x66FED7AA),
                1.0f to Color(0x26FDBA74),
            ),
            startY = m.y(topY),
            endY = m.y(146f),
        )
        CachedColoredPath(path = path, brush = brush)
    }
    val rimTopLeft = m.pt(262f, 152f)
    val rimSize = Size(m.sw(100f), m.sh(16f))
    val rimStroke = Stroke(width = m.s(1.3f))

    return {
        drawRect(brush = glow)
        for (wisp in wisps) {
            drawPath(path = wisp.path, brush = wisp.brush!!, style = steamStroke)
        }
        drawOval(color = Color(0x3DFED7AA), topLeft = rimTopLeft, size = rimSize, style = rimStroke)
    }
}

// --- Collection & Decades (5, 260x260 MEET mode + full-bleed wash) ---

private fun CacheDrawScope.buildForgottenGemsVinyl(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 260f, 260f, mode = ViewBoxScaleMode.MEET)
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
    val m = ViewBoxMapper(size.width, size.height, 260f, 260f, mode = ViewBoxScaleMode.MEET)
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
    val m = ViewBoxMapper(size.width, size.height, 260f, 260f, mode = ViewBoxScaleMode.MEET)
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
    val m = ViewBoxMapper(size.width, size.height, 260f, 260f, mode = ViewBoxScaleMode.MEET)
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
    val m = ViewBoxMapper(size.width, size.height, 260f, 260f, mode = ViewBoxScaleMode.MEET)
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
    val m = ViewBoxMapper(size.width, size.height, 260f, 260f, mode = ViewBoxScaleMode.MEET)
    val glow = radialGlowBrush(
        0.80f, 0.36f, 0.62f,
        0.0f to Color(0x38FFD166),
        0.60f to Color(0x12F472F4),
        1.0f to Color.Transparent,
    )
    val c = m.pt(210f, 88f)
    val arcStroke = Stroke(width = m.s(1.3f), cap = StrokeCap.Round)
    val arcs = ROLLING_COMFORT_ARCS.map { (r, a) ->
        val sr = m.s(r)
        CachedArc(
            topLeft = Offset(c.x - sr, c.y - sr),
            size = Size(sr * 2f, sr * 2f),
            color = Color(0xFFFFD166).copy(alpha = a),
        )
    }

    return {
        drawRect(brush = glow)
        for (arc in arcs) {
            drawArc(
                color = arc.color,
                startAngle = 110f,
                sweepAngle = 220f,
                useCenter = false,
                topLeft = arc.topLeft,
                size = arc.size,
                style = arcStroke,
            )
        }
    }
}

private fun CacheDrawScope.buildRollingRadar(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 260f, 260f, mode = ViewBoxScaleMode.MEET)
    val glow = radialGlowBrush(
        0.78f, 0.36f, 0.62f,
        0.0f to Color(0x3800FF9D),
        0.55f to Color(0x1438BDF8),
        1.0f to Color.Transparent,
    )
    val c = m.pt(204f, 92f)
    val ringStroke = Stroke(width = m.s(1.1f))
    val rings = ROLLING_RADAR_RINGS.map { (r, a) ->
        CachedCircle(center = c, radius = m.s(r), color = Color(0xFF33FFB0).copy(alpha = a))
    }
    val sweepEnd = Offset(c.x - m.s(44f), c.y - m.s(36f))
    val sweepWidth = m.s(1.4f)
    val blipCenter = Offset(c.x - m.s(30f), c.y - m.s(24f))
    val blipR = m.s(3.2f)

    return {
        drawRect(brush = glow)
        for (ring in rings) {
            drawCircle(color = ring.color, radius = ring.radius, center = ring.center, style = ringStroke)
        }
        drawLine(Color(0x5233FFB0), c, sweepEnd, strokeWidth = sweepWidth, cap = StrokeCap.Round)
        drawCircle(color = Color(0x9933FFB0), radius = blipR, center = blipCenter)
    }
}

private fun CacheDrawScope.buildRollingDeepCuts(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 260f, 260f, mode = ViewBoxScaleMode.MEET)
    val glow = radialGlowBrush(
        0.80f, 0.36f, 0.64f,
        0.0f to Color(0x42F472F4),
        0.55f to Color(0x148B5CF6),
        1.0f to Color.Transparent,
    )
    val dashStroke = Stroke(
        width = m.s(1.1f),
        pathEffect = PathEffect.dashPathEffect(floatArrayOf(m.s(14f), m.s(8f))),
    )
    val orbits = ROLLING_DEEP_CUTS_ORBITS.map { o ->
        val rx = o[0]
        val ry = o[1]
        CachedOval(
            topLeft = m.pt(202f - rx, 92f - ry),
            size = Size(m.sw(rx * 2f), m.sh(ry * 2f)),
            color = Color(0xFFF472F4).copy(alpha = o[2]),
        )
    }
    val starPath = buildFourPointStarPath(m.pt(166f, 80f), m.s(7f), m.s(1.8f))
    val dotCenter = m.pt(226f, 114f)
    val dotR = m.s(2.4f)

    return {
        drawRect(brush = glow)
        for (orbit in orbits) {
            drawOval(color = orbit.color, topLeft = orbit.topLeft, size = orbit.size, style = dashStroke)
        }
        drawPath(path = starPath, color = Color(0x8CF5D0FE))
        drawCircle(color = Color(0x8033FFB0), radius = dotR, center = dotCenter)
    }
}

// --- Seasonals (13) ---

private fun CacheDrawScope.buildSeasonalWinterHoliday(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.34f, 0.62f,
        0.0f to Color(0x47FFD166),
        0.52f to Color(0x1A10B981),
        1.0f to Color.Transparent,
    )
    val c = m.pt(312f, 88f)
    val mainStar = buildFourPointStarPath(c, m.s(22f), m.s(4.2f))
    val diagWidth = m.s(1.2f)
    val diagLines = WINTER_DIAGONAL_ANGLES.map { angle ->
        val rad = angle * PI / 180.0
        val dx = (cos(rad) * m.s(14f)).toFloat()
        val dy = (sin(rad) * m.s(14f)).toFloat()
        CachedLine(Offset(c.x - dx, c.y - dy), Offset(c.x + dx, c.y + dy), diagWidth, Color(0x52FEF08A))
    }
    val smallStar = buildFourPointStarPath(m.pt(262f, 128f), m.s(8f), m.s(2f))
    val dots = listOf(
        CachedCircle(m.pt(354f, 54f), m.s(2f), Color(0x57F0F9FF)),
        CachedCircle(m.pt(254f, 60f), m.s(1.8f), Color(0x47FFD166)),
        CachedCircle(m.pt(348f, 142f), m.s(2.2f), Color(0x40F0F9FF)),
    )

    return {
        drawRect(brush = glow)
        drawPath(path = mainStar, color = Color(0x75FFD166))
        for (line in diagLines) {
            drawLine(line.color, line.start, line.end, strokeWidth = line.strokeWidth, cap = StrokeCap.Round)
        }
        drawPath(path = smallStar, color = Color(0x5733FFB0))
        for (dot in dots) {
            drawCircle(color = dot.color, radius = dot.radius, center = dot.center)
        }
    }
}

private fun CacheDrawScope.buildSeasonalNewYearsEve(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.40f, 0.62f,
        0.0f to Color(0x52FDE68A),
        0.48f to Color(0x24F472F4),
        1.0f to Color.Transparent,
    )
    val c = m.pt(316f, 92f)
    val burstWidth = m.s(1.4f)
    val bursts = (0 until 12).map { i ->
        val rad = (i * 30.0) * PI / 180.0
        val innerR = if (i % 2 == 0) 10f else 16f
        val outerR = if (i % 2 == 0) 48f else 34f
        val p0 = Offset(c.x + (cos(rad) * m.s(innerR)).toFloat(), c.y + (sin(rad) * m.s(innerR)).toFloat())
        val p1 = Offset(c.x + (cos(rad) * m.s(outerR)).toFloat(), c.y + (sin(rad) * m.s(outerR)).toFloat())
        CachedLine(
            start = p0,
            end = p1,
            strokeWidth = burstWidth,
            brush = Brush.linearGradient(listOf(Color(0x75FDE68A), Color.Transparent), start = p0, end = p1),
        )
    }
    val starPath = buildFourPointStarPath(m.pt(252f, 64f), m.s(7f), m.s(1.8f))
    val dotCenter = m.pt(268f, 144f)
    val dotR = m.s(2f)

    return {
        drawRect(brush = glow)
        for (burst in bursts) {
            drawLine(
                brush = burst.brush!!,
                start = burst.start,
                end = burst.end,
                strokeWidth = burst.strokeWidth,
                cap = StrokeCap.Round,
            )
        }
        drawPath(path = starPath, color = Color(0x75F472F4))
        drawCircle(color = Color(0x66FDE68A), radius = dotR, center = dotCenter)
    }
}

private fun CacheDrawScope.buildSeasonalCozyWinter(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.76f, 0.44f, 0.60f,
        0.0f to Color(0x3DFDBA74),
        0.55f to Color(0x1A7DD3FC),
        1.0f to Color.Transparent,
    )
    val chevronStroke = Stroke(width = m.s(1.3f), cap = StrokeCap.Round, join = StrokeJoin.Round)
    val chevrons = (0..2).map { row ->
        val baseY = 66f + row * 34f
        val path = Path().apply {
            var vx = 216f
            moveTo(m.x(vx), m.y(baseY))
            while (vx < 386f) {
                lineTo(m.x(vx + 16f), m.y(baseY - 12f))
                lineTo(m.x(vx + 32f), m.y(baseY))
                vx += 32f
            }
        }
        CachedColoredPath(path = path, color = Color(0xFFFED7AA).copy(alpha = 0.28f - row * 0.06f))
    }

    return {
        drawRect(brush = glow)
        for (chevron in chevrons) {
            drawPath(path = chevron.path, color = chevron.color, style = chevronStroke)
        }
    }
}

private fun CacheDrawScope.buildSeasonalLunarNewYear(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.46f, 0.62f,
        0.0f to Color(0x47FF668C),
        0.50f to Color(0x26FFD166),
        1.0f to Color.Transparent,
    )
    val c = m.pt(314f, 94f)
    val outerTopLeft = m.pt(314f - 34f, 94f - 28f)
    val outerSize = Size(m.sw(68f), m.sh(56f))
    val outerStroke = Stroke(width = m.s(1.4f))
    val innerTopLeft = m.pt(314f - 18f, 94f - 28f)
    val innerSize = Size(m.sw(36f), m.sh(56f))
    val innerStroke = Stroke(width = m.s(1.1f))
    val topCordStart = m.pt(314f, 44f)
    val topCordEnd = m.pt(314f, 66f)
    val botCordStart = m.pt(314f, 122f)
    val botCordEnd = m.pt(314f, 152f)
    val cordWidth = m.s(1.2f)
    val dotR = m.s(3.2f)

    return {
        drawRect(brush = glow)
        drawOval(color = Color(0x52FFD166), topLeft = outerTopLeft, size = outerSize, style = outerStroke)
        drawOval(color = Color(0x38FFD166), topLeft = innerTopLeft, size = innerSize, style = innerStroke)
        drawLine(Color(0x47FFD166), topCordStart, topCordEnd, strokeWidth = cordWidth)
        drawLine(Color(0x47FFD166), botCordStart, botCordEnd, strokeWidth = cordWidth)
        drawCircle(color = Color(0x66FFD166), radius = dotR, center = c)
    }
}

private fun CacheDrawScope.buildSeasonalValentines(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.46f, 0.60f,
        0.0f to Color(0x4DFB7185),
        0.55f to Color(0x1FF472F4),
        1.0f to Color.Transparent,
    )
    val ringStroke = Stroke(width = m.s(1.4f))
    val ringR = m.s(34f)
    val leftCenter = m.pt(298f, 96f)
    val rightCenter = m.pt(330f, 96f)
    val starPath = buildFourPointStarPath(m.pt(314f, 64f), m.s(7f), m.s(1.8f))

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x47FDA4AF), radius = ringR, center = leftCenter, style = ringStroke)
        drawCircle(color = Color(0x3DF472F4), radius = ringR, center = rightCenter, style = ringStroke)
        drawPath(path = starPath, color = Color(0x80FFE4E6))
    }
}

private fun CacheDrawScope.buildSeasonalCarnival(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.44f, 0.62f,
        0.0f to Color(0x42FBBF24),
        0.48f to Color(0x21F472F4),
        1.0f to Color.Transparent,
    )
    val ribbonStroke = Stroke(width = m.s(2.0f), cap = StrokeCap.Round)
    val ribbons = CARNIVAL_RIBBONS.map { (dy, col) ->
        val path = Path().apply {
            moveTo(m.x(222f), m.y(148f + dy - 24f))
            cubicTo(
                m.x(272f), m.y(96f + dy - 24f),
                m.x(324f), m.y(132f + dy - 24f),
                m.x(388f), m.y(54f + dy - 24f),
            )
        }
        CachedColoredPath(path = path, color = col)
    }
    val dot1Center = m.pt(264f, 66f)
    val dot1R = m.s(2.4f)
    val dot2Center = m.pt(352f, 138f)
    val dot2R = m.s(2.0f)

    return {
        drawRect(brush = glow)
        for (ribbon in ribbons) {
            drawPath(path = ribbon.path, color = ribbon.color, style = ribbonStroke)
        }
        drawCircle(color = Color(0x66FCD34D), radius = dot1R, center = dot1Center)
        drawCircle(color = Color(0x5933FFB0), radius = dot2R, center = dot2Center)
    }
}

private fun CacheDrawScope.buildSeasonalCelticSpring(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.46f, 0.60f,
        0.0f to Color(0x3D00FF9D),
        0.55f to Color(0x1710B981),
        1.0f to Color.Transparent,
    )
    val r = m.s(26f)
    val triStroke = Stroke(width = m.s(1.3f))
    val cTop = m.pt(314f, 76f)
    val cLeft = m.pt(296f, 108f)
    val cRight = m.pt(332f, 108f)
    val outerCenter = m.pt(314f, 96f)
    val outerR = m.s(54f)
    val outerStroke = Stroke(width = m.s(0.9f))

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x4733FFB0), radius = r, center = cTop, style = triStroke)
        drawCircle(color = Color(0x3D6EE7B7), radius = r, center = cLeft, style = triStroke)
        drawCircle(color = Color(0x3D6EE7B7), radius = r, center = cRight, style = triStroke)
        drawCircle(color = Color(0x2633FFB0), radius = outerR, center = outerCenter, style = outerStroke)
    }
}

private fun CacheDrawScope.buildSeasonalSpring(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.42f, 0.62f,
        0.0f to Color(0x3D6EE7B7),
        0.55f to Color(0x1FF9A8D4),
        1.0f to Color.Transparent,
    )
    val c = m.pt(314f, 96f)
    val petalR = m.s(14f)
    val petalStroke = Stroke(width = m.s(1.2f))
    val petalCenters = (0 until 5).map { i ->
        val rad = (i * 72.0 - 90.0) * PI / 180.0
        Offset(c.x + (cos(rad) * m.s(22f)).toFloat(), c.y + (sin(rad) * m.s(22f)).toFloat())
    }
    val centerR = m.s(4f)
    val dot1Center = m.pt(256f, 68f)
    val dot1R = m.s(2f)
    val dot2Center = m.pt(356f, 142f)
    val dot2R = m.s(2.2f)

    return {
        drawRect(brush = glow)
        for (pt in petalCenters) {
            drawCircle(color = Color(0x38A7F3D0), radius = petalR, center = pt, style = petalStroke)
        }
        drawCircle(color = Color(0x5CF9A8D4), radius = centerR, center = c)
        drawCircle(color = Color(0x47A7F3D0), radius = dot1R, center = dot1Center)
        drawCircle(color = Color(0x47F9A8D4), radius = dot2R, center = dot2Center)
    }
}

private fun CacheDrawScope.buildSeasonalSummer(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.36f, 0.64f,
        0.0f to Color(0x52FB923C),
        0.50f to Color(0x1FF43F5E),
        1.0f to Color.Transparent,
    )
    val sunCenter = m.pt(318f, 76f)
    val sunR = m.s(24f)
    val waveStroke = Stroke(width = m.s(1.4f), cap = StrokeCap.Round)
    val waves = (0..2).map { row ->
        val y = 126f + row * 18f
        val path = Path().apply {
            moveTo(m.x(216f), m.y(y))
            cubicTo(m.x(256f), m.y(y - 10f), m.x(292f), m.y(y + 10f), m.x(332f), m.y(y))
            cubicTo(m.x(354f), m.y(y - 6f), m.x(374f), m.y(y + 6f), m.x(394f), m.y(y - 2f))
        }
        CachedColoredPath(path = path, color = Color(0xFFFDBA74).copy(alpha = 0.34f - row * 0.08f))
    }

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x42FDBA74), radius = sunR, center = sunCenter)
        for (wave in waves) {
            drawPath(path = wave.path, color = wave.color, style = waveStroke)
        }
    }
}

private fun CacheDrawScope.buildSeasonalSummerSolstice(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.42f, 0.66f,
        0.0f to Color(0x5CFDE047),
        0.48f to Color(0x21FB923C),
        1.0f to Color.Transparent,
    )
    val c = m.pt(316f, 92f)
    val coreR = m.s(18f)
    val midR = m.s(36f)
    val outerR = m.s(56f)
    val midStroke = Stroke(width = m.s(1.2f))
    val outerStroke = Stroke(width = m.s(1.0f))
    val rayWidth = m.s(1.2f)
    val rays = (0 until 8).map { i ->
        val rad = (i * 45.0) * PI / 180.0
        val p0 = Offset(c.x + (cos(rad) * m.s(24f)).toFloat(), c.y + (sin(rad) * m.s(24f)).toFloat())
        val p1 = Offset(c.x + (cos(rad) * m.s(64f)).toFloat(), c.y + (sin(rad) * m.s(64f)).toFloat())
        CachedLine(p0, p1, rayWidth, Color(0x42FEF08A))
    }

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x52FEF08A), radius = coreR, center = c)
        drawCircle(color = Color(0x3DFDE047), radius = midR, center = c, style = midStroke)
        drawCircle(color = Color(0x24FB923C), radius = outerR, center = c, style = outerStroke)
        for (ray in rays) {
            drawLine(ray.color, ray.start, ray.end, strokeWidth = ray.strokeWidth, cap = StrokeCap.Round)
        }
    }
}

private fun CacheDrawScope.buildSeasonalLateSummerReset(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.44f, 0.60f,
        0.0f to Color(0x3D5EEAD4),
        0.55f to Color(0x1AFDBA74),
        1.0f to Color.Transparent,
    )
    val lineWidth = m.s(1.4f)
    val lines = (0..4).map { i ->
        val y = 64f + i * 22f
        val startX = 216f + i * 18f
        CachedLine(
            start = m.pt(startX, y),
            end = m.pt(392f, y),
            strokeWidth = lineWidth,
            brush = Brush.horizontalGradient(
                colorStops = arrayOf(
                    0.0f to Color.Transparent,
                    0.5f to Color(0xFF99F6E4).copy(alpha = 0.30f - i * 0.04f),
                    1.0f to Color(0x40FDBA74),
                ),
                startX = m.x(startX),
                endX = m.x(392f),
            ),
        )
    }

    return {
        drawRect(brush = glow)
        for (line in lines) {
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

private fun CacheDrawScope.buildSeasonalAutumn(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.44f, 0.62f,
        0.0f to Color(0x47F59E0B),
        0.52f to Color(0x1FEA580C),
        1.0f to Color.Transparent,
    )
    val breeze = Path().apply {
        moveTo(m.x(218f), m.y(144f))
        cubicTo(m.x(278f), m.y(136f), m.x(314f), m.y(78f), m.x(384f), m.y(66f))
    }
    val breezeStroke = Stroke(width = m.s(1.1f), cap = StrokeCap.Round)
    val leaves = AUTUMN_LEAVES.map { (coords, col) ->
        val cx = coords[0]
        val cy = coords[1]
        val rx = coords[2]
        val ry = coords[3]
        val leaf = Path().apply {
            moveTo(m.x(cx), m.y(cy - ry))
            lineTo(m.x(cx + rx), m.y(cy))
            lineTo(m.x(cx), m.y(cy + ry))
            lineTo(m.x(cx - rx), m.y(cy))
            close()
        }
        CachedColoredPath(path = leaf, color = col)
    }

    return {
        drawRect(brush = glow)
        drawPath(breeze, color = Color(0x33FCD34D), style = breezeStroke)
        for (leaf in leaves) {
            drawPath(leaf.path, color = leaf.color)
        }
    }
}

private fun CacheDrawScope.buildSeasonalHalloween(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.78f, 0.42f, 0.62f,
        0.0f to Color(0x4DF97316),
        0.48f to Color(0x21A855F7),
        1.0f to Color.Transparent,
    )
    val moonCenter = m.pt(308f, 94f)
    val moonR = m.s(22f)
    val corner = m.pt(398f, 4f)
    val webWidth = m.s(0.9f)
    val webStroke = Stroke(width = webWidth)
    val radials = HALLOWEEN_WEB_ANGLES.map { angle ->
        val rad = angle * PI / 180.0
        val end = Offset(corner.x + (cos(rad) * m.s(124f)).toFloat(), corner.y + (sin(rad) * m.s(124f)).toFloat())
        CachedLine(corner, end, webWidth, Color(0x2EC4B5FD))
    }
    val arcs = HALLOWEEN_WEB_RADII.map { r ->
        val sr = m.s(r)
        CachedArc(
            topLeft = Offset(corner.x - sr, corner.y - sr),
            size = Size(sr * 2f, sr * 2f),
            color = Color(0x24C4B5FD),
        )
    }

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x42FB923C), radius = moonR, center = moonCenter)
        for (line in radials) {
            drawLine(line.color, line.start, line.end, strokeWidth = line.strokeWidth)
        }
        for (arc in arcs) {
            drawArc(
                color = arc.color,
                startAngle = 95f,
                sweepAngle = 80f,
                useCenter = false,
                topLeft = arc.topLeft,
                size = arc.size,
                style = webStroke,
            )
        }
    }
}

private fun CacheDrawScope.buildFallbackWash(): DrawScope.() -> Unit {
    val m = ViewBoxMapper(size.width, size.height, 400f, 200f)
    val glow = radialGlowBrush(
        0.80f, 0.36f, 0.62f,
        0.0f to Color(0x38FF00FF),
        0.55f to Color(0x1400FF9D),
        1.0f to Color.Transparent,
    )
    val c = m.pt(320f, 100f)
    val innerR = m.s(36f)
    val outerR = m.s(62f)
    val innerStroke = Stroke(width = m.s(1.1f))
    val outerStroke = Stroke(width = m.s(1.0f))

    return {
        drawRect(brush = glow)
        drawCircle(color = Color(0x24D8C2F5), radius = innerR, center = c, style = innerStroke)
        drawCircle(color = Color(0x17D8C2F5), radius = outerR, center = c, style = outerStroke)
    }
}
