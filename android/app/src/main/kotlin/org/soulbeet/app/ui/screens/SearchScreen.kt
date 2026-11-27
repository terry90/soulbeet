package org.soulbeet.app.ui.screens

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
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
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.ChevronRight
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.CloudDownload
import androidx.compose.material.icons.filled.History
import androidx.compose.material.icons.filled.MusicNote
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import coil.compose.SubcomposeAsyncImage
import coil.request.ImageRequest
import org.soulbeet.app.AppUiState
import org.soulbeet.app.RemoteAlbumDetailUi
import org.soulbeet.app.RemoteArtistDetailUi
import org.soulbeet.app.SearchFilter
import org.soulbeet.app.acquireTrackKey
import org.soulbeet.app.data.FormattingUtils
import org.soulbeet.app.formatFanCount
import org.soulbeet.app.formatReleaseYear
import org.soulbeet.app.formatRemoteAlbumMeta
import org.soulbeet.app.theme.LocalSoulbeetColors
import org.soulbeet.app.theme.LocalSoulbeetTypography
import org.soulbeet.app.theme.SoulbeetShapes
import org.soulbeet.app.ui.components.SeamLightBar
import org.soulbeet.app.ui.components.TrackArtwork
import org.soulbeet.app.ui.components.TrackRow
import org.soulbeet.core.FfiAlbum
import org.soulbeet.core.FfiArtist
import org.soulbeet.core.FfiOnlineAlbumMatch
import org.soulbeet.core.FfiOnlineArtistMatch
import org.soulbeet.core.FfiOnlineTrackMatch
import org.soulbeet.core.FfiPlaylist
import org.soulbeet.core.FfiTrack
import java.util.Locale

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
    onOpenRemoteAlbum: (FfiOnlineAlbumMatch) -> Unit,
    onOpenRemoteArtist: (FfiOnlineArtistMatch) -> Unit,
    onSearchDeepCatalog: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val focusManager = LocalFocusManager.current
    val results = state.searchResults

    Column(modifier = modifier.fillMaxSize()) {
        // Sticky Search Input & Filter Pills (All | Tracks | Albums | Artists)
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
                            "Search offline library..."
                        } else {
                            "Search library & global catalog..."
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
                keyboardActions = KeyboardActions(
                    onSearch = {
                        focusManager.clearFocus()
                        onSubmitQuery(state.searchQuery)
                    },
                ),
                modifier = Modifier.fillMaxWidth(),
            )

            LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                items(SearchFilter.entries) { filter ->
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
                            modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
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
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            if (state.searchQuery.isBlank()) {
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
                                            .clickable {
                                                focusManager.clearFocus()
                                                onSubmitQuery(recent)
                                            },
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
                                        .clickable {
                                            focusManager.clearFocus()
                                            onSubmitQuery(mood)
                                        },
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
            } else {
                val showTracks = state.searchFilter == SearchFilter.ALL || state.searchFilter == SearchFilter.TRACKS
                val showAlbums = state.searchFilter == SearchFilter.ALL || state.searchFilter == SearchFilter.ALBUMS
                val showArtists = state.searchFilter == SearchFilter.ALL || state.searchFilter == SearchFilter.ARTISTS
                val showPlaylists = state.searchFilter == SearchFilter.ALL
                val showCatalog = !state.config.offlineOnly

                val localArtists = if (showArtists) results?.localArtists.orEmpty() else emptyList()
                val localTracks = if (showTracks) results?.localTracks.orEmpty() else emptyList()
                val localAlbums = if (showAlbums) results?.localAlbums.orEmpty() else emptyList()
                val matchingPlaylists = if (showPlaylists) {
                    val q = state.searchQuery.trim().lowercase(Locale.ROOT)
                    state.playlists.filter { it.name.lowercase(Locale.ROOT).contains(q) }
                } else {
                    emptyList()
                }

                val onlineArtists = if (showCatalog && showArtists) results?.onlineArtists.orEmpty() else emptyList()
                val onlineTracks = if (showCatalog && showTracks) results?.onlineTracks.orEmpty() else emptyList()
                val onlineAlbums = if (showCatalog && showAlbums) results?.onlineAlbums.orEmpty() else emptyList()

                val hasLibraryHits = localArtists.isNotEmpty() ||
                    localTracks.isNotEmpty() ||
                    localAlbums.isNotEmpty() ||
                    matchingPlaylists.isNotEmpty()
                val hasCatalogHits = onlineArtists.isNotEmpty() ||
                    onlineTracks.isNotEmpty() ||
                    onlineAlbums.isNotEmpty()

                if (results != null) {
                    item(key = "search_telemetry") {
                        val localTotal = results.localTracks.size + results.localAlbums.size + results.localArtists.size
                        val catalogTotal = results.onlineTracks.size + results.onlineAlbums.size + results.onlineArtists.size
                        val catalogSuffix = when {
                            state.config.offlineOnly -> " · OFFLINE"
                            state.isSearchingRemote -> " · SEARCHING CATALOG..."
                            else -> " · $catalogTotal CATALOG"
                        }
                        Text(
                            text = "${results.elapsedMs}MS · $localTotal LIBRARY$catalogSuffix",
                            style = type.monoXs,
                            color = colors.textMuted,
                        )
                    }
                }

                // =============================================================
                // Section 1: In Your Library (instant local FTS5)
                // =============================================================
                if (hasLibraryHits) {
                    item(key = "header_library_section") {
                        Text(
                            text = "In Your Library",
                            style = type.titleMd,
                            color = colors.textPrimary,
                        )
                    }

                    if (localArtists.isNotEmpty()) {
                        if (state.searchFilter == SearchFilter.ALL) {
                            item(key = "sub_local_artists") {
                                Text(text = "ARTISTS", style = type.monoXs, color = colors.textMuted)
                            }
                        }
                        items(localArtists.take(if (state.searchFilter == SearchFilter.ARTISTS) 25 else 6), key = { "ar_${it.id}" }) { artist ->
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .heightIn(min = 56.dp)
                                    .clip(SoulbeetShapes.sm)
                                    .clickable { onOpenArtist(artist) }
                                    .padding(horizontal = 4.dp, vertical = 6.dp),
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
                                        text = if (artist.albumCount == 1L) "1 album" else "${artist.albumCount} albums",
                                        style = type.monoSm,
                                        color = colors.textMuted,
                                    )
                                }
                                Icon(
                                    imageVector = Icons.Filled.ChevronRight,
                                    contentDescription = null,
                                    tint = colors.textMuted,
                                    modifier = Modifier.size(18.dp),
                                )
                            }
                        }
                    }

                    if (localTracks.isNotEmpty()) {
                        if (state.searchFilter == SearchFilter.ALL) {
                            item(key = "sub_local_tracks") {
                                Text(text = "TRACKS", style = type.monoXs, color = colors.textMuted)
                            }
                        }
                        items(localTracks.take(if (state.searchFilter == SearchFilter.TRACKS) 30 else 10), key = { "lt_${it.id}" }) { track ->
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

                    if (localAlbums.isNotEmpty()) {
                        if (state.searchFilter == SearchFilter.ALL) {
                            item(key = "sub_local_albums") {
                                Text(text = "ALBUMS", style = type.monoXs, color = colors.textMuted)
                            }
                        }
                        items(localAlbums.take(if (state.searchFilter == SearchFilter.ALBUMS) 25 else 6), key = { "al_${it.id}" }) { album ->
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .heightIn(min = 56.dp)
                                    .clip(SoulbeetShapes.sm)
                                    .clickable { onOpenAlbum(album) }
                                    .padding(horizontal = 4.dp, vertical = 6.dp),
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
                                        text = buildString {
                                            append(album.artistName)
                                            album.year?.let { y -> append(" · $y") }
                                        },
                                        style = type.bodySm,
                                        color = colors.textSecondary,
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                }
                                Icon(
                                    imageVector = Icons.Filled.ChevronRight,
                                    contentDescription = null,
                                    tint = colors.textMuted,
                                    modifier = Modifier.size(18.dp),
                                )
                            }
                        }
                    }

                    if (matchingPlaylists.isNotEmpty()) {
                        item(key = "sub_local_playlists") {
                            Text(text = "PLAYLISTS", style = type.monoXs, color = colors.textMuted)
                        }
                        items(matchingPlaylists.take(8), key = { "pl_${it.id}" }) { pl ->
                            Row(
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .heightIn(min = 56.dp)
                                    .clip(SoulbeetShapes.sm)
                                    .clickable { onOpenPlaylist(pl) }
                                    .padding(horizontal = 4.dp, vertical = 6.dp),
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
                                        text = if (pl.songCount == 1L) "1 track" else "${pl.songCount} tracks",
                                        style = type.monoSm,
                                        color = colors.textMuted,
                                    )
                                }
                                Icon(
                                    imageVector = Icons.Filled.ChevronRight,
                                    contentDescription = null,
                                    tint = colors.textMuted,
                                    modifier = Modifier.size(18.dp),
                                )
                            }
                        }
                    }
                }

                // =============================================================
                // Section 2: Remote Catalog (debounced Deezer / MusicBrainz)
                // =============================================================
                if (showCatalog && (state.isSearchingRemote || hasCatalogHits || state.remoteSearchError != null)) {
                    item(key = "header_remote_catalog") {
                        Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                            if (hasLibraryHits) {
                                HorizontalDivider(
                                    color = colors.borderSubtle,
                                    modifier = Modifier.padding(bottom = 8.dp),
                                )
                            }
                            Row(
                                modifier = Modifier.fillMaxWidth(),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.SpaceBetween,
                            ) {
                                Row(
                                    verticalAlignment = Alignment.CenterVertically,
                                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                                ) {
                                    Text(
                                        text = "Catalog",
                                        style = type.titleMd,
                                        color = colors.textPrimary,
                                    )
                                    results?.remoteProvider?.takeIf { it.isNotBlank() && !state.isSearchingRemote }?.let { provider ->
                                        Surface(
                                            color = colors.surfaceHigh,
                                            shape = SoulbeetShapes.full,
                                            border = BorderStroke(1.dp, colors.borderSubtle),
                                        ) {
                                            Text(
                                                text = provider.uppercase(Locale.ROOT),
                                                style = type.monoXs,
                                                color = colors.textMuted,
                                                modifier = Modifier.padding(horizontal = 8.dp, vertical = 2.dp),
                                            )
                                        }
                                    }
                                }

                                if (state.isSearchingRemote) {
                                    Row(
                                        verticalAlignment = Alignment.CenterVertically,
                                        horizontalArrangement = Arrangement.spacedBy(6.dp),
                                    ) {
                                        CircularProgressIndicator(
                                            color = colors.leafSolid,
                                            strokeWidth = 2.dp,
                                            modifier = Modifier.size(14.dp),
                                        )
                                        Text(
                                            text = "Searching catalog...",
                                            style = type.monoXs,
                                            color = colors.textMuted,
                                        )
                                    }
                                }
                            }

                            if (hasCatalogHits && !state.isSearchingRemote) {
                                Text(
                                    text = "Not in library. Tap a track to acquire & play, or open an album or artist.",
                                    style = type.bodySm,
                                    color = colors.textMuted,
                                )
                            }
                        }
                    }

                    if (!state.remoteSearchError.isNullOrBlank() && !state.isSearchingRemote) {
                        item(key = "remote_search_error") {
                            Surface(
                                color = colors.crimsonSurface,
                                shape = SoulbeetShapes.md,
                                border = BorderStroke(1.dp, colors.crimsonText.copy(alpha = 0.35f)),
                            ) {
                                Text(
                                    text = state.remoteSearchError,
                                    style = type.bodySm,
                                    color = colors.crimsonText,
                                    modifier = Modifier
                                        .fillMaxWidth()
                                        .padding(14.dp),
                                )
                            }
                        }
                    }

                    // Remote Artists: horizontal scrollable circular avatar cards
                    if (onlineArtists.isNotEmpty()) {
                        if (state.searchFilter == SearchFilter.ALL) {
                            item(key = "sub_remote_artists") {
                                Text(text = "ARTISTS", style = type.monoXs, color = colors.textMuted)
                            }
                        }
                        item(key = "remote_artists_row") {
                            LazyRow(
                                horizontalArrangement = Arrangement.spacedBy(12.dp),
                                contentPadding = PaddingValues(vertical = 2.dp),
                            ) {
                                itemsIndexed(
                                    onlineArtists,
                                    key = { idx, artist -> "rar_${idx}_${artist.id}" },
                                ) { _, artist ->
                                    RemoteArtistCard(
                                        artist = artist,
                                        onClick = { onOpenRemoteArtist(artist) },
                                    )
                                }
                            }
                        }
                    }

                    // Remote Tracks: clean rows with cover art, variant badge, duration, and 1-tap Acquire & Play
                    if (onlineTracks.isNotEmpty()) {
                        if (state.searchFilter == SearchFilter.ALL) {
                            item(key = "sub_remote_tracks") {
                                Text(text = "TRACKS", style = type.monoXs, color = colors.textMuted)
                            }
                        }
                        itemsIndexed(
                            onlineTracks,
                            key = { idx, match -> "rtr_${idx}_${acquireTrackKey(match)}" },
                        ) { _, match ->
                            val acqKey = acquireTrackKey(match)
                            val stageLabel = state.acquiringStages[acqKey]
                            RemoteTrackRow(
                                match = match,
                                isAcquiring = stageLabel != null,
                                acquiringLabel = stageLabel,
                                onClick = { onAcquireTrack(match) },
                            )
                        }
                    }

                    // Remote Albums: horizontal carousel under All, 2-column grid under Albums
                    if (onlineAlbums.isNotEmpty()) {
                        if (state.searchFilter == SearchFilter.ALL) {
                            item(key = "sub_remote_albums") {
                                Text(text = "ALBUMS", style = type.monoXs, color = colors.textMuted)
                            }
                            item(key = "remote_albums_row") {
                                LazyRow(
                                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                                    contentPadding = PaddingValues(vertical = 2.dp),
                                ) {
                                    itemsIndexed(
                                        onlineAlbums,
                                        key = { idx, album -> "ral_${idx}_${album.id}" },
                                    ) { _, album ->
                                        RemoteAlbumCard(
                                            album = album,
                                            onClick = { onOpenRemoteAlbum(album) },
                                            modifier = Modifier.width(144.dp),
                                        )
                                    }
                                }
                            }
                        } else {
                            val albumRows = onlineAlbums.chunked(2)
                            itemsIndexed(
                                albumRows,
                                key = { idx, row -> "ral_grid_${idx}_${row.first().id}" },
                            ) { _, pair ->
                                Row(
                                    modifier = Modifier.fillMaxWidth(),
                                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                                ) {
                                    pair.forEach { album ->
                                        RemoteAlbumCard(
                                            album = album,
                                            onClick = { onOpenRemoteAlbum(album) },
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

                // Empty state only when both local and remote have settled with 0 shown results
                val totalShown = localArtists.size +
                    localTracks.size +
                    localAlbums.size +
                    matchingPlaylists.size +
                    onlineArtists.size +
                    onlineTracks.size +
                    onlineAlbums.size

                if (totalShown == 0 && !state.isSearching && !state.isSearchingRemote) {
                    item(key = "no_results") {
                        Surface(
                            color = colors.surface,
                            shape = SoulbeetShapes.md,
                            border = BorderStroke(1.dp, colors.borderSubtle),
                        ) {
                            Text(
                                text = "No matches for \"${state.searchQuery.trim()}\"",
                                style = type.body,
                                color = colors.textSecondary,
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(20.dp),
                            )
                        }
                    }
                }

                // Deep Catalog MusicBrainz Footer
                if (showCatalog && !state.isSearchingRemote && results != null) {
                    val isMusicBrainz = results.remoteProvider.equals("musicbrainz", ignoreCase = true)
                    item(key = "deep_catalog_footer") {
                        if (results.remoteFallbackUsed || isMusicBrainz) {
                            Text(
                                text = "Showing deeper catalog results from MusicBrainz",
                                style = type.bodySm,
                                color = colors.textMuted,
                                textAlign = TextAlign.Center,
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .padding(vertical = 8.dp),
                            )
                        } else {
                            Surface(
                                color = colors.surface,
                                shape = SoulbeetShapes.md,
                                border = BorderStroke(1.dp, colors.borderSubtle),
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .heightIn(min = 48.dp)
                                    .clip(SoulbeetShapes.md)
                                    .clickable { onSearchDeepCatalog() },
                            ) {
                                Row(
                                    modifier = Modifier
                                        .fillMaxWidth()
                                        .padding(horizontal = 14.dp, vertical = 12.dp),
                                    verticalAlignment = Alignment.CenterVertically,
                                    horizontalArrangement = Arrangement.Center,
                                ) {
                                    Icon(
                                        imageVector = Icons.Filled.Search,
                                        contentDescription = null,
                                        tint = colors.leafText,
                                        modifier = Modifier.size(16.dp),
                                    )
                                    Spacer(modifier = Modifier.width(8.dp))
                                    Text(
                                        text = "Didn't find a niche release? Search deeper catalog (MusicBrainz)",
                                        style = type.bodySm,
                                        color = colors.leafText,
                                        textAlign = TextAlign.Center,
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

@Composable
internal fun RemoteArtwork(
    imageUrl: String?,
    fallbackTitle: String,
    size: Dp? = null,
    shape: Shape = SoulbeetShapes.xs,
    preferMusicIcon: Boolean = false,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val context = LocalContext.current
    val cleanUrl = imageUrl?.trim()?.takeIf { it.isNotEmpty() }
    val sizeMod = if (size != null) modifier.size(size) else modifier

    val placeholder: @Composable () -> Unit = {
        Box(
            modifier = Modifier
                .fillMaxSize()
                .clip(shape)
                .background(
                    Brush.linearGradient(
                        colors = listOf(colors.surfaceHigh, colors.surfaceLow),
                        start = Offset.Zero,
                        end = Offset(120f, 120f),
                    ),
                )
                .border(1.dp, colors.borderSubtle, shape),
            contentAlignment = Alignment.Center,
        ) {
            val initial = fallbackTitle.trim().firstOrNull()?.uppercaseChar()?.toString()
            if (!preferMusicIcon && initial != null) {
                Text(
                    text = initial,
                    style = if (size != null && size >= 96.dp) type.display else type.titleMd,
                    color = colors.textMuted,
                )
            } else {
                Icon(
                    imageVector = Icons.Filled.MusicNote,
                    contentDescription = null,
                    tint = colors.textMuted,
                    modifier = Modifier.size(
                        if (size != null) (size * 0.44f).coerceIn(16.dp, 38.dp) else 28.dp,
                    ),
                )
            }
        }
    }

    if (cleanUrl == null) {
        Box(modifier = sizeMod) { placeholder() }
    } else {
        SubcomposeAsyncImage(
            model = ImageRequest.Builder(context)
                .data(cleanUrl)
                .crossfade(true)
                .build(),
            contentDescription = fallbackTitle,
            contentScale = ContentScale.Crop,
            loading = { placeholder() },
            error = { placeholder() },
            modifier = sizeMod
                .clip(shape)
                .border(1.dp, colors.borderSubtle, shape),
        )
    }
}

@Composable
private fun RemoteArtistCard(
    artist: FfiOnlineArtistMatch,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val subtitle = formatFanCount(artist.fanCount)
        ?: artist.disambiguation?.trim()?.takeIf { it.isNotEmpty() }

    Column(
        modifier = modifier
            .width(104.dp)
            .clip(SoulbeetShapes.md)
            .clickable(onClick = onClick)
            .padding(horizontal = 4.dp, vertical = 6.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        RemoteArtwork(
            imageUrl = artist.pictureUrl,
            fallbackTitle = artist.name,
            size = 76.dp,
            shape = SoulbeetShapes.full,
            preferMusicIcon = false,
        )
        Text(
            text = artist.name,
            style = type.bodySm,
            color = colors.textPrimary,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            textAlign = TextAlign.Center,
        )
        if (subtitle != null) {
            Text(
                text = subtitle,
                style = type.monoXs,
                color = colors.textMuted,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                textAlign = TextAlign.Center,
            )
        }
    }
}

@Composable
private fun RemoteAlbumCard(
    album: FfiOnlineAlbumMatch,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val year = formatReleaseYear(album.releaseDate)
    val trackCountLabel = album.trackCount?.takeIf { it > 0u }?.let { count ->
        if (count == 1u) "1 track" else "$count tracks"
    }
    val metaLine = listOfNotNull(year, trackCountLabel).joinToString(" · ")

    Surface(
        color = colors.surface,
        shape = SoulbeetShapes.md,
        border = BorderStroke(1.dp, colors.borderSubtle),
        modifier = modifier
            .clip(SoulbeetShapes.md)
            .clickable(onClick = onClick),
    ) {
        Column(
            modifier = Modifier.padding(10.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            RemoteArtwork(
                imageUrl = album.coverUrl,
                fallbackTitle = album.title,
                size = null,
                shape = SoulbeetShapes.sm,
                preferMusicIcon = true,
                modifier = Modifier
                    .fillMaxWidth()
                    .aspectRatio(1f),
            )
            Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(
                    text = album.title,
                    style = type.body,
                    color = colors.textPrimary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    text = album.artist,
                    style = type.bodySm,
                    color = colors.textSecondary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                if (metaLine.isNotEmpty()) {
                    Text(
                        text = metaLine,
                        style = type.monoXs,
                        color = colors.textMuted,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
        }
    }
}

@Composable
internal fun RemoteTrackRow(
    match: FfiOnlineTrackMatch,
    isAcquiring: Boolean,
    acquiringLabel: String?,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    trackNumber: Int? = null,
    showCoverArt: Boolean = true,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val subtitle = buildString {
        append(match.artist)
        val albumName = match.album.trim()
        if (albumName.isNotEmpty()) {
            append(" · ")
            append(albumName)
        }
    }
    val variant = match.variantLabel?.trim()?.takeIf { it.isNotEmpty() }
    val durationText = match.durationSecs.toLong().takeIf { it > 0L }?.let {
        FormattingUtils.formatDurationSecs(it)
    }

    Column(modifier = modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = 60.dp)
                .clip(SoulbeetShapes.sm)
                .clickable(enabled = !isAcquiring, onClick = onClick)
                .alpha(if (isAcquiring) 1f else 0.88f)
                .padding(horizontal = 8.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (trackNumber != null) {
                Text(
                    text = "$trackNumber",
                    style = type.monoSm,
                    color = colors.textMuted,
                    textAlign = TextAlign.End,
                    modifier = Modifier.width(24.dp),
                )
                Spacer(modifier = Modifier.width(10.dp))
            }

            if (showCoverArt) {
                RemoteArtwork(
                    imageUrl = match.coverUrl,
                    fallbackTitle = match.title,
                    size = 44.dp,
                    shape = SoulbeetShapes.xs,
                    preferMusicIcon = true,
                )
                Spacer(modifier = Modifier.width(12.dp))
            }

            Column(
                modifier = Modifier.weight(1f),
                verticalArrangement = Arrangement.spacedBy(2.dp),
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    Text(
                        text = match.title,
                        style = type.body,
                        color = colors.textPrimary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f, fill = false),
                    )
                    if (variant != null) {
                        Surface(
                            color = colors.accentSurface,
                            shape = SoulbeetShapes.full,
                            border = BorderStroke(1.dp, colors.accentText.copy(alpha = 0.35f)),
                        ) {
                            Text(
                                text = variant.uppercase(Locale.ROOT),
                                style = type.monoXs,
                                color = colors.accentText,
                                maxLines = 1,
                                softWrap = false,
                                modifier = Modifier.padding(horizontal = 6.dp, vertical = 1.dp),
                            )
                        }
                    }
                }
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
                    if (durationText != null) {
                        Text(
                            text = durationText,
                            style = type.monoXs,
                            color = colors.textMuted,
                            maxLines = 1,
                            softWrap = false,
                        )
                    }
                }
            }

            Spacer(modifier = Modifier.width(8.dp))

            Box(
                modifier = Modifier
                    .heightIn(min = 48.dp)
                    .clickable(enabled = !isAcquiring, onClick = onClick),
                contentAlignment = Alignment.Center,
            ) {
                Surface(
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
                            text = if (isAcquiring) (acquiringLabel ?: "ACQUIRING...") else "ACQUIRE",
                            style = type.monoXs,
                            color = if (isAcquiring) colors.accentText else colors.textPrimary,
                            maxLines = 1,
                            softWrap = false,
                        )
                    }
                }
            }
        }
        if (isAcquiring) {
            SeamLightBar(active = true)
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun RemoteAlbumDetailSheet(
    detail: RemoteAlbumDetailUi,
    state: AppUiState,
    onDismiss: () -> Unit,
    onBackToArtist: (() -> Unit)?,
    onOpenArtist: (FfiOnlineArtistMatch) -> Unit,
    onDownloadAlbum: () -> Unit,
    onAcquireTrack: (FfiOnlineTrackMatch) -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    val album = detail.album
    val metaLine = formatRemoteAlbumMeta(album)

    val resolvedArtistId = album.artistId?.trim()?.takeIf { it.isNotEmpty() }
        ?: detail.tracks.firstOrNull { !it.artistId.isNullOrBlank() }?.artistId?.trim()
    val canOpenArtist = detail.returnToArtist != null || resolvedArtistId != null || detail.album.artist.isNotBlank()

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
            if (onBackToArtist != null && detail.returnToArtist != null) {
                Row(
                    modifier = Modifier
                        .heightIn(min = 48.dp)
                        .clip(SoulbeetShapes.full)
                        .clickable(onClick = onBackToArtist)
                        .padding(end = 12.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    Icon(
                        imageVector = Icons.AutoMirrored.Filled.ArrowBack,
                        contentDescription = "Back to artist",
                        tint = colors.leafText,
                        modifier = Modifier.size(18.dp),
                    )
                    Text(
                        text = "Back to ${detail.returnToArtist.name}",
                        style = type.bodySm,
                        color = colors.leafText,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }

            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(14.dp),
            ) {
                RemoteArtwork(
                    imageUrl = album.coverUrl,
                    fallbackTitle = album.title,
                    size = 84.dp,
                    shape = SoulbeetShapes.md,
                    preferMusicIcon = true,
                )
                Column(
                    modifier = Modifier.weight(1f),
                    verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    Text(
                        text = album.title,
                        style = type.titleMd,
                        color = colors.textPrimary,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                    Row(
                        modifier = if (canOpenArtist) {
                            Modifier
                                .clip(SoulbeetShapes.xs)
                                .clickable {
                                    val backTarget = detail.returnToArtist
                                    val matchesBackTarget = backTarget != null && (
                                        (resolvedArtistId != null && backTarget.id == resolvedArtistId) ||
                                            (resolvedArtistId == null && (detail.album.artist.isBlank() || backTarget.name.equals(detail.album.artist.trim(), ignoreCase = true)))
                                        )
                                    if (matchesBackTarget && onBackToArtist != null) {
                                        onBackToArtist.invoke()
                                    } else if (resolvedArtistId != null) {
                                        onOpenArtist(
                                            FfiOnlineArtistMatch(
                                                id = resolvedArtistId,
                                                name = detail.album.artist,
                                                disambiguation = null,
                                                pictureUrl = null,
                                                fanCount = null,
                                                mbid = null,
                                                provider = detail.album.provider,
                                            ),
                                        )
                                    } else if (detail.album.artist.isNotBlank()) {
                                        onOpenArtist(
                                            FfiOnlineArtistMatch(
                                                id = "",
                                                name = detail.album.artist,
                                                disambiguation = null,
                                                pictureUrl = null,
                                                fanCount = null,
                                                mbid = null,
                                                provider = detail.album.provider,
                                            ),
                                        )
                                    }
                                }
                                .padding(vertical = 2.dp)
                        } else {
                            Modifier
                        },
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(2.dp),
                    ) {
                        Text(
                            text = album.artist,
                            style = type.body,
                            color = if (canOpenArtist) colors.leafText else colors.textSecondary,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.weight(1f, fill = false),
                        )
                        if (canOpenArtist) {
                            Icon(
                                imageVector = Icons.Filled.ChevronRight,
                                contentDescription = "Open artist",
                                tint = colors.leafText,
                                modifier = Modifier.size(16.dp),
                            )
                        }
                    }
                    if (metaLine.isNotEmpty()) {
                        Text(
                            text = metaLine,
                            style = type.monoXs,
                            color = colors.textMuted,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
            }

            // Prominent Download Album CTA + status feedback
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(
                    onClick = onDownloadAlbum,
                    enabled = !detail.isDownloading,
                    colors = ButtonDefaults.buttonColors(
                        containerColor = colors.leafSolid,
                        contentColor = colors.leafInk,
                        disabledContainerColor = colors.leafSurface,
                        disabledContentColor = colors.leafText,
                    ),
                    shape = SoulbeetShapes.full,
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(min = 48.dp),
                ) {
                    if (detail.isDownloading) {
                        CircularProgressIndicator(
                            color = colors.leafText,
                            strokeWidth = 2.dp,
                            modifier = Modifier.size(16.dp),
                        )
                        Spacer(modifier = Modifier.width(8.dp))
                        Text("Queuing Download...", style = type.body)
                    } else {
                        Icon(
                            imageVector = Icons.Filled.CloudDownload,
                            contentDescription = null,
                            modifier = Modifier.size(18.dp),
                        )
                        Spacer(modifier = Modifier.width(8.dp))
                        Text("Download Album", style = type.body)
                    }
                }

                detail.downloadMessage?.let { msg ->
                    Surface(
                        color = if (detail.downloadIsError) colors.crimsonSurface else colors.leafSurface,
                        shape = SoulbeetShapes.md,
                        border = BorderStroke(
                            1.dp,
                            if (detail.downloadIsError) {
                                colors.crimsonText.copy(alpha = 0.4f)
                            } else {
                                colors.leafText.copy(alpha = 0.4f)
                            },
                        ),
                    ) {
                        Text(
                            text = msg,
                            style = type.bodySm,
                            color = if (detail.downloadIsError) colors.crimsonText else colors.leafText,
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(horizontal = 12.dp, vertical = 10.dp),
                        )
                    }
                }
            }

            HorizontalDivider(color = colors.borderSubtle)

            when {
                detail.isLoading -> {
                    Row(
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(vertical = 24.dp),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        CircularProgressIndicator(
                            color = colors.leafSolid,
                            strokeWidth = 2.dp,
                            modifier = Modifier.size(20.dp),
                        )
                        Spacer(modifier = Modifier.width(10.dp))
                        Text(
                            text = "Loading tracklist...",
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
                        text = "No tracks listed for this release.",
                        style = type.bodySm,
                        color = colors.textMuted,
                        modifier = Modifier.padding(vertical = 12.dp),
                    )
                }

                else -> {
                    LazyColumn(
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(max = 380.dp),
                        verticalArrangement = Arrangement.spacedBy(2.dp),
                    ) {
                        itemsIndexed(
                            detail.tracks,
                            key = { idx, tr -> "rad_${idx}_${acquireTrackKey(tr)}" },
                        ) { idx, track ->
                            val acqKey = acquireTrackKey(track)
                            val stageLabel = state.acquiringStages[acqKey]
                            RemoteTrackRow(
                                match = track,
                                isAcquiring = stageLabel != null,
                                acquiringLabel = stageLabel,
                                onClick = { onAcquireTrack(track) },
                                trackNumber = idx + 1,
                                showCoverArt = false,
                            )
                        }
                    }
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun RemoteArtistDetailSheet(
    detail: RemoteArtistDetailUi,
    state: AppUiState,
    onDismiss: () -> Unit,
    onSelectDisambiguationArtist: (FfiOnlineArtistMatch) -> Unit,
    onAcquireTrack: (FfiOnlineTrackMatch) -> Unit,
    onOpenAlbum: (FfiOnlineAlbumMatch) -> Unit,
) {
    val colors = LocalSoulbeetColors.current
    val type = LocalSoulbeetTypography.current
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    val artist = detail.detail?.artist ?: detail.artist
    val loaded = detail.detail
    val subtitle = listOfNotNull(
        artist.disambiguation?.trim()?.takeIf { it.isNotEmpty() },
        formatFanCount(artist.fanCount),
    ).joinToString(" · ")
    val providerLabel = (loaded?.provider ?: artist.provider).trim()

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
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(14.dp),
            ) {
                RemoteArtwork(
                    imageUrl = artist.pictureUrl,
                    fallbackTitle = artist.name,
                    size = 72.dp,
                    shape = SoulbeetShapes.full,
                    preferMusicIcon = false,
                )
                Column(
                    modifier = Modifier.weight(1f),
                    verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    Text(
                        text = artist.name,
                        style = type.titleMd,
                        color = colors.textPrimary,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                    if (subtitle.isNotEmpty()) {
                        Text(
                            text = subtitle,
                            style = type.bodySm,
                            color = colors.textSecondary,
                            maxLines = 2,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                    if (providerLabel.isNotEmpty()) {
                        Surface(
                            color = colors.surfaceHigh,
                            shape = SoulbeetShapes.full,
                            border = BorderStroke(1.dp, colors.borderSubtle),
                        ) {
                            Text(
                                text = providerLabel.uppercase(Locale.ROOT),
                                style = type.monoXs,
                                color = colors.textMuted,
                                modifier = Modifier.padding(horizontal = 8.dp, vertical = 2.dp),
                            )
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
                            .padding(vertical = 24.dp),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        CircularProgressIndicator(
                            color = colors.leafSolid,
                            strokeWidth = 2.dp,
                            modifier = Modifier.size(20.dp),
                        )
                        Spacer(modifier = Modifier.width(10.dp))
                        Text(
                            text = "Loading artist catalog...",
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

                loaded != null -> {
                    LazyColumn(
                        modifier = Modifier
                            .fillMaxWidth()
                            .heightIn(max = 500.dp),
                        verticalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        if (loaded.others.isNotEmpty()) {
                            item(key = "artist_disambiguation") {
                                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                                    Text(
                                        text = "OTHER ARTISTS",
                                        style = type.monoXs,
                                        color = colors.textMuted,
                                    )
                                    LazyRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                        itemsIndexed(
                                            loaded.others,
                                            key = { idx, other -> "oth_${idx}_${other.id}" },
                                        ) { _, other ->
                                            val label = buildString {
                                                append(other.name)
                                                other.disambiguation?.trim()?.takeIf { it.isNotEmpty() }?.let { d ->
                                                    append(" (")
                                                    append(d)
                                                    append(")")
                                                }
                                            }
                                            OutlinedButton(
                                                onClick = { onSelectDisambiguationArtist(other) },
                                                shape = SoulbeetShapes.full,
                                                border = BorderStroke(1.dp, colors.borderDefault),
                                                modifier = Modifier.heightIn(min = 48.dp),
                                            ) {
                                                Text(
                                                    text = label,
                                                    style = type.bodySm,
                                                    color = colors.textPrimary,
                                                    maxLines = 1,
                                                    overflow = TextOverflow.Ellipsis,
                                                )
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if (loaded.topTracks.isNotEmpty()) {
                            item(key = "artist_top_tracks_header") {
                                Text(
                                    text = "Top Tracks",
                                    style = type.titleMd,
                                    color = colors.textPrimary,
                                )
                            }
                            itemsIndexed(
                                loaded.topTracks,
                                key = { idx, tr -> "att_${idx}_${acquireTrackKey(tr)}" },
                            ) { _, track ->
                                val acqKey = acquireTrackKey(track)
                                val stageLabel = state.acquiringStages[acqKey]
                                RemoteTrackRow(
                                    match = track,
                                    isAcquiring = stageLabel != null,
                                    acquiringLabel = stageLabel,
                                    onClick = { onAcquireTrack(track) },
                                )
                            }
                        }

                        if (loaded.albums.isNotEmpty()) {
                            item(key = "artist_albums_header") {
                                Text(
                                    text = "Discography",
                                    style = type.titleMd,
                                    color = colors.textPrimary,
                                )
                            }
                            itemsIndexed(
                                loaded.albums,
                                key = { idx, al -> "aal_${idx}_${al.id}" },
                            ) { _, album ->
                                val albumMeta = formatRemoteAlbumMeta(album)
                                Row(
                                    modifier = Modifier
                                        .fillMaxWidth()
                                        .heightIn(min = 60.dp)
                                        .clip(SoulbeetShapes.sm)
                                        .clickable { onOpenAlbum(album) }
                                        .padding(horizontal = 4.dp, vertical = 6.dp),
                                    verticalAlignment = Alignment.CenterVertically,
                                ) {
                                    RemoteArtwork(
                                        imageUrl = album.coverUrl,
                                        fallbackTitle = album.title,
                                        size = 48.dp,
                                        shape = SoulbeetShapes.xs,
                                        preferMusicIcon = true,
                                    )
                                    Spacer(modifier = Modifier.width(12.dp))
                                    Column(
                                        modifier = Modifier.weight(1f),
                                        verticalArrangement = Arrangement.spacedBy(2.dp),
                                    ) {
                                        Text(
                                            text = album.title,
                                            style = type.body,
                                            color = colors.textPrimary,
                                            maxLines = 1,
                                            overflow = TextOverflow.Ellipsis,
                                        )
                                        if (albumMeta.isNotEmpty()) {
                                            Text(
                                                text = albumMeta,
                                                style = type.monoXs,
                                                color = colors.textMuted,
                                                maxLines = 1,
                                                overflow = TextOverflow.Ellipsis,
                                            )
                                        }
                                    }
                                    Icon(
                                        imageVector = Icons.Filled.ChevronRight,
                                        contentDescription = "Open album",
                                        tint = colors.textMuted,
                                        modifier = Modifier.size(18.dp),
                                    )
                                }
                            }
                        }

                        if (loaded.topTracks.isEmpty() && loaded.albums.isEmpty()) {
                            item(key = "artist_empty_catalog") {
                                Text(
                                    text = "No top tracks or discography releases available for this artist.",
                                    style = type.bodySm,
                                    color = colors.textMuted,
                                    modifier = Modifier.padding(vertical = 8.dp),
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}
