package org.soulbeet.app.theme

import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color

// --- Surface Hierarchy (tokens.json) ---
val BeetSunken = Color(0xFF0B0312)
val BeetCanvas = Color(0xFF0F0518)
val BeetCanvasOled = Color(0xFF000000)
val BeetSurfaceLow = Color(0xFF160A24)
val BeetSurface = Color(0xFF1C0D2E)
val BeetSurfaceHigh = Color(0xFF26133D)
val BeetSurfaceHighest = Color(0xFF321A4F)
val BeetScrim = Color(0xC209030F)

// True-Black OLED Stepped Surfaces (DESIGN.md / prototype/styles.css)
val BeetOledSunken = Color(0xFF000000)
val BeetOledLow = Color(0xFF0B0512)
val BeetOledSurface = Color(0xFF130820)
val BeetOledHigh = Color(0xFF1E0E32)
val BeetOledHighest = Color(0xFF291442)

// --- Hairline Borders ---
val BeetBorderSubtle = Color(0x14D8C2F5) // rgba(216,194,245,0.08)
val BeetBorder = Color(0x24D8C2F5)       // rgba(216,194,245,0.14)
val BeetBorderStrong = Color(0xFF4A2B6B)

// --- Typography (WCAG 2.1 AA+ / AAA verified) ---
val TextPrimary = Color(0xFFF6F2FB)
val TextSecondary = Color(0xFFB8AAC9)
val TextMuted = Color(0xFF9E8EB3)
val TextDisabled = Color(0xFF5E4F73)

// --- Brand & Semantic Accents (10% Accent Budget) ---
val BeetAccentGlow = Color(0xFFFF00FF)
val BeetAccent = Color(0xFFF472F4)
val BeetAccentSolid = Color(0xFFA81BA8)
val BeetAccentSurface = Color(0x1FFF00FF) // rgba(255,0,255,0.12)

val BeetLeafGlow = Color(0xFF00FF9D)
val BeetLeaf = Color(0xFF33FFB0)
val BeetLeafSolid = Color(0xFF00E58D)
val BeetLeafInk = Color(0xFF081410)
val BeetLeafSurface = Color(0x1F00FF9D) // rgba(0,255,157,0.12)

val BeetGold = Color(0xFFFFD166)
val BeetGoldSurface = Color(0x21FFD166) // rgba(255,209,102,0.13)

val BeetCrimson = Color(0xFFFF668C)
val BeetCrimsonSolid = Color(0xFFC81E4C)
val BeetCrimsonSurface = Color(0x24FF668C) // rgba(255,102,140,0.14)

/**
 * Extended Soulbeet palette supplied via [LocalSoulbeetColors] so composables can read stepped
 * violet/OLED tonal surfaces and WCAG-verified semantic tokens directly.
 */
@Immutable
data class SoulbeetColors(
    val isOled: Boolean,
    val sunken: Color,
    val canvas: Color,
    val surfaceLow: Color,
    val surface: Color,
    val surfaceHigh: Color,
    val surfaceHighest: Color,
    val scrim: Color,
    val borderSubtle: Color,
    val borderDefault: Color,
    val borderStrong: Color,
    val textPrimary: Color,
    val textSecondary: Color,
    val textMuted: Color,
    val textDisabled: Color,
    val accentGlow: Color,
    val accentText: Color,
    val accentSolid: Color,
    val accentSurface: Color,
    val leafGlow: Color,
    val leafText: Color,
    val leafSolid: Color,
    val leafInk: Color,
    val leafSurface: Color,
    val gold: Color,
    val goldSurface: Color,
    val crimsonText: Color,
    val crimsonSolid: Color,
    val crimsonSurface: Color,
)

val MidnightStudioColors = SoulbeetColors(
    isOled = false,
    sunken = BeetSunken,
    canvas = BeetCanvas,
    surfaceLow = BeetSurfaceLow,
    surface = BeetSurface,
    surfaceHigh = BeetSurfaceHigh,
    surfaceHighest = BeetSurfaceHighest,
    scrim = BeetScrim,
    borderSubtle = BeetBorderSubtle,
    borderDefault = BeetBorder,
    borderStrong = BeetBorderStrong,
    textPrimary = TextPrimary,
    textSecondary = TextSecondary,
    textMuted = TextMuted,
    textDisabled = TextDisabled,
    accentGlow = BeetAccentGlow,
    accentText = BeetAccent,
    accentSolid = BeetAccentSolid,
    accentSurface = BeetAccentSurface,
    leafGlow = BeetLeafGlow,
    leafText = BeetLeaf,
    leafSolid = BeetLeafSolid,
    leafInk = BeetLeafInk,
    leafSurface = BeetLeafSurface,
    gold = BeetGold,
    goldSurface = BeetGoldSurface,
    crimsonText = BeetCrimson,
    crimsonSolid = BeetCrimsonSolid,
    crimsonSurface = BeetCrimsonSurface,
)

val OledStudioColors = MidnightStudioColors.copy(
    isOled = true,
    sunken = BeetOledSunken,
    canvas = BeetCanvasOled,
    surfaceLow = BeetOledLow,
    surface = BeetOledSurface,
    surfaceHigh = BeetOledHigh,
    surfaceHighest = BeetOledHighest,
)

fun soulbeetPaletteFor(oledMode: Boolean): SoulbeetColors =
    if (oledMode) OledStudioColors else MidnightStudioColors

val LocalSoulbeetColors = staticCompositionLocalOf { MidnightStudioColors }
