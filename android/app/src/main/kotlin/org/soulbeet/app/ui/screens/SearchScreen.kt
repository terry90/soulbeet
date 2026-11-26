package org.soulbeet.app.ui.screens

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.History
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import org.soulbeet.app.AppUiState
import org.soulbeet.app.SearchFilter
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.ui.components.SeamLightBar
import org.soulbeet.app.ui.components.TrackArtwork
import org.soulbeet.app.ui.components.TrackRow
import org.soulbeet.core.FfiAlbum
import org.soulbeet.core.FfiArtist
import org.soulbeet.core.FfiOnlineTrackMatch
import org.soulbeet.core.FfiPlaylist
import org.soulbeet.core.FfiTrack

private val QUICK_MOOD_PILLS = listOf(
    "Synthpop",
    "French Electro",
    "Late Night Jazz",
    "Ambient Focus",
    "Drive Rock",
    "Indie Chill",
)

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun SearchScreen(
    state: AppUiState,
    artAuthority: String,
    bottomContentPadding: PaddingValues,
    onQueryChange: (String) -> Unit,
    onSubmitQuery: (String) -> Unit,
    onClearRecents: () -> Unit,
    onSelectFilter: (SearchFilter) -> Unit,
    onPlayTrack: (FfiTrack) -> Unit,
    onStarTrack: (FfiTrack) -> Unit,
    onOpenAlbum: (FfiAlbum) -> Unit,
    onOpenArtist: (FfiArtist) -> Unit,
    onOpenPlaylist: (FfiPlaylist) -> Unit,
    onAcquireTrack: (FfiOnlineTrackMatch) -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val results = state.searchResults

    Column(modifier = modifier.fillMaxSize()) {
        // Sticky Search Input & Filter Chips
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 16.dp, vertical = 8.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            OutlinedTextField(
                value = state.searchQuery,
                onValueChange = onQueryChange,
                placeholder = {
                    Text(
                        text = if (state.config.offlineOnly) {
                            "Search offline library…"
                        } else {
                            "Search library & global catalog…"
                        },
                        style = type.body,
                        color = colors.textMuted,
                    )
                },
                leadingIcon = {
                    Icon(
                        imageVector = Icons.Filled.Search,
                        contentDescription = null,
                        tint = colors.textMuted,
                    )
                },
                trailingIcon = {
                    if (state.searchQuery.isNotEmpty()) {
                        IconButton(
                            onClick = { onQueryChange("") },
                            modifier = Modifier.size(48.dp),
                        ) {
                            Icon(
                                imageVector = Icons.Filled.Close,
                                contentDescription = "Clear search",
                                tint = colors.textSecondary,
                            )
                        }
                    }
                },
                singleLine = true,
                shape = SoulbeetShapes.full,
                colors = OutlinedTextFieldDefaults.colors(
                    focusedBorderColor = colors.leafSolid,
                    unfocusedBorderColor = colors.borderDefault,
                    focusedContainerColor = colors.surfaceHighest,
                    unfocusedContainerColor = colors.surface,
                    focusedTextColor = colors.textPrimary,
                    unfocusedTextColor = colors.textPrimary,
                    cursorColor = colors.leafSolid,
                ),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
                keyboardActions = KeyboardActions(onSearch = { onSubmitQuery(state.searchQuery) }),
                modifier = Modifier.fillMaxWidth(),
            )

            LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                val visibleFilters = if (state.config.offlineOnly) {
                    SearchFilter.entries.filterNot { it == SearchFilter.CATALOG }
                } else {
                    SearchFilter.entries
                }
                items(visibleFilters) { filter ->
                    val selected = state.searchFilter == filter
                    Surface(
                        color = if (selected) colors.leafSurface else colors.surfaceHigh,
                        shape = SoulbeetShapes.full,
                        border = BorderStroke(
                            1.dp,
                            if (selected) colors.leafText.copy(alpha = 0.45f) else colors.borderSubtle,
                        ),
                        modifier = Modifier
                            .heightIn(min = 48.dp)
                            .clickable { onSelectFilter(filter) },
                    ) {
                        Box(
                            modifier = Modifier.padding(horizontal = 14.dp, vertical = 12.dp),
                            contentAlignment = Alignment.Center,
                        ) {
                            Text(
                                text = filter.label,
                                style = type.bodySm,
                                color = if (selected) colors.leafText else colors.textSecondary,
                            )
                        }
                    }
                }
            }
        }

        SeamLightBar(active = state.isSearching)

        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(
                start = 16.dp,
                end = 16.dp,
                top = 8.dp,
                bottom = bottomContentPadding.calculateBottomPadding() + 24.dp,
            ),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            if (state.searchQuery.isBlank()) {
                // Never Blank! Recent Searches + Quick Mood & Seasonal Stations
                if (state.config.recentSearches.isNotEmpty()) {
                    item(key = "recent_searches") {
                        Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                            Row(
                                modifier = Modifier.fillMaxWidth(),
                                horizontalArrangement = Arrangement.SpaceBetween,
                                verticalAlignment = Alignment.CenterVertically,
                            ) {
                                Text(
                                    text = "Recent Searches",
                                    style = type.titleMd,
                                    color = colors.textPrimary,
                                )
                                TextButton(onClick = onClearRecents) {
                                    Text("Clear", style = type.monoSm, color = colors.textMuted)
                                }
                            }
                            FlowRow(
                                horizontalArrangement = Arrangement.spacedBy(8.dp),
                                verticalArrangement = Arrangement.spacedBy(8.dp),
                            ) {
                                state.config.recentSearches.forEach { recent ->
                                    Surface(
                                        color = colors.surface,
                                        shape = SoulbeetShapes.full,
                                        border = BorderStroke(1.dp, colors.borderSubtle),
                                        modifier = Modifier
                                            .heightIn(min = 48.dp)
                                            .clickable { onSubmitQuery(recent) },
                                    ) {
                                        Row(
                                            modifier = Modifier.padding(horizontal = 12.dp, vertical = 12.dp),
                                            verticalAlignment = Alignment.CenterVertically,
                                            horizontalArrangement = Arrangement.spacedBy(6.dp),
                                        ) {
                                            Icon(
                                                imageVector = Icons.Filled.History,
                                                contentDescription = null,
                                                tint = colors.textMuted,
                                                modifier = Modifier.size(16.dp),
                                            )
                                            Text(
                                                text = recent,
                                                style = type.bodySm,
                                                color = colors.textPrimary,
                                            )
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                item(key = "quick_moods") {
                    Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                        Text(
                            text = "Quick Moods & Genres",
                            style = type.titleMd,
                            color = colors.textPrimary,
                        )
                        FlowRow(
                            horizontalArrangement = Arrangement.spacedBy(8.dp),
                            verticalArrangement = Arrangement.spacedBy(8.dp),
                        ) {
                            QUICK_MOOD_PILLS.forEach { mood ->
                                Surface(
                                    color = colors.surfaceHigh,
                                    shape = SoulbeetShapes.full,
                                    border = BorderStroke(1.dp, colors.borderDefault),
                                    modifier = Modifier
                                        .heightIn(min = 48.dp)
                                        .clickable { onSubmitQuery(mood) },
                                ) {
                                    Box(
                                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                                        contentAlignment = Alignment.Center,
                                    ) {
                                        Text(
                                            text = mood,
                                            style = type.bodySm,
                                            color = colors.textPrimary,
                                        )
                                    }
                                }
                            }
                        }
                    }
                }
            } else if (results != null) {
                val showLocalTracks = state.searchFilter in setOf(SearchFilter.ALL, SearchFilter.IN_LIBRARY)
                val showArtists = state.searchFilter in setOf(SearchFilter.ALL, SearchFilter.ARTISTS)
                val showAlbums = state.searchFilter in setOf(SearchFilter.ALL, SearchFilter.ALBUMS)
                val showPlaylists = state.searchFilter in setOf(SearchFilter.ALL, SearchFilter.PLAYLISTS)
                val showCatalog = !state.config.offlineOnly &&
                    state.searchFilter in setOf(SearchFilter.ALL, SearchFilter.CATALOG)

                val matchingPlaylists = if (showPlaylists) {
                    val q = state.searchQuery.trim().lowercase()
                    state.playlists.filter { it.name.lowercase().contains(q) }
                } else {
                    emptyList()
                }

                // Telemetry chip (0ms FTS5)
                item(key = "search_telemetry") {
                    Text(
                        text = "${results.elapsedMs}MS · ${results.localTracks.size} LIBRARY" +
                            if (!state.config.offlineOnly) " · ${results.onlineTracks.size} CATALOG" else " · OFFLINE",
                        style = type.monoXs,
                        color = colors.textMuted,
                    )
                }

                if (showLocalTracks && results.localTracks.isNotEmpty()) {
                    item(key = "header_local_tracks") {
                        Text(text = "In Your Library", style = type.titleMd, color = colors.textPrimary)
                    }
                    items(results.localTracks, key = { "lt_${it.id}" }) { track ->
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
                            onClick = { onPlayTrack(track) },
                            onStarClick = { onStarTrack(track) },
                        )
                    }
                }

                if (showArtists && results.localArtists.isNotEmpty()) {
                    item(key = "header_artists") {
                        Text(text = "Artists", style = type.titleMd, color = colors.textPrimary)
                    }
                    items(results.localArtists.take(8), key = { "ar_${it.id}" }) { artist ->
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .heightIn(min = 56.dp)
                                .clickable { onOpenArtist(artist) }
                                .padding(vertical = 6.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            TrackArtwork(
                                coverArtId = artist.coverArtId,
                                artAuthority = artAuthority,
                                fallbackTitle = artist.name,
                                size = 44.dp,
                                shape = SoulbeetShapes.full,
                            )
                            Spacer(modifier = Modifier.width(12.dp))
                            Column(modifier = Modifier.weight(1f)) {
                                Text(
                                    text = artist.name,
                                    style = type.body,
                                    color = colors.textPrimary,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                                Text(
                                    text = "${artist.albumCount} albums",
                                    style = type.monoSm,
                                    color = colors.textMuted,
                                )
                            }
                        }
                    }
                }

                if (showAlbums && results.localAlbums.isNotEmpty()) {
                    item(key = "header_albums") {
                        Text(text = "Albums", style = type.titleMd, color = colors.textPrimary)
                    }
                    items(results.localAlbums.take(8), key = { "al_${it.id}" }) { album ->
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .heightIn(min = 56.dp)
                                .clickable { onOpenAlbum(album) }
                                .padding(vertical = 6.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            TrackArtwork(
                                coverArtId = album.coverArtId,
                                artAuthority = artAuthority,
                                fallbackTitle = album.name,
                                size = 44.dp,
                                shape = SoulbeetShapes.xs,
                            )
                            Spacer(modifier = Modifier.width(12.dp))
                            Column(modifier = Modifier.weight(1f)) {
                                Text(
                                    text = album.name,
                                    style = type.body,
                                    color = colors.textPrimary,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                                Text(
                                    text = album.artistName,
                                    style = type.bodySm,
                                    color = colors.textSecondary,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                            }
                        }
                    }
                }

                if (matchingPlaylists.isNotEmpty()) {
                    item(key = "header_playlists") {
                        Text(text = "Playlists", style = type.titleMd, color = colors.textPrimary)
                    }
                    items(matchingPlaylists.take(8), key = { "pl_${it.id}" }) { pl ->
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .heightIn(min = 56.dp)
                                .clickable { onOpenPlaylist(pl) }
                                .padding(vertical = 6.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            TrackArtwork(
                                coverArtId = pl.coverArtId,
                                artAuthority = artAuthority,
                                fallbackTitle = pl.name,
                                size = 44.dp,
                                shape = SoulbeetShapes.xs,
                            )
                            Spacer(modifier = Modifier.width(12.dp))
                            Column(modifier = Modifier.weight(1f)) {
                                Text(
                                    text = pl.name,
                                    style = type.body,
                                    color = colors.textPrimary,
                                    maxLines = 1,
                                    overflow = TextOverflow.Ellipsis,
                                )
                                Text(
                                    text = "${pl.songCount} tracks",
                                    style = type.monoSm,
                                    color = colors.textMuted,
                                )
                            }
                        }
                    }
                }

                if (showCatalog && results.onlineTracks.isNotEmpty()) {
                    item(key = "header_online_catalog") {
                        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                            Text(
                                text = "Global Catalog",
                                style = type.titleMd,
                                color = colors.textPrimary,
                            )
                            Text(
                                text = "Not in library — tap Acquire to download & play",
                                style = type.bodySm,
                                color = colors.textMuted,
                            )
                        }
                    }
                    itemsIndexed(
                        results.onlineTracks,
                        key = { idx, it -> "ol_${idx}_${it.mbid ?: "${it.artist}_${it.title}"}" },
                    ) { _, match ->
                        val acqKey = "${match.artist.lowercase()}::${match.title.lowercase()}"
                        val stageLabel = state.acquiringStages[acqKey]
                        TrackRow(
                            title = match.title,
                            artist = match.artist,
                            durationSecs = match.durationSecs?.toLong(),
                            coverArtId = null,
                            artAuthority = artAuthority,
                            isUnownedCatalog = true,
                            isAcquiring = stageLabel != null,
                            acquiringLabel = stageLabel,
                            onClick = { onAcquireTrack(match) },
                        )
                    }
                }

                val totalShown = (if (showLocalTracks) results.localTracks.size else 0) +
                    (if (showArtists) results.localArtists.size else 0) +
                    (if (showAlbums) results.localAlbums.size else 0) +
                    matchingPlaylists.size +
                    (if (showCatalog) results.onlineTracks.size else 0)

                if (totalShown == 0 && !state.isSearching) {
                    item(key = "no_results") {
                        Surface(
                            color = colors.surface,
                            shape = SoulbeetShapes.md,
                            border = BorderStroke(1.dp, colors.borderSubtle),
                        ) {
                            Text(
                                text = "No matches for \"${state.searchQuery}\"",
                                style = type.body,
                                color = colors.textSecondary,
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(20.dp),
                            )
                        }
                    }
                }
            }
        }
    }
}
