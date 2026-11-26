package org.soulbeet.app.theme

import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Shapes
import androidx.compose.ui.unit.dp

/**
 * Constrained radius tokens from `docs/design/tokens.json`.
 */
object SoulbeetShapes {
    val xs = RoundedCornerShape(6.dp)
    val sm = RoundedCornerShape(8.dp)
    val md = RoundedCornerShape(12.dp)
    val lg = RoundedCornerShape(16.dp)
    val full = CircleShape
}

val SoulbeetMaterialShapes = Shapes(
    extraSmall = SoulbeetShapes.xs,
    small = SoulbeetShapes.sm,
    medium = SoulbeetShapes.md,
    large = SoulbeetShapes.lg,
    extraLarge = SoulbeetShapes.lg,
)
