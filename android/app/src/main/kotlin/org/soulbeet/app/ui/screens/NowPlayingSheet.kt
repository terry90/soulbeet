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
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.PlaylistAdd
import androidx.compose.material.icons.automirrored.filled.QueueMusic
import androidx.compose.material.icons.filled.Cast
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.DeleteOutline
import androidx.compose.material.icons.filled.Devices
import androidx.compose.material.icons.filled.Favorite
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.Lyrics
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Radio
import androidx.compose.material.icons.filled.Repeat
import androidx.compose.material.icons.filled.RepeatOne
import androidx.compose.material.icons.filled.Shuffle
import androidx.compose.material.icons.filled.SkipNext
import androidx.compose.material.icons.filled.SkipPrevious
import androidx.compose.material.icons.filled.ThumbDown
import androidx.compose.material.icons.outlined.FavoriteBorder
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Slider
import androidx.compose.material3.SliderDefaults
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
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.media3.common.Player
import org.soulbeet.app.AppUiState
import org.soulbeet.app.data.FormattingUtils
import org.soulbeet.app.data.LyricsSync
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.ui.components.StarStrip
import org.soulbeet.app.ui.components.TrackArtwork
import org.soulbeet.app.ui.components.UndoDropSnackbar

/**
 * Full-Screen Draggable Now Playing Sheet with thumb-zone ergonomics, adaptive radial artwork glow
 * (pure Compose `Brush.radialGradient`, zero heavy blur), swipe-to-skip artwork, tappable codec
 * telemetry pill, context-aware Discovery vs Library curation row, and expandable Synced Karaoke
 * Lyrics + Connect Devices + Up Next Queue sheets.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun NowPlayingSheet(
    state: AppUiState,
    artAuthority: String,
    onDismiss: () -> Unit,
    onTogglePlayPause: () -> Unit,
    onSkipNext: () -> Unit,
    onSkipPrevious: () -> Unit,
    onSeekTo: (Long) -> Unit,
    onToggleShuffle: () -> Unit,
    onCycleRepeat: () -> Unit,
    onKeepDiscovery: () -> Unit,
    onDropDiscovery: () -> Unit,
    onStarToggle: () -> Unit,
    onRate: (Int) -> Unit,
    onRequestLibraryRemoval: () -> Unit,
    onAddToPlaylist: () -> Unit,
    onStartRadio: () -> Unit,
    onClickPlayingFrom: () -> Unit,
    onClickTitleOrAlbum: () -> Unit,
    onClickArtist: () -> Unit,
    onToggleLyrics: () -> Unit,
    onFetchLyrics: () -> Unit,
    onOpenQueue: () -> Unit,
    onDismissQueue: () -> Unit,
    onPlayQueueIndex: (Int) -> Unit,
    onRemoveQueueIndex: (Int) -> Unit,
    onMoveQueueItem: (fromIndex: Int, toIndex: Int) -> Unit,
    onClearQueue: () -> Unit,
    onOpenConnect: () -> Unit,
    onDismissConnect: () -> Unit,
    onReconnectConnect: () -> Unit,
    onClaimActiveDevice: () -> Unit,
    onTransferPlayback: (String) -> Unit,
    onUndoPendingDrop: () -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val pb = state.playback
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)

    var showCodecInfo by remember { mutableStateOf(false) }
    var isScrubbing by remember { mutableStateOf(false) }
    var scrubPositionMs by remember { mutableFloatStateOf(0f) }

    val durationMs = pb.durationMs.coerceAtLeast(1L)
    val displayPosMs = if (isScrubbing) scrubPositionMs.toLong() else state.livePositionMs.coerceIn(0L, durationMs)
    val codecBadge = remember(state.currentTrackDetails, pb.audioQualityProfile) {
        FormattingUtils.formatCodecBadge(
            suffix = state.currentTrackDetails?.suffix,
            bitRateKbps = state.currentTrackDetails?.bitRate,
            profile = pb.audioQualityProfile,
        )
    }
    val reasonPill = remember(state.currentDiscoveryInfo) {
        FormattingUtils.parseDiscoveryReason(state.currentDiscoveryInfo?.reasonJson)
    }
    val expiryPill = remember(state.currentDiscoveryInfo) {
        FormattingUtils.formatExpiryCountdown(state.currentDiscoveryInfo?.expiresAt)
    }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = sheetState,
        dragHandle = null,
        containerColor = colors.canvas,
        contentColor = colors.textPrimary,
        scrimColor = colors.scrim,
    ) {
        Box(
            modifier = Modifier
                .fillMaxSize()
                .background(
                    Brush.radialGradient(
                        colors = listOf(
                            if (pb.isPendingDiscovery) {
                                colors.accentGlow.copy(alpha = 0.22f)
                            } else {
                                colors.leafGlow.copy(alpha = 0.16f)
                            },
                            colors.surfaceLow.copy(alpha = 0.45f),
                            colors.canvas,
                        ),
                        center = Offset(540f, 440f),
                        radius = 860f,
                    ),
                )
                .statusBarsPadding()
                .navigationBarsPadding(),
        ) {
            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(horizontal = 24.dp, vertical = 12.dp),
                verticalArrangement = Arrangement.SpaceBetween,
            ) {
                // --- Top 20%: Dismiss, Playing From Context, Expiry Badge ---
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.SpaceBetween,
                ) {
                    IconButton(
                        onClick = onDismiss,
                        modifier = Modifier.size(48.dp),
                    ) {
                        Icon(
                            imageVector = Icons.Filled.KeyboardArrowDown,
                            contentDescription = "Collapse Now Playing",
                            tint = colors.textPrimary,
                            modifier = Modifier.size(28.dp),
                        )
                    }

                    Column(
                        horizontalAlignment = Alignment.CenterHorizontally,
                        modifier = Modifier
                            .weight(1f)
                            .clip(SoulbeetShapes.xs)
                            .clickable(enabled = pb.currentTrackId != null, onClick = onClickPlayingFrom)
                            .padding(horizontal = 8.dp, vertical = 4.dp),
                    ) {
                        Text(
                            text = "PLAYING FROM",
                            style = type.monoXs,
                            color = colors.textMuted,
                        )
                        Text(
                            text = (pb.sourceLabel ?: pb.album.ifBlank { "Soulbeet Library" }).uppercase(),
                            style = type.monoSm,
                            color = colors.textPrimary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }

                    IconButton(
                        onClick = onToggleLyrics,
                        modifier = Modifier.size(48.dp),
                    ) {
                        Icon(
                            imageVector = Icons.Filled.Lyrics,
                            contentDescription = "Toggle Synced Lyrics",
                            tint = if (state.showLyricsPanel) colors.leafText else colors.textSecondary,
                        )
                    }
                }

                // --- Middle 45%: Artwork (with swipe-to-skip) OR Expandable Synced Karaoke Lyrics ---
                if (state.showLyricsPanel) {
                    SyncedLyricsPanel(
                        state = state,
                        positionMs = displayPosMs,
                        onSeekTo = onSeekTo,
                        onFetchLyrics = onFetchLyrics,
                        modifier = Modifier
                            .fillMaxWidth()
                            .weight(1f)
                            .padding(vertical = 12.dp),
                    )
                } else {
                    var artDragX by remember(pb.currentTrackId) { mutableFloatStateOf(0f) }
                    Box(
                        modifier = Modifier
                            .fillMaxWidth()
                            .weight(1f)
                            .pointerInput(pb.currentTrackId) {
                                detectHorizontalDragGestures(
                                    onDragEnd = {
                                        when {
                                            artDragX < -120f -> onSkipNext()
                                            artDragX > 120f -> onSkipPrevious()
                                        }
                                        artDragX = 0f
                                    },
                                    onDragCancel = { artDragX = 0f },
                                    onHorizontalDrag = { _, delta ->
                                        artDragX += delta
                                    },
                                )
                            },
                        contentAlignment = Alignment.Center,
                    ) {
                        Column(
                            horizontalAlignment = Alignment.CenterHorizontally,
                            verticalArrangement = Arrangement.spacedBy(12.dp),
                        ) {
                            TrackArtwork(
                                coverArtId = pb.coverArtId,
                                artAuthority = artAuthority,
                                fallbackTitle = pb.title.ifBlank { "Soulbeet" },
                                size = 276.dp,
                                shape = SoulbeetShapes.lg,
                            )
                            if (pb.isPendingDiscovery) {
                                Surface(
                                    color = colors.accentSurface,
                                    shape = SoulbeetShapes.full,
                                    border = BorderStroke(1.dp, colors.accentText.copy(alpha = 0.45f)),
                                ) {
                                    Text(
                                        text = expiryPill?.let { "DISCOVERY · $it" } ?: "PENDING DISCOVERY",
                                        style = type.monoXs,
                                        color = colors.accentText,
                                        modifier = Modifier.padding(horizontal = 12.dp, vertical = 5.dp),
                                    )
                                }
                            }
                        }
                    }
                }

                // --- Bottom 35%: Thumb-Zone Ergonomics ---
                Column(verticalArrangement = Arrangement.spacedBy(14.dp)) {
                    // Title/Album, Artist & Discovery Reason Pill (clickable navigation)
                    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        Text(
                            text = pb.title.ifBlank { "Nothing Playing" },
                            style = type.titleLg,
                            color = colors.textPrimary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier
                                .clip(SoulbeetShapes.xs)
                                .clickable(enabled = pb.currentTrackId != null, onClick = onClickTitleOrAlbum)
                                .padding(vertical = 1.dp),
                        )
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            Text(
                                text = pb.artist.ifBlank { "Pick a track to begin" },
                                style = type.body,
                                color = colors.textSecondary,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                modifier = Modifier
                                    .weight(1f, fill = false)
                                    .clip(SoulbeetShapes.xs)
                                    .clickable(enabled = pb.artist.isNotBlank(), onClick = onClickArtist)
                                    .padding(vertical = 1.dp),
                            )
                            if (pb.isOfflineCached) {
                                Surface(
                                    color = colors.leafSurface,
                                    shape = SoulbeetShapes.full,
                                ) {
                                    Text(
                                        text = "CACHED",
                                        style = type.monoXs,
                                        color = colors.leafText,
                                        modifier = Modifier.padding(horizontal = 7.dp, vertical = 2.dp),
                                    )
                                }
                            }
                        }
                        if (pb.album.isNotBlank()) {
                            Text(
                                text = pb.album,
                                style = type.bodySm,
                                color = colors.textMuted,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                modifier = Modifier
                                    .clip(SoulbeetShapes.xs)
                                    .clickable(enabled = pb.currentTrackId != null, onClick = onClickTitleOrAlbum)
                                    .padding(vertical = 1.dp),
                            )
                        }
                        if (!reasonPill.isNullOrBlank()) {
                            Text(
                                text = reasonPill.uppercase(),
                                style = type.monoXs,
                                color = colors.accentText,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }

                    // Scrubber + Commit Mono timestamps + Tappable Codec Pill
                    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                        Slider(
                            value = displayPosMs.toFloat(),
                            onValueChange = {
                                isScrubbing = true
                                scrubPositionMs = it
                            },
                            onValueChangeFinished = {
                                isScrubbing = false
                                onSeekTo(scrubPositionMs.toLong())
                            },
                            valueRange = 0f..durationMs.toFloat(),
                            colors = SliderDefaults.colors(
                                thumbColor = colors.leafGlow,
                                activeTrackColor = colors.leafSolid,
                                inactiveTrackColor = colors.surfaceHighest,
                            ),
                            modifier = Modifier.fillMaxWidth(),
                        )
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Text(
                                text = FormattingUtils.formatDurationMs(displayPosMs),
                                style = type.monoSm,
                                color = colors.textMuted,
                            )
                            Surface(
                                onClick = { showCodecInfo = !showCodecInfo },
                                color = colors.surfaceHigh,
                                shape = SoulbeetShapes.full,
                                border = BorderStroke(1.dp, colors.borderDefault),
                            ) {
                                Text(
                                    text = codecBadge,
                                    style = type.monoXs,
                                    color = colors.leafText,
                                    modifier = Modifier.padding(horizontal = 10.dp, vertical = 4.dp),
                                )
                            }
                            Text(
                                text = FormattingUtils.formatDurationMs(pb.durationMs),
                                style = type.monoSm,
                                color = colors.textMuted,
                            )
                        }

                        if (showCodecInfo) {
                            Surface(
                                color = colors.surface,
                                shape = SoulbeetShapes.sm,
                                border = BorderStroke(1.dp, colors.borderSubtle),
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(top = 6.dp),
                            ) {
                                val srcDesc = if (pb.isOfflineCached) "Local Offline Cache" else "Navidrome OpenSubsonic Stream"
                                val origFmt = state.currentTrackDetails?.suffix?.uppercase() ?: "AUDIO"
                                val origBitrate = state.currentTrackDetails?.bitRate?.let { "${it} kbps" } ?: "Variable"
                                Text(
                                    text = "SIGNAL CHAIN: $codecBadge · Source $origFmt ($origBitrate) · $srcDesc",
                                    style = type.monoXs,
                                    color = colors.textSecondary,
                                    modifier = Modifier.padding(10.dp),
                                )
                            }
                        }
                    }

                    // Context-Aware Soulbeet Curation Row (Pending Discovery vs Permanent Library)
                    if (pb.currentTrackId != null) {
                        if (pb.isPendingDiscovery) {
                            Column(
                                verticalArrangement = Arrangement.spacedBy(6.dp),
                                horizontalAlignment = Alignment.CenterHorizontally,
                                modifier = Modifier.fillMaxWidth(),
                            ) {
                                Row(
                                    modifier = Modifier.fillMaxWidth(),
                                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                                    verticalAlignment = Alignment.CenterVertically,
                                ) {
                                    Surface(
                                        onClick = onDropDiscovery,
                                        color = colors.crimsonSurface,
                                        shape = SoulbeetShapes.full,
                                        modifier = Modifier
                                            .weight(1f)
                                            .heightIn(min = 48.dp),
                                    ) {
                                        Row(
                                            modifier = Modifier.padding(horizontal = 12.dp, vertical = 12.dp),
                                            horizontalArrangement = Arrangement.Center,
                                            verticalAlignment = Alignment.CenterVertically,
                                        ) {
                                            Icon(
                                                imageVector = Icons.Filled.ThumbDown,
                                                contentDescription = "Drop Discovery",
                                                tint = colors.crimsonText,
                                                modifier = Modifier.size(18.dp),
                                            )
                                            Spacer(Modifier.width(6.dp))
                                            Text("Drop", style = type.body, color = colors.crimsonText)
                                        }
                                    }

                                    IconButton(
                                        onClick = onStarToggle,
                                        modifier = Modifier
                                            .size(48.dp)
                                            .clip(CircleShape)
                                            .background(colors.accentSurface),
                                    ) {
                                        Icon(
                                            imageVector = if (pb.isStarred) Icons.Filled.Favorite else Icons.Outlined.FavoriteBorder,
                                            contentDescription = "Favorite and promote",
                                            tint = colors.accentText,
                                        )
                                    }

                                    Button(
                                        onClick = onKeepDiscovery,
                                        colors = ButtonDefaults.buttonColors(
                                            containerColor = colors.leafSolid,
                                            contentColor = colors.leafInk,
                                        ),
                                        shape = SoulbeetShapes.full,
                                        modifier = Modifier
                                            .weight(1.25f)
                                            .heightIn(min = 48.dp),
                                    ) {
                                        Icon(Icons.Filled.Check, contentDescription = null, modifier = Modifier.size(18.dp))
                                        Spacer(Modifier.width(6.dp))
                                        Text("KEEP", style = type.body)
                                    }
                                }

                                StarStrip(
                                    rating = pb.userRating,
                                    onRate = onRate,
                                )
                            }
                        } else {
                            // Permanent Library Curation Rows:
                            // Row 1: evenly spaced actions [Remove] [Favorite] [Add to Playlist] [Radio]
                            // Row 2: centered 1-5 star rating strip (prevents horizontal overflow on 360-390dp phones)
                            Column(
                                verticalArrangement = Arrangement.spacedBy(4.dp),
                                horizontalAlignment = Alignment.CenterHorizontally,
                                modifier = Modifier.fillMaxWidth(),
                            ) {
                                Row(
                                    modifier = Modifier.fillMaxWidth(),
                                    horizontalArrangement = Arrangement.SpaceEvenly,
                                    verticalAlignment = Alignment.CenterVertically,
                                ) {
                                    IconButton(
                                        onClick = onRequestLibraryRemoval,
                                        modifier = Modifier.size(48.dp),
                                    ) {
                                        Icon(
                                            imageVector = Icons.Filled.DeleteOutline,
                                            contentDescription = "Remove from library",
                                            tint = colors.textMuted,
                                        )
                                    }
                                    IconButton(
                                        onClick = onStarToggle,
                                        modifier = Modifier.size(48.dp),
                                    ) {
                                        Icon(
                                            imageVector = if (pb.isStarred) Icons.Filled.Favorite else Icons.Outlined.FavoriteBorder,
                                            contentDescription = if (pb.isStarred) "Remove from favorites" else "Favorite",
                                            tint = if (pb.isStarred) colors.accentText else colors.textSecondary,
                                        )
                                    }
                                    IconButton(
                                        onClick = onAddToPlaylist,
                                        modifier = Modifier.size(48.dp),
                                    ) {
                                        Icon(
                                            imageVector = Icons.AutoMirrored.Filled.PlaylistAdd,
                                            contentDescription = "Add to playlist",
                                            tint = colors.textSecondary,
                                        )
                                    }
                                    IconButton(
                                        onClick = onStartRadio,
                                        modifier = Modifier.size(48.dp),
                                    ) {
                                        Icon(
                                            imageVector = Icons.Filled.Radio,
                                            contentDescription = "Start track radio",
                                            tint = colors.textSecondary,
                                        )
                                    }
                                }

                                StarStrip(
                                    rating = pb.userRating,
                                    onRate = onRate,
                                )
                            }
                        }
                    }

                    // Primary Transport Controls (Play/Pause 64dp)
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceEvenly,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        IconButton(onClick = onToggleShuffle, modifier = Modifier.size(48.dp)) {
                            Icon(
                                imageVector = Icons.Filled.Shuffle,
                                contentDescription = "Toggle shuffle",
                                tint = if (state.shuffleEnabled) colors.leafText else colors.textMuted,
                            )
                        }
                        IconButton(onClick = onSkipPrevious, modifier = Modifier.size(52.dp)) {
                            Icon(
                                imageVector = Icons.Filled.SkipPrevious,
                                contentDescription = "Previous track",
                                tint = colors.textPrimary,
                                modifier = Modifier.size(32.dp),
                            )
                        }
                        Surface(
                            onClick = onTogglePlayPause,
                            color = colors.leafSolid,
                            shape = CircleShape,
                            modifier = Modifier.size(64.dp),
                        ) {
                            Box(contentAlignment = Alignment.Center) {
                                Icon(
                                    imageVector = if (pb.isPlaying) Icons.Filled.Pause else Icons.Filled.PlayArrow,
                                    contentDescription = if (pb.isPlaying) "Pause" else "Play",
                                    tint = colors.leafInk,
                                    modifier = Modifier.size(34.dp),
                                )
                            }
                        }
                        IconButton(onClick = onSkipNext, modifier = Modifier.size(52.dp)) {
                            Icon(
                                imageVector = Icons.Filled.SkipNext,
                                contentDescription = "Next track",
                                tint = colors.textPrimary,
                                modifier = Modifier.size(32.dp),
                            )
                        }
                        IconButton(onClick = onCycleRepeat, modifier = Modifier.size(48.dp)) {
                            val icon = if (state.repeatMode == Player.REPEAT_MODE_ONE) {
                                Icons.Filled.RepeatOne
                            } else {
                                Icons.Filled.Repeat
                            }
                            Icon(
                                imageVector = icon,
                                contentDescription = "Cycle repeat",
                                tint = if (state.repeatMode != Player.REPEAT_MODE_OFF) colors.leafText else colors.textMuted,
                            )
                        }
                    }

                    // Bottom Utility Strip: Connect Devices, Synced Lyrics, Up Next Queue
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        TextButton(
                            onClick = onOpenConnect,
                            modifier = Modifier.heightIn(min = 48.dp),
                        ) {
                            Icon(
                                imageVector = Icons.Filled.Cast,
                                contentDescription = null,
                                tint = if (state.connectState?.connected == true) colors.leafText else colors.textMuted,
                                modifier = Modifier.size(18.dp),
                            )
                            Spacer(Modifier.width(6.dp))
                            Text(
                                text = "Devices",
                                style = type.bodySm,
                                color = colors.textSecondary,
                            )
                        }

                        TextButton(
                            onClick = onToggleLyrics,
                            modifier = Modifier.heightIn(min = 48.dp),
                        ) {
                            Icon(
                                imageVector = Icons.Filled.Lyrics,
                                contentDescription = null,
                                tint = if (state.showLyricsPanel) colors.leafText else colors.textMuted,
                                modifier = Modifier.size(18.dp),
                            )
                            Spacer(Modifier.width(6.dp))
                            Text(
                                text = "Lyrics",
                                style = type.bodySm,
                                color = if (state.showLyricsPanel) colors.leafText else colors.textSecondary,
                            )
                        }

                        TextButton(
                            onClick = onOpenQueue,
                            modifier = Modifier.heightIn(min = 48.dp),
                        ) {
                            Icon(
                                imageVector = Icons.AutoMirrored.Filled.QueueMusic,
                                contentDescription = null,
                                tint = colors.textMuted,
                                modifier = Modifier.size(18.dp),
                            )
                            Spacer(Modifier.width(6.dp))
                            Text(
                                text = "Queue (${state.queueTracks.size})",
                                style = type.bodySm,
                                color = colors.textSecondary,
                            )
                        }
                    }
                }
            }

            // Always render 5-second Undo snackbar / status notice ABOVE NowPlayingSheet content
            if (state.pendingDrop != null) {
                UndoDropSnackbar(
                    pending = state.pendingDrop,
                    onUndo = onUndoPendingDrop,
                    modifier = Modifier
                        .align(Alignment.BottomCenter)
                        .padding(bottom = 16.dp),
                )
            } else if (!state.snackbarMessage.isNullOrBlank()) {
                Surface(
                    color = colors.surfaceHigh,
                    shape = SoulbeetShapes.md,
                    border = BorderStroke(1.dp, colors.borderDefault),
                    modifier = Modifier
                        .align(Alignment.BottomCenter)
                        .fillMaxWidth()
                        .padding(horizontal = 16.dp, vertical = 16.dp),
                ) {
                    Text(
                        text = state.snackbarMessage,
                        style = type.bodySm,
                        color = colors.textPrimary,
                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                    )
                }
            }
        }
    }

    if (state.showQueueSheet) {
        UpNextQueueSheet(
            state = state,
            artAuthority = artAuthority,
            onDismiss = onDismissQueue,
            onPlayIndex = { idx ->
                onPlayQueueIndex(idx)
                onDismissQueue()
            },
            onRemoveIndex = onRemoveQueueIndex,
            onMoveItem = onMoveQueueItem,
            onClearQueue = onClearQueue,
        )
    }

    if (state.showConnectSheet) {
        ConnectDevicesSheet(
            state = state,
            onDismiss = onDismissConnect,
            onReconnect = onReconnectConnect,
            onClaimActive = onClaimActiveDevice,
            onTransferTo = onTransferPlayback,
        )
    }
}

@Composable
private fun SyncedLyricsPanel(
    state: AppUiState,
    positionMs: Long,
    onSeekTo: (Long) -> Unit,
    onFetchLyrics: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val lyrics = state.lyricsData
    val listState = rememberLazyListState()

    val activeIndex = remember(lyrics, positionMs) {
        if (lyrics != null && lyrics.synced) {
            LyricsSync.findActiveLyricIndex(lyrics.lines, positionMs, lyrics.offsetMs)
        } else {
            -1
        }
    }

    LaunchedEffect(activeIndex) {
        if (activeIndex >= 0) {
            listState.animateScrollToItem((activeIndex - 2).coerceAtLeast(0))
        }
    }

    Surface(
        modifier = modifier,
        color = colors.surfaceLow.copy(alpha = 0.85f),
        shape = SoulbeetShapes.lg,
        border = BorderStroke(1.dp, colors.borderSubtle),
    ) {
        when {
            state.isFetchingLyrics -> {
                Box(modifier = Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    CircularProgressIndicator(color = colors.leafSolid)
                }
            }

            lyrics == null || lyrics.lines.isEmpty() -> {
                Column(
                    modifier = Modifier
                        .fillMaxSize()
                        .padding(24.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.Center,
                ) {
                    Text(
                        text = state.lyricsError ?: if (state.lyricsFetched) {
                            "No lyrics found for this track"
                        } else {
                            "No synced lyrics saved yet"
                        },
                        style = type.body,
                        color = colors.textSecondary,
                        textAlign = TextAlign.Center,
                    )
                    Spacer(Modifier.height(12.dp))
                    OutlinedButton(
                        onClick = onFetchLyrics,
                        shape = SoulbeetShapes.full,
                        border = BorderStroke(1.dp, colors.borderDefault),
                        modifier = Modifier.heightIn(min = 48.dp),
                    ) {
                        Text(
                            text = if (state.lyricsFetched) "Try Again" else "Fetch Lyrics",
                            style = type.body,
                            color = colors.leafText,
                        )
                    }
                }
            }

            else -> {
                LazyColumn(
                    state = listState,
                    contentPadding = PaddingValues(horizontal = 20.dp, vertical = 24.dp),
                    verticalArrangement = Arrangement.spacedBy(14.dp),
                    modifier = Modifier.fillMaxSize(),
                ) {
                    itemsIndexed(lyrics.lines) { idx, line ->
                        val isActive = idx == activeIndex
                        val isPast = activeIndex >= 0 && idx < activeIndex
                        val textColor = when {
                            isActive -> colors.leafText
                            isPast -> colors.textMuted
                            else -> colors.textSecondary
                        }
                        Text(
                            text = line.text,
                            style = if (isActive) type.titleMd else type.body,
                            color = textColor,
                            modifier = Modifier
                                .fillMaxWidth()
                                .clip(SoulbeetShapes.sm)
                                .clickable(enabled = line.startMs != null) {
                                    line.startMs?.let { onSeekTo(it) }
                                }
                                .padding(vertical = 4.dp),
                        )
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun UpNextQueueSheet(
    state: AppUiState,
    artAuthority: String,
    onDismiss: () -> Unit,
    onPlayIndex: (Int) -> Unit,
    onRemoveIndex: (Int) -> Unit,
    onMoveItem: (fromIndex: Int, toIndex: Int) -> Unit,
    onClearQueue: () -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = colors.surfaceHigh,
        contentColor = colors.textPrimary,
        scrimColor = colors.scrim,
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 16.dp)
                .padding(bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    Text("Up Next", style = type.titleMd, color = colors.textPrimary)
                    Text("${state.queueTracks.size} TRACKS", style = type.monoXs, color = colors.textMuted)
                }
                if (state.queueTracks.isNotEmpty()) {
                    TextButton(
                        onClick = onClearQueue,
                        modifier = Modifier.heightIn(min = 48.dp),
                    ) {
                        Text(
                            text = "Clear Queue",
                            style = type.monoSm,
                            color = colors.crimsonText,
                        )
                    }
                }
            }

            if (state.queueTracks.isEmpty()) {
                Text(
                    text = "Queue is empty",
                    style = type.bodySm,
                    color = colors.textSecondary,
                    modifier = Modifier.padding(vertical = 24.dp),
                )
            } else {
                LazyColumn(
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(max = 420.dp),
                    verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    itemsIndexed(state.queueTracks, key = { idx, t -> "q_${idx}_${t.id}" }) { idx, track ->
                        val isPlaying = idx == state.playback.currentIndex
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .heightIn(min = 56.dp)
                                .clip(SoulbeetShapes.sm)
                                .background(if (isPlaying) colors.surfaceLow else Color.Transparent)
                                .clickable { onPlayIndex(idx) }
                                .padding(start = 8.dp, end = 2.dp, top = 6.dp, bottom = 6.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            TrackArtwork(
                                coverArtId = track.coverArtId,
                                artAuthority = artAuthority,
                                fallbackTitle = track.title,
                                size = 40.dp,
                                shape = SoulbeetShapes.xs,
                            )
                            Spacer(modifier = Modifier.width(10.dp))
                            Column(
                                modifier = Modifier.weight(1f),
                                verticalArrangement = Arrangement.spacedBy(2.dp),
                            ) {
                                Text(
                                    text = track.title,
                                    style = type.body,
                                    color = if (isPlaying) colors.leafText else colors.textPrimary,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                                Text(
                                    text = track.artistName,
                                    style = type.bodySm,
                                    color = colors.textSecondary,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                            }
                            IconButton(
                                onClick = { onMoveItem(idx, idx - 1) },
                                enabled = idx > 0,
                                modifier = Modifier.size(36.dp),
                            ) {
                                Icon(
                                    imageVector = Icons.Filled.KeyboardArrowUp,
                                    contentDescription = "Move up in queue",
                                    tint = if (idx > 0) colors.textSecondary else colors.textDisabled,
                                    modifier = Modifier.size(20.dp),
                                )
                            }
                            IconButton(
                                onClick = { onMoveItem(idx, idx + 1) },
                                enabled = idx < state.queueTracks.lastIndex,
                                modifier = Modifier.size(36.dp),
                            ) {
                                Icon(
                                    imageVector = Icons.Filled.KeyboardArrowDown,
                                    contentDescription = "Move down in queue",
                                    tint = if (idx < state.queueTracks.lastIndex) colors.textSecondary else colors.textDisabled,
                                    modifier = Modifier.size(20.dp),
                                )
                            }
                            IconButton(
                                onClick = { onRemoveIndex(idx) },
                                modifier = Modifier.size(36.dp),
                            ) {
                                Icon(
                                    imageVector = Icons.Filled.Close,
                                    contentDescription = "Remove from queue",
                                    tint = colors.textMuted,
                                    modifier = Modifier.size(18.dp),
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ConnectDevicesSheet(
    state: AppUiState,
    onDismiss: () -> Unit,
    onReconnect: () -> Unit,
    onClaimActive: () -> Unit,
    onTransferTo: (String) -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val connect = state.connectState

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = colors.surfaceHigh,
        contentColor = colors.textPrimary,
        scrimColor = colors.scrim,
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 20.dp)
                .padding(bottom = 28.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("Connect Devices", style = type.titleMd, color = colors.textPrimary)
                Surface(
                    color = if (connect?.connected == true) colors.leafSurface else colors.surface,
                    shape = SoulbeetShapes.full,
                ) {
                    Text(
                        text = if (connect?.connected == true) "CONNECTED" else "DISCONNECTED",
                        style = type.monoXs,
                        color = if (connect?.connected == true) colors.leafText else colors.textMuted,
                        modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
                    )
                }
            }

            val devices = connect?.devices.orEmpty()
            if (connect?.connected != true) {
                Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                    Text(
                        text = "Disconnected from Soulbeet Connect",
                        style = type.bodySm,
                        color = colors.textSecondary,
                    )
                    OutlinedButton(
                        onClick = onReconnect,
                        shape = SoulbeetShapes.full,
                        border = BorderStroke(1.dp, colors.leafText.copy(alpha = 0.45f)),
                        modifier = Modifier.heightIn(min = 48.dp),
                    ) {
                        Text(
                            text = "Reconnect",
                            style = type.bodySm,
                            color = colors.leafText,
                        )
                    }
                }
            } else if (devices.isEmpty()) {
                Text(
                    text = "This Android phone is the only active Soulbeet Connect device right now.",
                    style = type.bodySm,
                    color = colors.textSecondary,
                )
            } else {
                LazyColumn(
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(max = 360.dp),
                    verticalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    items(devices, key = { it.deviceId }) { dev ->
                        val isSelf = dev.deviceId == state.config.deviceId
                        Surface(
                            onClick = {
                                if (isSelf) onClaimActive() else onTransferTo(dev.deviceId)
                            },
                            color = if (dev.isActive) colors.leafSurface else colors.surface,
                            shape = SoulbeetShapes.md,
                            border = BorderStroke(
                                1.dp,
                                if (dev.isActive) colors.leafText.copy(alpha = 0.45f) else colors.borderSubtle,
                            ),
                            modifier = Modifier
                                .fillMaxWidth()
                                .heightIn(min = 56.dp),
                        ) {
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(horizontal = 14.dp, vertical = 12.dp),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(12.dp),
                            ) {
                                Row(
                                    modifier = Modifier.weight(1f),
                                    verticalAlignment = Alignment.CenterVertically,
                                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                                ) {
                                    Icon(
                                        imageVector = Icons.Filled.Devices,
                                        contentDescription = null,
                                        tint = if (dev.isActive) colors.leafText else colors.textMuted,
                                    )
                                    Column(modifier = Modifier.weight(1f)) {
                                        Text(
                                            text = dev.name,
                                            style = type.body,
                                            color = colors.textPrimary,
                                            maxLines = 1,
                                            overflow = TextOverflow.Ellipsis,
                                        )
                                        Text(
                                            text = if (isSelf) {
                                                "${dev.platform.name} · THIS DEVICE"
                                            } else {
                                                dev.platform.name
                                            },
                                            style = type.monoXs,
                                            color = colors.textMuted,
                                            maxLines = 1,
                                            overflow = TextOverflow.Ellipsis,
                                        )
                                    }
                                }
                                Surface(
                                    color = if (dev.isActive) colors.leafSurface else colors.accentSurface,
                                    shape = SoulbeetShapes.full,
                                    border = BorderStroke(
                                        1.dp,
                                        if (dev.isActive) {
                                            colors.leafText.copy(alpha = 0.45f)
                                        } else {
                                            colors.accentText.copy(alpha = 0.45f)
                                        },
                                    ),
                                ) {
                                    Text(
                                        text = if (dev.isActive) "ACTIVE" else if (isSelf) "USE HERE" else "TRANSFER",
                                        style = type.monoXs,
                                        color = if (dev.isActive) colors.leafText else colors.accentText,
                                        maxLines = 1,
                                        softWrap = false,
                                        modifier = Modifier.padding(horizontal = 8.dp, vertical = 4.dp),
                                    )
                                }
                            }
                        }
                    }
                }
            }

            IconButton(
                onClick = onDismiss,
                modifier = Modifier.align(Alignment.End).size(48.dp),
            ) {
                Icon(Icons.Filled.Close, contentDescription = "Close", tint = colors.textMuted)
            }
        }
    }
}
