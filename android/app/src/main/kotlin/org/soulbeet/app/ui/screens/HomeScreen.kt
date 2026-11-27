package org.soulbeet.app.ui.screens

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowForward
import androidx.compose.material.icons.filled.AutoAwesome
import androidx.compose.material.icons.filled.CalendarMonth
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Schedule
import androidx.compose.material3.Icon
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.soulbeet.app.AppUiState
import org.soulbeet.app.QuickResumeItem
import org.soulbeet.app.data.ContextualMixUi
import org.soulbeet.app.data.CultureSteeringOption
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.ui.components.OfflineDot
import org.soulbeet.app.ui.components.SeasonalHeroCard
import org.soulbeet.app.ui.components.TrackArtwork
import org.soulbeet.app.ui.components.TrackRow
import org.soulbeet.core.FfiPinTarget
import org.soulbeet.core.FfiPlaylist
import org.soulbeet.core.FfiTrack

@Composable
fun HomeScreen(
    state: AppUiState,
    artAuthority: String,
    bottomContentPadding: PaddingValues,
    onPlayContainer: (mediaId: String, title: String) -> Unit,
    onPlayMix: (ContextualMixUi) -> Unit,
    onOpenTriage: () -> Unit,
    onSteerMix: (slug: String, option: CultureSteeringOption) -> Unit,
    onTogglePin: (target: FfiPinTarget, label: String) -> Unit,
    onOpenPlaylist: (FfiPlaylist) -> Unit,
    onPlayTrack: (FfiTrack) -> Unit,
    onStarTrack: (FfiTrack) -> Unit,
    onTriggerSync: () -> Unit,
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
        // 1. Quick-Resume 2x3 Compact Grid
        if (state.quickResume.isNotEmpty()) {
            item(key = "quick_resume") {
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(
                        text = "Jump Back In",
                        style = type.titleMd,
                        color = colors.textPrimary,
                    )
                    val rows = state.quickResume.chunked(2)
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        rows.forEach { pair ->
                            Row(
                                modifier = Modifier.fillMaxWidth(),
                                horizontalArrangement = Arrangement.spacedBy(8.dp),
                            ) {
                                pair.forEach { item ->
                                    QuickResumeTile(
                                        item = item,
                                        artAuthority = artAuthority,
                                        onClick = { onPlayContainer(item.containerMediaId, item.title) },
                                        modifier = Modifier.weight(1f),
                                    )
                                }
                                if (pair.size == 1) {
                                    Spacer(modifier = Modifier.weight(1f))
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. Seasonal / Contextual Hero Card
        state.seasonalHeroMix?.let { hero ->
            item(key = "seasonal_hero_${hero.slug}") {
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

        // 3. Pending Discovery Triage Banner
        if (state.pendingDiscoveryCount > 0) {
            item(key = "triage_banner") {
                Surface(
                    color = colors.accentSurface,
                    shape = SoulbeetShapes.md,
                    border = BorderStroke(1.dp, colors.accentText.copy(alpha = 0.35f)),
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(min = 56.dp)
                        .clickable(onClick = onOpenTriage),
                ) {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(horizontal = 16.dp, vertical = 14.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.SpaceBetween,
                    ) {
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(12.dp),
                            modifier = Modifier.weight(1f),
                        ) {
                            Icon(
                                imageVector = Icons.Filled.AutoAwesome,
                                contentDescription = null,
                                tint = colors.accentText,
                                modifier = Modifier.size(20.dp),
                            )
                            Column {
                                Text(
                                    text = "${state.pendingDiscoveryCount} discovery tracks awaiting verdict",
                                    style = type.body,
                                    color = colors.textPrimary,
                                )
                                Text(
                                    text = "Swipe Keep or Drop in Fast Triage",
                                    style = type.bodySm,
                                    color = colors.accentText,
                                )
                            }
                        }
                        Icon(
                            imageVector = Icons.AutoMirrored.Filled.ArrowForward,
                            contentDescription = "Open Triage",
                            tint = colors.accentText,
                        )
                    }
                }
            }
        }

        // 4. Your Playlists Shelf (focused on user playlists; falls back to mixes excluding seasonalHeroMix)
        val homeShelfPlaylists = state.userPlaylists.ifEmpty { state.homeFallbackMixPlaylists }
        if (homeShelfPlaylists.isNotEmpty()) {
            val isMixFallback = state.userPlaylists.isEmpty()
            item(key = "playlists_shelf") {
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(
                        text = if (isMixFallback) "Discovery & Mixes" else "Your Playlists",
                        style = type.titleMd,
                        color = colors.textPrimary,
                    )
                    LazyRow(
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        items(homeShelfPlaylists.take(12), key = { it.id }) { pl ->
                            val pinned = state.pinnedTargets.any { it.target == FfiPinTarget.Playlist(pl.id) }
                            val isSmartMix = pl.kind != "user" || pl.isSmart
                            Surface(
                                color = colors.surface,
                                shape = SoulbeetShapes.md,
                                border = BorderStroke(1.dp, colors.borderSubtle),
                                modifier = Modifier
                                    .width(148.dp)
                                    .clickable { onOpenPlaylist(pl) },
                            ) {
                                Column(
                                    modifier = Modifier.padding(10.dp),
                                    verticalArrangement = Arrangement.spacedBy(8.dp),
                                ) {
                                    TrackArtwork(
                                        coverArtId = pl.coverArtId,
                                        artAuthority = artAuthority,
                                        fallbackTitle = pl.name,
                                        size = 128.dp,
                                        shape = SoulbeetShapes.sm,
                                    )
                                    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                                        Row(
                                            verticalAlignment = Alignment.CenterVertically,
                                            horizontalArrangement = Arrangement.spacedBy(6.dp),
                                        ) {
                                            Text(
                                                text = pl.name,
                                                style = type.body,
                                                color = colors.textPrimary,
                                                maxLines = 1,
                                                overflow = TextOverflow.Ellipsis,
                                                modifier = Modifier.weight(1f, fill = false),
                                            )
                                            if (pinned) OfflineDot()
                                        }
                                        Row(
                                            verticalAlignment = Alignment.CenterVertically,
                                            horizontalArrangement = Arrangement.spacedBy(6.dp),
                                        ) {
                                            Text(
                                                text = "${pl.songCount} tracks",
                                                style = type.monoSm,
                                                color = colors.textMuted,
                                                maxLines = 1,
                                                overflow = TextOverflow.Ellipsis,
                                                modifier = Modifier.weight(1f, fill = false),
                                            )
                                            if (isSmartMix) {
                                                val (tagLabel, tagIcon, tagColor) = when (pl.kind) {
                                                    "seasonal" -> Triple("SEASONAL", Icons.Filled.CalendarMonth, colors.gold)
                                                    "moment" -> Triple("MOMENT", Icons.Filled.Schedule, colors.leafText)
                                                    "discovery" -> Triple("ROLLING", Icons.Filled.AutoAwesome, colors.accentText)
                                                    else -> Triple("SMART", Icons.Filled.AutoAwesome, colors.accentText)
                                                }
                                                Row(
                                                    verticalAlignment = Alignment.CenterVertically,
                                                    horizontalArrangement = Arrangement.spacedBy(3.dp),
                                                ) {
                                                    Icon(
                                                        imageVector = tagIcon,
                                                        contentDescription = null,
                                                        tint = tagColor,
                                                        modifier = Modifier.size(11.dp),
                                                    )
                                                    Text(
                                                        text = tagLabel,
                                                        style = type.monoXs,
                                                        color = tagColor,
                                                        maxLines = 1,
                                                        softWrap = false,
                                                    )
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 5. Recent & Starred Tracks
        if (state.recentTracks.isNotEmpty()) {
            item(key = "recent_header") {
                Text(
                    text = "Quick Play",
                    style = type.titleMd,
                    color = colors.textPrimary,
                )
            }
            items(state.recentTracks, key = { it.id }) { track ->
                val isCached = state.offlineTrackIds.contains(track.id)
                TrackRow(
                    title = track.title,
                    artist = track.artistName,
                    durationSecs = track.durationSecs,
                    coverArtId = track.coverArtId,
                    artAuthority = artAuthority,
                    isPlaying = state.playback.currentTrackId == track.id,
                    isOfflineCached = isCached,
                    isUnavailableOffline = state.config.offlineOnly && !isCached,
                    isPendingDiscovery = track.isDiscovery,
                    isStarred = track.starred,
                    userRating = track.userRating.toInt(),
                    onClick = { onPlayTrack(track) },
                    onStarClick = { onStarTrack(track) },
                )
            }
        }

        // Empty state when freshly connected with zero synced items
        if (state.quickResume.isEmpty() && state.playlists.isEmpty() && state.recentTracks.isEmpty()) {
            item(key = "home_empty") {
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
                        verticalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        Text(
                            text = "Your studio is ready",
                            style = type.titleMd,
                            color = colors.textPrimary,
                        )
                        Text(
                            text = "Sync your Navidrome library to populate Jump Back In, Discovery Triage, and offline playlists.",
                            style = type.bodySm,
                            color = colors.textSecondary,
                        )
                        TextButton(
                            onClick = onTriggerSync,
                            modifier = Modifier.heightIn(min = 48.dp),
                        ) {
                            Icon(
                                imageVector = Icons.Filled.Refresh,
                                contentDescription = null,
                                tint = colors.leafText,
                            )
                            Spacer(modifier = Modifier.width(6.dp))
                            Text(
                                text = if (state.isSyncing) "Syncing…" else "Sync Library Now",
                                style = type.body,
                                color = colors.leafText,
                            )
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun QuickResumeTile(
    item: QuickResumeItem,
    artAuthority: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    Surface(
        color = colors.surface,
        shape = SoulbeetShapes.sm,
        border = BorderStroke(1.dp, colors.borderSubtle),
        modifier = modifier
            .height(56.dp)
            .clickable(onClick = onClick),
    ) {
        Row(
            modifier = Modifier.fillMaxSize(),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            TrackArtwork(
                coverArtId = item.coverArtId,
                artAuthority = artAuthority,
                fallbackTitle = item.title,
                size = 56.dp,
                shape = SoulbeetShapes.sm,
            )
            Column(
                modifier = Modifier
                    .weight(1f)
                    .padding(horizontal = 10.dp),
                verticalArrangement = Arrangement.spacedBy(2.dp),
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    Text(
                        text = item.title,
                        style = type.bodySm,
                        color = colors.textPrimary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f, fill = false),
                    )
                    if (item.isOfflineReady) OfflineDot()
                }
                Text(
                    text = item.subtitle,
                    style = type.monoXs,
                    color = colors.textMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            Icon(
                imageVector = Icons.Filled.PlayArrow,
                contentDescription = null,
                tint = colors.textMuted,
                modifier = Modifier
                    .padding(end = 8.dp)
                    .size(16.dp),
            )
        }
    }
}
