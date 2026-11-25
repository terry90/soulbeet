//! Discovery 2.0 contextual, cultural, and seasonal candidate generation.
//!
//! Plugs into `CandidateGenerator` using only MusicBrainz/ListenBrainz (`MusicKnowledge`) and
//! Deezer's unauthenticated public API (`DeezerClient`). Enforces:
//! - Zero-config `UserCultureProfile` inference from listening history + artist areas + global tags
//! - Culturally blended Seasonal Packs (local + international + multi-genre taste wildcards)
//! - Live `CultureSteering` ratio shifts (`MoreLocal` / `Balanced` / `MoreInternational`)
//! - Strict `exclude_from_core_taste` isolation for seasonal tracks
//! - Artist diversity capping so no single artist dominates a contextual drop
//! - Moment Mixes (`Drive`, `Morning`, `Focus`, `LateNight`) spanning the user's top genres
//! - Explainable `reason_label` and `culture_bucket` on every `Candidate`

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use shared::recommendation::{
    discovery_name, Candidate, CandidateSet, CultureBucket, CultureSteering, MomentKind,
    ProfileConfig, SeasonalKind, SeasonalPackSpec, SourceErrors, TrackRef, UserCultureProfile,
    UserMusicProfile,
};

use crate::deezer::{DeezerClient, DeezerTrack};
use crate::knowledge::KnowledgeError;
use crate::traits::{CandidateGenerator, GeneratorOutput, MusicKnowledge, TagOp};

use super::merge_signals;

/// Maximum tracks by any single artist allowed in one seasonal or moment drop.
const MAX_TRACKS_PER_ARTIST: usize = 2;

/// Public regional/seasonal track pool source (backed by `DeezerClient` in production,
/// stubbed in deterministic fixture tests).
#[async_trait]
pub trait ContextualPoolSource: Send + Sync {
    async fn search_playlist_tracks(
        &self,
        query: &str,
        playlist_limit: usize,
        tracks_per_playlist: usize,
    ) -> Result<Vec<DeezerTrack>, KnowledgeError>;

    async fn editorial_chart(
        &self,
        genre_id: u64,
        limit: usize,
    ) -> Result<Vec<DeezerTrack>, KnowledgeError>;
}

/// Live `ContextualPoolSource` backed by `DeezerClient`.
pub struct DeezerPoolSource {
    client: Arc<DeezerClient>,
}

impl DeezerPoolSource {
    pub fn new(client: Arc<DeezerClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ContextualPoolSource for DeezerPoolSource {
    async fn search_playlist_tracks(
        &self,
        query: &str,
        playlist_limit: usize,
        tracks_per_playlist: usize,
    ) -> Result<Vec<DeezerTrack>, KnowledgeError> {
        let playlists = self.client.search_playlists(query, playlist_limit).await?;
        let mut tracks = Vec::new();
        for pl in playlists {
            match self.client.playlist_tracks(pl.id, tracks_per_playlist).await {
                Ok(mut items) => tracks.append(&mut items),
                Err(_) => continue,
            }
        }
        Ok(tracks)
    }

    async fn editorial_chart(
        &self,
        genre_id: u64,
        limit: usize,
    ) -> Result<Vec<DeezerTrack>, KnowledgeError> {
        self.client.editorial_chart(genre_id, limit).await
    }
}

/// What contextual mix `ContextualCandidateGenerator` is building.
#[derive(Debug, Clone)]
pub enum ContextSpec {
    Seasonal {
        pack: SeasonalPackSpec,
        culture: UserCultureProfile,
        target_count: usize,
    },
    Moment {
        kind: MomentKind,
        culture: UserCultureProfile,
        target_count: usize,
    },
}

/// Maps an ISO-3166 alpha-2 country code to its primary ListenBrainz/MusicBrainz
/// regional adjective tag. Returns `None` for unrecognized codes so callers fall back
/// to the user's actual local tags/genres instead of forcing a narrow default.
pub fn country_local_tag(country: &str) -> Option<&'static str> {
    Some(match country.trim().to_ascii_uppercase().as_str() {
        // Europe
        "FR" => "french",
        "BE" => "belgian",
        "CH" => "swiss",
        "DE" => "german",
        "AT" => "austrian",
        "GB" | "UK" => "british",
        "IE" => "irish",
        "ES" => "spanish",
        "PT" => "portuguese",
        "IT" => "italian",
        "NL" => "dutch",
        "SE" => "swedish",
        "NO" => "norwegian",
        "DK" => "danish",
        "FI" => "finnish",
        "IS" => "icelandic",
        "PL" => "polish",
        "CZ" => "czech",
        "HU" => "hungarian",
        "RO" => "romanian",
        "GR" => "greek",
        "TR" => "turkish",
        "UA" => "ukrainian",
        "RU" => "russian",
        "RS" | "HR" | "BA" => "balkan",
        // Americas
        "US" => "american",
        "CA" => "canadian",
        "MX" => "mexican",
        "BR" => "brazilian",
        "AR" => "argentine",
        "CO" => "colombian",
        "CL" => "chilean",
        "PE" => "peruvian",
        "CU" => "cuban",
        "PR" => "puerto rican",
        "JM" => "jamaican",
        // Asia & Oceania
        "JP" => "japanese",
        "KR" => "korean",
        "CN" => "chinese",
        "TW" => "taiwanese",
        "HK" => "cantopop",
        "IN" => "indian",
        "ID" => "indonesian",
        "TH" => "thai",
        "VN" => "vietnamese",
        "PH" => "filipino",
        "AU" => "australian",
        "NZ" => "new zealand",
        // Africa & Middle East
        "NG" => "nigerian",
        "GH" => "ghanaian",
        "ZA" => "south african",
        "SN" => "senegalese",
        "ML" => "malian",
        "ET" => "ethiopian",
        "EG" => "egyptian",
        "MA" => "moroccan",
        "DZ" => "algerian",
        "TN" => "tunisian",
        "LB" => "lebanese",
        "IL" => "israeli",
        "IR" => "persian",
        _ => return None,
    })
}

/// Regional musical genre/scene keywords associated with an ISO country code, used to
/// recognize local tags in a user's library without hardcoding a single culture.
fn country_scene_keywords(country: &str) -> &'static [&'static str] {
    match country.trim().to_ascii_uppercase().as_str() {
        "FR" => &["french", "chanson", "french touch"],
        "JP" => &["japanese", "j-pop", "j-rock", "city pop", "shibuya-kei"],
        "KR" => &["korean", "k-pop", "k-indie", "k-hip-hop"],
        "BR" => &["brazilian", "bossa nova", "mpb", "samba", "funk carioca"],
        "NG" | "GH" => &["afrobeat", "afrobeats", "afropop", "highlife", "nigerian", "ghanaian"],
        "ZA" => &["amapiano", "gqom", "kwaito", "south african"],
        "JM" => &["reggae", "dub", "dancehall", "ska", "jamaican"],
        "ES" => &["spanish", "flamenco", "latin"],
        "MX" | "CO" | "AR" | "CL" | "PE" | "CU" | "PR" => {
            &["latin", "cumbia", "salsa", "reggaeton", "corridos", "tango", "bolero"]
        }
        "PT" => &["portuguese", "fado"],
        "IN" => &["indian", "bollywood", "hindustani", "carnatic", "bhangra"],
        "DE" => &["german", "krautrock", "neue deutsche welle"],
        "SE" | "NO" | "DK" | "FI" | "IS" => &["nordic", "scandinavian", "swedish", "norwegian", "icelandic"],
        "DZ" | "MA" | "TN" | "EG" | "LB" => &["arabic", "rai", "raï", "maghreb", "habibi funk"],
        "CN" | "TW" | "HK" => &["mandopop", "cantopop", "c-pop", "chinese"],
        _ => &[],
    }
}

/// Default preferred language list inferred from an ISO-3166 country code when the user
/// has not explicitly configured languages.
fn country_default_languages(country: &str) -> Vec<String> {
    let primary = match country.trim().to_ascii_uppercase().as_str() {
        "FR" | "BE" | "CH" | "SN" | "ML" => Some("fr"),
        "DE" | "AT" => Some("de"),
        "ES" | "MX" | "AR" | "CO" | "CL" | "PE" | "CU" | "PR" => Some("es"),
        "IT" => Some("it"),
        "PT" | "BR" => Some("pt"),
        "NL" => Some("nl"),
        "SE" => Some("sv"),
        "NO" => Some("no"),
        "DK" => Some("da"),
        "FI" => Some("fi"),
        "IS" => Some("is"),
        "PL" => Some("pl"),
        "TR" => Some("tr"),
        "GR" => Some("el"),
        "UA" => Some("uk"),
        "RU" => Some("ru"),
        "JP" => Some("ja"),
        "KR" => Some("ko"),
        "CN" | "TW" | "HK" => Some("zh"),
        "IN" => Some("hi"),
        "ID" => Some("id"),
        "TH" => Some("th"),
        "VN" => Some("vi"),
        "EG" | "MA" | "DZ" | "TN" | "LB" => Some("ar"),
        "IR" => Some("fa"),
        _ => None,
    };
    match primary {
        Some(lang) => vec![lang.to_string(), "en".to_string()],
        None => vec!["en".to_string()],
    }
}

/// Dynamically builds diverse regional Deezer playlist search queries from the user's
/// country adjective and inferred local tags combined with the seasonal theme.
pub fn seasonal_local_queries(
    kind: SeasonalKind,
    country: Option<&str>,
    local_tags: &[String],
) -> Vec<String> {
    let theme = match kind {
        SeasonalKind::Holiday => "christmas",
        SeasonalKind::NewYearsEve => "party",
        SeasonalKind::Summer => "summer",
        SeasonalKind::Halloween => "halloween",
        SeasonalKind::Autumn => "autumn",
        SeasonalKind::Spring => "spring",
    };

    let mut queries = Vec::new();
    let mut push_unique = |candidate: String| {
        if !candidate.trim().is_empty()
            && !queries.iter().any(|q: &String| q.eq_ignore_ascii_case(&candidate))
        {
            queries.push(candidate);
        }
    };

    if let Some(code) = country.filter(|c| !c.trim().is_empty()) {
        if let Some(adj) = country_local_tag(code) {
            push_unique(format!("{adj} {theme}"));
        }
    }
    for tag in local_tags.iter().take(2) {
        let trimmed = tag.trim();
        if !trimmed.is_empty() {
            push_unique(format!("{trimmed} {theme}"));
        }
    }
    if queries.is_empty() {
        queries.push(seasonal_international_query(kind).to_string());
    }
    queries
}

/// Global Deezer playlist search query for international classics in a seasonal pack.
pub fn seasonal_international_query(kind: SeasonalKind) -> &'static str {
    match kind {
        SeasonalKind::Holiday => "Christmas Hits",
        SeasonalKind::NewYearsEve => "Party Hits",
        SeasonalKind::Summer => "Summer Anthems",
        SeasonalKind::Halloween => "Halloween Classics",
        SeasonalKind::Autumn => "Autumn Chill",
        SeasonalKind::Spring => "Fresh Indie",
    }
}

/// Infers a zero-config `UserCultureProfile` from the user's listening profile + MusicBrainz
/// artist areas (`MusicKnowledge::artist_areas`) + artist folksonomy tags.
pub async fn infer_culture_profile(
    profile: &UserMusicProfile,
    knowledge: &dyn MusicKnowledge,
    explicit_country: Option<String>,
    explicit_languages: Vec<String>,
    local_culture_ratio: f32,
) -> UserCultureProfile {
    let mut artist_weights: HashMap<String, f64> = HashMap::new();
    for pa in &profile.seed_artists {
        if let Some(ref mbid) = pa.artist.mbid {
            *artist_weights.entry(mbid.clone()).or_default() += (pa.plays as f64).max(1.0);
        }
    }
    for fav in &profile.favourite_artists {
        if let Some(ref mbid) = fav.mbid {
            *artist_weights.entry(mbid.clone()).or_default() += 5.0;
        }
    }

    let mbids: Vec<String> = artist_weights.keys().cloned().collect();
    let areas = if mbids.is_empty() {
        HashMap::new()
    } else {
        knowledge.artist_areas(&mbids).await.unwrap_or_default()
    };

    let mut country_scores: HashMap<String, f64> = HashMap::new();
    for (mbid, weight) in &artist_weights {
        if let Some(Some(code)) = areas.get(mbid) {
            *country_scores.entry(code.to_ascii_uppercase()).or_default() += *weight;
        }
    }

    // Broad global folksonomy hints when artist MBID areas are sparse.
    const TAG_COUNTRY_HINTS: &[(&str, &str)] = &[
        ("french", "FR"),
        ("chanson", "FR"),
        ("japanese", "JP"),
        ("j-pop", "JP"),
        ("city pop", "JP"),
        ("korean", "KR"),
        ("k-pop", "KR"),
        ("german", "DE"),
        ("krautrock", "DE"),
        ("spanish", "ES"),
        ("flamenco", "ES"),
        ("italian", "IT"),
        ("brazilian", "BR"),
        ("bossa nova", "BR"),
        ("mpb", "BR"),
        ("portuguese", "PT"),
        ("fado", "PT"),
        ("swedish", "SE"),
        ("norwegian", "NO"),
        ("icelandic", "IS"),
        ("dutch", "NL"),
        ("polish", "PL"),
        ("turkish", "TR"),
        ("greek", "GR"),
        ("afrobeat", "NG"),
        ("afrobeats", "NG"),
        ("amapiano", "ZA"),
        ("reggae", "JM"),
        ("dub", "JM"),
        ("bollywood", "IN"),
        ("mandopop", "TW"),
        ("cantopop", "HK"),
        ("cumbia", "CO"),
        ("tango", "AR"),
        ("mariachi", "MX"),
        ("raï", "DZ"),
        ("balkan", "RS"),
        ("australian", "AU"),
        ("canadian", "CA"),
        ("british", "GB"),
        ("irish", "IE"),
    ];

    for wt in &profile.genre_distribution {
        let lower = wt.name.to_ascii_lowercase();
        for (needle, code) in TAG_COUNTRY_HINTS {
            if lower.contains(needle) {
                *country_scores.entry((*code).to_string()).or_default() += wt.weight * 10.0;
            }
        }
    }

    let total: f64 = country_scores.values().sum();
    let mut inferred_area_weights: Vec<(String, f32)> = if total > 0.0 {
        country_scores
            .into_iter()
            .map(|(code, w)| (code, (w / total) as f32))
            .collect()
    } else {
        Vec::new()
    };
    inferred_area_weights.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    inferred_area_weights.truncate(6);

    let effective_country = explicit_country
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_ascii_uppercase())
        .or_else(|| inferred_area_weights.first().map(|(c, _)| c.clone()));

    let inferred_local_tags = if let Some(ref country) = effective_country {
        let adj = country_local_tag(country);
        let scene = country_scene_keywords(country);
        let mut tags: Vec<String> = profile
            .genre_distribution
            .iter()
            .filter(|t| {
                let lower = t.name.to_ascii_lowercase();
                adj.is_some_and(|needle| lower.contains(needle))
                    || scene.iter().any(|kw| lower.contains(kw))
            })
            .map(|t| t.name.clone())
            .collect();
        tags.truncate(5);
        tags
    } else {
        Vec::new()
    };

    let preferred_languages = if !explicit_languages.is_empty() {
        explicit_languages
    } else if let Some(ref country) = effective_country {
        country_default_languages(country)
    } else {
        vec!["en".to_string()]
    };

    UserCultureProfile {
        home_country: explicit_country
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_ascii_uppercase()),
        preferred_languages,
        local_culture_ratio: local_culture_ratio.clamp(0.0, 1.0),
        inferred_area_weights,
        inferred_local_tags,
    }
}

/// Filters out seasonal/excluded tracks when building a user's core taste profile so holiday
/// listening never pollutes January's Weekly Radar (`exclude_from_core_taste = true`).
pub fn filter_core_taste_tracks<T, F>(items: Vec<T>, is_excluded: F) -> Vec<T>
where
    F: Fn(&T) -> bool,
{
    items.into_iter().filter(|item| !is_excluded(item)).collect()
}

/// Computes exact `(local, international, taste_wildcard)` track quotas for a seasonal pack.
pub fn seasonal_bucket_quotas(
    target_count: usize,
    steering: CultureSteering,
    base_local_ratio: f32,
) -> (usize, usize, usize) {
    if target_count == 0 {
        return (0, 0, 0);
    }
    let effective_local = steering.effective_ratio(base_local_ratio);
    let wildcard = ((target_count as f32) * 0.20).round() as usize;
    let wildcard = wildcard.min(target_count.saturating_sub(1));
    let remaining_after_wildcard = target_count.saturating_sub(wildcard);
    let local = (((target_count as f32) * effective_local).round() as usize)
        .min(remaining_after_wildcard);
    let international = target_count.saturating_sub(local + wildcard);
    (local, international, wildcard)
}

/// Selects up to `quota` candidates from `pool` while enforcing a per-artist cap across
/// `artist_counts` so no single artist monopolizes a drop.
fn take_diverse(
    pool: Vec<Candidate>,
    quota: usize,
    artist_counts: &mut HashMap<String, usize>,
) -> Vec<Candidate> {
    let mut picked = Vec::with_capacity(quota);
    for c in pool {
        if picked.len() >= quota {
            break;
        }
        let artist_key = discovery_name(&c.artist);
        let count = artist_counts.entry(artist_key).or_insert(0);
        if *count >= MAX_TRACKS_PER_ARTIST {
            continue;
        }
        *count += 1;
        picked.push(c);
    }
    picked
}

/// Interleaves multiple candidate lists round-robin so diverse sources/genres appear evenly.
fn interleave<T>(lists: Vec<Vec<T>>) -> Vec<T> {
    let mut iters: Vec<std::vec::IntoIter<T>> = lists.into_iter().map(IntoIterator::into_iter).collect();
    let mut out = Vec::new();
    loop {
        let mut progressed = false;
        for it in &mut iters {
            if let Some(item) = it.next() {
                out.push(item);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    out
}

/// Contextual candidate generator supporting Seasonal Packs (culturally blended) and Moment Mixes.
pub struct ContextualCandidateGenerator {
    spec: ContextSpec,
    knowledge: Arc<dyn MusicKnowledge>,
    pools: Option<Arc<dyn ContextualPoolSource>>,
    /// Normalized `CandidateSet::key(artist, track)` keys from `discovery_rejections`.
    rejected_keys: HashSet<String>,
}

impl ContextualCandidateGenerator {
    pub fn new(
        spec: ContextSpec,
        knowledge: Arc<dyn MusicKnowledge>,
        pools: Option<Arc<dyn ContextualPoolSource>>,
        rejected_keys: HashSet<String>,
    ) -> Self {
        Self {
            spec,
            knowledge,
            pools,
            rejected_keys,
        }
    }

    fn is_excluded(
        &self,
        profile: &UserMusicProfile,
        artist: &str,
        track: &str,
        mbid: Option<&str>,
    ) -> bool {
        let key = CandidateSet::key(artist, track);
        if self.rejected_keys.contains(&key) {
            return true;
        }
        let tr = TrackRef {
            artist: shared::recommendation::ArtistRef {
                name: artist.to_string(),
                mbid: None,
            },
            title: track.to_string(),
            mbid: mbid.map(str::to_string),
        };
        profile.known.knows_track(&tr)
    }

    #[allow(clippy::too_many_arguments)]
    fn make_candidate(
        artist: String,
        track: String,
        mbid: Option<String>,
        artist_mbid: Option<String>,
        score: f64,
        signal: &str,
        source: &str,
        reason_label: String,
        bucket: CultureBucket,
    ) -> Candidate {
        Candidate {
            artist,
            track,
            album: None,
            mbid,
            score,
            signals: vec![signal.to_string()],
            source: source.to_string(),
            artist_listener_count: None,
            primary_genre: None,
            release_year: None,
            artist_mbid,
            reason_label: Some(reason_label),
            culture_bucket: Some(bucket),
        }
    }

    fn user_top_genres(profile: &UserMusicProfile, limit: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for g in profile
            .tag_comfort_zone
            .iter()
            .chain(profile.genre_distribution.iter().map(|t| &t.name))
        {
            if !out.iter().any(|existing| existing.eq_ignore_ascii_case(g)) {
                out.push(g.clone());
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    async fn generate_seasonal(
        &self,
        profile: &UserMusicProfile,
        pack: &SeasonalPackSpec,
        culture: &UserCultureProfile,
        target_count: usize,
    ) -> GeneratorOutput {
        let mut errors = SourceErrors::default();
        let country = culture.effective_country();
        let theme_tags = pack.kind.theme_tags();
        let primary_tag = pack.kind.primary_tag();
        let (local_quota, intl_quota, wildcard_quota) =
            seasonal_bucket_quotas(target_count, pack.steering, culture.local_culture_ratio);

        let slug_upper = pack.kind.slug().to_ascii_uppercase();
        let local_label = match country {
            Some(c) => format!("{c} {slug_upper} PICK"),
            None => format!("{slug_upper} LOCAL PICK"),
        };
        let intl_label = format!("INTERNATIONAL {slug_upper} CLASSIC");

        let mut seen_keys: HashSet<String> = HashSet::new();

        // 1. Local pool: dynamic regional Deezer playlists + ListenBrainz multi-tag across
        //    the user's country adjective & inferred local tags.
        let mut local_streams: Vec<Vec<Candidate>> = Vec::new();

        if let Some(ref pools) = self.pools {
            for query in seasonal_local_queries(pack.kind, country, &culture.inferred_local_tags) {
                let mut stream = Vec::new();
                match pools.search_playlist_tracks(&query, 2, 25).await {
                    Ok(tracks) => {
                        for (idx, t) in tracks.into_iter().enumerate() {
                            if self.is_excluded(profile, &t.artist, &t.title, None) {
                                continue;
                            }
                            let key = CandidateSet::key(&t.artist, &t.title);
                            if seen_keys.insert(key) {
                                let score = 1.0 / (1.0 + idx as f64 * 0.08);
                                stream.push(Self::make_candidate(
                                    t.artist,
                                    t.title,
                                    None,
                                    None,
                                    score,
                                    "seasonal_local_deezer",
                                    "contextual",
                                    local_label.clone(),
                                    CultureBucket::Local,
                                ));
                            }
                        }
                    }
                    Err(e) => errors.record(e.service(), e),
                }
                if !stream.is_empty() {
                    local_streams.push(stream);
                }
            }
        }

        let mut regional_tags: Vec<String> = Vec::new();
        if let Some(adj) = country.and_then(country_local_tag) {
            regional_tags.push(adj.to_string());
        }
        for lt in culture.inferred_local_tags.iter().take(2) {
            if !regional_tags.iter().any(|t| t.eq_ignore_ascii_case(lt)) {
                regional_tags.push(lt.clone());
            }
        }
        for regional in &regional_tags {
            let mut stream = Vec::new();
            match self
                .knowledge
                .multi_tag_tracks(&[primary_tag, regional.as_str()], TagOp::And, None, local_quota.max(15))
                .await
            {
                Ok(tracks) => {
                    for (idx, tr) in tracks.into_iter().enumerate() {
                        if self.is_excluded(profile, &tr.artist.name, &tr.title, tr.mbid.as_deref())
                        {
                            continue;
                        }
                        let key = CandidateSet::key(&tr.artist.name, &tr.title);
                        if seen_keys.insert(key) {
                            let score = 0.95 / (1.0 + idx as f64 * 0.08);
                            stream.push(Self::make_candidate(
                                tr.artist.name,
                                tr.title,
                                tr.mbid,
                                tr.artist.mbid,
                                score,
                                "seasonal_local_lb",
                                "contextual",
                                local_label.clone(),
                                CultureBucket::Local,
                            ));
                        }
                    }
                }
                Err(e) => errors.record(e.service(), e),
            }
            if !stream.is_empty() {
                local_streams.push(stream);
            }
        }
        let local_pool = interleave(local_streams);

        // 2. International classics pool: blend across multiple seasonal theme tags + global playlists
        let mut intl_streams: Vec<Vec<Candidate>> = Vec::new();
        for theme_tag in theme_tags.iter().take(2) {
            let mut stream = Vec::new();
            match self
                .knowledge
                .multi_tag_tracks(&[theme_tag], TagOp::And, Some((60, 100)), intl_quota.max(15))
                .await
            {
                Ok(tracks) => {
                    for (idx, tr) in tracks.into_iter().enumerate() {
                        if self.is_excluded(profile, &tr.artist.name, &tr.title, tr.mbid.as_deref())
                        {
                            continue;
                        }
                        let key = CandidateSet::key(&tr.artist.name, &tr.title);
                        if seen_keys.insert(key) {
                            let score = 1.0 / (1.0 + idx as f64 * 0.08);
                            stream.push(Self::make_candidate(
                                tr.artist.name,
                                tr.title,
                                tr.mbid,
                                tr.artist.mbid,
                                score,
                                "seasonal_intl_lb",
                                "contextual",
                                intl_label.clone(),
                                CultureBucket::International,
                            ));
                        }
                    }
                }
                Err(e) => errors.record(e.service(), e),
            }
            if !stream.is_empty() {
                intl_streams.push(stream);
            }
        }

        if let Some(ref pools) = self.pools {
            let mut stream = Vec::new();
            match pools
                .search_playlist_tracks(seasonal_international_query(pack.kind), 2, 25)
                .await
            {
                Ok(tracks) => {
                    for (idx, t) in tracks.into_iter().enumerate() {
                        if self.is_excluded(profile, &t.artist, &t.title, None) {
                            continue;
                        }
                        let key = CandidateSet::key(&t.artist, &t.title);
                        if seen_keys.insert(key) {
                            let score = 0.9 / (1.0 + idx as f64 * 0.08);
                            stream.push(Self::make_candidate(
                                t.artist,
                                t.title,
                                None,
                                None,
                                score,
                                "seasonal_intl_deezer",
                                "contextual",
                                intl_label.clone(),
                                CultureBucket::International,
                            ));
                        }
                    }
                }
                Err(e) => errors.record(e.service(), e),
            }
            if !stream.is_empty() {
                intl_streams.push(stream);
            }
        }
        let intl_pool = interleave(intl_streams);

        // 3. Taste-matched seasonal wildcards across up to 3 of the user's top genres,
        //    prioritizing genres not already queried in `regional_tags` so wildcards add
        //    genuinely fresh musical facets instead of duplicating the local pool.
        let mut top_genres = Self::user_top_genres(profile, 6);
        top_genres.sort_by_key(|g| {
            usize::from(
                regional_tags
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(g)),
            )
        });
        top_genres.truncate(3);
        let mut wildcard_streams: Vec<Vec<Candidate>> = Vec::new();
        for genre in &top_genres {
            let reason = format!("{slug_upper} × {}", genre.to_ascii_uppercase());
            let mut stream = Vec::new();
            match self
                .knowledge
                .multi_tag_tracks(
                    &[primary_tag, genre.as_str()],
                    TagOp::And,
                    None,
                    wildcard_quota.max(8),
                )
                .await
            {
                Ok(tracks) => {
                    for (idx, tr) in tracks.into_iter().enumerate() {
                        if self.is_excluded(profile, &tr.artist.name, &tr.title, tr.mbid.as_deref())
                        {
                            continue;
                        }
                        let key = CandidateSet::key(&tr.artist.name, &tr.title);
                        if seen_keys.insert(key) {
                            let score = 0.92 / (1.0 + idx as f64 * 0.1);
                            stream.push(Self::make_candidate(
                                tr.artist.name,
                                tr.title,
                                tr.mbid,
                                tr.artist.mbid,
                                score,
                                "seasonal_taste_wildcard",
                                "contextual",
                                reason.clone(),
                                CultureBucket::TasteWildcard,
                            ));
                        }
                    }
                }
                Err(e) => errors.record(e.service(), e),
            }
            if !stream.is_empty() {
                wildcard_streams.push(stream);
            }
        }
        let wildcard_pool = interleave(wildcard_streams);

        // Assemble final quota-enforced sets with per-artist diversity capping.
        let mut artist_counts: HashMap<String, usize> = HashMap::new();
        let mut local_set = CandidateSet::new();
        let mut intl_set = CandidateSet::new();
        let mut wildcard_set = CandidateSet::new();

        for c in take_diverse(local_pool, local_quota, &mut artist_counts) {
            local_set.insert(c);
        }
        for c in take_diverse(intl_pool, intl_quota, &mut artist_counts) {
            intl_set.insert(c);
        }
        for c in take_diverse(wildcard_pool, wildcard_quota, &mut artist_counts) {
            wildcard_set.insert(c);
        }

        let (candidates, signals) = merge_signals(vec![
            ("seasonal_local", local_set),
            ("seasonal_international", intl_set),
            ("seasonal_wildcard", wildcard_set),
        ]);

        GeneratorOutput {
            candidates,
            signals,
            errors,
        }
    }

    async fn generate_moment(
        &self,
        profile: &UserMusicProfile,
        kind: MomentKind,
        target_count: usize,
    ) -> GeneratorOutput {
        let mut errors = SourceErrors::default();
        let reason = format!("{} PICK", kind.title().to_ascii_uppercase());
        let top_genres = Self::user_top_genres(profile, 3);
        let mut streams: Vec<Vec<Candidate>> = Vec::new();
        let mut seen_keys: HashSet<String> = HashSet::new();

        for (i, mood_tag) in kind.mood_tags().iter().enumerate() {
            let paired_genre = top_genres.get(i % top_genres.len().max(1)).map(String::as_str);
            let query_tags: Vec<&str> = match paired_genre {
                Some(g) if !g.eq_ignore_ascii_case(mood_tag) => vec![mood_tag, g],
                _ => vec![mood_tag],
            };
            let mut stream = Vec::new();
            match self
                .knowledge
                .multi_tag_tracks(&query_tags, TagOp::And, None, target_count.max(10))
                .await
            {
                Ok(tracks) => {
                    for (idx, tr) in tracks.into_iter().enumerate() {
                        if self.is_excluded(profile, &tr.artist.name, &tr.title, tr.mbid.as_deref())
                        {
                            continue;
                        }
                        let key = CandidateSet::key(&tr.artist.name, &tr.title);
                        if seen_keys.insert(key) {
                            let score = 1.0 / (1.0 + idx as f64 * 0.08);
                            stream.push(Self::make_candidate(
                                tr.artist.name,
                                tr.title,
                                tr.mbid,
                                tr.artist.mbid,
                                score,
                                "moment_mood_tags",
                                "contextual",
                                reason.clone(),
                                CultureBucket::TasteWildcard,
                            ));
                        }
                    }
                }
                Err(e) => errors.record(e.service(), e),
            }
            if !stream.is_empty() {
                streams.push(stream);
            }
        }

        let mut artist_counts: HashMap<String, usize> = HashMap::new();
        let mut set = CandidateSet::new();
        for c in take_diverse(interleave(streams), target_count.max(1), &mut artist_counts) {
            set.insert(c);
        }

        let (candidates, signals) = merge_signals(vec![("moment_mood", set)]);
        GeneratorOutput {
            candidates,
            signals,
            errors,
        }
    }
}

#[async_trait]
impl CandidateGenerator for ContextualCandidateGenerator {
    fn name(&self) -> &'static str {
        match self.spec {
            ContextSpec::Seasonal { .. } => "contextual_seasonal",
            ContextSpec::Moment { .. } => "contextual_moment",
        }
    }

    fn label(&self) -> &'static str {
        match self.spec {
            ContextSpec::Seasonal { .. } => "Seasonal & Cultural Discovery",
            ContextSpec::Moment { .. } => "Contextual Moment Mix",
        }
    }

    fn weight(&self) -> f64 {
        1.0
    }

    async fn generate(
        &self,
        profile: &UserMusicProfile,
        _config: &ProfileConfig,
    ) -> GeneratorOutput {
        match &self.spec {
            ContextSpec::Seasonal {
                pack,
                culture,
                target_count,
            } => {
                self.generate_seasonal(profile, pack, culture, *target_count)
                    .await
            }
            ContextSpec::Moment {
                kind, target_count, ..
            } => self.generate_moment(profile, *kind, *target_count).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::{balanced, played, tag, track, FakeKnowledge};

    struct FakePoolSource {
        playlists: HashMap<String, Vec<DeezerTrack>>,
    }

    #[async_trait]
    impl ContextualPoolSource for FakePoolSource {
        async fn search_playlist_tracks(
            &self,
            query: &str,
            _playlist_limit: usize,
            _tracks_per_playlist: usize,
        ) -> Result<Vec<DeezerTrack>, KnowledgeError> {
            Ok(self.playlists.get(query).cloned().unwrap_or_default())
        }

        async fn editorial_chart(
            &self,
            _genre_id: u64,
            _limit: usize,
        ) -> Result<Vec<DeezerTrack>, KnowledgeError> {
            Ok(Vec::new())
        }
    }

    fn eclectic_profile() -> UserMusicProfile {
        UserMusicProfile {
            genre_distribution: vec![
                tag("city pop", 0.35),
                tag("bossa nova", 0.25),
                tag("indie rock", 0.20),
                tag("jazz", 0.20),
            ],
            tag_comfort_zone: vec![
                "city pop".into(),
                "bossa nova".into(),
                "indie rock".into(),
                "jazz".into(),
            ],
            seed_artists: vec![
                played("Tatsuro Yamashita", Some("mb-tatsuro"), 80),
                played("Mariya Takeuchi", Some("mb-mariya"), 60),
                played("Radiohead", Some("mb-radiohead"), 40),
            ],
            ..Default::default()
        }
    }

    fn build_holiday_fixtures() -> (Arc<FakeKnowledge>, Arc<FakePoolSource>) {
        let mut kb = FakeKnowledge::default();
        kb.areas.insert("mb-tatsuro", Some("JP".into()));
        kb.areas.insert("mb-mariya", Some("JP".into()));
        kb.areas.insert("mb-radiohead", Some("GB".into()));

        // Local LB tracks (christmas AND japanese)
        let local_lb: Vec<TrackRef> = (1..=14)
            .map(|i| track(&format!("JP Artist {i}"), None, &format!("Winter JP {i}"), None))
            .collect();
        kb.multi_tag_tracks
            .insert("AND:christmas+japanese".into(), local_lb);

        // International LB classics (christmas + holiday)
        let intl_lb: Vec<TrackRef> = (1..=16)
            .map(|i| track(&format!("Global Artist {i}"), None, &format!("Holiday Classic {i}"), None))
            .collect();
        kb.multi_tag_tracks
            .insert("AND:christmas".into(), intl_lb);

        // Taste-matched wildcards across multiple user genres
        kb.multi_tag_tracks.insert(
            "AND:christmas+city pop".into(),
            (1..=4)
                .map(|i| track(&format!("CityPop Winter {i}"), None, &format!("Neon Snow {i}"), None))
                .collect(),
        );
        kb.multi_tag_tracks.insert(
            "AND:christmas+bossa nova".into(),
            (1..=4)
                .map(|i| track(&format!("Bossa Winter {i}"), None, &format!("Rio Snow {i}"), None))
                .collect(),
        );
        kb.multi_tag_tracks.insert(
            "AND:christmas+indie rock".into(),
            (1..=4)
                .map(|i| track(&format!("Indie Winter {i}"), None, &format!("Fuzzy Snow {i}"), None))
                .collect(),
        );
        kb.multi_tag_tracks.insert(
            "AND:christmas+jazz".into(),
            (1..=4)
                .map(|i| track(&format!("Jazz Winter {i}"), None, &format!("Velvet Snow {i}"), None))
                .collect(),
        );

        let mut playlists = HashMap::new();
        playlists.insert(
            "japanese christmas".into(),
            (1..=6)
                .map(|i| DeezerTrack {
                    artist: format!("Tokyo Singer {i}"),
                    title: format!("Snow Lights {i}"),
                })
                .collect(),
        );

        (Arc::new(kb), Arc::new(FakePoolSource { playlists }))
    }

    #[tokio::test]
    async fn infers_global_culture_profile_from_artist_areas_and_tags() {
        let (kb, _) = build_holiday_fixtures();
        let profile = eclectic_profile();
        let culture = infer_culture_profile(&profile, kb.as_ref(), None, vec![], 0.45).await;

        assert_eq!(culture.effective_country(), Some("JP"));
        assert_eq!(culture.inferred_area_weights[0].0, "JP");
        assert!(culture.preferred_languages.contains(&"ja".to_string()));
        assert!(culture
            .inferred_local_tags
            .iter()
            .any(|t| t == "city pop"));

        // Global coverage across continents (Africa, Latin America, Europe, Asia)
        assert_eq!(country_local_tag("NG"), Some("nigerian"));
        assert_eq!(country_local_tag("BR"), Some("brazilian"));
        assert_eq!(country_local_tag("FR"), Some("french"));
        assert_eq!(country_local_tag("IS"), Some("icelandic"));
        assert_eq!(country_local_tag("ZZ"), None);
    }

    #[tokio::test]
    async fn holiday_pack_enforces_quotas_steering_and_multi_genre_wildcard_diversity() {
        let (kb, pools) = build_holiday_fixtures();
        let profile = eclectic_profile();
        let culture = UserCultureProfile {
            home_country: Some("JP".into()),
            preferred_languages: vec!["ja".into(), "en".into()],
            local_culture_ratio: 0.45,
            inferred_area_weights: vec![("JP".into(), 0.78), ("GB".into(), 0.22)],
            inferred_local_tags: vec!["city pop".into()],
        };

        let run_with = |steering: CultureSteering| {
            let kb = kb.clone();
            let pools = pools.clone();
            let profile = profile.clone();
            let culture = culture.clone();
            async move {
                let pack = SeasonalPackSpec::for_kind(SeasonalKind::Holiday, &culture, steering);
                assert!(pack.exclude_from_core_taste);
                assert_eq!(pack.title, "Holiday Classics & Gems");
                let gen = ContextualCandidateGenerator::new(
                    ContextSpec::Seasonal {
                        pack,
                        culture,
                        target_count: 20,
                    },
                    kb,
                    Some(pools),
                    HashSet::new(),
                );
                let out = gen.generate(&profile, &balanced()).await;
                let items = out.candidates.into_vec();
                let local = items
                    .iter()
                    .filter(|c| c.culture_bucket == Some(CultureBucket::Local))
                    .count();
                let intl = items
                    .iter()
                    .filter(|c| c.culture_bucket == Some(CultureBucket::International))
                    .count();
                let wildcard_reasons: HashSet<String> = items
                    .iter()
                    .filter(|c| c.culture_bucket == Some(CultureBucket::TasteWildcard))
                    .filter_map(|c| c.reason_label.clone())
                    .collect();
                let wildcard = items
                    .iter()
                    .filter(|c| c.culture_bucket == Some(CultureBucket::TasteWildcard))
                    .count();
                assert!(items.iter().all(|c| c.reason_label.is_some()));
                (local, intl, wildcard, wildcard_reasons)
            }
        };

        // Balanced (0.45 * 20 = 9 local, 0.20 * 20 = 4 wildcard, 7 intl)
        let (bal_local, bal_intl, bal_wild, wild_reasons) =
            run_with(CultureSteering::Balanced).await;
        assert_eq!((bal_local, bal_intl, bal_wild), (9, 7, 4));
        // Wildcards span all 3 of the user's top genres (city pop, bossa nova, indie rock)
        assert_eq!(
            wild_reasons.len(),
            3,
            "expected wildcards across 3 genres, got {wild_reasons:?}"
        );

        // MoreLocal (0.70 * 20 = 14 local, 4 wildcard, 2 intl)
        let (more_local, more_local_intl, _, _) = run_with(CultureSteering::MoreLocal).await;
        assert_eq!((more_local, more_local_intl), (14, 2));
        assert!(more_local > bal_local);

        // MoreInternational (0.20 * 20 = 4 local, 4 wildcard, 12 intl)
        let (less_local, more_intl, _, _) = run_with(CultureSteering::MoreInternational).await;
        assert_eq!((less_local, more_intl), (4, 12));
        assert!(more_intl > bal_intl);
    }

    #[tokio::test]
    async fn rejected_tracks_and_artist_cap_are_enforced_and_core_taste_isolates_seasonal() {
        let (kb, pools) = build_holiday_fixtures();
        let profile = eclectic_profile();
        let culture = UserCultureProfile {
            home_country: Some("JP".into()),
            ..Default::default()
        };
        let rejected = HashSet::from([CandidateSet::key("Tokyo Singer 1", "Snow Lights 1")]);
        let pack = SeasonalPackSpec::for_kind(
            SeasonalKind::Holiday,
            &culture,
            CultureSteering::Balanced,
        );
        let gen = ContextualCandidateGenerator::new(
            ContextSpec::Seasonal {
                pack,
                culture,
                target_count: 10,
            },
            kb,
            Some(pools),
            rejected,
        );
        let out = gen.generate(&profile, &balanced()).await;
        assert!(
            !out.candidates
                .candidates
                .contains_key(&CandidateSet::key("Tokyo Singer 1", "Snow Lights 1"))
        );

        // Per-artist cap check
        let mut counts = HashMap::new();
        let same_artist: Vec<Candidate> = (1..=5)
            .map(|i| {
                ContextualCandidateGenerator::make_candidate(
                    "Same Artist".into(),
                    format!("Track {i}"),
                    None,
                    None,
                    1.0,
                    "test",
                    "contextual",
                    "TEST".into(),
                    CultureBucket::Local,
                )
            })
            .collect();
        let capped = take_diverse(same_artist, 5, &mut counts);
        assert_eq!(capped.len(), MAX_TRACKS_PER_ARTIST);

        // Anti-pollution core taste isolation check
        let tracks = vec![("Winter Song", true), ("Regular Track", false)];
        let core_only = filter_core_taste_tracks(tracks, |(_, excluded)| *excluded);
        assert_eq!(core_only, vec![("Regular Track", false)]);
    }
}
