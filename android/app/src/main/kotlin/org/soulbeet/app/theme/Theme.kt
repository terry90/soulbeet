package org.soulbeet.app.theme

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.ReadOnlyComposable

/**
 * Refined Midnight Studio Material 3 + extended CompositionLocal theme.
 *
 * Supports both default Deep Midnight Violet (`#0F0518`) and optional True-Black OLED mode
 * (`#000000` canvas with stepped near-black violet surfaces).
 */
@Composable
fun SoulbeetTheme(
    oledMode: Boolean = false,
    content: @Composable () -> Unit,
) {
    val palette = soulbeetPaletteFor(oledMode)
    val colorScheme = darkColorScheme(
        primary = palette.leafSolid,
        onPrimary = palette.leafInk,
        primaryContainer = palette.leafSurface,
        onPrimaryContainer = palette.leafText,
        secondary = palette.accentSolid,
        onSecondary = palette.textPrimary,
        secondaryContainer = palette.accentSurface,
        onSecondaryContainer = palette.accentText,
        tertiary = palette.gold,
        onTertiary = palette.leafInk,
        tertiaryContainer = palette.goldSurface,
        onTertiaryContainer = palette.gold,
        background = palette.canvas,
        onBackground = palette.textPrimary,
        surface = palette.canvas,
        onSurface = palette.textPrimary,
        surfaceVariant = palette.surface,
        onSurfaceVariant = palette.textSecondary,
        surfaceContainerLowest = palette.sunken,
        surfaceContainerLow = palette.surfaceLow,
        surfaceContainer = palette.surface,
        surfaceContainerHigh = palette.surfaceHigh,
        surfaceContainerHighest = palette.surfaceHighest,
        outline = palette.borderDefault,
        outlineVariant = palette.borderSubtle,
        error = palette.crimsonSolid,
        onError = palette.textPrimary,
        errorContainer = palette.crimsonSurface,
        onErrorContainer = palette.crimsonText,
        scrim = palette.scrim,
    )

    CompositionLocalProvider(
        LocalSoulbeetColors provides palette,
        LocalSoulbeetTypography provides DefaultSoulbeetTypography,
    ) {
        MaterialTheme(
            colorScheme = colorScheme,
            typography = SoulbeetMaterialTypography,
            shapes = SoulbeetMaterialShapes,
            content = content,
        )
    }
}

object SoulbeetThemeAccess {
    val colors: SoulbeetColors
        @Composable
        @ReadOnlyComposable
        get() = LocalSoulbeetColors.current

    val typography: SoulbeetTypography
        @Composable
        @ReadOnlyComposable
        get() = LocalSoulbeetTypography.current
}
