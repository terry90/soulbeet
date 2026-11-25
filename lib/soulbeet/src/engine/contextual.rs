//! Discovery 2.0 contextual, cultural, and seasonal candidate generation.
//!
//! Plugs into `CandidateGenerator` using only MusicBrainz/ListenBrainz (`MusicKnowledge`) and
//! Deezer's unauthenticated public API (`DeezerClient`). Enforces:
//! - Zero-config `UserCultureProfile` inference from listening history + artist areas + tags
//! - Culturally blended Seasonal Packs (local + international + taste-matched wildcards)
//! - Live `CultureSteering` ratio shifts (`MoreLocal` / `Balanced` / `MoreInternational`)
//! - Strict `exclude_from_core_taste` isolation for seasonal tracks
//! - Moment Mixes (`Drive`, `Morning`, `Focus`, `LateNight`)
//! - Explainable `reason_label` and `culture_bucket` on every `Candidate`

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use shared::recommendation::{
    Candidate, CandidateSet, CultureBucket, CultureSteering, MomentKind, ProfileConfig,
    SeasonalKind, SeasonalPackSpec, SourceErrors, TrackRef, UserCultureProfile, UserMusicProfile,
};

use crate::deezer::{DeezerClient, DeezerTrack};
use crate::knowledge::KnowledgeError;
use crate::traits::{CandidateGenerator, GeneratorOutput, MusicKnowledge, TagOp};

use super::merge_signals;

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

/// Maps an ISO country code to ListenBrainz folksonomy adjective tag and regional Deezer queries.
pub fn country_local_tag(country: &str) -> &'static str {
    match country.trim().to_ascii_uppercase().as_str() {
        "FR" => "french",
        "DE" => "german",
        "JP" => "japanese",
        "ES" => "spanish",
        "IT" => "italian",
        "BR" => "brazilian",
        "KR" => "korean",
        "SE" => "swedish",
        "NO" => "norwegian",
        "GB" => "british",
        "CA" => "canadian",
        "BE" => "belgian",
        "PT" => "portuguese",
        "NL" => "dutch",
        _ => "indie",
    }
}

/// Regional Deezer playlist search queries for a seasonal pack + ISO country.
pub fn seasonal_local_queries(kind: SeasonalKind, country: Option<&str>) -> Vec<&'static str> {
    let code = country.unwrap_or("").to_ascii_uppercase();
    match (kind, code.as_str()) {
        (SeasonalKind::Noel, "FR") => vec!["Chansons de Noël", "Noël Français"],
        (SeasonalKind::NewYearsEve, "FR") => vec!["Soirée Réveillon", "Hits Français Soirée"],
        (SeasonalKind::Summer, "FR") => vec!["Été Français", "Soleil French Pop"],
        (SeasonalKind::Halloween, "FR") => vec!["Halloween France", "Darksynth"],
        (SeasonalKind::Autumn, "FR") => vec!["Automne Chanson Française", "French Indie Chill"],
        (SeasonalKind::Spring, "FR") => vec!["Nouveautés Françaises", "French Indie Pop"],
        (SeasonalKind::Noel, _) => vec!["Christmas Classics", "Holiday Hits"],
        (SeasonalKind::NewYearsEve, _) => vec!["New Years Eve Party", "Dance Anthems"],
        (SeasonalKind::Summer, _) => vec!["Summer Hits", "Indie Summer"],
        (SeasonalKind::Halloween, _) => vec!["Halloween Party", "Spooky Classics"],
        (SeasonalKind::Autumn, _) => vec!["Autumn Acoustic", "Cozy Indie"],
        (SeasonalKind::Spring, _) => vec!["Spring Indie", "Fresh Pop"],
    }
}

/// Global Deezer playlist search query for international classics in a seasonal pack.
pub fn seasonal_international_query(kind: SeasonalKind) -> &'static str {
    match kind {
        SeasonalKind::Noel => "Christmas Hits",
        SeasonalKind::NewYearsEve => "Party Hits",
        SeasonalKind::Summer => "Summer Anthems",
        SeasonalKind::Halloween => "Halloween Classics",
        SeasonalKind::Autumn => "Autumn Chill",
        SeasonalKind::Spring => "Fresh Indie",
    }
}

/// Infers a zero-config `UserCultureProfile` from the user's listening profile + MusicBrainz
/// artist areas (`MusicKnowledge::artist_areas`) + artist tags.
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

    // Also inspect folksonomy genre tags for regional hints when areas are sparse.
    let mut tag_country_hints: Vec<(&str, &str)> = vec![
        ("french", "FR"),
        ("chanson", "FR"),
        ("japanese", "JP"),
        ("j-pop", "JP"),
        ("german", "DE"),
        ("spanish", "ES"),
        ("italian", "IT"),
        ("brazilian", "BR"),
        ("k-pop", "KR"),
        ("korean", "KR"),
        ("swedish", "SE"),
    ];
    tag_country_hints.shrink_to_fit();

    for wt in &profile.genre_distribution {
        let lower = wt.name.to_ascii_lowercase();
        for (needle, code) in &tag_country_hints {
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
        let needle = country_local_tag(country);
        let mut tags: Vec<String> = profile
            .genre_distribution
            .iter()
            .filter(|t| {
                let lower = t.name.to_ascii_lowercase();
                lower.contains(needle)
                    || (country == "FR" && lower.contains("chanson"))
                    || (country == "JP" && (lower.contains("j-pop") || lower.contains("city pop")))
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
        let lang = match country.as_str() {
            "FR" | "BE" | "CH" => vec!["fr".to_string(), "en".to_string()],
            "DE" | "AT" => vec!["de".to_string(), "en".to_string()],
            "ES" | "MX" | "AR" => vec!["es".to_string(), "en".to_string()],
            "IT" => vec!["it".to_string(), "en".to_string()],
            "JP" => vec!["ja".to_string(), "en".to_string()],
            "BR" | "PT" => vec!["pt".to_string(), "en".to_string()],
            "KR" => vec!["ko".to_string(), "en".to_string()],
            _ => vec!["en".to_string()],
        };
        lang
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
    let wildcard = ((target_count as f32) * 0.15).round() as usize;
    let wildcard = wildcard.min(target_count.saturating_sub(1));
    let remaining_after_wildcard = target_count.saturating_sub(wildcard);
    let local = (((target_count as f32) * effective_local).round() as usize)
        .min(remaining_after_wildcard);
    let international = target_count.saturating_sub(local + wildcard);
    (local, international, wildcard)
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

    fn is_excluded(&self, profile: &UserMusicProfile, artist: &str, track: &str, mbid: Option<&str>) -> bool {
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

    async fn generate_seasonal(
        &self,
        profile: &UserMusicProfile,
        pack: &SeasonalPackSpec,
        culture: &UserCultureProfile,
        target_count: usize,
    ) -> GeneratorOutput {
        let mut errors = SourceErrors::default();
        let country = culture.effective_country();
        let local_tag = country.map(country_local_tag).unwrap_or("indie");
        let holiday_tag = pack.kind.primary_tag();
        let (local_quota, intl_quota, wildcard_quota) =
            seasonal_bucket_quotas(target_count, pack.steering, culture.local_culture_ratio);

        let local_label = match (pack.kind, country) {
            (SeasonalKind::Noel, Some("FR")) => "FRENCH NOËL PICK".to_string(),
            (SeasonalKind::Noel, Some(c)) => format!("{c} HOLIDAY PICK"),
            (_, Some(c)) => format!("{c} {} PICK", pack.kind.slug().to_ascii_uppercase()),
            (_, None) => format!("{} LOCAL PICK", pack.kind.slug().to_ascii_uppercase()),
        };
        let intl_label = match pack.kind {
            SeasonalKind::Noel => "INTERNATIONAL NOËL CLASSIC".to_string(),
            _ => format!("INTERNATIONAL {} CLASSIC", pack.kind.slug().to_ascii_uppercase()),
        };

        // 1. Local pool: Deezer regional playlists + ListenBrainz multi-tag (holiday AND local_tag)
        let mut local_pool: Vec<Candidate> = Vec::new();
        let mut seen_keys: HashSet<String> = HashSet::new();

        if let Some(ref pools) = self.pools {
            for query in seasonal_local_queries(pack.kind, country) {
                match pools.search_playlist_tracks(query, 2, 25).await {
                    Ok(tracks) => {
                        for (idx, t) in tracks.into_iter().enumerate() {
                            if self.is_excluded(profile, &t.artist, &t.title, None) {
                                continue;
                            }
                            let key = CandidateSet::key(&t.artist, &t.title);
                            if seen_keys.insert(key) {
                                let score = 1.0 / (1.0 + idx as f64 * 0.08);
                                local_pool.push(Self::make_candidate(
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
            }
        }

        match self
            .knowledge
            .multi_tag_tracks(&[holiday_tag, local_tag], TagOp::And, None, local_quota.max(15))
            .await
        {
            Ok(tracks) => {
                for (idx, tr) in tracks.into_iter().enumerate() {
                    if self.is_excluded(profile, &tr.artist.name, &tr.title, tr.mbid.as_deref()) {
                        continue;
                    }
                    let key = CandidateSet::key(&tr.artist.name, &tr.title);
                    if seen_keys.insert(key) {
                        let score = 0.95 / (1.0 + idx as f64 * 0.08);
                        local_pool.push(Self::make_candidate(
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

        // 2. International classics pool: ListenBrainz popular holiday + Deezer global playlist/chart
        let mut intl_pool: Vec<Candidate> = Vec::new();
        match self
            .knowledge
            .multi_tag_tracks(
                &[holiday_tag],
                TagOp::And,
                Some((65, 100)),
                intl_quota.max(15),
            )
            .await
        {
            Ok(tracks) => {
                for (idx, tr) in tracks.into_iter().enumerate() {
                    if self.is_excluded(profile, &tr.artist.name, &tr.title, tr.mbid.as_deref()) {
                        continue;
                    }
                    let key = CandidateSet::key(&tr.artist.name, &tr.title);
                    if seen_keys.insert(key) {
                        let score = 1.0 / (1.0 + idx as f64 * 0.08);
                        intl_pool.push(Self::make_candidate(
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

        if let Some(ref pools) = self.pools {
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
                            intl_pool.push(Self::make_candidate(
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
        }

        // 3. Taste-matched seasonal wildcards (holiday AND user's top comfort genre)
        let mut wildcard_pool: Vec<Candidate> = Vec::new();
        let top_genre = profile
            .tag_comfort_zone
            .first()
            .or_else(|| profile.genre_distribution.first().map(|t| &t.name))
            .map(String::as_str);

        if let Some(genre) = top_genre {
            let reason = format!(
                "{} × {}",
                pack.kind.slug().to_ascii_uppercase(),
                genre.to_ascii_uppercase()
            );
            match self
                .knowledge
                .multi_tag_tracks(&[holiday_tag, genre], TagOp::And, None, wildcard_quota.max(10))
                .await
            {
                Ok(tracks) => {
                    for (idx, tr) in tracks.into_iter().enumerate() {
                        if self.is_excluded(profile, &tr.artist.name, &tr.title, tr.mbid.as_deref()) {
                            continue;
                        }
                        let key = CandidateSet::key(&tr.artist.name, &tr.title);
                        if seen_keys.insert(key) {
                            let score = 0.92 / (1.0 + idx as f64 * 0.1);
                            wildcard_pool.push(Self::make_candidate(
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
        }

        // Assemble final quota-enforced sets.
        let mut local_set = CandidateSet::new();
        let mut intl_set = CandidateSet::new();
        let mut wildcard_set = CandidateSet::new();

        for c in local_pool.into_iter().take(local_quota) {
            local_set.insert(c);
        }
        for c in intl_pool.into_iter().take(intl_quota) {
            intl_set.insert(c);
        }
        for c in wildcard_pool.into_iter().take(wildcard_quota) {
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
        let mut set = CandidateSet::new();
        let reason = format!("{} PICK", kind.title().to_ascii_uppercase());
        let top_genre = profile.tag_comfort_zone.first().map(String::as_str);

        for mood_tag in kind.mood_tags() {
            let query_tags: Vec<&str> = match top_genre {
                Some(g) if !g.eq_ignore_ascii_case(mood_tag) => vec![mood_tag, g],
                _ => vec![mood_tag],
            };
            match self
                .knowledge
                .multi_tag_tracks(&query_tags, TagOp::Or, None, target_count.max(10))
                .await
            {
                Ok(tracks) => {
                    for (idx, tr) in tracks.into_iter().enumerate() {
                        if self.is_excluded(profile, &tr.artist.name, &tr.title, tr.mbid.as_deref()) {
                            continue;
                        }
                        let score = 1.0 / (1.0 + idx as f64 * 0.08);
                        set.insert(Self::make_candidate(
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
                Err(e) => errors.record(e.service(), e),
            }
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

    fn french_profile() -> UserMusicProfile {
        UserMusicProfile {
            genre_distribution: vec![
                tag("french electro", 0.5),
                tag("chanson", 0.3),
                tag("indie rock", 0.2),
            ],
            tag_comfort_zone: vec!["french electro".into(), "indie rock".into()],
            seed_artists: vec![
                played("Daft Punk", Some("mb-daft"), 80),
                played("Air", Some("mb-air"), 60),
                played("Radiohead", Some("mb-radiohead"), 40),
            ],
            ..Default::default()
        }
    }

    fn build_noel_fixtures() -> (Arc<FakeKnowledge>, Arc<FakePoolSource>) {
        let mut kb = FakeKnowledge::default();
        kb.areas.insert("mb-daft", Some("FR".into()));
        kb.areas.insert("mb-air", Some("FR".into()));
        kb.areas.insert("mb-radiohead", Some("GB".into()));

        // Local LB tracks (christmas AND french)
        let local_lb: Vec<TrackRef> = (1..=12)
            .map(|i| track(&format!("Artiste FR {i}"), None, &format!("Noël FR {i}"), None))
            .collect();
        kb.multi_tag_tracks
            .insert("AND:christmas+french".into(), local_lb);

        // International LB classics (christmas)
        let intl_lb: Vec<TrackRef> = (1..=15)
            .map(|i| track(&format!("Global Artist {i}"), None, &format!("Xmas Classic {i}"), None))
            .collect();
        kb.multi_tag_tracks
            .insert("AND:christmas".into(), intl_lb);

        // Taste-matched wildcards (christmas AND french electro)
        let wildcards: Vec<TrackRef> = (1..=6)
            .map(|i| track(&format!("Electro Xmas {i}"), None, &format!("Synth Snow {i}"), None))
            .collect();
        kb.multi_tag_tracks
            .insert("AND:christmas+french electro".into(), wildcards);

        let mut playlists = HashMap::new();
        playlists.insert(
            "Chansons de Noël".into(),
            (1..=6)
                .map(|i| DeezerTrack {
                    artist: format!("Chanteur {i}"),
                    title: format!("Petit Papa {i}"),
                })
                .collect(),
        );

        (Arc::new(kb), Arc::new(FakePoolSource { playlists }))
    }

    #[tokio::test]
    async fn infers_french_culture_profile_from_artist_areas_and_tags() {
        let (kb, _) = build_noel_fixtures();
        let profile = french_profile();
        let culture = infer_culture_profile(&profile, kb.as_ref(), None, vec![], 0.45).await;

        assert_eq!(culture.effective_country(), Some("FR"));
        assert!(culture.inferred_area_weights[0].0 == "FR");
        assert!(culture.inferred_area_weights[0].1 > 0.65);
        assert!(culture.preferred_languages.contains(&"fr".to_string()));
        assert!(culture
            .inferred_local_tags
            .iter()
            .any(|t| t == "french electro"));
    }

    #[tokio::test]
    async fn noel_pack_enforces_bucket_quotas_and_steering_shifts() {
        let (kb, pools) = build_noel_fixtures();
        let profile = french_profile();
        let culture = UserCultureProfile {
            home_country: Some("FR".into()),
            preferred_languages: vec!["fr".into(), "en".into()],
            local_culture_ratio: 0.45,
            inferred_area_weights: vec![("FR".into(), 0.78), ("GB".into(), 0.22)],
            inferred_local_tags: vec!["french electro".into()],
        };

        let run_with = |steering: CultureSteering| {
            let kb = kb.clone();
            let pools = pools.clone();
            let profile = profile.clone();
            let culture = culture.clone();
            async move {
                let pack = SeasonalPackSpec::for_kind(SeasonalKind::Noel, &culture, steering);
                assert!(pack.exclude_from_core_taste);
                assert_eq!(pack.title, "Noël à la Française");
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
                let wildcard = items
                    .iter()
                    .filter(|c| c.culture_bucket == Some(CultureBucket::TasteWildcard))
                    .count();
                assert!(items.iter().all(|c| c.reason_label.is_some()));
                (local, intl, wildcard)
            }
        };

        // Balanced (0.45 * 20 = 9 local, 0.15 * 20 = 3 wildcard, 8 intl)
        let (bal_local, bal_intl, bal_wild) = run_with(CultureSteering::Balanced).await;
        assert_eq!((bal_local, bal_intl, bal_wild), (9, 8, 3));

        // MoreLocal (0.70 * 20 = 14 local, 3 wildcard, 3 intl)
        let (more_local, more_local_intl, _) = run_with(CultureSteering::MoreLocal).await;
        assert_eq!((more_local, more_local_intl), (14, 3));
        assert!(more_local > bal_local);

        // MoreInternational (0.20 * 20 = 4 local, 3 wildcard, 13 intl)
        let (less_local, more_intl, _) = run_with(CultureSteering::MoreInternational).await;
        assert_eq!((less_local, more_intl), (4, 13));
        assert!(more_intl > bal_intl);
    }

    #[tokio::test]
    async fn rejected_tracks_are_never_recommended_again_and_core_taste_isolates_seasonal() {
        let (kb, pools) = build_noel_fixtures();
        let profile = french_profile();
        let culture = UserCultureProfile {
            home_country: Some("FR".into()),
            ..Default::default()
        };
        let rejected = HashSet::from([CandidateSet::key("Chanteur 1", "Petit Papa 1")]);
        let pack = SeasonalPackSpec::for_kind(
            SeasonalKind::Noel,
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
                .contains_key(&CandidateSet::key("Chanteur 1", "Petit Papa 1"))
        );

        // Anti-pollution core taste isolation check
        let tracks = vec![("Winter Song", true), ("Regular Track", false)];
        let core_only = filter_core_taste_tracks(tracks, |(_, excluded)| *excluded);
        assert_eq!(core_only, vec![("Regular Track", false)]);
    }
}
