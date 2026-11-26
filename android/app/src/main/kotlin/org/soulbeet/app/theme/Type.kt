package org.soulbeet.app.theme

import androidx.compose.material3.Typography
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.em
import androidx.compose.ui.unit.sp
import org.soulbeet.app.R

/**
 * Bundled Space Grotesk font family instantiated directly from Soulbeet's OFL variable font.
 */
val SpaceGroteskFamily: FontFamily = FontFamily(
    Font(R.font.space_grotesk_regular, FontWeight.Normal),
    Font(R.font.space_grotesk_medium, FontWeight.Medium),
    Font(R.font.space_grotesk_semibold, FontWeight.SemiBold),
    Font(R.font.space_grotesk_bold, FontWeight.Bold),
)

/**
 * Bundled Commit Mono font family for durations, bitrates, codecs, and telemetry pills.
 */
val CommitMonoFamily: FontFamily = FontFamily(
    Font(R.font.commit_mono_regular, FontWeight.Normal),
    Font(R.font.commit_mono_medium, FontWeight.Medium),
    Font(R.font.commit_mono_semibold, FontWeight.SemiBold),
)

private const val MONO_FEATURES = "tnum, ss03, ss04, ss05"

/**
 * Soulbeet's dual-voice typographic scale matching `docs/design/tokens.json`.
 */
@Immutable
data class SoulbeetTypography(
    val display: TextStyle = TextStyle(
        fontFamily = SpaceGroteskFamily,
        fontWeight = FontWeight.Bold,
        fontSize = 32.sp,
        lineHeight = 36.sp,
        letterSpacing = (-0.025).em,
    ),
    val titleLg: TextStyle = TextStyle(
        fontFamily = SpaceGroteskFamily,
        fontWeight = FontWeight.Bold,
        fontSize = 24.sp,
        lineHeight = 29.sp,
        letterSpacing = (-0.02).em,
    ),
    val titleMd: TextStyle = TextStyle(
        fontFamily = SpaceGroteskFamily,
        fontWeight = FontWeight.SemiBold,
        fontSize = 18.sp,
        lineHeight = 22.5.sp,
        letterSpacing = (-0.015).em,
    ),
    val body: TextStyle = TextStyle(
        fontFamily = SpaceGroteskFamily,
        fontWeight = FontWeight.Medium,
        fontSize = 15.sp,
        lineHeight = 22.sp,
        letterSpacing = 0.em,
    ),
    val bodySm: TextStyle = TextStyle(
        fontFamily = SpaceGroteskFamily,
        fontWeight = FontWeight.Normal,
        fontSize = 13.sp,
        lineHeight = 18.sp,
        letterSpacing = 0.em,
    ),
    val monoSm: TextStyle = TextStyle(
        fontFamily = CommitMonoFamily,
        fontWeight = FontWeight.Medium,
        fontSize = 12.sp,
        lineHeight = 15.5.sp,
        letterSpacing = 0.02.em,
        fontFeatureSettings = MONO_FEATURES,
    ),
    val monoXs: TextStyle = TextStyle(
        fontFamily = CommitMonoFamily,
        fontWeight = FontWeight.SemiBold,
        fontSize = 11.sp,
        lineHeight = 13.sp,
        letterSpacing = 0.05.em,
        fontFeatureSettings = MONO_FEATURES,
    ),
)

val DefaultSoulbeetTypography = SoulbeetTypography()

val LocalSoulbeetTypography = staticCompositionLocalOf { DefaultSoulbeetTypography }

val SoulbeetMaterialTypography = Typography(
    displaySmall = DefaultSoulbeetTypography.display,
    headlineMedium = DefaultSoulbeetTypography.titleLg,
    titleLarge = DefaultSoulbeetTypography.titleLg,
    titleMedium = DefaultSoulbeetTypography.titleMd,
    titleSmall = DefaultSoulbeetTypography.body,
    bodyLarge = DefaultSoulbeetTypography.body,
    bodyMedium = DefaultSoulbeetTypography.bodySm,
    bodySmall = DefaultSoulbeetTypography.bodySm,
    labelLarge = DefaultSoulbeetTypography.body,
    labelMedium = DefaultSoulbeetTypography.monoSm,
    labelSmall = DefaultSoulbeetTypography.monoXs,
)
