package org.soulbeet.app.ui.components

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

private data class PixelRect(
    val x: Int,
    val y: Int,
    val w: Int,
    val h: Int,
    val color: Color,
)

/**
 * Exact 16x16 pixel-art Soulbeet beet mark (`ui/assets/logo.svg` scaled from 64x64 grid / 4).
 * Per `DESIGN.md`, must only be rendered at exact multiples of 16dp (`16.dp`, `32.dp`, `48.dp`, `64.dp`).
 */
private val BEET_PIXELS: List<PixelRect> = listOf(
    // Leaf bright (#00FF9D)
    PixelRect(11, 0, 3, 1, Color(0xFF00FF9D)),
    PixelRect(2, 1, 2, 1, Color(0xFF00FF9D)),
    PixelRect(9, 1, 3, 1, Color(0xFF00FF9D)),
    PixelRect(1, 2, 3, 1, Color(0xFF00FF9D)),
    PixelRect(8, 2, 2, 1, Color(0xFF00FF9D)),
    PixelRect(7, 3, 1, 1, Color(0xFF00FF9D)),
    // Leaf mid (#00C47F)
    PixelRect(12, 1, 2, 1, Color(0xFF00C47F)),
    PixelRect(4, 2, 1, 1, Color(0xFF00C47F)),
    PixelRect(10, 2, 3, 1, Color(0xFF00C47F)),
    PixelRect(2, 3, 4, 1, Color(0xFF00C47F)),
    PixelRect(8, 3, 3, 1, Color(0xFF00C47F)),
    // Leaf shadow (#008F5F)
    PixelRect(6, 3, 1, 1, Color(0xFF008F5F)),
    PixelRect(7, 4, 2, 1, Color(0xFF008F5F)),
    // Beet body top & highlights
    PixelRect(5, 4, 2, 1, Color(0xFFFF2EE6)),
    PixelRect(9, 4, 2, 1, Color(0xFFFF2EE6)),
    PixelRect(3, 5, 1, 1, Color(0xFFFF17F2)),
    PixelRect(6, 5, 6, 1, Color(0xFFFF17F2)),
    PixelRect(4, 5, 2, 1, Color(0xFFFFA7FA)),
    PixelRect(12, 5, 1, 1, Color(0xFFD012CD)),
    // Row 6
    PixelRect(2, 6, 1, 1, Color(0xFFFF00FF)),
    PixelRect(5, 6, 7, 1, Color(0xFFFF00FF)),
    PixelRect(3, 6, 2, 1, Color(0xFFFF9EFF)),
    PixelRect(12, 6, 2, 1, Color(0xFFD000D6)),
    // Row 7
    PixelRect(1, 7, 1, 1, Color(0xFFF700FF)),
    PixelRect(4, 7, 9, 1, Color(0xFFF700FF)),
    PixelRect(2, 7, 2, 1, Color(0xFFFC9EFF)),
    PixelRect(13, 7, 2, 1, Color(0xFFC900D6)),
    // Row 8
    PixelRect(1, 8, 1, 1, Color(0xFFEE00FF)),
    PixelRect(3, 8, 9, 1, Color(0xFFEE00FF)),
    PixelRect(2, 8, 1, 1, Color(0xFFF99EFF)),
    PixelRect(12, 8, 1, 1, Color(0xFFC300D6)),
    PixelRect(13, 8, 2, 1, Color(0xFF9B00B1)),
    // Row 9
    PixelRect(2, 9, 9, 1, Color(0xFFE600FF)),
    PixelRect(11, 9, 2, 1, Color(0xFFBC00D6)),
    PixelRect(13, 9, 1, 1, Color(0xFF9600B1)),
    // Row 10-15 tapered beet root
    PixelRect(3, 10, 8, 1, Color(0xFFD40AFF)),
    PixelRect(11, 10, 2, 1, Color(0xFFAD0ED6)),
    PixelRect(3, 11, 8, 1, Color(0xFFC314FF)),
    PixelRect(11, 11, 2, 1, Color(0xFFA110D6)),
    PixelRect(4, 12, 7, 1, Color(0xFFB11EFF)),
    PixelRect(11, 12, 1, 1, Color(0xFF9318D6)),
    PixelRect(5, 13, 5, 1, Color(0xFF9F28FF)),
    PixelRect(10, 13, 1, 1, Color(0xFF851FD6)),
    PixelRect(6, 14, 4, 1, Color(0xFF8C32FF)),
    PixelRect(7, 15, 2, 1, Color(0xFF7C3CFF)),
)

@Composable
fun SoulbeetPixelLogo(
    size: Dp = 32.dp,
    modifier: Modifier = Modifier,
) {
    Canvas(
        modifier = modifier
            .size(size)
            .semantics { contentDescription = "Soulbeet logo" },
    ) {
        val unit = this.size.minDimension / 16f
        for (px in BEET_PIXELS) {
            drawRect(
                color = px.color,
                topLeft = Offset(px.x * unit, px.y * unit),
                size = Size(px.w * unit + 0.5f, px.h * unit + 0.5f),
            )
        }
    }
}
