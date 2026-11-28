package org.soulbeet.app.ui.screens

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
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
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.QueueMusic
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.AutoAwesome
import androidx.compose.material.icons.filled.CalendarMonth
import androidx.compose.material.icons.filled.CloudOff
import androidx.compose.material.icons.filled.DeleteOutline
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.OfflinePin
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.RemoveCircleOutline
import androidx.compose.material.icons.filled.Schedule
import androidx.compose.material.icons.outlined.OfflinePin
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.soulbeet.app.AppUiState
import org.soulbeet.app.ContainerDetailUi
import org.soulbeet.app.ContainerKind
import org.soulbeet.app.LibraryCategory
import org.soulbeet.app.TriageCardItem
import org.soulbeet.app.data.FormattingUtils
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.ui.components.ArtworkFallbackKind
import org.soulbeet.app.ui.components.PlaylistArtwork
import org.soulbeet.app.ui.components.TrackArtwork
import org.soulbeet.app.ui.components.TrackRow
import org.soulbeet.core.FfiAlbum
import org.soulbeet.core.FfiArtist
import org.soulbeet.core.FfiDiscoveryTrack
import org.soulbeet.core.FfiPinTarget
import org.soulbeet.core.FfiPlaylist
import org.soulbeet.core.FfiTrack

@Composable
fun LibraryScreen(
    state: AppUiState,
    artAuthority: String,
    bottomContentPadding: PaddingValues,
    onToggleOfflineOnly: () -> Unit,
    onSelectCategory: (LibraryCategory) -> Unit,
    onOpenPlaylist: (FfiPlaylist) -> Unit,
    onOpenAlbum: (FfiAlbum) -> Unit,
    onOpenArtist: (FfiArtist) -> Unit,
    onTogglePin: (target: FfiPinTarget, label: String) -> Unit,
    onPlayQueue: (tracks: List<FfiTrack>, startIndex: Int, sourceLabel: String) -> Unit,
    onPlayTriageCard: (TriageCardItem) -> Unit,
    onStarTrack: (FfiTrack) -> Unit,
    onRemoveTrack: (FfiTrack) -> Unit,
    onAddToPlaylist: (FfiTrack) -> Unit,
    onCreatePlaylist: (name: String, comment: String?) -> Unit,
    onKeepDiscovery: (FfiDiscoveryTrack) -> Unit,
    onDropDiscovery: (FfiDiscoveryTrack) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val cacheStats = state.cacheStats
    val usedBytes = cacheStats?.totalAudioBytes?.toLong() ?: 0L
    val maxBytes = cacheStats?.maxAudioBytes?.toLong() ?: (4L * 1024L * 1024L * 1024L)
    val budgetFrac = FormattingUtils.cacheBudgetFraction(usedBytes, maxBytes)
    var showCreatePlaylistDialog by remember { mutableStateOf(false) }

    LazyColumn(
        modifier = modifier.fillMaxSize(),
        contentPadding = PaddingValues(
            start = 16.dp,
            end = 16.dp,
            top = 12.dp,
            bottom = bottomContentPadding.calculateBottomPadding() + 24.dp,
        ),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        // 1. Pinned Offline Only Switch + Cache Disk Budget Meter
        item(key = "offline_budget_card") {
            val offlineGold = colors.gold
            val offlineGoldBorder = colors.gold.copy(alpha = 0.45f)
            val offlineGoldInk = colors.leafInk
            Surface(
                color = colors.surface,
                shape = SoulbeetShapes.lg,
                border = BorderStroke(
                    1.dp,
                    if (state.config.offlineOnly) offlineGoldBorder else colors.borderSubtle,
                ),
            ) {
                Column(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(16.dp),
                    verticalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(10.dp),
                            modifier = Modifier.weight(1f),
                        ) {
                            Icon(
                                imageVector = Icons.Filled.CloudOff,
                                contentDescription = null,
                                tint = if (state.config.offlineOnly) offlineGold else colors.textMuted,
                            )
                            Column {
                                Text(
                                    text = "Offline Only Mode",
                                    style = type.body,
                                    color = colors.textPrimary,
                                )
                                Text(
                                    text = if (state.config.offlineOnly) {
                                        "Playing locally cached audio only"
                                    } else {
                                        "Streaming + local offline cache"
                                    },
                                    style = type.bodySm,
                                    color = colors.textSecondary,
                                )
                            }
                        }
                        Switch(
                            checked = state.config.offlineOnly,
                            onCheckedChange = { onToggleOfflineOnly() },
                            colors = SwitchDefaults.colors(
                                checkedThumbColor = offlineGoldInk,
                                checkedTrackColor = offlineGold,
                                uncheckedThumbColor = colors.textMuted,
                                uncheckedTrackColor = colors.surfaceHigh,
                            ),
                        )
                    }

                    // Cache Disk Budget Meter
                    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                        ) {
                            Text(
                                text = "OFFLINE CACHE · ${cacheStats?.cachedTrackCount ?: 0} TRACKS",
                                style = type.monoXs,
                                color = colors.textMuted,
                            )
                            Text(
                                text = FormattingUtils.formatCacheBudget(usedBytes, maxBytes),
                                style = type.monoXs,
                                color = colors.leafText,
                            )
                        }
                        Box(
                            modifier = Modifier
                                .fillMaxWidth()
                                .height(6.dp)
                                .clip(SoulbeetShapes.full)
                                .background(colors.surfaceHigh),
                        ) {
                            Box(
                                modifier = Modifier
                                    .fillMaxWidth(budgetFrac.coerceAtLeast(if (usedBytes > 0L) 0.02f else 0f))
                                    .height(6.dp)
                                    .background(colors.leafSolid),
                            )
                        }
                    }
                }
            }
        }

        // 2. Category Filter Chips
        item(key = "library_categories") {
            LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                items(LibraryCategory.entries) { cat ->
                    val selected = state.libraryCategory == cat
                    Surface(
                        onClick = { onSelectCategory(cat) },
                        color = if (selected) colors.leafSurface else colors.surfaceHigh,
                        shape = SoulbeetShapes.full,
                        border = BorderStroke(
                            1.dp,
                            if (selected) colors.leafText.copy(alpha = 0.45f) else colors.borderSubtle,
                        ),
                        modifier = Modifier.heightIn(min = 48.dp),
                    ) {
                        Box(
                            modifier = Modifier.padding(horizontal = 14.dp, vertical = 12.dp),
                            contentAlignment = Alignment.Center,
                        ) {
                            Text(
                                text = cat.label,
                                style = type.bodySm,
                                color = if (selected) colors.leafText else colors.textSecondary,
                            )
                        }
                    }
                }
            }
        }

        // 3. Category Contents
        when (state.libraryCategory) {
            LibraryCategory.PLAYLISTS -> {
                val userPlaylists = state.userPlaylists
                val discoveryAndMixPlaylists = state.playlists.filter { pl ->
                    pl.kind != "user" && !(pl.kind == "seasonal" && pl.songCount == 0L)
                }
                item(key = "section_user_playlists") {
                    PlaylistSectionHeader(
                        icon = Icons.AutoMirrored.Filled.QueueMusic,
                        iconTint = colors.leafText,
                        title = "Your Playlists",
                        count = userPlaylists.size,
                        actionLabel = "New Playlist",
                        actionEnabled = !state.config.offlineOnly && !state.playlistMutationInFlight,
                        onActionClick = { showCreatePlaylistDialog = true },
                    )
                }
                if (userPlaylists.isEmpty()) {
                    item(key = "empty_user_playlists") {
                        EmptyLibraryNotice("No custom playlists yet. Tap + New Playlist to create one.")
                    }
                } else {
                    items(userPlaylists, key = { "user_${it.id}" }) { pl ->
                        val target = FfiPinTarget.Playlist(pl.id)
                        val isPinned = state.pinnedTargets.any { it.target == target }
                        PinnableContainerRow(
                            title = pl.name,
                            subtitle = "${pl.songCount} tracks",
                            coverArtId = pl.coverArtId,
                            artAuthority = artAuthority,
                            isPinned = isPinned,
                            fallbackKind = ArtworkFallbackKind.PLAYLIST,
                            playlistSlug = pl.slug,
                            playlistKind = pl.kind,
                            onClick = { onOpenPlaylist(pl) },
                            onTogglePin = { onTogglePin(target, pl.name) },
                        )
                    }
                }
                if (discoveryAndMixPlaylists.isNotEmpty()) {
                    item(key = "section_discovery_mixes") {
                        PlaylistSectionHeader(
                            icon = Icons.Filled.AutoAwesome,
                            iconTint = colors.accentText,
                            title = "Discovery & Mixes",
                            count = discoveryAndMixPlaylists.size,
                        )
                    }
                    items(discoveryAndMixPlaylists, key = { "mix_${it.id}" }) { pl ->
                        val target = FfiPinTarget.Playlist(pl.id)
                        val isPinned = state.pinnedTargets.any { it.target == target }
                        val (tagLabel, tagIcon, tagTint, tagBg) = when (pl.kind) {
                            "seasonal" -> Quad(
                                "SEASONAL",
                                Icons.Filled.CalendarMonth,
                                colors.gold,
                                colors.goldSurface,
                            )
                            "moment" -> Quad(
                                "MOMENT",
                                Icons.Filled.Schedule,
                                colors.leafText,
                                colors.leafSurface,
                            )
                            "discovery" -> Quad(
                                "ROLLING",
                                Icons.Filled.AutoAwesome,
                                colors.accentText,
                                colors.accentSurface,
                            )
                            else -> Quad(
                                "SMART",
                                Icons.Filled.AutoAwesome,
                                colors.accentText,
                                colors.accentSurface,
                            )
                        }
                        PinnableContainerRow(
                            title = pl.name,
                            subtitle = "${pl.songCount} tracks",
                            coverArtId = pl.coverArtId,
                            artAuthority = artAuthority,
                            isPinned = isPinned,
                            fallbackKind = ArtworkFallbackKind.PLAYLIST,
                            playlistSlug = pl.slug,
                            playlistKind = pl.kind,
                            badgeLabel = tagLabel,
                            badgeIcon = tagIcon,
                            badgeColor = tagTint,
                            badgeBackground = tagBg,
                            onClick = { onOpenPlaylist(pl) },
                            onTogglePin = { onTogglePin(target, pl.name) },
                        )
                    }
                }
            }

            LibraryCategory.ARTISTS -> {
                if (state.artists.isEmpty()) {
                    item(key = "empty_artists") { EmptyLibraryNotice("No artists synced yet.") }
                } else {
                    items(state.artists, key = { it.id }) { artist ->
                        val target = FfiPinTarget.Artist(artist.id)
                        val isPinned = state.pinnedTargets.any { it.target == target }
                        PinnableContainerRow(
                            title = artist.name,
                            subtitle = "${artist.albumCount} albums",
                            coverArtId = artist.coverArtId,
                            artAuthority = artAuthority,
                            isPinned = isPinned,
                            circularArt = true,
                            fallbackKind = ArtworkFallbackKind.ARTIST,
                            onClick = { onOpenArtist(artist) },
                            onTogglePin = { onTogglePin(target, artist.name) },
                        )
                    }
                }
            }

            LibraryCategory.ALBUMS -> {
                if (state.albums.isEmpty()) {
                    item(key = "empty_albums") { EmptyLibraryNotice("No albums synced yet.") }
                } else {
                    items(state.albums, key = { it.id }) { album ->
                        val target = FfiPinTarget.Album(album.id)
                        val isPinned = state.pinnedTargets.any { it.target == target }
                        PinnableContainerRow(
                            title = album.name,
                            subtitle = buildString {
                                append(album.artistName)
                                album.year?.let { append(" · $it") }
                            },
                            coverArtId = album.coverArtId,
                            artAuthority = artAuthority,
                            isPinned = isPinned,
                            fallbackKind = ArtworkFallbackKind.ALBUM,
                            onClick = { onOpenAlbum(album) },
                            onTogglePin = { onTogglePin(target, album.name) },
                        )
                    }
                }
            }

            LibraryCategory.STARRED -> {
                if (state.starredTracks.isEmpty()) {
                    item(key = "empty_starred") { EmptyLibraryNotice("No starred tracks yet.") }
                } else {
                    itemsIndexed(state.starredTracks, key = { _, t -> "star_${t.id}" }) { index, track ->
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
                            isStarred = true,
                            userRating = track.userRating.toInt(),
                            onClick = { onPlayQueue(state.starredTracks, index, "Starred") },
                            onStarClick = { onStarTrack(track) },
                            onRemoveClick = { onRemoveTrack(track) },
                            onAddToPlaylistClick = { onAddToPlaylist(track) },
                        )
                    }
                }
            }

            LibraryCategory.DISCOVERY -> {
                if (state.triageDeck.isEmpty()) {
                    item(key = "empty_discovery") { EmptyLibraryNotice("No pending discovery tracks.") }
                } else {
                    items(state.triageDeck, key = { "disc_${it.discovery.id}" }) { card ->
                        val disc = card.discovery
                        val songId = disc.songId
                        val isCached = card.isOfflineCached
                        TrackRow(
                            title = disc.title,
                            artist = disc.artist,
                            durationSecs = card.track?.durationSecs,
                            coverArtId = card.track?.coverArtId,
                            artAuthority = artAuthority,
                            isPlaying = songId != null && state.playback.currentTrackId == songId,
                            isOfflineCached = isCached,
                            isUnavailableOffline = state.config.offlineOnly && !isCached,
                            isPendingDiscovery = true,
                            discoveryExpiryLabel = FormattingUtils.formatExpiryCountdown(disc.expiresAt),
                            userRating = card.track?.userRating?.toInt() ?: 0,
                            onClick = {
                                onPlayTriageCard(card)
                            },
                            onKeepClick = { onKeepDiscovery(disc) },
                            onDropClick = { onDropDiscovery(disc) },
                        )
                    }
                }
            }

            LibraryCategory.OFFLINE -> {
                if (state.offlineTracks.isEmpty()) {
                    item(key = "empty_offline") {
                        EmptyLibraryNotice("Pin any playlist or album to download tracks for offline listening.")
                    }
                } else {
                    itemsIndexed(state.offlineTracks, key = { _, t -> "off_${t.id}" }) { index, track ->
                        TrackRow(
                            title = track.title,
                            artist = track.artistName,
                            durationSecs = track.durationSecs,
                            coverArtId = track.coverArtId,
                            artAuthority = artAuthority,
                            isPlaying = state.playback.currentTrackId == track.id,
                            isOfflineCached = true,
                            isStarred = track.starred,
                            userRating = track.userRating.toInt(),
                            onClick = { onPlayQueue(state.offlineTracks, index, "Cached") },
                            onStarClick = { onStarTrack(track) },
                            onRemoveClick = { onRemoveTrack(track) },
                            onAddToPlaylistClick = { onAddToPlaylist(track) },
                        )
                    }
                }
            }
        }
    }

    if (showCreatePlaylistDialog) {
        PlaylistFormDialog(
            title = "New Playlist",
            confirmLabel = "Create",
            initialName = "",
            initialComment = "",
            isBusy = state.playlistMutationInFlight,
            onDismiss = { showCreatePlaylistDialog = false },
            onConfirm = { name, comment ->
                showCreatePlaylistDialog = false
                onCreatePlaylist(name, comment)
            },
        )
    }
}

private data class Quad<A, B, C, D>(val first: A, val second: B, val third: C, val fourth: D)

@Composable
private fun PlaylistSectionHeader(
    icon: ImageVector,
    iconTint: Color,
    title: String,
    count: Int,
    actionLabel: String? = null,
    actionEnabled: Boolean = true,
    onActionClick: (() -> Unit)? = null,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(top = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            modifier = Modifier.weight(1f),
        ) {
            Icon(
                imageVector = icon,
                contentDescription = null,
                tint = iconTint,
                modifier = Modifier.size(18.dp),
            )
            Text(
                text = title,
                style = type.titleMd,
                color = colors.textPrimary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                text = "$count",
                style = type.monoXs,
                color = colors.textMuted,
            )
        }
        if (actionLabel != null && onActionClick != null) {
            Surface(
                onClick = onActionClick,
                enabled = actionEnabled,
                color = colors.leafSurface,
                shape = SoulbeetShapes.full,
                border = BorderStroke(1.dp, colors.leafText.copy(alpha = 0.4f)),
                modifier = Modifier.heightIn(min = 36.dp),
            ) {
                Row(
                    modifier = Modifier.padding(horizontal = 10.dp, vertical = 6.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    Icon(
                        imageVector = Icons.Filled.Add,
                        contentDescription = null,
                        tint = if (actionEnabled) colors.leafText else colors.textDisabled,
                        modifier = Modifier.size(15.dp),
                    )
                    Text(
                        text = actionLabel,
                        style = type.monoXs,
                        color = if (actionEnabled) colors.leafText else colors.textDisabled,
                        maxLines = 1,
                        softWrap = false,
                    )
                }
            }
        }
    }
}

@Composable
private fun PinnableContainerRow(
    title: String,
    subtitle: String,
    coverArtId: String?,
    artAuthority: String,
    isPinned: Boolean,
    circularArt: Boolean = false,
    fallbackKind: ArtworkFallbackKind = ArtworkFallbackKind.ALBUM,
    playlistSlug: String? = null,
    playlistKind: String? = null,
    badgeLabel: String? = null,
    badgeIcon: ImageVector? = null,
    badgeColor: Color? = null,
    badgeBackground: Color? = null,
    onClick: () -> Unit,
    onTogglePin: () -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 64.dp)
            .clip(SoulbeetShapes.sm)
            .clickable(onClick = onClick)
            .padding(horizontal = 4.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (fallbackKind == ArtworkFallbackKind.PLAYLIST) {
            PlaylistArtwork(
                name = title,
                coverArtId = coverArtId,
                artAuthority = artAuthority,
                size = 48.dp,
                slug = playlistSlug,
                kind = playlistKind,
                shape = SoulbeetShapes.xs,
            )
        } else {
            TrackArtwork(
                coverArtId = coverArtId,
                artAuthority = artAuthority,
                fallbackTitle = title,
                size = 48.dp,
                shape = if (circularArt) SoulbeetShapes.full else SoulbeetShapes.xs,
                fallbackKind = fallbackKind,
            )
        }
        Spacer(modifier = Modifier.width(12.dp))
        Column(modifier = Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(
                text = title,
                style = type.body,
                color = colors.textPrimary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                Text(
                    text = subtitle,
                    style = type.bodySm,
                    color = colors.textSecondary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f, fill = false),
                )
                if (badgeLabel != null) {
                    val tint = badgeColor ?: colors.accentText
                    val bg = badgeBackground ?: colors.accentSurface
                    Surface(
                        color = bg,
                        shape = SoulbeetShapes.full,
                    ) {
                        Row(
                            modifier = Modifier.padding(horizontal = 6.dp, vertical = 2.dp),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(3.dp),
                        ) {
                            if (badgeIcon != null) {
                                Icon(
                                    imageVector = badgeIcon,
                                    contentDescription = null,
                                    tint = tint,
                                    modifier = Modifier.size(11.dp),
                                )
                            }
                            Text(
                                text = badgeLabel,
                                style = type.monoXs,
                                color = tint,
                                maxLines = 1,
                                softWrap = false,
                            )
                        }
                    }
                }
            }
        }
        IconButton(
            onClick = onTogglePin,
            modifier = Modifier.size(48.dp),
        ) {
            Icon(
                imageVector = if (isPinned) Icons.Filled.OfflinePin else Icons.Outlined.OfflinePin,
                contentDescription = if (isPinned) "Unpin offline" else "Pin offline",
                tint = if (isPinned) colors.leafText else colors.textMuted,
            )
        }
    }
}

@Composable
private fun EmptyLibraryNotice(message: String) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    Surface(
        color = colors.surface,
        shape = SoulbeetShapes.md,
        border = BorderStroke(1.dp, colors.borderSubtle),
    ) {
        Text(
            text = message,
            style = type.bodySm,
            color = colors.textSecondary,
            modifier = Modifier
                .fillMaxWidth()
                .padding(20.dp),
        )
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun ContainerDetailSheet(
    detail: ContainerDetailUi,
    state: AppUiState,
    artAuthority: String,
    onDismiss: () -> Unit,
    onPlayAll: () -> Unit,
    onPlayIndex: (Int) -> Unit,
    onTogglePin: () -> Unit,
    onStarTrack: (FfiTrack) -> Unit,
    onAddToPlaylist: (FfiTrack) -> Unit,
    onRemoveFromPlaylist: (FfiTrack) -> Unit,
    onRemoveFromLibrary: (FfiTrack) -> Unit,
    onRenamePlaylist: (name: String, comment: String?) -> Unit,
    onDeletePlaylist: () -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    var showOverflowMenu by remember(detail.id) { mutableStateOf(false) }
    var showRenameDialog by remember(detail.id) { mutableStateOf(false) }
    var showDeleteConfirm by remember(detail.id) { mutableStateOf(false) }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = sheetState,
        containerColor = colors.surfaceLow,
        contentColor = colors.textPrimary,
        scrimColor = colors.scrim,
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 16.dp)
                .padding(bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(14.dp),
            ) {
                if (detail.kind == ContainerKind.USER_PLAYLIST || detail.kind == ContainerKind.SMART_PLAYLIST) {
                    PlaylistArtwork(
                        name = detail.title,
                        coverArtId = detail.coverArtId,
                        artAuthority = artAuthority,
                        size = 72.dp,
                        slug = detail.slug,
                        kind = detail.playlistKind,
                        shape = SoulbeetShapes.md,
                    )
                } else {
                    TrackArtwork(
                        coverArtId = detail.coverArtId,
                        artAuthority = artAuthority,
                        fallbackTitle = detail.title,
                        size = 72.dp,
                        shape = if (detail.kind == ContainerKind.ARTIST) SoulbeetShapes.full else SoulbeetShapes.md,
                        fallbackKind = if (detail.kind == ContainerKind.ARTIST) {
                            ArtworkFallbackKind.ARTIST
                        } else {
                            ArtworkFallbackKind.ALBUM
                        },
                    )
                }
                Column(modifier = Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Text(
                        text = detail.title,
                        style = type.titleMd,
                        color = colors.textPrimary,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        text = detail.subtitle,
                        style = type.bodySm,
                        color = colors.textSecondary,
                    )
                    if (!detail.comment.isNullOrBlank()) {
                        Text(
                            text = detail.comment,
                            style = type.bodySm,
                            color = colors.textMuted,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                if (detail.isStaticUserPlaylist) {
                    Box {
                        IconButton(
                            onClick = { showOverflowMenu = true },
                            modifier = Modifier.size(48.dp),
                        ) {
                            Icon(
                                imageVector = Icons.Filled.MoreVert,
                                contentDescription = "Playlist actions",
                                tint = colors.textSecondary,
                            )
                        }
                        DropdownMenu(
                            expanded = showOverflowMenu,
                            onDismissRequest = { showOverflowMenu = false },
                            containerColor = colors.surfaceHigh,
                        ) {
                            DropdownMenuItem(
                                text = {
                                    Text(
                                        text = "Rename / Edit",
                                        style = type.bodySm,
                                        color = colors.textPrimary,
                                    )
                                },
                                leadingIcon = {
                                    Icon(
                                        imageVector = Icons.Filled.Edit,
                                        contentDescription = null,
                                        tint = colors.leafText,
                                    )
                                },
                                onClick = {
                                    showOverflowMenu = false
                                    showRenameDialog = true
                                },
                            )
                            DropdownMenuItem(
                                text = {
                                    Text(
                                        text = "Delete Playlist",
                                        style = type.bodySm,
                                        color = colors.crimsonText,
                                    )
                                },
                                leadingIcon = {
                                    Icon(
                                        imageVector = Icons.Filled.DeleteOutline,
                                        contentDescription = null,
                                        tint = colors.crimsonText,
                                    )
                                },
                                onClick = {
                                    showOverflowMenu = false
                                    showDeleteConfirm = true
                                },
                            )
                        }
                    }
                }
            }

            Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                Button(
                    onClick = onPlayAll,
                    colors = ButtonDefaults.buttonColors(
                        containerColor = colors.leafSolid,
                        contentColor = colors.leafInk,
                    ),
                    shape = SoulbeetShapes.full,
                    modifier = Modifier.heightIn(min = 48.dp),
                ) {
                    Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(6.dp))
                    Text("Play All", style = type.body)
                }

                if (detail.pinTarget != null) {
                    OutlinedButton(
                        onClick = onTogglePin,
                        shape = SoulbeetShapes.full,
                        border = BorderStroke(
                            1.dp,
                            if (detail.isPinned) colors.leafText.copy(alpha = 0.5f) else colors.borderDefault,
                        ),
                        modifier = Modifier.heightIn(min = 48.dp),
                    ) {
                        Icon(
                            imageVector = if (detail.isPinned) Icons.Filled.OfflinePin else Icons.Outlined.OfflinePin,
                            contentDescription = null,
                            tint = if (detail.isPinned) colors.leafText else colors.textPrimary,
                            modifier = Modifier.size(18.dp),
                        )
                        Spacer(Modifier.width(6.dp))
                        Text(
                            text = if (detail.isPinned) "Pinned" else "Pin Offline",
                            style = type.bodySm,
                            color = if (detail.isPinned) colors.leafText else colors.textPrimary,
                        )
                    }
                }
            }

            LazyColumn(
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(max = 420.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                itemsIndexed(detail.tracks, key = { idx, t -> "cd_${idx}_${t.id}" }) { idx, track ->
                    val isCached = state.offlineTrackIds.contains(track.id)
                    val removeAction: (() -> Unit)? = when {
                        detail.isStaticUserPlaylist -> ({ onRemoveFromPlaylist(track) })
                        detail.isSmartOrSystemPlaylist -> null
                        else -> ({ onRemoveFromLibrary(track) })
                    }
                    val removeLabel = if (detail.isStaticUserPlaylist) {
                        "Remove from playlist"
                    } else {
                        "Remove from library"
                    }
                    val removeIcon = if (detail.isStaticUserPlaylist) {
                        Icons.Filled.RemoveCircleOutline
                    } else {
                        Icons.Filled.DeleteOutline
                    }
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
                        onClick = { onPlayIndex(idx) },
                        onStarClick = { onStarTrack(track) },
                        onRemoveClick = removeAction,
                        removeLabel = removeLabel,
                        removeIcon = removeIcon,
                        onAddToPlaylistClick = { onAddToPlaylist(track) },
                    )
                }
            }
        }
    }

    if (showRenameDialog && detail.isStaticUserPlaylist) {
        PlaylistFormDialog(
            title = "Edit Playlist",
            confirmLabel = "Save",
            initialName = detail.title,
            initialComment = detail.comment.orEmpty(),
            isBusy = state.playlistMutationInFlight,
            onDismiss = { showRenameDialog = false },
            onConfirm = { name, comment ->
                showRenameDialog = false
                onRenamePlaylist(name, comment)
            },
        )
    }

    if (showDeleteConfirm && detail.isStaticUserPlaylist) {
        AlertDialog(
            onDismissRequest = { showDeleteConfirm = false },
            containerColor = colors.surfaceHigh,
            title = {
                Text(
                    text = "Delete Playlist?",
                    style = type.titleMd,
                    color = colors.textPrimary,
                )
            },
            text = {
                Text(
                    text = "Delete \"${detail.title}\"? Your audio files stay safe in your library.",
                    style = type.bodySm,
                    color = colors.textSecondary,
                )
            },
            confirmButton = {
                TextButton(
                    onClick = {
                        showDeleteConfirm = false
                        onDeletePlaylist()
                    },
                    enabled = !state.playlistMutationInFlight,
                ) {
                    Text("Delete", color = colors.crimsonText)
                }
            },
            dismissButton = {
                TextButton(onClick = { showDeleteConfirm = false }) {
                    Text("Cancel", color = colors.textSecondary)
                }
            },
        )
    }
}
