package org.soulbeet.app.ui.screens

import android.Manifest
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Favorite
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
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
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import org.soulbeet.app.AppUiState
import org.soulbeet.app.TriageCardItem
import org.soulbeet.app.data.CoarseLocationProvider
import org.soulbeet.app.data.ContextualMixUi
import org.soulbeet.app.data.CultureSteeringOption
import org.soulbeet.app.data.FormattingUtils
import org.soulbeet.app.data.UserCultureProfileUi
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.ui.components.MixShelfBucket
import org.soulbeet.app.ui.components.MixSleeveCard
import org.soulbeet.app.ui.components.MixSleeveLayout
import org.soulbeet.app.ui.components.SeasonalHeroCard
import org.soulbeet.app.ui.components.TrackArtwork
import org.soulbeet.app.ui.components.classifyMixShelf
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
    onSaveCultureProfile: (homeCountry: String?, ratio: Float, weatherCity: String?) -> Unit,
    liveWeatherLocationEnabled: Boolean,
    onToggleLiveWeatherLocation: (Boolean) -> Unit,
    onShowMessage: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val context = LocalContext.current
    val locationPermissionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.RequestPermission(),
    ) { granted ->
        if (granted) {
            onToggleLiveWeatherLocation(true)
        } else {
            onToggleLiveWeatherLocation(false)
            onShowMessage("Location permission denied. Using saved city or IP for weather mixes.")
        }
    }

    val (rightNowMixes, collectionMixes) = remember(state.rollingAndMomentMixes) {
        state.rollingAndMomentMixes.partition { classifyMixShelf(it) == MixShelfBucket.RIGHT_NOW }
    }

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

        // 2. Split-Shelf Curated Mixes ("Right Now" wide sleeves + "From Your Collection" square sleeves)
        if (rightNowMixes.isNotEmpty()) {
            item(key = "right_now_mixes") {
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            text = "Right Now",
                            style = type.titleMd,
                            color = colors.textPrimary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.weight(1f),
                        )
                        Text(
                            text = "LIVE WEATHER & TIME",
                            style = type.monoXs,
                            color = colors.textMuted,
                            maxLines = 1,
                            softWrap = false,
                        )
                    }
                    LazyRow(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                        items(rightNowMixes, key = { it.slug }) { mix ->
                            MixSleeveCard(
                                mix = mix,
                                layout = MixSleeveLayout.WIDE,
                                onClick = { onPlayMix(mix) },
                            )
                        }
                    }
                }
            }
        }

        if (collectionMixes.isNotEmpty()) {
            item(key = "collection_mixes") {
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            text = "From Your Collection",
                            style = type.titleMd,
                            color = colors.textPrimary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.weight(1f),
                        )
                        Text(
                            text = "LIBRARY & DECADES",
                            style = type.monoXs,
                            color = colors.textMuted,
                            maxLines = 1,
                            softWrap = false,
                        )
                    }
                    LazyRow(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                        items(collectionMixes, key = { it.slug }) { mix ->
                            MixSleeveCard(
                                mix = mix,
                                layout = MixSleeveLayout.SQUARE,
                                onClick = { onPlayMix(mix) },
                            )
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

                val topCard = state.playback.currentTrackId?.let { curId ->
                    state.triageDeck.firstOrNull { it.discovery.songId == curId }
                } ?: state.triageDeck.firstOrNull()
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
                    val isCardBusy = topCard.discovery.id in state.inFlightTriageIds ||
                        (topCard.discovery.songId != null && topCard.discovery.songId in state.inFlightTriageIds)
                    SwipeableTriageCard(
                        item = topCard,
                        artAuthority = artAuthority,
                        isPlaying = topCard.discovery.songId != null &&
                            state.playback.currentTrackId == topCard.discovery.songId &&
                            state.playback.isPlaying,
                        isActionInFlight = isCardBusy,
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
                liveWeatherLocationEnabled = liveWeatherLocationEnabled,
                onEditClick = onOpenCultureEditor,
            )
        }
    }

    if (state.showCultureEditor) {
        CultureProfileEditorDialog(
            initial = state.cultureProfile,
            liveWeatherLocationEnabled = liveWeatherLocationEnabled,
            onToggleLiveWeatherLocation = { enabled ->
                if (!enabled) {
                    onToggleLiveWeatherLocation(false)
                } else if (CoarseLocationProvider.hasCoarseLocationPermission(context)) {
                    onToggleLiveWeatherLocation(true)
                } else {
                    locationPermissionLauncher.launch(Manifest.permission.ACCESS_COARSE_LOCATION)
                }
            },
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
    isActionInFlight: Boolean,
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
            .pointerInput(item.discovery.id, isActionInFlight) {
                if (isActionInFlight) return@pointerInput
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
                                imageVector = if (isPlaying) Icons.Filled.Pause else Icons.Filled.PlayArrow,
                                contentDescription = if (isPlaying) "Pause triage track" else "Play triage queue",
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
                        .clickable(enabled = !isActionInFlight, onClick = onDrop),
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
                    enabled = !isActionInFlight,
                    modifier = Modifier
                        .size(48.dp)
                        .clip(CircleShape)
                        .background(colors.accentSurface),
                ) {
                    Icon(
                        imageVector = Icons.Filled.Favorite,
                        contentDescription = "Favorite and Keep",
                        tint = colors.accentText,
                    )
                }

                Button(
                    onClick = onKeep,
                    enabled = !isActionInFlight,
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
    liveWeatherLocationEnabled: Boolean,
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

            val weatherLabel = when {
                liveWeatherLocationEnabled -> "Weather: Live (Approx.)"
                !profile?.weatherCity.isNullOrBlank() -> "Weather: ${profile!!.weatherCity!!.trim()}"
                else -> "Weather: Auto (IP)"
            }
            Text(
                text = weatherLabel,
                style = type.monoXs,
                color = colors.textMuted,
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
    liveWeatherLocationEnabled: Boolean,
    onToggleLiveWeatherLocation: (Boolean) -> Unit,
    onDismiss: () -> Unit,
    onSave: (homeCountry: String?, ratio: Float, weatherCity: String?) -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    var country by remember(initial?.homeCountry) { mutableStateOf(initial?.homeCountry ?: "") }
    var weatherCity by remember(initial?.weatherCity) { mutableStateOf(initial?.weatherCity ?: "") }
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
            Column(
                modifier = Modifier.verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(16.dp),
            ) {
                OutlinedTextField(
                    value = country,
                    onValueChange = { country = it.take(2).uppercase() },
                    label = { Text("Home Country (ISO-2, e.g. FR, US, JP)") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = weatherCity,
                    onValueChange = { weatherCity = it.take(100) },
                    label = { Text("Weather City (blank = auto by IP)") },
                    placeholder = { Text("e.g. Lyon") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(min = 52.dp),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(
                        modifier = Modifier
                            .weight(1f)
                            .padding(end = 12.dp),
                        verticalArrangement = Arrangement.spacedBy(2.dp),
                    ) {
                        Text(
                            text = "Live Approximate Location",
                            style = type.body,
                            color = colors.textPrimary,
                        )
                        Text(
                            text = "Updates Rainy/Snow/Storm/Sunny mixes as you drive (~11 km accuracy, foreground only)",
                            style = type.bodySm,
                            color = colors.textSecondary,
                        )
                    }
                    Switch(
                        checked = liveWeatherLocationEnabled,
                        onCheckedChange = onToggleLiveWeatherLocation,
                        colors = SwitchDefaults.colors(
                            checkedThumbColor = colors.leafInk,
                            checkedTrackColor = colors.leafSolid,
                            uncheckedThumbColor = colors.textMuted,
                            uncheckedTrackColor = colors.surface,
                        ),
                    )
                }
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
            TextButton(
                onClick = {
                    val trimmedCity = weatherCity.trim()
                    val existingCity = initial?.weatherCity?.trim().orEmpty()
                    onSave(
                        country.takeIf { it.isNotBlank() },
                        ratio,
                        if (trimmedCity.equals(existingCity, ignoreCase = true)) null else trimmedCity,
                    )
                },
            ) {
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
