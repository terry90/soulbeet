//! Canonical ranking and deduplication engine for Deezer catalog search results.
//!
//! Turns raw Deezer `/search/track`, `/search/album`, and `/search/artist` payloads into clean,
//! relevance-ranked, studio-preferring, karaoke-filtered `CatalogTrack`, `CatalogAlbum`, and
//! `CatalogArtist` lists.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use shared::metadata::{CatalogAlbum, CatalogArtist, CatalogProvider, CatalogTrack};
use shared::search_norm::{
    base_song_title_key, canonical_album_key, canonical_artist_key, canonical_title_key,
    classify_track_variant, fold_search_text, is_compilation_album_title, is_karaoke_album_title,
    is_karaoke_or_tribute_artist, is_live_album_title, query_targets_artist, QueryVariantIntent,
    TrackVariantKind,
};

use crate::deezer::models::{
    DeezerArtistAlbum, DeezerSearchAlbum, DeezerSearchArtist, DeezerSearchTrack,
};

/// Formats a non-zero duration in seconds as `MM:SS`.
pub fn format_duration_secs(secs: u32) -> Option<String> {
    (secs > 0).then(|| format!("{:02}:{:02}", secs / 60, secs % 60))
}

fn sanitize_http_url(raw: Option<String>) -> Option<String> {
    let trimmed = raw?.trim().to_string();
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    if rest.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let parsed = url::Url::parse(&trimmed).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || !parsed.has_host() {
        return None;
    }
    Some(trimmed)
}

/// Returns the first valid `http://` or `https://` image URL among `primary` and `fallback`.
pub(crate) fn pick_url(primary: Option<String>, fallback: Option<String>) -> Option<String> {
    sanitize_http_url(primary).or_else(|| sanitize_http_url(fallback))
}

/// Normalizes Deezer release dates, dropping empty or placeholder `"0000-00-00"` values.
pub(crate) fn clean_release_date(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.starts_with("0000"))
}

/// Maps Deezer's lowercase `record_type` into MusicBrainz-style `release_types`.
pub(crate) fn map_record_type(record_type: Option<&str>) -> Option<Vec<String>> {
    match record_type.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("album") => Some(vec!["Album".into()]),
        Some("ep") => Some(vec!["EP".into()]),
        Some("single") => Some(vec!["Single".into()]),
        Some("compile") | Some("compilation") => {
            Some(vec!["Album".into(), "Compilation".into()])
        }
        _ => None,
    }
}

/// Computes token coverage `[0.0, 1.0]` and exact full-token match flag for `query_tokens`
/// against `haystack_words`.
fn token_match_stats(query_tokens: &[&str], haystack_words: &[&str]) -> (f64, bool) {
    if query_tokens.is_empty() {
        return (0.0, false);
    }
    let mut sum = 0.0;
    let mut all_exact = true;
    for &qt in query_tokens {
        if haystack_words.contains(&qt) {
            sum += 1.0;
        } else {
            all_exact = false;
            if qt.len() >= 3 && haystack_words.iter().any(|w| w.starts_with(qt)) {
                sum += 0.75;
            }
        }
    }
    (sum / query_tokens.len() as f64, all_exact)
}

fn contains_phrase_words(haystack: &str, needle: &str) -> bool {
    if haystack.is_empty() || needle.is_empty() {
        return false;
    }
    let h: Vec<&str> = haystack.split_whitespace().collect();
    let n: Vec<&str> = needle.split_whitespace().collect();
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w == n.as_slice())
}

fn is_exact_artist_and_title(folded_query: &str, folded_artist: &str, candidate_title: &str) -> bool {
    if folded_query.is_empty() || folded_artist.is_empty() || candidate_title.is_empty() {
        return false;
    }
    let mut at = String::with_capacity(folded_artist.len() + 1 + candidate_title.len());
    at.push_str(folded_artist);
    at.push(' ');
    at.push_str(candidate_title);
    if at == folded_query {
        return true;
    }
    let mut ta = String::with_capacity(candidate_title.len() + 1 + folded_artist.len());
    ta.push_str(candidate_title);
    ta.push(' ');
    ta.push_str(folded_artist);
    ta == folded_query
}

/// Converts a raw `DeezerSearchTrack` and its classified variant into a `CatalogTrack`.
pub fn deezer_track_to_catalog(track: DeezerSearchTrack, variant: TrackVariantKind) -> CatalogTrack {
    let duration_secs = (track.duration > 0).then_some(track.duration);
    CatalogTrack {
        id: format!("deezer:track:{}", track.id),
        title: track.title,
        artist: track.artist.name,
        artist_id: Some(format!("deezer:artist:{}", track.artist.id)),
        album_id: Some(format!("deezer:album:{}", track.album.id)),
        album_title: (!track.album.title.trim().is_empty()).then_some(track.album.title),
        release_date: None,
        duration: format_duration_secs(track.duration),
        duration_secs,
        mbid: None,
        release_mbid: None,
        cover_url: pick_url(track.album.cover_medium, track.album.cover_big),
        variant_label: variant.label().map(str::to_string),
        provider: CatalogProvider::Deezer,
    }
}

/// Picks the best release copy within a `(canonical_artist_key, canonical_title_key, variant)`
/// duplicate group, strongly preferring studio/non-compilation albums over compilations.
fn pick_best_track_in_group(group: Vec<(usize, DeezerSearchTrack)>) -> (usize, DeezerSearchTrack, u64) {
    let max_rank = group.iter().map(|(_, t)| t.rank).max().unwrap_or(0);

    let mut non_comp: Vec<(usize, DeezerSearchTrack)> = Vec::new();
    let mut comp: Vec<(usize, DeezerSearchTrack)> = Vec::new();
    for item in group {
        if is_compilation_album_title(&item.1.album.title) {
            comp.push(item);
        } else {
            non_comp.push(item);
        }
    }

    let is_clean_original = |t: &DeezerSearchTrack| {
        fold_search_text(&t.title) == canonical_title_key(&t.title)
            && fold_search_text(&t.album.title) == canonical_album_key(&t.album.title)
    };

    let rank_cmp = |a: &(usize, DeezerSearchTrack), b: &(usize, DeezerSearchTrack)| {
        let a_clean = is_clean_original(&a.1);
        let b_clean = is_clean_original(&b.1);
        b.1.rank
            .cmp(&a.1.rank)
            .then_with(|| b_clean.cmp(&a_clean))
            .then_with(|| a.1.album.title.len().cmp(&b.1.album.title.len()))
            .then_with(|| a.0.cmp(&b.0))
    };

    non_comp.sort_by(rank_cmp);
    comp.sort_by(rank_cmp);

    // Among non-compilation releases, prefer a clean un-remastered studio release (e.g.
    // "Agents Of Fortune") over a remastered/deluxe edition (e.g. "Cult Classic (Remastered)")
    // whenever its rank is at least 30% of the top non-compilation rank.
    let best_non_comp = if let Some(top_nc) = non_comp.first() {
        let min_clean_rank = top_nc.1.rank.saturating_mul(3) / 10;
        let preferred_idx = if is_clean_original(&top_nc.1) {
            0
        } else {
            non_comp
                .iter()
                .position(|(_, t)| is_clean_original(t) && t.rank >= min_clean_rank)
                .unwrap_or(0)
        };
        Some(non_comp.swap_remove(preferred_idx))
    } else {
        None
    };
    let best_comp = comp.into_iter().next();

    let (idx, winner) = match (best_non_comp, best_comp) {
        (Some(nc), Some(c)) => {
            // Prefer the non-compilation studio release whenever its rank is reasonable
            // (at least 5% of the top compilation rank, or compilation rank is 0).
            if c.1.rank == 0 || (nc.1.rank > 0 && nc.1.rank.saturating_mul(20) >= c.1.rank) {
                nc
            } else {
                c
            }
        }
        (Some(nc), None) => nc,
        (None, Some(c)) => c,
        (None, None) => unreachable!("group is non-empty"),
    };

    (idx, winner, max_rank)
}

/// Ranks, deduplicates, collapses compilation copies, removes karaoke spam (unless queried),
/// and caps secondary non-studio variants for Deezer track search results.
pub fn rank_and_dedup_deezer_tracks(
    query: &str,
    raw: Vec<DeezerSearchTrack>,
    limit: usize,
) -> Vec<CatalogTrack> {
    if limit == 0 || raw.is_empty() {
        return Vec::new();
    }

    let intent = QueryVariantIntent::from_query(query);
    let folded_query = fold_search_text(query);
    let query_tokens: Vec<&str> = folded_query.split_whitespace().collect();

    // Step 1: filter karaoke unless explicitly requested, and group exact duplicates.
    let mut groups: HashMap<(String, String, TrackVariantKind), Vec<(usize, DeezerSearchTrack)>> =
        HashMap::new();

    for (idx, track) in raw.into_iter().enumerate() {
        let targets_artist = query_targets_artist(query, &track.artist.name);
        let variant = classify_track_variant(
            if targets_artist { "" } else { &track.artist.name },
            &track.title,
            Some(&track.album.title),
            None,
            false,
        );
        if variant == TrackVariantKind::Karaoke
            && !(intent.karaoke || intent.cover || targets_artist)
        {
            continue;
        }
        let artist_key = canonical_artist_key(&track.artist.name);
        let title_key = canonical_title_key(&track.title);
        groups
            .entry((artist_key, title_key, variant))
            .or_default()
            .push((idx, track));
    }

    struct ScoredTrack {
        track: DeezerSearchTrack,
        variant: TrackVariantKind,
        variant_allowed: bool,
        artist_key: String,
        base_song_key: String,
        effective_rank: u64,
        score: f64,
        orig_idx: usize,
    }

    // Step 2: pick canonical release per group and compute relevance + popularity score.
    let mut candidates: Vec<ScoredTrack> = Vec::with_capacity(groups.len());
    for ((artist_key, _title_key, variant), group) in groups {
        let (orig_idx, winner, effective_rank) = pick_best_track_in_group(group);
        let base_song_key = base_song_title_key(&winner.title);
        let targets_artist = query_targets_artist(query, &winner.artist.name);
        let variant_allowed =
            intent.allows(variant) || (variant == TrackVariantKind::Karaoke && targets_artist);

        let folded_artist = fold_search_text(&winner.artist.name);
        let folded_title = fold_search_text(&winner.title);
        let canon_title = canonical_title_key(&winner.title);
        let folded_album = fold_search_text(&winner.album.title);

        let mut combined_words: Vec<&str> = folded_artist.split_whitespace().collect();
        combined_words.extend(folded_title.split_whitespace());

        let (coverage, all_exact) = token_match_stats(&query_tokens, &combined_words);
        let mut score = coverage * 100.0;
        if all_exact {
            score += 45.0;
        }

        if !folded_query.is_empty() {
            if is_exact_artist_and_title(&folded_query, &folded_artist, &folded_title)
                || is_exact_artist_and_title(&folded_query, &folded_artist, &canon_title)
                || is_exact_artist_and_title(&folded_query, &folded_artist, &base_song_key)
            {
                score += 20.0;
            } else if folded_title == folded_query
                || canon_title == folded_query
                || base_song_key == folded_query
            {
                score += 10.0;
            } else if contains_phrase_words(&canon_title, &folded_query)
                || contains_phrase_words(&base_song_key, &folded_query)
            {
                score += 7.0;
            } else if folded_artist == folded_query {
                score += 12.0;
            }

            // Secondary gentle bonus when query words match the album name (only when not already
            // fully matched by artist+title, so self-titled singles don't double-count over studio albums).
            if !all_exact && !folded_album.is_empty() {
                let album_words: Vec<&str> = folded_album.split_whitespace().collect();
                let (album_cov, _) = token_match_stats(&query_tokens, &album_words);
                score += album_cov * 8.0;
            }
        }

        // Logarithmic popularity signal calibrated so iconic hits beat obscure namesake tracks
        // when both match the query tokens, while token coverage (100+45) still governs relevance.
        score += 4.5 * (1.0 + effective_rank as f64).ln();

        // Penalize standalone compilation-only winners gently relative to studio releases.
        if is_compilation_album_title(&winner.album.title) {
            score -= 4.0;
        }

        // Variant preference: boost explicitly requested variants, penalize unrequested ones.
        if variant != TrackVariantKind::Studio && variant_allowed {
            score += 15.0;
        } else if !variant_allowed {
            score += match variant {
                TrackVariantKind::Studio => 0.0,
                // Keep a hit remix (e.g. OMI - Cheerleader Felix Jaehn Remix) able to hold #1
                // when it is significantly more popular than (or replaces) the studio version.
                TrackVariantKind::Remix => -2.0,
                TrackVariantKind::Acoustic | TrackVariantKind::Live => -24.0,
                TrackVariantKind::Demo | TrackVariantKind::Instrumental => -28.0,
                TrackVariantKind::Karaoke => -100.0,
            };
        }

        candidates.push(ScoredTrack {
            track: winner,
            variant,
            variant_allowed,
            artist_key,
            base_song_key,
            effective_rank,
            score,
            orig_idx,
        });
    }

    // Record best studio score & rank for each (artist, base_song) pair.
    let mut studio_by_song: HashMap<(String, String), (f64, u64)> = HashMap::new();
    for c in &candidates {
        if c.variant == TrackVariantKind::Studio {
            let key = (c.artist_key.clone(), c.base_song_key.clone());
            studio_by_song
                .entry(key)
                .and_modify(|e| {
                    if c.score > e.0 {
                        *e = (c.score, c.effective_rank);
                    }
                })
                .or_insert((c.score, c.effective_rank));
        }
    }

    // Ensure non-remix secondary variants never sort above an existing Studio row unless queried.
    for c in &mut candidates {
        if c.variant != TrackVariantKind::Studio
            && c.variant != TrackVariantKind::Remix
            && !c.variant_allowed
        {
            if let Some(&(studio_score, _)) =
                studio_by_song.get(&(c.artist_key.clone(), c.base_song_key.clone()))
            {
                if c.score >= studio_score {
                    c.score = studio_score - 1.0;
                }
            }
        }
    }

    // Step 3: sort descending by score with deterministic tie-breaking.
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.effective_rank.cmp(&a.effective_rank))
            .then_with(|| a.orig_idx.cmp(&b.orig_idx))
            .then_with(|| a.track.id.cmp(&b.track.id))
    });

    // Step 4: enforce at most 1 secondary non-studio variant per (artist, base_song) when a Studio
    // track exists and the user did not explicitly query that variant; drop low-rank variants.
    let mut secondary_emitted: HashMap<(String, String), usize> = HashMap::new();
    let mut out = Vec::with_capacity(limit.min(candidates.len()));

    for c in candidates {
        if out.len() >= limit {
            break;
        }
        let song_key = (c.artist_key, c.base_song_key);
        if c.variant != TrackVariantKind::Studio && !c.variant_allowed {
            if let Some(&(_, studio_rank)) = studio_by_song.get(&song_key) {
                let count = secondary_emitted.entry(song_key).or_insert(0);
                if *count >= 1 {
                    continue;
                }
                // Drop very low-rank secondary live/acoustic/demo/instrumental versions when a
                // much stronger Studio version exists.
                if studio_rank > 0 && c.effective_rank.saturating_mul(20) < studio_rank {
                    continue;
                }
                *count += 1;
            }
        }
        out.push(deezer_track_to_catalog(c.track, c.variant));
    }

    out
}

/// Cleans and deduplicates an artist's top tracks (dropping karaoke, preferring studio albums
/// over compilation duplicates, preserving Deezer's popularity ordering).
pub fn dedup_artist_top_tracks(raw: Vec<DeezerSearchTrack>, limit: usize) -> Vec<CatalogTrack> {
    if limit == 0 || raw.is_empty() {
        return Vec::new();
    }

    let mut groups: HashMap<(String, String, TrackVariantKind), Vec<(usize, DeezerSearchTrack)>> =
        HashMap::new();
    for (idx, track) in raw.into_iter().enumerate() {
        // When browsing a tribute/lullaby artist's own page, keep their top tracks.
        let is_tribute_artist = is_karaoke_or_tribute_artist(&track.artist.name);
        let variant = classify_track_variant(
            if is_tribute_artist { "" } else { &track.artist.name },
            &track.title,
            Some(&track.album.title),
            None,
            false,
        );
        if variant == TrackVariantKind::Karaoke && !is_tribute_artist {
            continue;
        }
        let key = (
            canonical_artist_key(&track.artist.name),
            canonical_title_key(&track.title),
            variant,
        );
        groups.entry(key).or_default().push((idx, track));
    }

    let mut winners: Vec<(usize, u64, TrackVariantKind, DeezerSearchTrack)> = groups
        .into_iter()
        .map(|((_, _, variant), group)| {
            let first_idx = group.iter().map(|(i, _)| *i).min().unwrap_or(0);
            let (_, winner, max_rank) = pick_best_track_in_group(group);
            (first_idx, max_rank, variant, winner)
        })
        .collect();

    winners.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.3.id.cmp(&b.3.id))
    });

    winners
        .into_iter()
        .take(limit)
        .map(|(_, _, variant, track)| deezer_track_to_catalog(track, variant))
        .collect()
}

fn album_edition_score(album: &DeezerSearchAlbum) -> f64 {
    let mut s = 0.0;
    let is_comp = is_compilation_album_title(&album.title)
        || matches!(
            album.record_type.as_deref().map(str::to_ascii_lowercase).as_deref(),
            Some("compile") | Some("compilation")
        );
    if !is_comp {
        s += 100.0;
    }
    s += match album
        .record_type
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("album") => 30.0,
        Some("ep") => 20.0,
        Some("single") => 5.0,
        Some("compile") | Some("compilation") => -40.0,
        _ => 0.0,
    };
    let tracks = album.nb_tracks.unwrap_or(0);
    if tracks >= 4 {
        s += 20.0;
    }
    if fold_search_text(&album.title) == canonical_album_key(&album.title) {
        s += 10.0;
    }
    s += (tracks.min(20) as f64) * 0.2;
    if album.cover_big.is_some() || album.cover_medium.is_some() {
        s += 2.0;
    }
    s
}

/// Ranks and deduplicates Deezer album search results.
pub fn rank_and_dedup_deezer_albums(
    query: &str,
    raw: Vec<DeezerSearchAlbum>,
    limit: usize,
) -> Vec<CatalogAlbum> {
    if limit == 0 || raw.is_empty() {
        return Vec::new();
    }

    let intent = QueryVariantIntent::from_query(query);
    let folded_query = fold_search_text(query);
    let query_tokens: Vec<&str> = folded_query.split_whitespace().collect();
    let total_raw = raw.len();

    let mut groups: HashMap<(String, String), Vec<(usize, DeezerSearchAlbum)>> = HashMap::new();
    for (idx, album) in raw.into_iter().enumerate() {
        let allow_karaoke =
            intent.karaoke || intent.cover || query_targets_artist(query, &album.artist.name);
        if !allow_karaoke
            && (is_karaoke_or_tribute_artist(&album.artist.name)
                || is_karaoke_album_title(&fold_search_text(&album.title)))
        {
            continue;
        }
        let key = (
            canonical_artist_key(&album.artist.name),
            canonical_album_key(&album.title),
        );
        groups.entry(key).or_default().push((idx, album));
    }

    struct ScoredAlbum {
        album: DeezerSearchAlbum,
        score: f64,
        first_idx: usize,
    }

    let mut scored: Vec<ScoredAlbum> = Vec::with_capacity(groups.len());
    for (_, mut group) in groups {
        let first_idx = group.iter().map(|(i, _)| *i).min().unwrap_or(0);
        group.sort_by(|a, b| {
            album_edition_score(&b.1)
                .partial_cmp(&album_edition_score(&a.1))
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        let (_, winner) = group.into_iter().next().expect("non-empty group");

        let folded_artist = fold_search_text(&winner.artist.name);
        let folded_title = fold_search_text(&winner.title);
        let canon_title = canonical_album_key(&winner.title);

        let mut combined_words: Vec<&str> = folded_artist.split_whitespace().collect();
        combined_words.extend(folded_title.split_whitespace());

        let (coverage, all_exact) = token_match_stats(&query_tokens, &combined_words);
        let mut score = coverage * 100.0;
        if all_exact {
            score += 40.0;
        }

        if !folded_query.is_empty() {
            if is_exact_artist_and_title(&folded_query, &folded_artist, &folded_title)
                || is_exact_artist_and_title(&folded_query, &folded_artist, &canon_title)
            {
                score += 35.0;
            } else if folded_title == folded_query || canon_title == folded_query {
                score += 25.0;
            } else if folded_artist == folded_query {
                score += 15.0;
            }
        }

        let rtype = winner
            .record_type
            .as_deref()
            .map(str::to_ascii_lowercase);
        let is_comp = is_compilation_album_title(&winner.title)
            || matches!(rtype.as_deref(), Some("compile") | Some("compilation"));

        if is_comp {
            score -= 22.0;
        } else {
            score += match rtype.as_deref() {
                Some("album") => 12.0,
                Some("ep") => 8.0,
                Some("single") => -4.0,
                _ => 0.0,
            };
        }

        if !intent.live && is_live_album_title(&folded_title) {
            score -= 15.0;
        }

        match winner.nb_tracks {
            Some(0) => score -= 8.0,
            Some(n) => score += (n.min(20) as f64).ln_1p() * 1.5,
            None => {}
        }

        // Preserve Deezer's relevance/popularity order as a gentle tie-breaker.
        if total_raw > 0 {
            score += ((total_raw - first_idx) as f64 / total_raw as f64) * 5.0;
        }

        scored.push(ScoredAlbum {
            album: winner,
            score,
            first_idx,
        });
    }

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.first_idx.cmp(&b.first_idx))
            .then_with(|| a.album.id.cmp(&b.album.id))
    });

    scored
        .into_iter()
        .take(limit)
        .map(|s| {
            let a = s.album;
            CatalogAlbum {
                id: format!("deezer:album:{}", a.id),
                title: a.title,
                artist: a.artist.name,
                artist_id: Some(format!("deezer:artist:{}", a.artist.id)),
                release_date: None,
                first_release_date: None,
                cover_url: pick_url(a.cover_big, a.cover_medium),
                mbid: None,
                release_types: map_record_type(a.record_type.as_deref()),
                track_count: a.nb_tracks,
                provider: CatalogProvider::Deezer,
            }
        })
        .collect()
}

/// Deduplicates and sorts an artist's discography albums (Albums & EPs first, then Singles,
/// then Compilations, newest first within each group).
pub fn dedup_and_order_artist_albums(
    artist_id: u64,
    artist_name: &str,
    raw: Vec<DeezerArtistAlbum>,
) -> Vec<CatalogAlbum> {
    let mut best_by_key: HashMap<String, (usize, DeezerArtistAlbum)> = HashMap::new();
    for (idx, album) in raw.into_iter().enumerate() {
        let key = canonical_album_key(&album.title);
        let replace = match best_by_key.get(&key) {
            None => true,
            Some((_, existing)) => artist_album_edition_rank(&album) > artist_album_edition_rank(existing),
        };
        if replace {
            best_by_key.insert(key, (idx, album));
        }
    }

    let mut items: Vec<(u8, String, usize, DeezerArtistAlbum)> = best_by_key
        .into_values()
        .map(|(idx, album)| {
            let bucket = discography_bucket(&album);
            let date = clean_release_date(album.release_date.clone()).unwrap_or_default();
            (bucket, date, idx, album)
        })
        .collect();

    // Bucket ascending (0 = Album/EP, 1 = Single, 2 = Compilation), release_date descending.
    items.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let artist_id_str = format!("deezer:artist:{artist_id}");
    items
        .into_iter()
        .map(|(_, _, _, a)| {
            let release_date = clean_release_date(a.release_date);
            CatalogAlbum {
                id: format!("deezer:album:{}", a.id),
                title: a.title,
                artist: artist_name.to_string(),
                artist_id: Some(artist_id_str.clone()),
                release_date: release_date.clone(),
                first_release_date: release_date,
                cover_url: pick_url(a.cover_big, a.cover_medium),
                mbid: None,
                release_types: map_record_type(a.record_type.as_deref()),
                track_count: None,
                provider: CatalogProvider::Deezer,
            }
        })
        .collect()
}

fn discography_bucket(album: &DeezerArtistAlbum) -> u8 {
    let rtype = album.record_type.as_deref().map(str::to_ascii_lowercase);
    if matches!(rtype.as_deref(), Some("compile") | Some("compilation"))
        || is_compilation_album_title(&album.title)
    {
        return 2;
    }
    match rtype.as_deref() {
        Some("single") => 1,
        _ => 0,
    }
}

fn artist_album_edition_rank(album: &DeezerArtistAlbum) -> i32 {
    let mut score = 0;
    if discography_bucket(album) == 0 {
        score += 20;
    }
    if fold_search_text(&album.title) == canonical_album_key(&album.title) {
        score += 10;
    }
    if album.cover_big.is_some() || album.cover_medium.is_some() {
        score += 2;
    }
    if clean_release_date(album.release_date.clone()).is_some() {
        score += 2;
    }
    score
}

/// Ranks and deduplicates Deezer artist search results.
pub fn rank_and_dedup_deezer_artists(
    query: &str,
    raw: Vec<DeezerSearchArtist>,
    limit: usize,
) -> Vec<CatalogArtist> {
    if limit == 0 || raw.is_empty() {
        return Vec::new();
    }

    let intent = QueryVariantIntent::from_query(query);
    let folded_query = fold_search_text(query);
    let query_tokens: Vec<&str> = folded_query.split_whitespace().collect();
    let query_token_set: HashSet<&str> = query_tokens.iter().copied().collect();

    // Deduplicate by canonical_artist_key, keeping the most popular / complete profile.
    let mut best_by_key: HashMap<String, (usize, DeezerSearchArtist)> = HashMap::new();
    for (idx, artist) in raw.into_iter().enumerate() {
        if !(intent.karaoke || intent.cover || query_targets_artist(query, &artist.name))
            && is_karaoke_or_tribute_artist(&artist.name)
        {
            continue;
        }
        let key = canonical_artist_key(&artist.name);
        if key.is_empty() {
            continue;
        }
        let replace = match best_by_key.get(&key) {
            None => true,
            Some((_, existing)) => {
                let new_tuple = (
                    artist.nb_fan.unwrap_or(0),
                    artist.picture_big.is_some() || artist.picture_medium.is_some(),
                    artist.nb_album.unwrap_or(0),
                );
                let old_tuple = (
                    existing.nb_fan.unwrap_or(0),
                    existing.picture_big.is_some() || existing.picture_medium.is_some(),
                    existing.nb_album.unwrap_or(0),
                );
                new_tuple > old_tuple
            }
        };
        if replace {
            best_by_key.insert(key, (idx, artist));
        }
    }

    struct ScoredArtist {
        artist: DeezerSearchArtist,
        score: f64,
        orig_idx: usize,
    }

    let mut scored: Vec<ScoredArtist> = best_by_key
        .into_values()
        .map(|(orig_idx, artist)| {
            let folded_name = fold_search_text(&artist.name);
            let name_words: Vec<&str> = folded_name.split_whitespace().collect();

            let mut score = 0.0;
            if !folded_query.is_empty() && !folded_name.is_empty() {
                if folded_name == folded_query {
                    score += 160.0;
                } else if folded_name.starts_with(&folded_query) {
                    score += 110.0;
                } else if folded_query.starts_with(&folded_name) {
                    // Handles "Artist Song" queries such as "Ours Samplus Saliverne".
                    score += 100.0;
                }

                let (q_cov, q_all_exact) = token_match_stats(&query_tokens, &name_words);
                score += q_cov * 60.0;
                if q_all_exact {
                    score += 30.0;
                }

                if !name_words.is_empty() {
                    let matched_name = name_words
                        .iter()
                        .filter(|w| query_token_set.contains(*w))
                        .count();
                    let name_cov = matched_name as f64 / name_words.len() as f64;
                    score += name_cov * 50.0;
                }
            }

            score += 2.0 * (1.0 + artist.nb_fan.unwrap_or(0) as f64).ln();

            ScoredArtist {
                artist,
                score,
                orig_idx,
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.artist.nb_fan.cmp(&a.artist.nb_fan))
            .then_with(|| a.orig_idx.cmp(&b.orig_idx))
            .then_with(|| a.artist.id.cmp(&b.artist.id))
    });

    scored
        .into_iter()
        .take(limit)
        .map(|s| {
            let a = s.artist;
            CatalogArtist {
                id: format!("deezer:artist:{}", a.id),
                name: a.name,
                disambiguation: None,
                picture_url: pick_url(a.picture_big, a.picture_medium),
                fan_count: a.nb_fan,
                mbid: None,
                provider: CatalogProvider::Deezer,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deezer::models::{DeezerTrackAlbum, DeezerTrackArtist};

    fn mk_track(
        id: u64,
        artist: &str,
        title: &str,
        album: &str,
        rank: u64,
        duration: u32,
    ) -> DeezerSearchTrack {
        DeezerSearchTrack {
            id,
            title: title.into(),
            title_short: None,
            duration,
            rank,
            artist: DeezerTrackArtist {
                id: id * 10,
                name: artist.into(),
                picture_medium: None,
            },
            album: DeezerTrackAlbum {
                id: id * 100,
                title: album.into(),
                cover_medium: Some(format!("https://cdn.deezer.test/cov/{id}_m.jpg")),
                cover_big: Some(format!("https://cdn.deezer.test/cov/{id}_b.jpg")),
            },
        }
    }

    fn mk_album(
        id: u64,
        artist: &str,
        title: &str,
        record_type: &str,
        nb_tracks: u32,
    ) -> DeezerSearchAlbum {
        DeezerSearchAlbum {
            id,
            title: title.into(),
            cover_medium: Some(format!("https://cdn.deezer.test/alb/{id}_m.jpg")),
            cover_big: Some(format!("https://cdn.deezer.test/alb/{id}_b.jpg")),
            nb_tracks: Some(nb_tracks),
            record_type: Some(record_type.into()),
            artist: DeezerTrackArtist {
                id: id * 10,
                name: artist.into(),
                picture_medium: None,
            },
        }
    }

    fn mk_artist(id: u64, name: &str, nb_fan: u64, nb_album: u32) -> DeezerSearchArtist {
        DeezerSearchArtist {
            id,
            name: name.into(),
            picture_medium: Some(format!("https://cdn.deezer.test/art/{id}_m.jpg")),
            picture_big: Some(format!("https://cdn.deezer.test/art/{id}_b.jpg")),
            nb_fan: Some(nb_fan),
            nb_album: Some(nb_album),
        }
    }

    #[test]
    fn compilation_collapsing_picks_agents_of_fortune_over_essential() {
        let raw = vec![
            mk_track(
                1,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper",
                "The Essential Blue Öyster Cult",
                737_000,
                308,
            ),
            mk_track(
                2,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper",
                "Agents Of Fortune",
                415_000,
                308,
            ),
            mk_track(
                3,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper (Remastered)",
                "Halloween Classics: Hellbent For Halloween",
                320_000,
                308,
            ),
            mk_track(
                4,
                "Blue Öyster Cult",
                "Don't Fear the Reaper",
                "Worlds Best Dad 2006",
                210_000,
                308,
            ),
        ];

        let res = rank_and_dedup_deezer_tracks("Blue Oyster Cult Reaper", raw, 10);
        assert_eq!(res.len(), 1, "all 4 studio copies must collapse into 1");
        assert_eq!(res[0].id, "deezer:track:2");
        assert_eq!(res[0].album_title.as_deref(), Some("Agents Of Fortune"));
        assert_eq!(res[0].duration.as_deref(), Some("05:08"));
        assert_eq!(res[0].duration_secs, Some(308));
        assert_eq!(res[0].variant_label, None);
        assert_eq!(res[0].provider, CatalogProvider::Deezer);
    }

    #[test]
    fn stayin_alive_and_live_well_remain_studio() {
        let stayin_alive = mk_track(1, "Bee Gees", "Stayin' Alive", "Saturday Night Fever", 900_000, 285);
        let live_well = mk_track(2, "Palace", "Live Well", "So Long Forever", 450_000, 224);

        let r1 = rank_and_dedup_deezer_tracks("Bee Gees Stayin Alive", vec![stayin_alive], 10);
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].variant_label, None);

        let r2 = rank_and_dedup_deezer_tracks("Palace Live Well", vec![live_well], 10);
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0].variant_label, None);
    }

    #[test]
    fn karaoke_and_tribute_spam_removed_unless_queried() {
        let raw = vec![
            mk_track(
                1,
                "ProSource Karaoke",
                "(Don't Fear) The Reaper (In the Style of Blue Oyster Cult)",
                "Karaoke Hits",
                800_000,
                305,
            ),
            mk_track(
                2,
                "Acoustified Hits",
                "Don't Fear the Reaper (Acoustic Version) [Blue Öyster Cult Cover]",
                "Acoustic Covers",
                600_000,
                290,
            ),
            mk_track(
                3,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper",
                "Agents Of Fortune",
                415_000,
                308,
            ),
        ];

        let normal = rank_and_dedup_deezer_tracks("Blue Oyster Cult Reaper", raw.clone(), 10);
        assert_eq!(normal.len(), 1);
        assert_eq!(normal[0].artist, "Blue Öyster Cult");

        let explicit = rank_and_dedup_deezer_tracks("Blue Oyster Cult Reaper karaoke", raw, 10);
        assert!(explicit.iter().any(|t| t.artist == "ProSource Karaoke"));
    }

    #[test]
    fn omi_cheerleader_remix_stays_number_one() {
        // Case A: hit remix is vastly more popular than studio original
        let raw = vec![
            mk_track(
                1,
                "OMI",
                "Cheerleader (Felix Jaehn Remix) (Radio Edit)",
                "Cheerleader (Felix Jaehn Remix)",
                950_000,
                180,
            ),
            mk_track(2, "OMI", "Cheerleader", "Me 4 U", 35_000, 195),
        ];
        let res = rank_and_dedup_deezer_tracks("OMI Cheerleader", raw, 10);
        assert_eq!(res[0].id, "deezer:track:1");
        assert_eq!(res[0].variant_label.as_deref(), Some("Remix"));

        // Case B: only remix exists
        let only_remix = vec![mk_track(
            1,
            "OMI",
            "Cheerleader (Felix Jaehn Remix) (Radio Edit)",
            "Cheerleader (Felix Jaehn Remix)",
            950_000,
            180,
        )];
        let res_only = rank_and_dedup_deezer_tracks("OMI Cheerleader", only_remix, 10);
        assert_eq!(res_only.len(), 1);
        assert_eq!(res_only[0].id, "deezer:track:1");
    }

    #[test]
    fn ours_samplus_saliverne_beats_more_popular_spell_on_you() {
        let raw = vec![
            mk_track(1, "Ours Samplus", "Spell on You", "Orphan Loops", 750_000, 210),
            mk_track(2, "Ours Samplus", "Saliverne", "One Day One Beat", 90_000, 195),
        ];
        let res = rank_and_dedup_deezer_tracks("Ours Samplus Saliverne", raw, 10);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].title, "Saliverne");
        assert_eq!(res[0].variant_label, None);
        assert_eq!(res[1].title, "Spell on You");
    }

    #[test]
    fn hermanos_gutierrez_tres_hermanos_beats_unrelated_popular_track() {
        let raw = vec![
            mk_track(
                1,
                "Hermanos Gutiérrez",
                "El Bueno y el Malo",
                "El Bueno y el Malo",
                700_000,
                190,
            ),
            mk_track(
                2,
                "Hermanos Gutiérrez",
                "Tres Hermanos",
                "El Bueno y el Malo",
                120_000,
                215,
            ),
        ];
        let res = rank_and_dedup_deezer_tracks("Hermanos Gutierrez Tres hermanos", raw, 10);
        assert_eq!(res[0].title, "Tres Hermanos");
    }

    #[test]
    fn caps_secondary_variants_strictly_below_studio_and_drops_low_rank() {
        let raw = vec![
            mk_track(
                1,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper",
                "Agents Of Fortune",
                500_000,
                308,
            ),
            mk_track(
                2,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper (Live 1978)",
                "Some Enchanted Evening",
                150_000,
                340,
            ),
            mk_track(
                3,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper (Live at Budokan)",
                "Live in Japan",
                100_000,
                350,
            ),
            mk_track(
                4,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper (Acoustic)",
                "Acoustic Sessions",
                80_000,
                290,
            ),
        ];
        let res = rank_and_dedup_deezer_tracks("Blue Oyster Cult Reaper", raw, 10);
        assert_eq!(res.len(), 2, "studio + at most 1 secondary variant");
        assert_eq!(res[0].id, "deezer:track:1");
        assert_eq!(res[0].variant_label, None);
        assert_eq!(res[1].id, "deezer:track:2");
        assert_eq!(res[1].variant_label.as_deref(), Some("Live"));

        // Low-rank live version (< 5% of studio rank) is dropped completely
        let low_rank = vec![
            mk_track(
                1,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper",
                "Agents Of Fortune",
                500_000,
                308,
            ),
            mk_track(
                2,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper (Live 1978)",
                "Bootleg",
                1_000,
                340,
            ),
        ];
        let res_low = rank_and_dedup_deezer_tracks("Blue Oyster Cult Reaper", low_rank, 10);
        assert_eq!(res_low.len(), 1);
        assert_eq!(res_low[0].id, "deezer:track:1");
    }

    #[test]
    fn explicit_live_query_allows_multiple_live_tracks() {
        let raw = vec![
            mk_track(
                1,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper",
                "Agents Of Fortune",
                500_000,
                308,
            ),
            mk_track(
                2,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper (Live 1978)",
                "Some Enchanted Evening",
                150_000,
                340,
            ),
            mk_track(
                3,
                "Blue Öyster Cult",
                "(Don't Fear) The Reaper (Live at Budokan)",
                "Live in Japan",
                100_000,
                350,
            ),
        ];
        let res = rank_and_dedup_deezer_tracks("Blue Oyster Cult Reaper live", raw, 10);
        assert_eq!(res.len(), 3);
        assert_eq!(res[0].variant_label.as_deref(), Some("Live"));
    }

    #[test]
    fn blue_oyster_cult_beats_deep_purple_in_artist_search() {
        let raw = vec![
            mk_artist(10, "Deep Purple", 3_500_000, 45),
            mk_artist(20, "Blue Öyster Cult", 550_000, 28),
            mk_artist(21, "Blue Oyster Cult", 1_200, 2),
            mk_artist(30, "ProSource Karaoke", 900_000, 500),
        ];
        let res = rank_and_dedup_deezer_artists("Blue Oyster Cult", raw, 10);
        assert_eq!(res.len(), 2, "duplicate BOC collapsed and karaoke removed");
        assert_eq!(res[0].id, "deezer:artist:20");
        assert_eq!(res[0].name, "Blue Öyster Cult");
        assert_eq!(res[1].name, "Deep Purple");
    }

    #[test]
    fn album_ranking_dedups_editions_and_prefers_studio_over_compilation() {
        let raw = vec![
            mk_album(1, "Blue Öyster Cult", "The Essential Blue Öyster Cult", "compile", 32),
            mk_album(2, "Blue Öyster Cult", "Agents Of Fortune (Expanded Edition)", "album", 14),
            mk_album(3, "Blue Öyster Cult", "Agents Of Fortune", "album", 10),
            mk_album(4, "ProSource Karaoke", "Agents Of Fortune Karaoke", "album", 10),
        ];
        let res = rank_and_dedup_deezer_albums("Blue Oyster Cult", raw, 10);
        assert_eq!(res.len(), 2, "expanded edition collapsed and karaoke dropped");
        assert_eq!(res[0].id, "deezer:album:3");
        assert_eq!(res[0].title, "Agents Of Fortune");
        assert_eq!(res[0].release_types, Some(vec!["Album".into()]));
        assert_eq!(res[1].id, "deezer:album:1");
        assert_eq!(
            res[1].release_types,
            Some(vec!["Album".into(), "Compilation".into()])
        );
    }

    #[test]
    fn pick_url_accepts_only_valid_http_and_https_urls() {
        assert_eq!(
            pick_url(Some("  HTTPS://cdn.deezer.com/img.jpg  ".into()), None).as_deref(),
            Some("HTTPS://cdn.deezer.com/img.jpg")
        );
        assert_eq!(
            pick_url(Some("http://cdn.deezer.com/img.jpg".into()), None).as_deref(),
            Some("http://cdn.deezer.com/img.jpg")
        );
        for bad in [
            "javascript:alert(1)",
            "JAVASCRIPT:alert(1)",
            "data:image/png;base64,AAAA",
            "file:///etc/passwd",
            "https://",
            "http://",
            "https://cdn.deezer.com/with space.jpg",
            "https://[invalid-host",
            "ftp://cdn.deezer.com/img.jpg",
            "   ",
        ] {
            assert_eq!(
                pick_url(Some(bad.into()), None),
                None,
                "expected rejection for {bad:?}"
            );
            assert_eq!(
                pick_url(
                    Some(bad.into()),
                    Some("https://cdn.deezer.com/fallback.jpg".into())
                )
                .as_deref(),
                Some("https://cdn.deezer.com/fallback.jpg"),
                "expected fallback when primary is {bad:?}"
            );
        }
    }

    #[test]
    fn legitimate_lullaby_cover_and_long_live_tracks_and_albums_are_never_dropped() {
        let tracks = vec![
            mk_track(1, "The Cure", "Lullaby", "Lullaby", 600_000, 249),
            mk_track(2, "Motörhead", "Under Cover", "Under Cover", 400_000, 210),
            mk_track(3, "Dr. Dre", "Deep Cover", "Deep Cover", 550_000, 260),
            mk_track(4, "Taylor Swift", "Long Live", "Long Live", 700_000, 317),
        ];
        for (query, expected_artist, expected_title) in [
            ("The Cure Lullaby", "The Cure", "Lullaby"),
            ("Motorhead Under Cover", "Motörhead", "Under Cover"),
            ("Dr Dre Deep Cover", "Dr. Dre", "Deep Cover"),
            ("Taylor Swift Long Live", "Taylor Swift", "Long Live"),
        ] {
            let res = rank_and_dedup_deezer_tracks(query, tracks.clone(), 10);
            assert_eq!(res[0].artist, expected_artist, "query={query}");
            assert_eq!(res[0].title, expected_title, "query={query}");
            assert_eq!(res[0].variant_label, None, "query={query}");
        }

        let albums = vec![
            mk_album(1, "The Cure", "Lullaby", "single", 2),
            mk_album(2, "Motörhead", "Under Cover", "album", 11),
            mk_album(3, "Dr. Dre", "Deep Cover", "album", 12),
            mk_album(4, "Taylor Swift", "Long Live", "album", 14),
        ];
        let res = rank_and_dedup_deezer_albums("Under Cover", albums, 10);
        assert_eq!(res.len(), 4);
        assert_eq!(res[0].artist, "Motörhead");
        assert_eq!(res[0].title, "Under Cover");
    }

    #[test]
    fn directly_searching_tribute_artist_returns_tracks_albums_and_artists() {
        let tracks = vec![mk_track(
            1,
            "Vitamin String Quartet",
            "Bohemian Rhapsody",
            "Vitamin String Quartet Performs Queen",
            320_000,
            350,
        )];
        let albums = vec![mk_album(
            10,
            "Vitamin String Quartet",
            "Vitamin String Quartet Performs Queen",
            "album",
            12,
        )];
        let artists = vec![mk_artist(100, "Vitamin String Quartet", 180_000, 90)];

        let res_tracks = rank_and_dedup_deezer_tracks("Vitamin String Quartet", tracks, 10);
        assert_eq!(res_tracks.len(), 1);
        assert_eq!(res_tracks[0].artist, "Vitamin String Quartet");
        assert_eq!(res_tracks[0].variant_label, None);

        let res_albums = rank_and_dedup_deezer_albums("Vitamin String Quartet", albums, 10);
        assert_eq!(res_albums.len(), 1);
        assert_eq!(res_albums[0].artist, "Vitamin String Quartet");

        let res_artists = rank_and_dedup_deezer_artists("Vitamin String Quartet", artists, 10);
        assert_eq!(res_artists.len(), 1);
        assert_eq!(res_artists[0].name, "Vitamin String Quartet");
    }
}
