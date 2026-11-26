package org.soulbeet.app.ui.screens

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Star
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Slider
import androidx.compose.material3.SliderDefaults
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import org.soulbeet.app.AppUiState
import org.soulbeet.app.TriageCardItem
import org.soulbeet.app.data.ContextualMixUi
import org.soulbeet.app.data.CultureSteeringOption
import org.soulbeet.app.data.FormattingUtils
import org.soulbeet.app.data.UserCultureProfileUi
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.ui.components.SeasonalHeroCard
import org.soulbeet.app.ui.components.TrackArtwork
import org.soulbeet.core.FfiPinTarget
import kotlin.math.roundToInt

@Composable
fun DiscoverScreen(
    state: AppUiState,
    artAuthority: String,
    bottomContentPadding: PaddingValues,
    onPlayMix: (ContextualMixUi) -> Unit,
    onTogglePin: (target: FfiPinTarget, label: String) -> Unit,
    onSteerMix: (slug: String, option: CultureSteeringOption) -> Unit,
    onPreviewTriageCard: (TriageCardItem) -> Unit,
    onKeepTriageCard: (TriageCardItem) -> Unit,
    onStarKeepTriageCard: (TriageCardItem) -> Unit,
    onDropTriageCard: (TriageCardItem) -> Unit,
    onOpenCultureEditor: () -> Unit,
    onDismissCultureEditor: () -> Unit,
    onSaveCultureProfile: (homeCountry: String?, ratio: Float) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current

    LazyColumn(
        modifier = modifier.fillMaxSize(),
        contentPadding = PaddingValues(
            start = 16.dp,
            end = 16.dp,
            top = 12.dp,
            bottom = bottomContentPadding.calculateBottomPadding() + 24.dp,
        ),
        verticalArrangement = Arrangement.spacedBy(24.dp),
    ) {
        // 1. Active Seasonal Pack with Live Culture Steering
        state.seasonalHeroMix?.let { hero ->
            item(key = "discover_seasonal_hero") {
                val pinTarget = if (hero.navidromePlaylistId != null) {
                    FfiPinTarget.Playlist(hero.navidromePlaylistId)
                } else {
                    FfiPinTarget.SeasonalPack(hero.slug)
                }
                val isPinned = state.pinnedTargets.any { it.target == pinTarget }
                SeasonalHeroCard(
                    mix = hero,
                    isPinned = isPinned,
                    steeringInFlight = state.steeringInFlight,
                    onPlay = { onPlayMix(hero) },
                    onTogglePin = { onTogglePin(pinTarget, hero.title) },
                    onSteer = { option -> onSteerMix(hero.slug, option) },
                )
            }
        }

        // 2. Rolling & Moment Mixes
        if (state.rollingAndMomentMixes.isNotEmpty()) {
            item(key = "rolling_mixes") {
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(
                        text = "Rolling & Moment Mixes",
                        style = type.titleMd,
                        color = colors.textPrimary,
                    )
                    LazyRow(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                        items(state.rollingAndMomentMixes, key = { it.slug }) { mix ->
                            Surface(
                                color = colors.surface,
                                shape = SoulbeetShapes.md,
                                border = BorderStroke(1.dp, colors.borderSubtle),
                                modifier = Modifier
                                    .width(200.dp)
                                    .clickable { onPlayMix(mix) },
                            ) {
                                Column(
                                    modifier = Modifier.padding(16.dp),
                                    verticalArrangement = Arrangement.spacedBy(8.dp),
                                ) {
                                    Surface(
                                        color = colors.surfaceHigh,
                                        shape = SoulbeetShapes.full,
                                    ) {
                                        Text(
                                            text = mix.kind.uppercase(),
                                            style = type.monoXs,
                                            color = colors.leafText,
                                            modifier = Modifier.padding(horizontal = 8.dp, vertical = 3.dp),
                                        )
                                    }
                                    Text(
                                        text = mix.title,
                                        style = type.titleMd,
                                        color = colors.textPrimary,
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                    Text(
                                        text = mix.subtitle,
                                        style = type.bodySm,
                                        color = colors.textSecondary,
                                        maxLines = 2,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                    Text(
                                        text = "${mix.trackCount} tracks · ${mix.ownedPct}% library",
                                        style = type.monoXs,
                                        color = colors.textMuted,
                                    )
                                }
                            }
                        }
                    }
                }
            }
        }

        // 3. Fast Triage Swipeable Card Deck
        item(key = "fast_triage_deck") {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(
                        text = "Fast Triage Deck",
                        style = type.titleMd,
                        color = colors.textPrimary,
                    )
                    Text(
                        text = "${state.triageDeck.size} PENDING",
                        style = type.monoXs,
                        color = colors.accentText,
                    )
                }

                val topCard = state.triageDeck.firstOrNull()
                if (topCard == null) {
                    Surface(
                        color = colors.surface,
                        shape = SoulbeetShapes.lg,
                        border = BorderStroke(1.dp, colors.borderSubtle),
                    ) {
                        Column(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(24.dp),
                            horizontalAlignment = Alignment.CenterHorizontally,
                            verticalArrangement = Arrangement.spacedBy(6.dp),
                        ) {
                            Text(
                                text = "All caught up",
                                style = type.titleMd,
                                color = colors.textPrimary,
                            )
                            Text(
                                text = "Every pending discovery track has received a verdict.",
                                style = type.bodySm,
                                color = colors.textSecondary,
                            )
                        }
                    }
                } else {
                    SwipeableTriageCard(
                        item = topCard,
                        artAuthority = artAuthority,
                        isPlaying = state.playback.currentTrackId == topCard.discovery.songId && state.playback.isPlaying,
                        onPreview = { onPreviewTriageCard(topCard) },
                        onKeep = { onKeepTriageCard(topCard) },
                        onStarAndKeep = { onStarKeepTriageCard(topCard) },
                        onDrop = { onDropTriageCard(topCard) },
                    )
                }
            }
        }

        // 4. Your Taste DNA Card
        item(key = "taste_dna") {
            TasteDnaCard(
                profile = state.cultureProfile,
                onEditClick = onOpenCultureEditor,
            )
        }
    }

    if (state.showCultureEditor) {
        CultureProfileEditorDialog(
            initial = state.cultureProfile,
            onDismiss = onDismissCultureEditor,
            onSave = onSaveCultureProfile,
        )
    }
}

@Composable
private fun SwipeableTriageCard(
    item: TriageCardItem,
    artAuthority: String,
    isPlaying: Boolean,
    onPreview: () -> Unit,
    onKeep: () -> Unit,
    onStarAndKeep: () -> Unit,
    onDrop: () -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    var dragX by remember(item.discovery.id) { mutableFloatStateOf(0f) }

    val reasonText = remember(item.discovery.reasonJson) {
        FormattingUtils.parseDiscoveryReason(item.discovery.reasonJson)
            ?: item.discovery.mixSlug?.replace('_', ' ')?.uppercase()
            ?: "DISCOVERY PICK"
    }
    val expiryText = remember(item.discovery.expiresAt) {
        FormattingUtils.formatExpiryCountdown(item.discovery.expiresAt)
    }

    Surface(
        color = colors.surface,
        shape = SoulbeetShapes.lg,
        border = BorderStroke(
            width = 1.dp,
            color = when {
                dragX > 60f -> colors.leafText.copy(alpha = 0.7f)
                dragX < -60f -> colors.crimsonText.copy(alpha = 0.7f)
                else -> colors.borderDefault
            },
        ),
        modifier = Modifier
            .fillMaxWidth()
            .offset { IntOffset(dragX.roundToInt(), 0) }
            .rotate((dragX / 45f).coerceIn(-8f, 8f))
            .pointerInput(item.discovery.id) {
                detectHorizontalDragGestures(
                    onDragEnd = {
                        when {
                            dragX > 160f -> onKeep()
                            dragX < -160f -> onDrop()
                        }
                        dragX = 0f
                    },
                    onDragCancel = { dragX = 0f },
                    onHorizontalDrag = { _, delta ->
                        dragX = (dragX + delta).coerceIn(-280f, 280f)
                    },
                )
            },
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Surface(
                    color = colors.accentSurface,
                    shape = SoulbeetShapes.full,
                ) {
                    Text(
                        text = reasonText.uppercase(),
                        style = type.monoXs,
                        color = colors.accentText,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.padding(horizontal = 10.dp, vertical = 4.dp),
                    )
                }
                if (expiryText != null) {
                    Text(
                        text = expiryText,
                        style = type.monoXs,
                        color = colors.textMuted,
                    )
                }
            }

            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(16.dp),
            ) {
                Box(contentAlignment = Alignment.Center) {
                    TrackArtwork(
                        coverArtId = item.track?.coverArtId,
                        artAuthority = artAuthority,
                        fallbackTitle = item.discovery.title,
                        size = 84.dp,
                        shape = SoulbeetShapes.md,
                    )
                    Surface(
                        color = colors.scrim,
                        shape = CircleShape,
                        modifier = Modifier
                            .size(48.dp)
                            .clickable(onClick = onPreview),
                    ) {
                        Box(contentAlignment = Alignment.Center) {
                            Icon(
                                imageVector = Icons.Filled.PlayArrow,
                                contentDescription = if (isPlaying) "Playing preview" else "Play preview",
                                tint = if (isPlaying) colors.leafGlow else colors.textPrimary,
                                modifier = Modifier.size(26.dp),
                            )
                        }
                    }
                }

                Column(
                    modifier = Modifier.weight(1f),
                    verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    Text(
                        text = item.discovery.title,
                        style = type.titleMd,
                        color = colors.textPrimary,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        text = item.discovery.artist,
                        style = type.body,
                        color = colors.textSecondary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    val albumName = item.discovery.album
                    if (!albumName.isNullOrBlank()) {
                        Text(
                            text = albumName,
                            style = type.bodySm,
                            color = colors.textMuted,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
            }

            if (dragX > 60f || dragX < -60f) {
                val keepSwipe = dragX > 0f
                Text(
                    text = if (keepSwipe) "RELEASE TO KEEP IN LIBRARY" else "RELEASE TO DROP (5S UNDO)",
                    style = type.monoXs,
                    color = if (keepSwipe) colors.leafText else colors.crimsonText,
                )
            } else {
                Text(
                    text = "Swipe right to Keep · Swipe left to Drop",
                    style = type.monoXs,
                    color = colors.textMuted,
                )
            }

            // Action row (>= 48dp touch targets, >= 8dp spacing between Drop & Keep)
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Surface(
                    color = colors.crimsonSurface,
                    shape = SoulbeetShapes.full,
                    modifier = Modifier
                        .weight(1f)
                        .heightIn(min = 48.dp)
                        .clickable(onClick = onDrop),
                ) {
                    Row(
                        modifier = Modifier.padding(horizontal = 12.dp, vertical = 12.dp),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Icon(
                            imageVector = Icons.Filled.Close,
                            contentDescription = "Drop",
                            tint = colors.crimsonText,
                            modifier = Modifier.size(18.dp),
                        )
                        Spacer(modifier = Modifier.width(6.dp))
                        Text(text = "Drop", style = type.body, color = colors.crimsonText)
                    }
                }

                IconButton(
                    onClick = onStarAndKeep,
                    modifier = Modifier
                        .size(48.dp)
                        .clip(CircleShape)
                        .background(colors.goldSurface),
                ) {
                    Icon(
                        imageVector = Icons.Filled.Star,
                        contentDescription = "Star and Keep",
                        tint = colors.gold,
                    )
                }

                Button(
                    onClick = onKeep,
                    colors = ButtonDefaults.buttonColors(
                        containerColor = colors.leafSolid,
                        contentColor = colors.leafInk,
                    ),
                    shape = SoulbeetShapes.full,
                    modifier = Modifier
                        .weight(1.2f)
                        .heightIn(min = 48.dp),
                ) {
                    Icon(
                        imageVector = Icons.Filled.Check,
                        contentDescription = null,
                        modifier = Modifier.size(18.dp),
                    )
                    Spacer(modifier = Modifier.width(6.dp))
                    Text(text = "Keep", style = type.body)
                }
            }
        }
    }
}

@Composable
private fun TasteDnaCard(
    profile: UserCultureProfileUi?,
    onEditClick: () -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val weights = profile?.inferredAreaWeights.orEmpty()
    val summary = remember(weights, profile) {
        if (weights.isNotEmpty()) {
            FormattingUtils.formatTasteDnaSummary(weights)
        } else {
            val localPct = ((profile?.localCultureRatio ?: 0.45f) * 100f).roundToInt()
            val country = profile?.homeCountry ?: "LOCAL"
            "$country $localPct% · INTL ${100 - localPct}%"
        }
    }

    Surface(
        color = colors.surface,
        shape = SoulbeetShapes.lg,
        border = BorderStroke(1.dp, colors.borderSubtle),
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onEditClick),
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = "Your Taste DNA",
                    style = type.titleMd,
                    color = colors.textPrimary,
                )
                Icon(
                    imageVector = Icons.Filled.Tune,
                    contentDescription = "Edit culture profile",
                    tint = colors.textMuted,
                    modifier = Modifier.size(20.dp),
                )
            }

            // Visual horizontal locale breakdown bar
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(8.dp)
                    .clip(SoulbeetShapes.full)
                    .background(colors.surfaceHigh),
                horizontalArrangement = Arrangement.spacedBy(2.dp),
            ) {
                val barSegments = if (weights.isNotEmpty()) {
                    weights.take(4)
                } else {
                    val ratio = (profile?.localCultureRatio ?: 0.45f).coerceIn(0.05f, 0.95f)
                    listOf("LOCAL" to ratio, "INTL" to (1f - ratio))
                }
                val palette = listOf(colors.leafSolid, colors.accentText, colors.gold, colors.borderStrong)
                barSegments.forEachIndexed { idx, (_, weight) ->
                    Box(
                        modifier = Modifier
                            .weight(weight.coerceAtLeast(0.05f))
                            .height(8.dp)
                            .background(palette[idx % palette.size]),
                    )
                }
            }

            Text(
                text = summary,
                style = type.monoSm,
                color = colors.textSecondary,
            )

            if (!profile?.inferredLocalTags.isNullOrEmpty()) {
                Text(
                    text = profile!!.inferredLocalTags.take(5).joinToString(" · "),
                    style = type.bodySm,
                    color = colors.textMuted,
                )
            }
        }
    }
}

@Composable
private fun CultureProfileEditorDialog(
    initial: UserCultureProfileUi?,
    onDismiss: () -> Unit,
    onSave: (homeCountry: String?, ratio: Float) -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    var country by remember(initial?.homeCountry) { mutableStateOf(initial?.homeCountry ?: "") }
    var ratio by remember(initial?.localCultureRatio) {
        mutableFloatStateOf(initial?.localCultureRatio ?: 0.45f)
    }

    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = colors.surfaceHigh,
        title = {
            Text(text = "Edit Taste DNA", style = type.titleMd, color = colors.textPrimary)
        },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                OutlinedTextField(
                    value = country,
                    onValueChange = { country = it.take(2).uppercase() },
                    label = { Text("Home Country (ISO-2, e.g. FR, US, JP)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Column {
                    val pct = (ratio * 100f).roundToInt()
                    Text(
                        text = "Local Culture Ratio: $pct% Local · ${100 - pct}% Intl",
                        style = type.monoSm,
                        color = colors.leafText,
                    )
                    Slider(
                        value = ratio,
                        onValueChange = { ratio = it },
                        colors = SliderDefaults.colors(
                            thumbColor = colors.leafSolid,
                            activeTrackColor = colors.leafSolid,
                            inactiveTrackColor = colors.surfaceLow,
                        ),
                    )
                }
            }
        },
        confirmButton = {
            TextButton(onClick = { onSave(country.takeIf { it.isNotBlank() }, ratio) }) {
                Text("Save", color = colors.leafText)
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text("Cancel", color = colors.textSecondary)
            }
        },
    )
}
