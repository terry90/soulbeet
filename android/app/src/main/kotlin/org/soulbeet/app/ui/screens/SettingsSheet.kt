package org.soulbeet.app.ui.screens

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Logout
import androidx.compose.material.icons.filled.CloudSync
import androidx.compose.material.icons.filled.DirectionsCar
import androidx.compose.material.icons.filled.GraphicEq
import androidx.compose.material.icons.filled.Nightlight
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import org.soulbeet.app.AppUiState
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.media.LibrarySecondaryAction
import org.soulbeet.media.OfflineSyncWorker

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsSheet(
    state: AppUiState,
    onDismiss: () -> Unit,
    onToggleOled: (Boolean) -> Unit,
    onSelectAutoSecondaryAction: (LibrarySecondaryAction) -> Unit,
    onSelectQualityProfile: (String) -> Unit,
    onTriggerSync: () -> Unit,
    onDrainOutbox: () -> Unit,
    onSignOut: () -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    val outbox = state.outboxSummary

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
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 20.dp)
                .padding(bottom = 32.dp),
            verticalArrangement = Arrangement.spacedBy(18.dp),
        ) {
            Text(text = "Studio Settings", style = type.titleLg, color = colors.textPrimary)

            // 1. Server & Account Info
            Surface(
                color = colors.surface,
                shape = SoulbeetShapes.md,
                border = BorderStroke(1.dp, colors.borderSubtle),
            ) {
                Column(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(14.dp),
                    verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    Text(
                        text = state.config.username.ifBlank { "Connected Account" },
                        style = type.body,
                        color = colors.textPrimary,
                    )
                    Text(
                        text = state.config.serverUrl,
                        style = type.monoSm,
                        color = colors.textMuted,
                    )
                }
            }

            // 2. Appearance — True-Black OLED Mode
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = 52.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                    modifier = Modifier.weight(1f),
                ) {
                    Icon(Icons.Filled.Nightlight, contentDescription = null, tint = colors.accentText)
                    Column {
                        Text("True-Black OLED Mode", style = type.body, color = colors.textPrimary)
                        Text(
                            "Pure #000000 canvas for AMOLED displays",
                            style = type.bodySm,
                            color = colors.textSecondary,
                        )
                    }
                }
                Switch(
                    checked = state.config.oledMode,
                    onCheckedChange = onToggleOled,
                    colors = SwitchDefaults.colors(
                        checkedThumbColor = colors.leafInk,
                        checkedTrackColor = colors.leafSolid,
                        uncheckedThumbColor = colors.textMuted,
                        uncheckedTrackColor = colors.surface,
                    ),
                )
            }

            HorizontalDivider(color = colors.borderSubtle)

            // 3. Audio Quality Profile
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    Icon(Icons.Filled.GraphicEq, contentDescription = null, tint = colors.leafText)
                    Text("Streaming & Cache Quality", style = type.body, color = colors.textPrimary)
                }
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    val isFlac = state.config.qualityProfileKey == OfflineSyncWorker.PROFILE_LOSSLESS_FLAC
                    SegmentedOptionChip(
                        label = "Opus 160 kbps",
                        selected = !isFlac,
                        onClick = { onSelectQualityProfile(OfflineSyncWorker.PROFILE_CELLULAR_OPUS_160) },
                        modifier = Modifier.weight(1f),
                    )
                    SegmentedOptionChip(
                        label = "Lossless FLAC",
                        selected = isFlac,
                        onClick = { onSelectQualityProfile(OfflineSyncWorker.PROFILE_LOSSLESS_FLAC) },
                        modifier = Modifier.weight(1f),
                    )
                }
            }

            HorizontalDivider(color = colors.borderSubtle)

            // 4. Android Auto Secondary Library Action
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    Icon(Icons.Filled.DirectionsCar, contentDescription = null, tint = colors.gold)
                    Column {
                        Text("Android Auto Secondary Button", style = type.body, color = colors.textPrimary)
                        Text(
                            "Slot 2 action when playing permanent Library tracks",
                            style = type.bodySm,
                            color = colors.textSecondary,
                        )
                    }
                }
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    SegmentedOptionChip(
                        label = "1★ & Skip",
                        selected = state.config.librarySecondaryAction == LibrarySecondaryAction.DISLIKE_SKIP,
                        onClick = { onSelectAutoSecondaryAction(LibrarySecondaryAction.DISLIKE_SKIP) },
                        modifier = Modifier.weight(1f),
                    )
                    SegmentedOptionChip(
                        label = "Track Radio",
                        selected = state.config.librarySecondaryAction == LibrarySecondaryAction.START_RADIO,
                        onClick = { onSelectAutoSecondaryAction(LibrarySecondaryAction.START_RADIO) },
                        modifier = Modifier.weight(1f),
                    )
                }
            }

            HorizontalDivider(color = colors.borderSubtle)

            // 5. Sync & Offline Outbox Status
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
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                    ) {
                        Icon(Icons.Filled.CloudSync, contentDescription = null, tint = colors.leafText)
                        Column {
                            Text("Sync & Mutation Outbox", style = type.body, color = colors.textPrimary)
                            val pending = outbox?.pendingCount ?: 0uL
                            val dead = outbox?.deadLetterCount ?: 0uL
                            Text(
                                text = "$pending pending · $dead dead-letter",
                                style = type.monoSm,
                                color = if (dead > 0uL) colors.crimsonText else colors.textSecondary,
                            )
                        }
                    }

                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedButton(
                            onClick = onTriggerSync,
                            shape = SoulbeetShapes.full,
                            border = BorderStroke(1.dp, colors.borderDefault),
                            modifier = Modifier
                                .weight(1f)
                                .heightIn(min = 48.dp),
                        ) {
                            Text(
                                text = if (state.isSyncing) "Syncing…" else "Sync Now",
                                style = type.bodySm,
                                color = colors.leafText,
                            )
                        }
                        OutlinedButton(
                            onClick = onDrainOutbox,
                            shape = SoulbeetShapes.full,
                            border = BorderStroke(1.dp, colors.borderDefault),
                            modifier = Modifier
                                .weight(1f)
                                .heightIn(min = 48.dp),
                        ) {
                            Text(
                                text = "Drain Outbox",
                                style = type.bodySm,
                                color = colors.textPrimary,
                            )
                        }
                    }
                }
            }

            // 6. Disconnect / Sign Out
            OutlinedButton(
                onClick = onSignOut,
                shape = SoulbeetShapes.full,
                border = BorderStroke(1.dp, colors.crimsonText.copy(alpha = 0.45f)),
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = 48.dp),
            ) {
                Icon(
                    imageVector = Icons.AutoMirrored.Filled.Logout,
                    contentDescription = null,
                    tint = colors.crimsonText,
                    modifier = Modifier.size(18.dp),
                )
                Spacer(Modifier.width(8.dp))
                Text("Disconnect Server", style = type.body, color = colors.crimsonText)
            }
        }
    }
}

@Composable
private fun SegmentedOptionChip(
    label: String,
    selected: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    Surface(
        color = if (selected) colors.leafSurface else colors.surface,
        shape = SoulbeetShapes.full,
        border = BorderStroke(
            1.dp,
            if (selected) colors.leafText.copy(alpha = 0.5f) else colors.borderSubtle,
        ),
        modifier = modifier
            .heightIn(min = 48.dp)
            .clickable(onClick = onClick),
    ) {
        Box(contentAlignment = Alignment.Center, modifier = Modifier.padding(horizontal = 12.dp, vertical = 10.dp)) {
            Text(
                text = label,
                style = type.bodySm,
                color = if (selected) colors.leafText else colors.textSecondary,
            )
        }
    }
}
