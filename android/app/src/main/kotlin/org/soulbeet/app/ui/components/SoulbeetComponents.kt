package org.soulbeet.app.ui.components

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.PlaylistAdd
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.CloudDownload
import androidx.compose.material.icons.filled.DeleteOutline
import androidx.compose.material.icons.filled.Favorite
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.MusicNote
import androidx.compose.material.icons.filled.OfflinePin
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Shield
import androidx.compose.material.icons.filled.Star
import androidx.compose.material.icons.outlined.FavoriteBorder
import androidx.compose.material.icons.outlined.StarOutline
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import coil.compose.SubcomposeAsyncImage
import coil.request.ImageRequest
import kotlinx.coroutines.delay
import org.soulbeet.app.data.ContextualMixUi
import org.soulbeet.app.data.CultureSteeringOption
import org.soulbeet.app.data.DeleteGuardPreviewUi
import org.soulbeet.app.data.FormattingUtils
import org.soulbeet.app.data.PendingDropAction
import org.soulbeet.app.data.PendingDropMode
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.media.CoverArtContentProvider
import kotlin.math.roundToInt

/**
 * Soulbeet's signature `seam-light` 2dp sweeping gradient (`transparent -> #FF00FF -> #00FF9D -> transparent`)
 * pinned to acquiring/importing rows and buffering mini-player edges.
 */
@Composable
fun SeamLightBar(
    active: Boolean,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    if (!active) return

    val infiniteTransition = rememberInfiniteTransition(label = "seam_light")
    val phase by infiniteTransition.animateFloat(
        initialValue = -0.5f,
        targetValue = 1.5f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 1400, easing = LinearEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "seam_phase",
    )

    Box(
        modifier = modifier
            .fillMaxWidth()
            .height(2.dp)
            .background(
                brush = Brush.linearGradient(
                    colorStops = arrayOf(
                        (phase - 0.35f).coerceIn(0f, 1f) to Color.Transparent,
                        (phase - 0.08f).coerceIn(0f, 1f) to colors.accentGlow,
                        (phase + 0.14f).coerceIn(0f, 1f) to colors.leafGlow,
                        (phase + 0.38f).coerceIn(0f, 1f) to Color.Transparent,
                    ),
                ),
            ),
    )
}

/**
 * Loads artwork via Coil from [CoverArtContentProvider] (`content://<authority>/art/<coverArtId>`)
 * with zero network blocking, falling back to a refined Midnight Studio monogram tile.
 */
@Composable
fun TrackArtwork(
    coverArtId: String?,
    artAuthority: String,
    fallbackTitle: String,
    size: Dp,
    modifier: Modifier = Modifier,
    shape: Shape = SoulbeetShapes.xs,
    showBorder: Boolean = true,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val context = LocalContext.current
    val uri = remember(coverArtId, artAuthority) {
        coverArtId?.trim()?.takeIf { it.isNotEmpty() }?.let {
            CoverArtContentProvider.uriFor(artAuthority, it)
        }
    }

    val placeholder: @Composable (Boolean) -> Unit = { drawBorder ->
        Box(
            modifier = Modifier
                .size(size)
                .clip(shape)
                .background(
                    Brush.linearGradient(
                        colors = listOf(colors.surfaceHigh, colors.surfaceLow),
                        start = Offset.Zero,
                        end = Offset(120f, 120f),
                    ),
                )
                .then(
                    if (drawBorder) Modifier.border(1.dp, colors.borderSubtle, shape) else Modifier,
                ),
            contentAlignment = Alignment.Center,
        ) {
            val initial = fallbackTitle.trim().firstOrNull()?.uppercaseChar()?.toString()
            if (initial != null && size >= 44.dp) {
                Text(
                    text = initial,
                    style = if (size >= 120.dp) type.display else type.titleMd,
                    color = colors.textMuted,
                )
            } else {
                Icon(
                    imageVector = Icons.Filled.MusicNote,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size((size * 0.45f).coerceIn(16.dp, 40.dp)),
                )
            }
        }
    }

    if (uri == null) {
        Box(modifier = modifier) { placeholder(showBorder) }
    } else {
        SubcomposeAsyncImage(
            model = ImageRequest.Builder(context)
                .data(uri)
                .crossfade(true)
                .build(),
            contentDescription = fallbackTitle,
            contentScale = ContentScale.Crop,
            loading = { placeholder(false) },
            error = { placeholder(false) },
            modifier = modifier
                .size(size)
                .clip(shape)
                .then(
                    if (showBorder) Modifier.border(1.dp, colors.borderSubtle, shape) else Modifier,
                ),
        )
    }
}

@Composable
fun OfflineDot(modifier: Modifier = Modifier) {
    val colors = LocalSoulbeetColors.current
    Box(
        modifier = modifier
            .size(6.dp)
            .clip(CircleShape)
            .background(colors.leafGlow)
            .semantics { contentDescription = "Available offline" },
    )
}

/**
 * Compact gold star rating badge (e.g. `★ 4`) displayed on track rows and mini-player when rated.
 */
@Composable
fun CompactStarBadge(
    rating: Int,
    modifier: Modifier = Modifier,
) {
    if (rating <= 0) return
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    Row(
        modifier = modifier
            .semantics { contentDescription = "Rated $rating out of 5 stars" },
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Icon(
            imageVector = Icons.Filled.Star,
            contentDescription = null,
            tint = colors.gold,
            modifier = Modifier.size(12.dp),
        )
        Text(
            text = "$rating",
            style = type.monoXs,
            color = colors.gold,
            maxLines = 1,
            softWrap = false,
        )
    }
}

/**
 * Unified track list row supporting Owned Library, Pending Discovery (with inline Keep/Drop and
 * horizontal swipe triage), and Unowned Catalog (`0.78` opacity + `Acquire & Play` + `seam-light`).
 */
@Composable
fun TrackRow(
    title: String,
    artist: String,
    durationSecs: Long?,
    coverArtId: String?,
    artAuthority: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    isPlaying: Boolean = false,
    isOfflineCached: Boolean = false,
    isUnavailableOffline: Boolean = false,
    isPendingDiscovery: Boolean = false,
    discoveryExpiryLabel: String? = null,
    isStarred: Boolean = false,
    userRating: Int = 0,
    isUnownedCatalog: Boolean = false,
    isAcquiring: Boolean = false,
    acquiringLabel: String? = null,
    onStarClick: (() -> Unit)? = null,
    onKeepClick: (() -> Unit)? = null,
    onDropClick: (() -> Unit)? = null,
    onRemoveClick: (() -> Unit)? = null,
    removeLabel: String = "Remove from library",
    removeIcon: ImageVector = Icons.Filled.DeleteOutline,
    onAddToPlaylistClick: (() -> Unit)? = null,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    var dragOffsetX by remember(title, artist) { mutableFloatStateOf(0f) }
    val currentKeepClick by rememberUpdatedState(onKeepClick)
    val currentDropClick by rememberUpdatedState(onDropClick)

    val swipeModifier = if (isPendingDiscovery && (onKeepClick != null || onDropClick != null)) {
        Modifier.pointerInput(title, artist, isPendingDiscovery) {
            detectHorizontalDragGestures(
                onDragEnd = {
                    when {
                        dragOffsetX > 140f -> currentKeepClick?.invoke()
                        dragOffsetX < -140f -> currentDropClick?.invoke()
                    }
                    dragOffsetX = 0f
                },
                onDragCancel = { dragOffsetX = 0f },
                onHorizontalDrag = { _, dragAmount ->
                    dragOffsetX = (dragOffsetX + dragAmount).coerceIn(-220f, 220f)
                },
            )
        }
    } else {
        Modifier
    }

    val rowAlpha = when {
        isUnavailableOffline -> 0.42f
        isUnownedCatalog && !isAcquiring -> 0.78f
        else -> 1f
    }

    Column(modifier = modifier.fillMaxWidth()) {
        Box(modifier = Modifier.fillMaxWidth()) {
            // Subtle swipe background hint for Discovery rows
            if (isPendingDiscovery && dragOffsetX != 0f) {
                val isKeepHint = dragOffsetX > 0f
                Row(
                    modifier = Modifier
                        .matchParentSize()
                        .clip(SoulbeetShapes.sm)
                        .background(if (isKeepHint) colors.leafSurface else colors.crimsonSurface)
                        .padding(horizontal = 16.dp),
                    horizontalArrangement = if (isKeepHint) Arrangement.Start else Arrangement.End,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = if (isKeepHint) "KEEP" else "DROP",
                        style = type.monoXs,
                        color = if (isKeepHint) colors.leafText else colors.crimsonText,
                    )
                }
            }

            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .offset { IntOffset(dragOffsetX.roundToInt(), 0) }
                    .then(swipeModifier)
                    .heightIn(min = 60.dp)
                    .clip(SoulbeetShapes.sm)
                    .background(if (isPlaying) colors.surfaceLow else Color.Transparent)
                    .clickable(enabled = !isUnavailableOffline, onClick = onClick)
                    .alpha(rowAlpha)
                    .padding(horizontal = 12.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                TrackArtwork(
                    coverArtId = coverArtId,
                    artAuthority = artAuthority,
                    fallbackTitle = title,
                    size = 44.dp,
                    shape = SoulbeetShapes.xs,
                )

                Spacer(modifier = Modifier.width(12.dp))

                Column(
                    modifier = Modifier.weight(1f),
                    verticalArrangement = Arrangement.spacedBy(2.dp),
                ) {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(6.dp),
                    ) {
                        Text(
                            text = title,
                            style = type.body,
                            color = if (isPlaying) colors.leafText else colors.textPrimary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.weight(1f, fill = false),
                        )
                        if (isOfflineCached) {
                            OfflineDot()
                        }
                    }

                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(6.dp),
                    ) {
                        Text(
                            text = if (isUnavailableOffline) "$artist · Not cached" else artist,
                            style = type.bodySm,
                            color = colors.textSecondary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.weight(1f, fill = false),
                        )
                        if (userRating > 0) {
                            CompactStarBadge(rating = userRating)
                        }
                        if (isPendingDiscovery) {
                            Surface(
                                color = colors.accentSurface,
                                shape = SoulbeetShapes.full,
                            ) {
                                Text(
                                    text = discoveryExpiryLabel?.let { "DISC · $it" } ?: "DISCOVERY",
                                    style = type.monoXs,
                                    color = colors.accentText,
                                    modifier = Modifier.padding(horizontal = 6.dp, vertical = 2.dp),
                                )
                            }
                        }
                    }
                }

                Spacer(modifier = Modifier.width(8.dp))

                when {
                    isUnownedCatalog -> {
                        Surface(
                            onClick = onClick,
                            enabled = !isAcquiring,
                            modifier = Modifier.height(32.dp),
                            color = if (isAcquiring) colors.accentSurface else colors.surfaceHigh,
                            shape = SoulbeetShapes.full,
                            border = BorderStroke(
                                1.dp,
                                if (isAcquiring) colors.accentText.copy(alpha = 0.45f) else colors.borderDefault,
                            ),
                        ) {
                            Row(
                                modifier = Modifier.padding(horizontal = 10.dp),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(5.dp),
                            ) {
                                Icon(
                                    imageVector = Icons.Filled.CloudDownload,
                                    contentDescription = "Acquire and Play",
                                    tint = if (isAcquiring) colors.accentText else colors.leafText,
                                    modifier = Modifier.size(14.dp),
                                )
                                Text(
                                    text = if (isAcquiring) (acquiringLabel ?: "ACQUIRING…") else "ACQUIRE",
                                    style = type.monoXs,
                                    color = if (isAcquiring) colors.accentText else colors.textPrimary,
                                    maxLines = 1,
                                    softWrap = false,
                                )
                            }
                        }
                    }

                    isPendingDiscovery && onKeepClick != null && onDropClick != null -> {
                        IconButton(
                            onClick = onDropClick,
                            modifier = Modifier.size(48.dp),
                        ) {
                            Icon(
                                imageVector = Icons.Filled.Close,
                                contentDescription = "Drop Discovery track",
                                tint = colors.crimsonText,
                                modifier = Modifier.size(20.dp),
                            )
                        }
                        Spacer(modifier = Modifier.width(8.dp))
                        IconButton(
                            onClick = onKeepClick,
                            modifier = Modifier.size(48.dp),
                        ) {
                            Icon(
                                imageVector = Icons.Filled.Check,
                                contentDescription = "Keep Discovery track",
                                tint = colors.leafText,
                                modifier = Modifier.size(20.dp),
                            )
                        }
                    }

                    else -> {
                        if (durationSecs != null && durationSecs > 0L) {
                            Text(
                                text = FormattingUtils.formatDurationSecs(durationSecs),
                                style = type.monoSm,
                                color = colors.textMuted,
                            )
                        }
                        if (onStarClick != null) {
                            IconButton(
                                onClick = onStarClick,
                                modifier = Modifier.size(48.dp),
                            ) {
                                Icon(
                                    imageVector = if (isStarred) Icons.Filled.Favorite else Icons.Outlined.FavoriteBorder,
                                    contentDescription = if (isStarred) "Remove from favorites" else "Favorite track",
                                    tint = if (isStarred) colors.accentText else colors.textMuted,
                                    modifier = Modifier.size(20.dp),
                                )
                            }
                        }
                        if (onAddToPlaylistClick != null) {
                            var menuExpanded by remember { mutableStateOf(false) }
                            Box {
                                IconButton(
                                    onClick = { menuExpanded = true },
                                    modifier = Modifier.size(48.dp),
                                ) {
                                    Icon(
                                        imageVector = Icons.Filled.MoreVert,
                                        contentDescription = "Track options",
                                        tint = colors.textMuted,
                                        modifier = Modifier.size(20.dp),
                                    )
                                }
                                DropdownMenu(
                                    expanded = menuExpanded,
                                    onDismissRequest = { menuExpanded = false },
                                    containerColor = colors.surfaceHigh,
                                ) {
                                    DropdownMenuItem(
                                        text = {
                                            Text(
                                                text = "Add to playlist",
                                                style = type.bodySm,
                                                color = colors.textPrimary,
                                            )
                                        },
                                        leadingIcon = {
                                            Icon(
                                                imageVector = Icons.AutoMirrored.Filled.PlaylistAdd,
                                                contentDescription = null,
                                                tint = colors.leafText,
                                            )
                                        },
                                        onClick = {
                                            menuExpanded = false
                                            onAddToPlaylistClick()
                                        },
                                    )
                                    if (onRemoveClick != null) {
                                        DropdownMenuItem(
                                            text = {
                                                Text(
                                                    text = removeLabel,
                                                    style = type.bodySm,
                                                    color = colors.crimsonText,
                                                )
                                            },
                                            leadingIcon = {
                                                Icon(
                                                    imageVector = removeIcon,
                                                    contentDescription = null,
                                                    tint = colors.crimsonText,
                                                )
                                            },
                                            onClick = {
                                                menuExpanded = false
                                                onRemoveClick()
                                            },
                                        )
                                    }
                                }
                            }
                        } else if (onRemoveClick != null) {
                            IconButton(
                                onClick = onRemoveClick,
                                modifier = Modifier.size(48.dp),
                            ) {
                                Icon(
                                    imageVector = removeIcon,
                                    contentDescription = removeLabel,
                                    tint = colors.textMuted,
                                    modifier = Modifier.size(20.dp),
                                )
                            }
                        }
                    }
                }
            }
        }
        if (isAcquiring) {
            SeamLightBar(active = true)
        }
    }
}

/**
 * Interactive 1–5★ gold rating strip with 48dp touch targets.
 */
@Composable
fun StarStrip(
    rating: Int,
    onRate: (Int) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    Row(
        modifier = modifier,
        horizontalArrangement = Arrangement.spacedBy(2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        for (star in 1..5) {
            val filled = star <= rating
            IconButton(
                onClick = { onRate(if (star == rating) 0 else star) },
                modifier = Modifier.size(48.dp),
            ) {
                Icon(
                    imageVector = if (filled) Icons.Filled.Star else Icons.Outlined.StarOutline,
                    contentDescription = "Rate $star stars",
                    tint = if (filled) colors.gold else colors.textMuted,
                    modifier = Modifier.size(22.dp),
                )
            }
        }
    }
}

/**
 * 5-second non-blocking Undo snackbar with live linear shrinking countdown rail along bottom edge.
 */
@Composable
fun UndoDropSnackbar(
    pending: PendingDropAction,
    onUndo: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    var remainingFraction by remember(pending.trackId, pending.createdAtMs) {
        mutableFloatStateOf(1f)
    }

    LaunchedEffect(pending.trackId, pending.createdAtMs) {
        val total = pending.durationMs.coerceAtLeast(100L).toFloat()
        while (true) {
            val elapsed = (System.currentTimeMillis() - pending.createdAtMs).coerceAtLeast(0L)
            remainingFraction = (1f - (elapsed / total)).coerceIn(0f, 1f)
            if (remainingFraction <= 0f) break
            delay(50L)
        }
    }

    Surface(
        modifier = modifier
            .fillMaxWidth()
            .padding(horizontal = 12.dp),
        color = colors.surfaceHigh,
        shape = SoulbeetShapes.md,
        border = BorderStroke(1.dp, colors.borderDefault),
    ) {
        Column {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(start = 16.dp, end = 8.dp, top = 8.dp, bottom = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Column(modifier = Modifier.weight(1f)) {
                    val headline = when (pending.mode) {
                        PendingDropMode.DropDiscovery -> {
                            val origin = pending.sourceLabel?.let { " from $it" }.orEmpty()
                            "Dropped \"${pending.title}\"$origin"
                        }
                        PendingDropMode.DeleteFromLibrary -> "Deleting \"${pending.title}\" from Library"
                        PendingDropMode.UnstarAndHide -> "Hiding \"${pending.title}\" from your mixes"
                    }
                    Text(
                        text = headline,
                        style = type.body,
                        color = colors.textPrimary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        text = pending.artist,
                        style = type.bodySm,
                        color = colors.textSecondary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
                TextButton(
                    onClick = onUndo,
                    modifier = Modifier.heightIn(min = 48.dp),
                ) {
                    Text(
                        text = "UNDO",
                        style = type.monoSm,
                        color = colors.leafText,
                    )
                }
            }

            // Live shrinking countdown rail
            Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(3.dp)
                    .background(colors.surfaceLow),
            ) {
                Box(
                    modifier = Modifier
                        .fillMaxWidth(remainingFraction)
                        .height(3.dp)
                        .background(colors.crimsonText),
                )
            }
        }
    }
}

internal data class HeroScrimSpec(
    val horizontalStops: List<Pair<Float, Float>>,
    val verticalStops: List<Pair<Float, Float>>?,
)

internal fun seasonalHeroScrimSpec(hasPhotoArt: Boolean, isOled: Boolean): HeroScrimSpec =
    if (hasPhotoArt) {
        HeroScrimSpec(
            horizontalStops = listOf(
                0.0f to 0.94f,
                0.56f to 0.80f,
                1.0f to 0.48f,
            ),
            verticalStops = listOf(
                0.0f to 0.0f,
                0.45f to 0.0f,
                1.0f to 0.92f,
            ),
        )
    } else {
        HeroScrimSpec(
            horizontalStops = listOf(
                0.0f to if (isOled) 0.84f else 0.80f,
                0.54f to if (isOled) 0.50f else 0.46f,
                1.0f to 0.06f,
            ),
            verticalStops = null,
        )
    }

/**
 * Seasonal / Contextual Hero Card with adaptive OLED-aware scrim, live Culture Ratio Steering chips,
 * primary Play CTA, and Pin Offline toggle.
 */
@Composable
fun SeasonalHeroCard(
    mix: ContextualMixUi,
    isPinned: Boolean,
    steeringInFlight: Boolean,
    onPlay: () -> Unit,
    onTogglePin: () -> Unit,
    onSteer: (CultureSteeringOption) -> Unit,
    modifier: Modifier = Modifier,
    onOpenDetail: (() -> Unit)? = null,
    artworkUrl: String? = null,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val context = LocalContext.current
    val country = mix.countryCode ?: "LOCAL"
    val localPct = mix.localPct
    val intlPct = localPct?.let { (100 - it).coerceIn(0, 100) }

    val sleeveSpec = remember(mix.slug) { mixSleeveSpecFor(mix.slug) }
    val cleanArtworkUrl = remember(artworkUrl) { artworkUrl?.trim()?.takeIf { it.isNotEmpty() } }
    var photoLoaded by remember(cleanArtworkUrl) { mutableStateOf(false) }
    val hasPhotoArt = cleanArtworkUrl != null && photoLoaded
    val scrimSpec = remember(hasPhotoArt, colors.isOled) {
        seasonalHeroScrimSpec(hasPhotoArt = hasPhotoArt, isOled = colors.isOled)
    }
    val horizontalScrim = remember(scrimSpec, colors.canvas) {
        Brush.horizontalGradient(
            colorStops = scrimSpec.horizontalStops.map { (pos, alpha) ->
                pos to colors.canvas.copy(alpha = alpha)
            }.toTypedArray(),
        )
    }
    val verticalScrim = remember(scrimSpec, colors.canvas) {
        scrimSpec.verticalStops?.let { stops ->
            Brush.verticalGradient(
                colorStops = stops.map { (pos, alpha) ->
                    pos to if (alpha <= 0f) Color.Transparent else colors.canvas.copy(alpha = alpha)
                }.toTypedArray(),
            )
        }
    }

    Surface(
        onClick = { onOpenDetail?.invoke() },
        enabled = onOpenDetail != null,
        modifier = modifier.fillMaxWidth(),
        color = colors.surface,
        shape = SoulbeetShapes.lg,
        border = BorderStroke(1.dp, if (hasPhotoArt) colors.borderDefault else sleeveSpec.borderColor),
    ) {
        Box(modifier = Modifier.fillMaxWidth()) {
            if (cleanArtworkUrl != null) {
                SubcomposeAsyncImage(
                    model = ImageRequest.Builder(context)
                        .data(cleanArtworkUrl)
                        .crossfade(true)
                        .build(),
                    contentDescription = null,
                    contentScale = ContentScale.Crop,
                    onLoading = { photoLoaded = false },
                    onSuccess = { photoLoaded = true },
                    onError = { photoLoaded = false },
                    loading = {
                        MixSleeveBackdrop(
                            slug = mix.slug,
                            modifier = Modifier.matchParentSize(),
                        )
                    },
                    error = {
                        MixSleeveBackdrop(
                            slug = mix.slug,
                            modifier = Modifier.matchParentSize(),
                        )
                    },
                    modifier = Modifier.matchParentSize(),
                )
            } else {
                MixSleeveBackdrop(
                    slug = mix.slug,
                    modifier = Modifier.matchParentSize(),
                )
            }
            Box(
                modifier = Modifier
                    .matchParentSize()
                    .background(horizontalScrim)
                    .then(
                        if (verticalScrim != null) {
                            Modifier.background(verticalScrim)
                        } else {
                            Modifier
                        },
                    ),
            )
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(20.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Surface(
                        color = colors.accentSurface,
                        shape = SoulbeetShapes.full,
                    ) {
                        Text(
                            text = if (mix.kind == "seasonal") "SEASONAL PACK" else "CONTEXTUAL MIX",
                            style = type.monoXs,
                            color = colors.accentText,
                            modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
                        )
                    }
                    if (localPct != null && intlPct != null) {
                        Surface(
                            color = colors.surfaceHigh,
                            shape = SoulbeetShapes.full,
                            border = BorderStroke(1.dp, colors.borderSubtle),
                        ) {
                            Text(
                                text = "$localPct% $country · $intlPct% INTL",
                                style = type.monoXs,
                                color = colors.textSecondary,
                                modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
                            )
                        }
                    }
                }

                Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Text(
                        text = mix.title,
                        style = type.titleLg,
                        color = colors.textPrimary,
                    )
                    Text(
                        text = mix.subtitle,
                        style = type.bodySm,
                        color = colors.textSecondary,
                    )
                }

                if (mix.kind == "seasonal" || localPct != null) {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .horizontalScroll(rememberScrollState()),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        CultureSteeringOption.entries.forEach { option ->
                            val selected = mix.steering == option
                            val label = when (option) {
                                CultureSteeringOption.MORE_LOCAL -> "More $country"
                                CultureSteeringOption.BALANCED -> "Balanced"
                                CultureSteeringOption.MORE_INTERNATIONAL -> "More Intl"
                            }
                            Surface(
                                onClick = { onSteer(option) },
                                enabled = !steeringInFlight,
                                color = if (selected) colors.accentSurface else colors.surfaceHigh,
                                shape = SoulbeetShapes.full,
                                border = BorderStroke(
                                    1.dp,
                                    if (selected) colors.accentText.copy(alpha = 0.5f) else colors.borderSubtle,
                                ),
                                modifier = Modifier.heightIn(min = 48.dp),
                            ) {
                                Box(
                                    modifier = Modifier.padding(horizontal = 12.dp, vertical = 12.dp),
                                    contentAlignment = Alignment.Center,
                                ) {
                                    Text(
                                        text = label,
                                        style = type.monoXs,
                                        color = if (selected) colors.accentText else colors.textSecondary,
                                        maxLines = 1,
                                        softWrap = false,
                                    )
                                }
                            }
                        }
                    }
                }

                Row(
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Button(
                        onClick = onPlay,
                        colors = ButtonDefaults.buttonColors(
                            containerColor = colors.leafSolid,
                            contentColor = colors.leafInk,
                        ),
                        shape = SoulbeetShapes.full,
                        contentPadding = PaddingValues(horizontal = 20.dp, vertical = 12.dp),
                        modifier = Modifier.heightIn(min = 48.dp),
                    ) {
                        Icon(
                            imageVector = Icons.Filled.PlayArrow,
                            contentDescription = null,
                            modifier = Modifier.size(18.dp),
                        )
                        Spacer(modifier = Modifier.width(6.dp))
                        Text(text = "Play Mix", style = type.body)
                    }

                    OutlinedButton(
                        onClick = onTogglePin,
                        shape = SoulbeetShapes.full,
                        border = BorderStroke(
                            1.dp,
                            if (isPinned) colors.leafText.copy(alpha = 0.5f) else colors.borderDefault,
                        ),
                        colors = ButtonDefaults.outlinedButtonColors(
                            containerColor = if (isPinned) colors.leafSurface else Color.Transparent,
                            contentColor = if (isPinned) colors.leafText else colors.textPrimary,
                        ),
                        modifier = Modifier.heightIn(min = 48.dp),
                    ) {
                        Icon(
                            imageVector = Icons.Filled.OfflinePin,
                            contentDescription = null,
                            modifier = Modifier.size(18.dp),
                        )
                        Spacer(modifier = Modifier.width(6.dp))
                        Text(
                            text = if (isPinned) "Pinned Offline" else "Pin Offline",
                            style = type.bodySm,
                        )
                    }
                }

                if (mix.excludeFromCoreTaste) {
                    Text(
                        text = "Excluded from core taste profile until you tap Keep",
                        style = type.monoXs,
                        color = colors.textMuted,
                    )
                }
            }
        }
    }
}

/**
 * Multi-Listener Delete Guard bottom sheet shown before removing any permanent library track.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MultiListenerGuardSheet(
    preview: DeleteGuardPreviewUi,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = sheetState,
        containerColor = colors.surfaceHigh,
        contentColor = colors.textPrimary,
        scrimColor = colors.scrim,
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 24.dp)
                .padding(bottom = 32.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            val dropMode = preview.toPendingDropMode()
            val isHardDelete = dropMode == PendingDropMode.DeleteFromLibrary
            val isDiscoveryDrop = dropMode == PendingDropMode.DropDiscovery
            val householdListeners = preview.otherListeners.filter { it.isHouseholdListenerProtection }
            val otherBlocks = preview.otherListeners.filterNot { it.isHouseholdListenerProtection }

            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Box(
                    modifier = Modifier
                        .size(44.dp)
                        .clip(CircleShape)
                        .background(
                            if (isHardDelete || isDiscoveryDrop) {
                                colors.crimsonSurface
                            } else {
                                colors.leafSurface
                            },
                        ),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        imageVector = Icons.Filled.Shield,
                        contentDescription = null,
                        tint = if (isHardDelete || isDiscoveryDrop) colors.crimsonText else colors.leafText,
                    )
                }
                Column(modifier = Modifier.weight(1f)) {
                    val sheetTitle = when {
                        isDiscoveryDrop -> "Drop Discovery Track?"
                        isHardDelete -> "Remove from Library?"
                        householdListeners.isNotEmpty() -> "Shared Household Track"
                        else -> "Protected Library Track"
                    }
                    Text(
                        text = sheetTitle,
                        style = type.titleMd,
                        color = colors.textPrimary,
                    )
                    Text(
                        text = "${preview.trackTitle} · ${preview.trackArtist}",
                        style = type.bodySm,
                        color = colors.textSecondary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }

            when {
                isDiscoveryDrop -> {
                    Text(
                        text = "This track is still staged in Discovery. Dropping removes it from your triage queue after a 5-second Undo window.",
                        style = type.bodySm,
                        color = colors.textSecondary,
                    )
                }

                isHardDelete -> {
                    Text(
                        text = "You are the sole listener for this track. Confirming will schedule permanent deletion after a 5-second Undo window.",
                        style = type.bodySm,
                        color = colors.textSecondary,
                    )
                    if (preview.ownPlaylists.isNotEmpty()) {
                        Text(
                            text = "Also in your playlists: ${preview.ownPlaylists.joinToString(", ")}",
                            style = type.monoSm,
                            color = colors.textMuted,
                        )
                    }
                }

                householdListeners.isNotEmpty() -> {
                    Surface(
                        color = colors.surface,
                        shape = SoulbeetShapes.md,
                        border = BorderStroke(1.dp, colors.borderSubtle),
                    ) {
                        Column(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(14.dp),
                            verticalArrangement = Arrangement.spacedBy(10.dp),
                        ) {
                            Text(
                                text = "PROTECTED BY OTHER LISTENERS",
                                style = type.monoXs,
                                color = colors.leafText,
                            )
                            householdListeners.forEach { protection ->
                                Row(
                                    verticalAlignment = Alignment.CenterVertically,
                                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                                ) {
                                    Box(
                                        modifier = Modifier
                                            .size(32.dp)
                                            .clip(CircleShape)
                                            .background(colors.accentSurface),
                                        contentAlignment = Alignment.Center,
                                    ) {
                                        Text(
                                            text = protection.summary.trim().firstOrNull()?.uppercase() ?: "?",
                                            style = type.monoSm,
                                            color = colors.accentText,
                                        )
                                    }
                                    Text(
                                        text = protection.summary,
                                        style = type.bodySm,
                                        color = colors.textPrimary,
                                    )
                                }
                            }
                        }
                    }
                    Text(
                        text = "File kept on disk for other household listeners — confirming will unstar and rate 1★ to hide it from your mixes.",
                        style = type.bodySm,
                        color = colors.textSecondary,
                    )
                }

                else -> {
                    if (otherBlocks.isNotEmpty()) {
                        Surface(
                            color = colors.surface,
                            shape = SoulbeetShapes.md,
                            border = BorderStroke(1.dp, colors.borderSubtle),
                        ) {
                            Column(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(14.dp),
                                verticalArrangement = Arrangement.spacedBy(8.dp),
                            ) {
                                Text(
                                    text = "DELETION GUARD ACTIVE",
                                    style = type.monoXs,
                                    color = colors.leafText,
                                )
                                otherBlocks.forEach { protection ->
                                    Text(
                                        text = protection.summary,
                                        style = type.bodySm,
                                        color = colors.textPrimary,
                                    )
                                }
                            }
                        }
                    }
                    Text(
                        text = preview.blockReason
                            ?: "File is kept safe on disk — confirming will unstar and rate 1★ to hide it from your mixes.",
                        style = type.bodySm,
                        color = colors.textSecondary,
                    )
                }
            }

            HorizontalDivider(color = colors.borderSubtle)

            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                OutlinedButton(
                    onClick = onDismiss,
                    shape = SoulbeetShapes.full,
                    border = BorderStroke(1.dp, colors.borderDefault),
                    modifier = Modifier
                        .weight(1f)
                        .heightIn(min = 48.dp),
                ) {
                    Text(text = "Cancel", style = type.body, color = colors.textPrimary)
                }

                val confirmLabel = when (dropMode) {
                    PendingDropMode.DropDiscovery -> "Drop Track"
                    PendingDropMode.DeleteFromLibrary -> "Delete Track"
                    PendingDropMode.UnstarAndHide -> "Hide from My Mixes"
                }
                Button(
                    onClick = onConfirm,
                    shape = SoulbeetShapes.full,
                    colors = ButtonDefaults.buttonColors(
                        containerColor = if (isHardDelete || isDiscoveryDrop) {
                            colors.crimsonSolid
                        } else {
                            colors.accentSolid
                        },
                        contentColor = colors.textPrimary,
                    ),
                    modifier = Modifier
                        .weight(1f)
                        .heightIn(min = 48.dp),
                ) {
                    Text(
                        text = confirmLabel,
                        style = type.body,
                    )
                }
            }
        }
    }
}
