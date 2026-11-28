package org.soulbeet.app.ui.screens

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
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
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.OfflinePin
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Shuffle
import androidx.compose.material.icons.outlined.OfflinePin
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Surface
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
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.soulbeet.app.AppUiState
import org.soulbeet.app.MixDetailUi
import org.soulbeet.app.data.CultureSteeringOption
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.ui.components.MixSleeveBackdrop
import org.soulbeet.app.ui.components.PlaylistArtwork
import org.soulbeet.app.ui.components.TrackRow
import org.soulbeet.app.ui.components.formatMixSleeveMeta
import org.soulbeet.app.ui.components.formatMixSleeveTag
import org.soulbeet.app.ui.components.mixSleeveSpecFor
import org.soulbeet.core.FfiPinTarget
import org.soulbeet.core.FfiPlaylist
import org.soulbeet.core.FfiTrack

/**
 * Full Mix Detail bottom sheet showing sleeve header, Play, Shuffle, Pin Offline,
 * optional seasonal Culture Steering pills, and full playable tracklist.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun MixDetailSheet(
    detail: MixDetailUi,
    state: AppUiState,
    artAuthority: String,
    onDismiss: () -> Unit,
    onPlayAll: () -> Unit,
    onShuffle: () -> Unit,
    onPlayIndex: (Int) -> Unit,
    onSteer: (CultureSteeringOption) -> Unit,
    onTogglePin: (FfiPinTarget, String) -> Unit,
    onStarTrack: (FfiTrack) -> Unit,
    onAddToPlaylist: (FfiTrack) -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    val mix = detail.mix
    val spec = remember(mix.slug) { mixSleeveSpecFor(mix.slug) }
    val tag = remember(mix.slug, mix.kind, mix.offlinePriority) { formatMixSleeveTag(mix) }
    val metaLine = remember(mix) { formatMixSleeveMeta(mix, compact = false) }
    val country = mix.countryCode ?: "LOCAL"
    val localPct = mix.localPct
    val pinTarget = remember(mix.navidromePlaylistId, mix.slug) {
        if (!mix.navidromePlaylistId.isNullOrBlank()) {
            FfiPinTarget.Playlist(mix.navidromePlaylistId)
        } else {
            FfiPinTarget.SeasonalPack(mix.slug)
        }
    }
    val isPinned = state.pinnedTargets.any { it.target == pinTarget }

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
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            // Mix Sleeve Header Card
            Surface(
                shape = SoulbeetShapes.lg,
                color = spec.gradientEnd,
                border = BorderStroke(1.dp, spec.borderColor),
                modifier = Modifier.fillMaxWidth(),
            ) {
                Box(modifier = Modifier.fillMaxWidth()) {
                    MixSleeveBackdrop(
                        slug = mix.slug,
                        modifier = Modifier.matchParentSize(),
                    )
                    Box(
                        modifier = Modifier
                            .matchParentSize()
                            .background(
                                Brush.horizontalGradient(
                                    colorStops = arrayOf(
                                        0.0f to colors.canvas.copy(alpha = 0.86f),
                                        0.60f to colors.canvas.copy(alpha = 0.56f),
                                        1.0f to colors.canvas.copy(alpha = 0.18f),
                                    ),
                                ),
                            ),
                    )
                    Column(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(16.dp),
                        verticalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            Surface(
                                color = spec.pillBackground,
                                shape = SoulbeetShapes.full,
                                border = BorderStroke(1.dp, spec.borderColor),
                            ) {
                                Text(
                                    text = tag,
                                    style = type.monoXs,
                                    color = spec.pillForeground,
                                    maxLines = 1,
                                    softWrap = false,
                                    modifier = Modifier.padding(horizontal = 8.dp, vertical = 3.dp),
                                )
                            }
                            Text(
                                text = metaLine,
                                style = type.monoXs,
                                color = colors.textSecondary,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                        Text(
                            text = mix.title,
                            style = type.titleLg,
                            color = colors.textPrimary,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                        )
                        if (mix.subtitle.isNotBlank()) {
                            Text(
                                text = mix.subtitle,
                                style = type.bodySm,
                                color = colors.textSecondary,
                                maxLines = 3,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                }
            }

            // Primary Controls: Play, Shuffle, Pin Offline
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .horizontalScroll(rememberScrollState()),
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Button(
                    onClick = onPlayAll,
                    colors = ButtonDefaults.buttonColors(
                        containerColor = colors.leafSolid,
                        contentColor = colors.leafInk,
                    ),
                    shape = SoulbeetShapes.full,
                    contentPadding = PaddingValues(horizontal = 20.dp, vertical = 12.dp),
                    modifier = Modifier.heightIn(min = 48.dp),
                ) {
                    Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(6.dp))
                    Text("Play", style = type.body, maxLines = 1, softWrap = false)
                }

                OutlinedButton(
                    onClick = onShuffle,
                    shape = SoulbeetShapes.full,
                    border = BorderStroke(1.dp, colors.borderDefault),
                    modifier = Modifier.heightIn(min = 48.dp),
                ) {
                    Icon(
                        imageVector = Icons.Filled.Shuffle,
                        contentDescription = null,
                        tint = colors.textPrimary,
                        modifier = Modifier.size(18.dp),
                    )
                    Spacer(Modifier.width(6.dp))
                    Text("Shuffle", style = type.bodySm, color = colors.textPrimary, maxLines = 1, softWrap = false)
                }

                OutlinedButton(
                    onClick = { onTogglePin(pinTarget, mix.title) },
                    shape = SoulbeetShapes.full,
                    border = BorderStroke(
                        1.dp,
                        if (isPinned) colors.leafText.copy(alpha = 0.5f) else colors.borderDefault,
                    ),
                    modifier = Modifier.heightIn(min = 48.dp),
                ) {
                    Icon(
                        imageVector = if (isPinned) Icons.Filled.OfflinePin else Icons.Outlined.OfflinePin,
                        contentDescription = null,
                        tint = if (isPinned) colors.leafText else colors.textPrimary,
                        modifier = Modifier.size(18.dp),
                    )
                    Spacer(Modifier.width(6.dp))
                    Text(
                        text = if (isPinned) "Pinned" else "Pin Offline",
                        style = type.bodySm,
                        color = if (isPinned) colors.leafText else colors.textPrimary,
                        maxLines = 1,
                        softWrap = false,
                    )
                }
            }

            // Seasonal Culture Steering Pills
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
                            enabled = !state.steeringInFlight,
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

            HorizontalDivider(color = colors.borderSubtle)

            when {
                detail.isLoading -> {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(vertical = 28.dp),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        CircularProgressIndicator(
                            color = colors.leafSolid,
                            strokeWidth = 2.dp,
                            modifier = Modifier.size(20.dp),
                        )
                        Spacer(Modifier.width(10.dp))
                        Text(
                            text = "Loading mix tracks...",
                            style = type.bodySm,
                            color = colors.textSecondary,
                        )
                    }
                }

                detail.error != null -> {
                    Surface(
                        color = colors.crimsonSurface,
                        shape = SoulbeetShapes.md,
                        border = BorderStroke(1.dp, colors.crimsonText.copy(alpha = 0.35f)),
                    ) {
                        Text(
                            text = detail.error,
                            style = type.bodySm,
                            color = colors.crimsonText,
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(14.dp),
                        )
                    }
                }

                detail.tracks.isEmpty() -> {
                    Text(
                        text = "No playable tracks in this mix yet.",
                        style = type.bodySm,
                        color = colors.textMuted,
                        modifier = Modifier.padding(vertical = 16.dp),
                    )
                }

                else -> {
                    LazyColumn(
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(max = 400.dp),
                        verticalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        itemsIndexed(detail.tracks, key = { idx, t -> "md_${idx}_${t.id}" }) { idx, track ->
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
                                onClick = { onPlayIndex(idx) },
                                onStarClick = { onStarTrack(track) },
                                onAddToPlaylistClick = { onAddToPlaylist(track) },
                            )
                        }
                    }
                }
            }
        }
    }
}

/**
 * Reusable bottom sheet to add a track to an existing static user playlist or create a new one.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun AddToPlaylistSheet(
    track: FfiTrack,
    playlists: List<FfiPlaylist>,
    artAuthority: String,
    isBusy: Boolean,
    onDismiss: () -> Unit,
    onSelectPlaylist: (FfiPlaylist) -> Unit,
    onCreatePlaylistWithTrack: (name: String, comment: String?) -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    var showCreateDialog by remember { mutableStateOf(false) }

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
                .padding(horizontal = 16.dp)
                .padding(bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(
                    text = "Add to Playlist",
                    style = type.titleMd,
                    color = colors.textPrimary,
                )
                Text(
                    text = "${track.title} · ${track.artistName}",
                    style = type.bodySm,
                    color = colors.textSecondary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }

            Surface(
                onClick = { showCreateDialog = true },
                enabled = !isBusy,
                color = colors.leafSurface,
                shape = SoulbeetShapes.md,
                border = BorderStroke(1.dp, colors.leafText.copy(alpha = 0.45f)),
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = 52.dp),
            ) {
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 14.dp, vertical = 12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    Icon(
                        imageVector = Icons.Filled.Add,
                        contentDescription = null,
                        tint = colors.leafText,
                        modifier = Modifier.size(20.dp),
                    )
                    Text(
                        text = "New Playlist",
                        style = type.body,
                        color = colors.leafText,
                    )
                }
            }

            if (playlists.isEmpty()) {
                Text(
                    text = "No custom playlists yet. Create one above to add this track.",
                    style = type.bodySm,
                    color = colors.textMuted,
                    modifier = Modifier.padding(vertical = 12.dp),
                )
            } else {
                LazyColumn(
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(max = 360.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    items(playlists, key = { "add_pl_${it.id}" }) { pl ->
                        Surface(
                            onClick = { onSelectPlaylist(pl) },
                            enabled = !isBusy,
                            color = colors.surface,
                            shape = SoulbeetShapes.md,
                            border = BorderStroke(1.dp, colors.borderSubtle),
                            modifier = Modifier
                                .fillMaxWidth()
                                .heightIn(min = 56.dp),
                        ) {
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(horizontal = 10.dp, vertical = 8.dp),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(12.dp),
                            ) {
                                PlaylistArtwork(
                                    name = pl.name,
                                    coverArtId = pl.coverArtId,
                                    artAuthority = artAuthority,
                                    size = 40.dp,
                                    slug = pl.slug,
                                    kind = pl.kind,
                                    shape = SoulbeetShapes.xs,
                                )
                                Column(modifier = Modifier.weight(1f)) {
                                    Text(
                                        text = pl.name,
                                        style = type.body,
                                        color = colors.textPrimary,
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                    Text(
                                        text = if (pl.songCount == 1L) "1 track" else "${pl.songCount} tracks",
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
    }

    if (showCreateDialog) {
        PlaylistFormDialog(
            title = "New Playlist",
            confirmLabel = "Create",
            initialName = "",
            initialComment = "",
            isBusy = isBusy,
            onDismiss = { showCreateDialog = false },
            onConfirm = { name, comment ->
                showCreateDialog = false
                onCreatePlaylistWithTrack(name, comment)
            },
        )
    }
}

/**
 * Reusable dialog for creating or renaming/editing a static user playlist.
 */
@Composable
internal fun PlaylistFormDialog(
    title: String,
    confirmLabel: String,
    initialName: String,
    initialComment: String,
    isBusy: Boolean,
    onDismiss: () -> Unit,
    onConfirm: (name: String, comment: String?) -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    var name by remember(initialName) { mutableStateOf(initialName) }
    var comment by remember(initialComment) { mutableStateOf(initialComment) }
    val canSubmit = name.trim().isNotEmpty() && !isBusy

    val fieldColors = OutlinedTextFieldDefaults.colors(
        focusedBorderColor = colors.leafSolid,
        unfocusedBorderColor = colors.borderDefault,
        focusedTextColor = colors.textPrimary,
        unfocusedTextColor = colors.textPrimary,
        focusedContainerColor = colors.surface,
        unfocusedContainerColor = colors.surface,
        focusedLabelColor = colors.leafText,
        unfocusedLabelColor = colors.textMuted,
        cursorColor = colors.leafSolid,
    )

    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = colors.surfaceHigh,
        title = {
            Text(text = title, style = type.titleMd, color = colors.textPrimary)
        },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it.take(120) },
                    label = { Text("Playlist Name") },
                    singleLine = true,
                    colors = fieldColors,
                    modifier = Modifier.fillMaxWidth(),
                )
                OutlinedTextField(
                    value = comment,
                    onValueChange = { comment = it.take(300) },
                    label = { Text("Description (optional)") },
                    maxLines = 3,
                    colors = fieldColors,
                    modifier = Modifier.fillMaxWidth(),
                )
            }
        },
        confirmButton = {
            TextButton(
                onClick = { onConfirm(name.trim(), comment.trim().takeIf { it.isNotEmpty() }) },
                enabled = canSubmit,
            ) {
                Text(
                    text = confirmLabel,
                    color = if (canSubmit) colors.leafText else colors.textDisabled,
                )
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) {
                Text("Cancel", color = colors.textSecondary)
            }
        },
    )
}
