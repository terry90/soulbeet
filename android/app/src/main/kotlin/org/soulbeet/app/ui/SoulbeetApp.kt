package org.soulbeet.app.ui

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectDragGestures
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
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.CloudDownload
import androidx.compose.material.icons.filled.CloudOff
import androidx.compose.material.icons.filled.Explore
import androidx.compose.material.icons.filled.Favorite
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.LibraryMusic
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.outlined.Explore
import androidx.compose.material.icons.outlined.FavoriteBorder
import androidx.compose.material.icons.outlined.Home
import androidx.compose.material.icons.outlined.LibraryMusic
import androidx.compose.material.icons.outlined.Search
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationBarItemDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import org.soulbeet.app.AppTab
import org.soulbeet.app.AppUiState
import org.soulbeet.app.MainViewModel
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.theme.SoulbeetTheme
import org.soulbeet.app.ui.components.MultiListenerGuardSheet
import org.soulbeet.app.ui.components.SeamLightBar
import org.soulbeet.app.ui.components.SoulbeetPixelLogo
import org.soulbeet.app.ui.components.TrackArtwork
import org.soulbeet.app.ui.components.UndoDropSnackbar
import org.soulbeet.app.ui.screens.DiscoverScreen
import org.soulbeet.app.ui.screens.HomeScreen
import org.soulbeet.app.ui.screens.LibraryScreen
import org.soulbeet.app.ui.screens.NowPlayingSheet
import org.soulbeet.app.ui.screens.OnboardingScreen
import org.soulbeet.app.ui.screens.SearchScreen
import org.soulbeet.app.ui.screens.SettingsSheet
import kotlin.math.abs

@Composable
fun SoulbeetApp(viewModel: MainViewModel) {
    val state by viewModel.uiState.collectAsStateWithLifecycle()

    SoulbeetTheme(oledMode = state.config.oledMode) {
        val colors = LocalSoulbeetColors.current

        when {
            state.isBooting -> {
                Box(
                    modifier = Modifier
                        .fillMaxSize()
                        .background(colors.canvas),
                    contentAlignment = Alignment.Center,
                ) {
                    Column(
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(16.dp),
                    ) {
                        SoulbeetPixelLogo(size = 64.dp)
                        CircularProgressIndicator(
                            color = colors.leafSolid,
                            strokeWidth = 2.dp,
                            modifier = Modifier.size(28.dp),
                        )
                    }
                }
            }

            !state.config.isConfigured -> {
                OnboardingScreen(
                    initialServerUrl = state.config.serverUrl,
                    isConnecting = state.isConnecting,
                    errorMessage = state.connectError,
                    onConnectPassword = viewModel::connectWithPassword,
                    onConnectToken = viewModel::connectWithToken,
                )
            }

            else -> {
                MainStudioShell(
                    state = state,
                    viewModel = viewModel,
                )
            }
        }
    }
}

@Composable
private fun MainStudioShell(
    state: AppUiState,
    viewModel: MainViewModel,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val hasActiveTrack = state.playback.currentTrackId != null
    val miniPlayerExtraBottom = if (hasActiveTrack) 80.dp else 0.dp

    Scaffold(
        containerColor = colors.canvas,
        topBar = {
            SoulbeetTopAppBar(
                title = state.selectedTab.label,
                offlineOnly = state.config.offlineOnly,
                activeAcquisitions = state.activeAcquisitionsCount,
                onToggleOffline = viewModel::toggleOfflineOnly,
                onOpenSettings = { viewModel.setShowSettingsSheet(true) },
            )
        },
        bottomBar = {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .background(colors.canvas),
            ) {
                if (state.pendingDrop != null) {
                    UndoDropSnackbar(
                        pending = state.pendingDrop,
                        onUndo = viewModel::undoPendingDrop,
                        modifier = Modifier.padding(bottom = 8.dp),
                    )
                } else if (!state.snackbarMessage.isNullOrBlank()) {
                    Surface(
                        color = colors.surfaceHigh,
                        shape = SoulbeetShapes.md,
                        border = BorderStroke(1.dp, colors.borderDefault),
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(horizontal = 12.dp, vertical = 6.dp),
                    ) {
                        Text(
                            text = state.snackbarMessage,
                            style = type.bodySm,
                            color = colors.textPrimary,
                            modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                        )
                    }
                }

                if (hasActiveTrack) {
                    FloatingMiniPlayer(
                        state = state,
                        artAuthority = viewModel.artAuthority,
                        onExpand = { viewModel.setShowNowPlayingSheet(true) },
                        onTogglePlayPause = viewModel::togglePlayPause,
                        onSkipNext = viewModel::skipNext,
                        onSkipPrevious = viewModel::skipPrevious,
                        onContextAction = {
                            val pb = state.playback
                            val id = pb.currentTrackId ?: return@FloatingMiniPlayer
                            if (pb.isPendingDiscovery) {
                                viewModel.keepDiscoveryTrack(
                                    trackId = id,
                                    discoveryTrackId = state.currentDiscoveryInfo?.id,
                                    title = pb.title,
                                )
                            } else {
                                viewModel.toggleStarTrack(id, pb.isStarred)
                            }
                        },
                        modifier = Modifier.padding(horizontal = 8.dp, vertical = 6.dp),
                    )
                }

                HorizontalDivider(color = colors.borderSubtle)

                NavigationBar(
                    containerColor = colors.surface,
                    contentColor = colors.textPrimary,
                    tonalElevation = 0.dp,
                    modifier = Modifier.navigationBarsPadding(),
                ) {
                    AppTab.entries.forEach { tab ->
                        val selected = state.selectedTab == tab
                        val icon = when (tab) {
                            AppTab.HOME -> if (selected) Icons.Filled.Home else Icons.Outlined.Home
                            AppTab.DISCOVER -> if (selected) Icons.Filled.Explore else Icons.Outlined.Explore
                            AppTab.SEARCH -> if (selected) Icons.Filled.Search else Icons.Outlined.Search
                            AppTab.LIBRARY -> if (selected) Icons.Filled.LibraryMusic else Icons.Outlined.LibraryMusic
                        }
                        NavigationBarItem(
                            selected = selected,
                            onClick = { viewModel.selectTab(tab) },
                            icon = { Icon(imageVector = icon, contentDescription = tab.label) },
                            label = { Text(text = tab.label, style = type.monoXs) },
                            colors = NavigationBarItemDefaults.colors(
                                selectedIconColor = colors.leafText,
                                selectedTextColor = colors.leafText,
                                indicatorColor = colors.leafSurface,
                                unselectedIconColor = colors.textMuted,
                                unselectedTextColor = colors.textMuted,
                            ),
                        )
                    }
                }
            }
        },
    ) { innerPadding ->
        val contentPadding = PaddingValues(bottom = miniPlayerExtraBottom)
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(innerPadding),
        ) {
            when (state.selectedTab) {
                AppTab.HOME -> HomeScreen(
                    state = state,
                    artAuthority = viewModel.artAuthority,
                    bottomContentPadding = contentPadding,
                    onPlayContainer = viewModel::playContainer,
                    onPlayMix = viewModel::playContextualMix,
                    onOpenTriage = { viewModel.selectTab(AppTab.DISCOVER) },
                    onSteerMix = viewModel::steerSeasonalPack,
                    onTogglePin = viewModel::togglePin,
                    onOpenPlaylist = viewModel::openPlaylistDetail,
                    onPlayTrack = { track -> viewModel.playTrack(track, sourceLabel = "Quick Play") },
                    onStarTrack = { track -> viewModel.toggleStarTrack(track.id, track.starred) },
                    onTriggerSync = { viewModel.triggerManualSync() },
                )

                AppTab.DISCOVER -> DiscoverScreen(
                    state = state,
                    artAuthority = viewModel.artAuthority,
                    bottomContentPadding = contentPadding,
                    onPlayMix = viewModel::playContextualMix,
                    onTogglePin = viewModel::togglePin,
                    onSteerMix = viewModel::steerSeasonalPack,
                    onPreviewTriageCard = { card ->
                        card.track?.let { viewModel.playTrack(it, sourceLabel = "Discovery Triage") }
                    },
                    onKeepTriageCard = { card ->
                        val songId = card.discovery.songId ?: return@DiscoverScreen
                        viewModel.keepDiscoveryTrack(songId, card.discovery.id, card.discovery.title)
                    },
                    onStarKeepTriageCard = { card ->
                        val songId = card.discovery.songId ?: return@DiscoverScreen
                        viewModel.starAndKeepDiscoveryTrack(songId, card.discovery.id, card.discovery.title)
                    },
                    onDropTriageCard = { card ->
                        val songId = card.discovery.songId ?: return@DiscoverScreen
                        viewModel.dropDiscoveryTrackWithUndo(
                            trackId = songId,
                            title = card.discovery.title,
                            artist = card.discovery.artist,
                            discoveryTrackId = card.discovery.id,
                            sourceLabel = card.discovery.mixSlug ?: "Discovery",
                        )
                    },
                    onOpenCultureEditor = { viewModel.setShowCultureEditor(true) },
                    onDismissCultureEditor = { viewModel.setShowCultureEditor(false) },
                    onSaveCultureProfile = viewModel::updateCultureProfile,
                )

                AppTab.SEARCH -> SearchScreen(
                    state = state,
                    artAuthority = viewModel.artAuthority,
                    bottomContentPadding = contentPadding,
                    onQueryChange = viewModel::onSearchQueryChanged,
                    onSubmitQuery = viewModel::submitSearch,
                    onClearRecents = viewModel::clearRecentSearches,
                    onSelectFilter = viewModel::setSearchFilter,
                    onPlayTrack = { track -> viewModel.playTrack(track, sourceLabel = "Search") },
                    onStarTrack = { track -> viewModel.toggleStarTrack(track.id, track.starred) },
                    onOpenAlbum = viewModel::openAlbumDetail,
                    onOpenArtist = viewModel::openArtistDetail,
                    onOpenPlaylist = viewModel::openPlaylistDetail,
                    onAcquireTrack = viewModel::acquireAndPlayTrack,
                )

                AppTab.LIBRARY -> LibraryScreen(
                    state = state,
                    artAuthority = viewModel.artAuthority,
                    bottomContentPadding = contentPadding,
                    onToggleOfflineOnly = viewModel::toggleOfflineOnly,
                    onSelectCategory = viewModel::setLibraryCategory,
                    onOpenPlaylist = viewModel::openPlaylistDetail,
                    onOpenAlbum = viewModel::openAlbumDetail,
                    onOpenArtist = viewModel::openArtistDetail,
                    onTogglePin = viewModel::togglePin,
                    onPlayQueue = viewModel::playQueue,
                    onStarTrack = { track -> viewModel.toggleStarTrack(track.id, track.starred) },
                    onRemoveTrack = { track ->
                        viewModel.requestLibraryTrackRemoval(track.id, track.title, track.artistName)
                    },
                    onKeepDiscovery = { disc ->
                        val songId = disc.songId ?: return@LibraryScreen
                        viewModel.keepDiscoveryTrack(songId, disc.id, disc.title)
                    },
                    onDropDiscovery = { disc ->
                        val songId = disc.songId ?: return@LibraryScreen
                        viewModel.dropDiscoveryTrackWithUndo(songId, disc.title, disc.artist, disc.id)
                    },
                    onCloseContainerDetail = viewModel::closeContainerDetail,
                )
            }
        }
    }

    // --- Overlays & Bottom Sheets ---
    if (state.showNowPlayingSheet) {
        NowPlayingSheet(
            state = state,
            artAuthority = viewModel.artAuthority,
            onDismiss = { viewModel.setShowNowPlayingSheet(false) },
            onTogglePlayPause = viewModel::togglePlayPause,
            onSkipNext = viewModel::skipNext,
            onSkipPrevious = viewModel::skipPrevious,
            onSeekTo = viewModel::seekTo,
            onToggleShuffle = viewModel::toggleShuffle,
            onCycleRepeat = viewModel::cycleRepeatMode,
            onKeepDiscovery = {
                val pb = state.playback
                val id = pb.currentTrackId ?: return@NowPlayingSheet
                viewModel.keepDiscoveryTrack(id, state.currentDiscoveryInfo?.id, pb.title)
            },
            onDropDiscovery = {
                val pb = state.playback
                val id = pb.currentTrackId ?: return@NowPlayingSheet
                viewModel.dropDiscoveryTrackWithUndo(
                    trackId = id,
                    title = pb.title,
                    artist = pb.artist,
                    discoveryTrackId = state.currentDiscoveryInfo?.id,
                    sourceLabel = pb.sourceLabel ?: "Discovery",
                )
            },
            onStarToggle = {
                val pb = state.playback
                val id = pb.currentTrackId ?: return@NowPlayingSheet
                viewModel.toggleStarTrack(id, pb.isStarred, state.currentDiscoveryInfo?.id)
            },
            onRate = { rating ->
                val id = state.playback.currentTrackId ?: return@NowPlayingSheet
                viewModel.rateTrack(id, rating, state.currentDiscoveryInfo?.id)
            },
            onRequestLibraryRemoval = {
                val pb = state.playback
                val id = pb.currentTrackId ?: return@NowPlayingSheet
                viewModel.requestLibraryTrackRemoval(id, pb.title, pb.artist)
            },
            onStartRadio = viewModel::startTrackRadio,
            onToggleLyrics = { viewModel.setShowLyricsPanel(!state.showLyricsPanel) },
            onFetchLyrics = viewModel::fetchCurrentTrackLyrics,
            onOpenQueue = { viewModel.setShowQueueSheet(true) },
            onDismissQueue = { viewModel.setShowQueueSheet(false) },
            onPlayQueueIndex = viewModel::playQueueIndex,
            onOpenConnect = { viewModel.setShowConnectSheet(true) },
            onDismissConnect = { viewModel.setShowConnectSheet(false) },
            onReconnectConnect = { viewModel.reconnectConnect(force = true) },
            onClaimActiveDevice = viewModel::claimActiveConnectDevice,
            onTransferPlayback = viewModel::transferPlaybackToDevice,
            onUndoPendingDrop = viewModel::undoPendingDrop,
        )
    }

    if (state.showSettingsSheet) {
        SettingsSheet(
            state = state,
            onDismiss = { viewModel.setShowSettingsSheet(false) },
            onToggleOled = viewModel::toggleOledMode,
            onSelectAutoSecondaryAction = viewModel::setLibrarySecondaryAction,
            onSelectQualityProfile = viewModel::setAudioQualityProfile,
            onTriggerSync = { viewModel.triggerManualSync() },
            onDrainOutbox = viewModel::drainOutboxNow,
            onSignOut = viewModel::disconnectSession,
        )
    }

    state.guardSheetPreview?.let { preview ->
        MultiListenerGuardSheet(
            preview = preview,
            onDismiss = viewModel::dismissGuardSheet,
            onConfirm = { viewModel.confirmGuardedRemoval(preview) },
        )
    }
}

@Composable
private fun SoulbeetTopAppBar(
    title: String,
    offlineOnly: Boolean,
    activeAcquisitions: Int,
    onToggleOffline: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(colors.canvas)
            .statusBarsPadding(),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .height(56.dp)
                .padding(horizontal = 16.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(
                modifier = Modifier.weight(1f),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                SoulbeetPixelLogo(size = 32.dp)
                Text(
                    text = title,
                    style = type.titleLg,
                    color = colors.textPrimary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }

            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (activeAcquisitions > 0) {
                    Box(
                        modifier = Modifier.heightIn(min = 48.dp),
                        contentAlignment = Alignment.Center,
                    ) {
                        Surface(
                            modifier = Modifier.height(32.dp),
                            color = colors.accentSurface,
                            shape = SoulbeetShapes.full,
                            border = BorderStroke(1.dp, colors.accentText.copy(alpha = 0.45f)),
                        ) {
                            Row(
                                modifier = Modifier.padding(horizontal = 10.dp),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(5.dp),
                            ) {
                                Icon(
                                    imageVector = Icons.Filled.CloudDownload,
                                    contentDescription = "Active acquisitions",
                                    tint = colors.accentText,
                                    modifier = Modifier.size(14.dp),
                                )
                                Text(
                                    text = "$activeAcquisitions",
                                    style = type.monoXs,
                                    color = colors.accentText,
                                    maxLines = 1,
                                    softWrap = false,
                                )
                            }
                        }
                    }
                }

                // Global Online / Offline Only Pill (>= 48dp touch target, 32dp visual height)
                val offlineAmber = Color(0xFFF59E0B)
                val offlineAmberSurface = Color(0x26F59E0B)
                val offlineAmberBorder = Color(0x73F59E0B)
                Box(
                    modifier = Modifier
                        .heightIn(min = 48.dp)
                        .clickable(onClick = onToggleOffline),
                    contentAlignment = Alignment.Center,
                ) {
                    Surface(
                        modifier = Modifier.height(32.dp),
                        color = if (offlineOnly) offlineAmberSurface else colors.surface,
                        shape = SoulbeetShapes.full,
                        border = BorderStroke(
                            1.dp,
                            if (offlineOnly) offlineAmberBorder else colors.borderDefault,
                        ),
                    ) {
                        Row(
                            modifier = Modifier.padding(horizontal = 10.dp),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(5.dp),
                        ) {
                            if (offlineOnly) {
                                Icon(
                                    imageVector = Icons.Filled.CloudOff,
                                    contentDescription = "Offline Only",
                                    tint = offlineAmber,
                                    modifier = Modifier.size(14.dp),
                                )
                            } else {
                                Box(
                                    modifier = Modifier.size(14.dp),
                                    contentAlignment = Alignment.Center,
                                ) {
                                    Box(
                                        modifier = Modifier
                                            .size(7.dp)
                                            .clip(CircleShape)
                                            .background(colors.leafGlow),
                                    )
                                }
                            }
                            Text(
                                text = if (offlineOnly) "Offline Only" else "Online",
                                style = type.monoXs,
                                color = if (offlineOnly) offlineAmber else colors.textPrimary,
                                maxLines = 1,
                                softWrap = false,
                            )
                        }
                    }
                }

                IconButton(
                    onClick = onOpenSettings,
                    modifier = Modifier.size(48.dp),
                ) {
                    Icon(
                        imageVector = Icons.Filled.Settings,
                        contentDescription = "Settings",
                        tint = colors.textSecondary,
                    )
                }
            }
        }
        HorizontalDivider(color = colors.borderSubtle)
    }
}

@Composable
private fun FloatingMiniPlayer(
    state: AppUiState,
    artAuthority: String,
    onExpand: () -> Unit,
    onTogglePlayPause: () -> Unit,
    onSkipNext: () -> Unit,
    onSkipPrevious: () -> Unit,
    onContextAction: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val pb = state.playback
    val progress = if (pb.durationMs > 0L) {
        (state.livePositionMs.toFloat() / pb.durationMs.toFloat()).coerceIn(0f, 1f)
    } else {
        0f
    }

    var dragAccumX by remember { mutableFloatStateOf(0f) }
    var dragAccumY by remember { mutableFloatStateOf(0f) }

    Surface(
        modifier = modifier
            .fillMaxWidth()
            .pointerInput(pb.currentTrackId) {
                detectDragGestures(
                    onDragEnd = {
                        when {
                            dragAccumY < -80f && abs(dragAccumY) > abs(dragAccumX) -> onExpand()
                            dragAccumX < -100f && abs(dragAccumX) > abs(dragAccumY) -> onSkipNext()
                            dragAccumX > 100f && abs(dragAccumX) > abs(dragAccumY) -> onSkipPrevious()
                        }
                        dragAccumX = 0f
                        dragAccumY = 0f
                    },
                    onDragCancel = {
                        dragAccumX = 0f
                        dragAccumY = 0f
                    },
                    onDrag = { _, dragAmount ->
                        dragAccumX += dragAmount.x
                        dragAccumY += dragAmount.y
                    },
                )
            }
            .clickable(onClick = onExpand),
        color = colors.surface,
        shape = SoulbeetShapes.md,
        border = BorderStroke(1.dp, colors.borderDefault),
    ) {
        Column {
            if (pb.isBuffering || state.activeAcquisitionsCount > 0) {
                SeamLightBar(active = true)
            } else {
                Box(
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(2.dp)
                        .background(colors.surfaceHigh),
                ) {
                    Box(
                        modifier = Modifier
                            .fillMaxWidth(progress)
                            .height(2.dp)
                            .background(colors.leafSolid),
                    )
                }
            }

            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(64.dp)
                    .padding(horizontal = 10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                TrackArtwork(
                    coverArtId = pb.coverArtId,
                    artAuthority = artAuthority,
                    fallbackTitle = pb.title,
                    size = 44.dp,
                    shape = SoulbeetShapes.xs,
                )

                Spacer(modifier = Modifier.width(10.dp))

                Column(
                    modifier = Modifier.weight(1f),
                    verticalArrangement = Arrangement.spacedBy(2.dp),
                ) {
                    Text(
                        text = pb.title,
                        style = type.body,
                        color = colors.textPrimary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Text(
                        text = pb.artist,
                        style = type.bodySm,
                        color = colors.textSecondary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }

                // 1-Tap Context Action (Keep if Discovery, Favorite if Library)
                IconButton(
                    onClick = onContextAction,
                    modifier = Modifier.size(48.dp),
                ) {
                    if (pb.isPendingDiscovery) {
                        Icon(
                            imageVector = Icons.Filled.Check,
                            contentDescription = "Keep Discovery track",
                            tint = colors.leafText,
                        )
                    } else {
                        Icon(
                            imageVector = if (pb.isStarred) Icons.Filled.Favorite else Icons.Outlined.FavoriteBorder,
                            contentDescription = if (pb.isStarred) "Remove from favorites" else "Favorite track",
                            tint = if (pb.isStarred) colors.accentText else colors.textMuted,
                        )
                    }
                }

                IconButton(
                    onClick = onTogglePlayPause,
                    modifier = Modifier.size(48.dp),
                ) {
                    Icon(
                        imageVector = if (pb.isPlaying) Icons.Filled.Pause else Icons.Filled.PlayArrow,
                        contentDescription = if (pb.isPlaying) "Pause" else "Play",
                        tint = colors.leafText,
                        modifier = Modifier.size(28.dp),
                    )
                }
            }
        }
    }
}
