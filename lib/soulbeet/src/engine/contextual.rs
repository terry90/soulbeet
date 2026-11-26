//! Discovery 2.0 contextual, cultural, and seasonal candidate generation.
//!
//! Powered 100% by open MetaBrainz data (**ListenBrainz + MusicBrainz** — zero commercial
//! streaming APIs). Uses a **Cross-Playlist Consensus** algorithm for high-quality seasonal
//! playlists without artist-level tag leakage:
//! 1. Searches public ListenBrainz community playlists (`GET /1/playlist/search`) with strict
//!    whole-word title matching (rejecting PostgreSQL trigram noise like `"Nose"`/`"nope"`).
//! 2. Fetches MBID-rich JSPF tracks (`GET /1/playlist/{mbid}`), deduplicates per distinct creator,
//!    filters out `[no artist]` / noise entries, and ranks tracks by
//!    `distinct_creators × ln(1 + recording_user_count)` via `POST /1/popularity/recording`.
//! 3. Classifies verified seasonal tracks into:
//!    - **Regional / Local bucket** via MusicBrainz `artist_areas` + native-language community playlists
//!    - **Taste-Matched Wildcards bucket** via `artist_tags` matched against the user's own genres
//!    - **International Classics bucket** via highest cross-playlist consensus score
//! 4. Enforces `CultureSteering` quotas, `MAX_TRACKS_PER_ARTIST = 2`, `discovery_rejections`,
//!    `exclude_from_core_taste`, and emits structured client-localized `CandidateReason` values.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use shared::recommendation::{
    discovery_name, ArtistRef, Candidate, CandidateReason, CandidateSet, ContextualMixDefinition,
    ContextualMixKind, CultureBucket, CultureSteering, ProfileConfig, SourceErrors, TrackRef,
    UserCultureProfile, UserMusicProfile,
};

use crate::knowledge::KnowledgeError;
use crate::listenbrainz::client::ListenBrainzClient;
use crate::traits::{CandidateGenerator, GeneratorOutput, MusicKnowledge, TagOp};

use super::merge_signals;

/// Maximum tracks by any single artist allowed in one contextual drop.
pub const MAX_TRACKS_PER_ARTIST: usize = 2;

/// Minimum verified consensus tracks before falling back to `multi_tag_tracks` for sparse custom mixes.
const SPARSE_CONSENSUS_FLOOR: usize = 5;

/// Reference to a public community playlist returned by ListenBrainz playlist search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunityPlaylistRef {
    pub mbid: String,
    pub title: String,
    pub creator: String,
}

/// Community playlist + recording-popularity pool source for seasonal consensus discovery.
/// Backed by `ListenBrainzPlaylistPool` in production and stubbed in deterministic tests.
#[async_trait]
pub trait SeasonalPlaylistPool: Send + Sync {
    async fn search_public_playlists(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<CommunityPlaylistRef>, KnowledgeError>;

    async fn playlist_tracks(&self, playlist_mbid: &str) -> Result<Vec<TrackRef>, KnowledgeError>;

    async fn recording_popularity(
        &self,
        recording_mbids: &[String],
    ) -> Result<HashMap<String, u64>, KnowledgeError>;
}

/// Live pure-ListenBrainz `SeasonalPlaylistPool` (no API key or token required).
pub struct ListenBrainzPlaylistPool {
    client: Arc<ListenBrainzClient>,
}

impl ListenBrainzPlaylistPool {
    pub fn new(client: Arc<ListenBrainzClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl SeasonalPlaylistPool for ListenBrainzPlaylistPool {
    async fn search_public_playlists(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<CommunityPlaylistRef>, KnowledgeError> {
        let hits = self
            .client
            .search_playlists(query, limit)
            .await
            .map_err(KnowledgeError::Remote)?;
        Ok(hits
            .into_iter()
            .map(|h| CommunityPlaylistRef {
                mbid: h.mbid,
                title: h.title,
                creator: h.creator,
            })
            .collect())
    }

    async fn playlist_tracks(&self, playlist_mbid: &str) -> Result<Vec<TrackRef>, KnowledgeError> {
        let tracks = self
            .client
            .playlist(playlist_mbid)
            .await
            .map_err(KnowledgeError::Remote)?;
        Ok(tracks
            .into_iter()
            .map(|t| TrackRef {
                artist: ArtistRef {
                    name: t.creator,
                    mbid: t.artist_mbid,
                },
                title: t.title,
                mbid: t.recording_mbid,
            })
            .collect())
    }

    async fn recording_popularity(
        &self,
        recording_mbids: &[String],
    ) -> Result<HashMap<String, u64>, KnowledgeError> {
        let entries = self
            .client
            .recording_popularity(recording_mbids)
            .await
            .map_err(KnowledgeError::Remote)?;
        Ok(entries
            .into_iter()
            .map(|e| (e.recording_mbid, e.total_user_count.unwrap_or(0)))
            .collect())
    }
}

/// What contextual mix `ContextualCandidateGenerator` is building — open to any built-in
/// or user/admin-created `ContextualMixDefinition`.
#[derive(Debug, Clone)]
pub struct ContextSpec {
    pub definition: ContextualMixDefinition,
    pub culture: UserCultureProfile,
    pub steering: CultureSteering,
    pub target_count: usize,
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF // Hiragana & Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xAC00..=0xD7AF // Hangul Syllables
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
    )
}

fn has_cjk(s: &str) -> bool {
    s.chars().any(is_cjk)
}

/// Tokenizes a playlist title or query by splitting on whitespace and punctuation separators
/// (hyphens, slashes, colons, etc.) while stripping combining marks and keeping apostrophes
/// joined (e.g. `"Christmas-Hits"` -> `["christmas", "hits"]`, `"Noël/Hiver"` -> `["noel", "hiver"]`).
fn tokenize_title(s: &str) -> Vec<String> {
    let folded = shared::recommendation::without_marks(s).to_lowercase();
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for ch in folded.chars() {
        if ch.is_alphanumeric() {
            cur.push(ch);
        } else if matches!(ch, '\'' | '\u{2018}' | '\u{2019}') {
            // Keep contractions/possessives joined (e.g. "Year's" -> "years").
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Returns true when `title_word` matches `query_word` either identically or as a Germanic/Nordic
/// compound word (e.g. `"weihnachtslieder"` for `"weihnachten"`, `"kerstliedjes"` for `"kerst"`,
/// `"julmusik"`/`"julesange"` for `"jul"` without matching `"july"`/`"julia"`).
fn title_word_matches(title_word: &str, query_word: &str) -> bool {
    if title_word == query_word {
        return true;
    }
    match query_word {
        "weihnachten" | "weihnacht" => title_word.starts_with("weihnacht"),
        "kerst" => title_word.starts_with("kerst"),
        "sommer" => title_word.starts_with("sommer"),
        "herbst" => title_word.starts_with("herbst"),
        "jul" => {
            let base = title_word.strip_suffix('s').unwrap_or(title_word);
            title_word.starts_with("jul")
                && !matches!(
                    base,
                    "july" | "julia" | "julian" | "juliet" | "julius" | "jule" | "julie"
                )
        }
        _ => false,
    }
}

/// Strict title match for ListenBrainz trigram playlist search results.
/// Rejects trigram false positives (e.g. `"Nose"`, `"nope"`, `"Nowa"` for `"noel"`)
/// by requiring every query token (>= 3 chars, or >= 2 chars for CJK) to appear as a whole word
/// (splitting on hyphens/slashes, diacritic-insensitive, with Germanic/Nordic compound-word
/// prefix matching and CJK substring matching).
pub fn playlist_title_matches(title: &str, query: &str) -> bool {
    let folded_title = shared::recommendation::without_marks(title).to_lowercase();
    let title_words = tokenize_title(title);
    if title_words.is_empty() {
        return false;
    }
    let query_words: Vec<String> = tokenize_title(query)
        .into_iter()
        .filter(|w| {
            let len = w.chars().count();
            if has_cjk(w) {
                len >= 2
            } else {
                len >= 3
            }
        })
        .collect();
    if query_words.is_empty() {
        return false;
    }
    query_words.iter().all(|qw| {
        if has_cjk(qw) {
            folded_title.contains(qw.as_str())
        } else {
            title_words.iter().any(|tw| title_word_matches(tw, qw))
        }
    })
}

/// Whole-word / exact genre matching so short genre names like `"ska"` match `"ska"` or
/// `"ska punk"` without false-matching substrings like `"alaska"` or `"nebraska"`.
fn tag_matches_genre(tag: &str, genre: &str) -> bool {
    let t = tokenize_title(tag);
    let g = tokenize_title(genre);
    if t.is_empty() || g.is_empty() || g.len() > t.len() {
        return false;
    }
    t.windows(g.len()).any(|w| w == g.as_slice())
}

/// Filters out placeholder artists and sound-effect noise tracks from community playlists.
pub fn is_junk_track(artist: &str, title: &str) -> bool {
    let a = artist.trim().to_ascii_lowercase();
    let t = title.trim().to_ascii_lowercase();
    if a.is_empty() || t.is_empty() {
        return true;
    }
    matches!(
        a.as_str(),
        "[no artist]" | "[unknown]" | "various artists" | "sound effects"
    ) || t.contains("sound effect")
        || t == "halloween sounds"
}

/// Maps an ISO-3166 alpha-2 country code to its primary ListenBrainz/MusicBrainz
/// regional adjective tag. Returns `None` for unrecognized codes.
pub fn country_local_tag(country: &str) -> Option<&'static str> {
    Some(match country.trim().to_ascii_uppercase().as_str() {
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
        "SG" => "singaporean",
        "MY" => "malaysian",
        "AU" => "australian",
        "NZ" => "new zealand",
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

fn country_scene_keywords(country: &str) -> &'static [&'static str] {
    match country.trim().to_ascii_uppercase().as_str() {
        "FR" => &["french", "chanson", "french touch"],
        "JP" => &["japanese", "j-pop", "j-rock", "city pop", "shibuya-kei"],
        "KR" => &["korean", "k-pop", "k-indie", "k-hip-hop"],
        "BR" => &["brazilian", "bossa nova", "mpb", "samba", "funk carioca"],
        "NG" | "GH" => &[
            "afrobeat",
            "afrobeats",
            "afropop",
            "highlife",
            "nigerian",
            "ghanaian",
        ],
        "ZA" => &["amapiano", "gqom", "kwaito", "south african"],
        "JM" => &["reggae", "dub", "dancehall", "ska", "jamaican"],
        "ES" => &["spanish", "flamenco", "latin"],
        "MX" | "CO" | "AR" | "CL" | "PE" | "CU" | "PR" => &[
            "latin",
            "cumbia",
            "salsa",
            "reggaeton",
            "corridos",
            "tango",
            "bolero",
        ],
        "PT" => &["portuguese", "fado"],
        "IN" => &["indian", "bollywood", "hindustani", "carnatic", "bhangra"],
        "DE" => &["german", "krautrock", "neue deutsche welle"],
        "SE" | "NO" | "DK" | "FI" | "IS" => &[
            "nordic",
            "scandinavian",
            "swedish",
            "norwegian",
            "icelandic",
        ],
        "DZ" | "MA" | "TN" | "EG" | "LB" => &["arabic", "rai", "raï", "maghreb", "habibi funk"],
        "CN" | "TW" | "HK" => &["mandopop", "cantopop", "c-pop", "chinese"],
        _ => &[],
    }
}

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

/// Maps `(mix_slug, language_code)` to authentic native-language community playlist search words
/// used on ListenBrainz `GET /1/playlist/search` to discover regional seasonal playlists.
fn localized_playlist_terms(slug: &str, languages: &[String]) -> Vec<&'static str> {
    let mut terms = Vec::new();
    for lang in languages {
        let hit = match (slug, lang.to_ascii_lowercase().as_str()) {
            ("winter_holiday", "fr") => Some("noel"),
            ("winter_holiday", "de") => Some("weihnachten"),
            ("winter_holiday", "es") => Some("navidad"),
            ("winter_holiday", "it") => Some("natale"),
            ("winter_holiday", "pt") => Some("natal"),
            ("winter_holiday", "sv" | "no" | "da") => Some("jul"),
            ("winter_holiday", "nl") => Some("kerst"),
            ("winter_holiday", "pl") => Some("swieta"),
            ("carnival", "pt" | "es" | "fr") => Some("carnaval"),
            ("summer", "fr") => Some("ete"),
            ("summer", "es") => Some("verano"),
            ("summer", "pt") => Some("verao"),
            ("summer", "de") => Some("sommer"),
            ("autumn", "fr") => Some("automne"),
            ("autumn", "de") => Some("herbst"),
            ("autumn", "es") => Some("otono"),
            _ => None,
        };
        if let Some(term) = hit {
            if !terms.contains(&term) {
                terms.push(term);
            }
        }
    }
    terms
}

/// Builds the list of `(query, is_regional_query)` pairs to search ListenBrainz community
/// playlists for a given mix definition and culture profile.
pub fn consensus_search_queries(
    def: &ContextualMixDefinition,
    culture: &UserCultureProfile,
) -> Vec<(String, bool)> {
    fn push(out: &mut Vec<(String, bool)>, q: String, regional: bool) {
        let trimmed = q.trim();
        let min_len = if has_cjk(trimmed) { 2 } else { 3 };
        if trimmed.chars().count() < min_len {
            return;
        }
        if !out
            .iter()
            .any(|(existing, _)| existing.eq_ignore_ascii_case(trimmed))
        {
            out.push((trimmed.to_string(), regional));
        }
    }

    let mut queries = Vec::new();
    // 1. Primary theme tag / definition keywords (global community consensus)
    if let Some(tag) = def.primary_tag() {
        push(&mut queries, tag.to_string(), false);
    }
    for q in def.international_queries.iter().take(2) {
        push(&mut queries, q.clone(), false);
    }
    // 2. Native-language / regional community playlist terms
    let effective_languages: Vec<String> = if !culture.preferred_languages.is_empty() {
        culture.preferred_languages.clone()
    } else if let Some(country) = culture.effective_country() {
        country_default_languages(country)
    } else {
        Vec::new()
    };
    for term in localized_playlist_terms(&def.slug, &effective_languages) {
        push(&mut queries, term.to_string(), true);
    }
    if let (Some(country), Some(theme)) = (culture.effective_country(), def.primary_tag()) {
        if let Some(adj) = country_local_tag(country) {
            push(&mut queries, format!("{adj} {theme}"), true);
        }
    }
    queries
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
/// listening never pollutes core Weekly Radar recommendations (`exclude_from_core_taste = true`).
pub fn filter_core_taste_tracks<T, F>(items: Vec<T>, is_excluded: F) -> Vec<T>
where
    F: Fn(&T) -> bool,
{
    items.into_iter().filter(|item| !is_excluded(item)).collect()
}

/// Computes exact `(local, international, taste_wildcard)` track quotas for a culturally blended pack.
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
/// `artist_counts` so no single artist monopolizes a drop. Returns `(picked, remaining)` so
/// callers can backfill unmet quotas from overflow pools without dropping diversity rules.
fn take_diverse_with_remainder(
    pool: Vec<Candidate>,
    quota: usize,
    artist_counts: &mut HashMap<String, usize>,
) -> (Vec<Candidate>, Vec<Candidate>) {
    let mut picked = Vec::with_capacity(quota);
    let mut remaining = Vec::new();
    for c in pool {
        if picked.len() >= quota {
            remaining.push(c);
            continue;
        }
        let artist_key = discovery_name(&c.artist);
        let count = artist_counts.entry(artist_key).or_insert(0);
        if *count >= MAX_TRACKS_PER_ARTIST {
            continue;
        }
        *count += 1;
        picked.push(c);
    }
    (picked, remaining)
}

/// Selects up to `quota` candidates from `pool` while enforcing a per-artist cap across
/// `artist_counts` so no single artist monopolizes a drop.
fn take_diverse(
    pool: Vec<Candidate>,
    quota: usize,
    artist_counts: &mut HashMap<String, usize>,
) -> Vec<Candidate> {
    take_diverse_with_remainder(pool, quota, artist_counts).0
}

fn interleave<T>(lists: Vec<Vec<T>>) -> Vec<T> {
    let mut iters: Vec<std::vec::IntoIter<T>> =
        lists.into_iter().map(IntoIterator::into_iter).collect();
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

/// Aggregated ListenBrainz cross-playlist consensus record for one normalized track.
struct ConsensusEntry {
    track: TrackRef,
    creators: HashSet<String>,
    from_regional_query: bool,
    lb_users: u64,
    score: f64,
}

/// Contextual candidate generator supporting any open `ContextualMixDefinition`,
/// built 100% on ListenBrainz + MusicBrainz.
pub struct ContextualCandidateGenerator {
    spec: ContextSpec,
    knowledge: Arc<dyn MusicKnowledge>,
    pool: Option<Arc<dyn SeasonalPlaylistPool>>,
    rejected_keys: HashSet<String>,
}

impl ContextualCandidateGenerator {
    pub fn new(
        spec: ContextSpec,
        knowledge: Arc<dyn MusicKnowledge>,
        pool: Option<Arc<dyn SeasonalPlaylistPool>>,
        rejected_keys: HashSet<String>,
    ) -> Self {
        Self {
            spec,
            knowledge,
            pool,
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
        if is_junk_track(artist, track) {
            return true;
        }
        let key = CandidateSet::key(artist, track);
        if self.rejected_keys.contains(&key) {
            return true;
        }
        let tr = TrackRef {
            artist: ArtistRef {
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
        reason: CandidateReason,
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
            reason: Some(reason),
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

    /// Collects and scores verified seasonal tracks from ListenBrainz community playlists
    /// using strict title matching, distinct-creator consensus, and recording-level popularity.
    async fn collect_consensus(
        &self,
        profile: &UserMusicProfile,
        errors: &mut SourceErrors,
    ) -> Vec<ConsensusEntry> {
        let Some(ref pool) = self.pool else {
            return Vec::new();
        };

        let mut fetched_playlists: HashSet<String> = HashSet::new();
        let mut entries: HashMap<String, ConsensusEntry> = HashMap::new();

        for (query, is_regional) in consensus_search_queries(&self.spec.definition, &self.spec.culture)
        {
            let hits = match pool.search_public_playlists(&query, 12).await {
                Ok(h) => h,
                Err(e) => {
                    errors.record(e.service(), e);
                    continue;
                }
            };

            let mut used_for_query = 0usize;
            for hit in hits {
                if used_for_query >= 6 {
                    break;
                }
                if !playlist_title_matches(&hit.title, &query) {
                    continue;
                }
                if !fetched_playlists.insert(hit.mbid.clone()) {
                    continue;
                }
                used_for_query += 1;

                let tracks = match pool.playlist_tracks(&hit.mbid).await {
                    Ok(t) => t,
                    Err(e) => {
                        errors.record(e.service(), e);
                        continue;
                    }
                };

                let creator_key = if hit.creator.trim().is_empty() {
                    hit.mbid.clone()
                } else {
                    hit.creator.trim().to_ascii_lowercase()
                };

                let mut seen_in_playlist: HashSet<String> = HashSet::new();
                for tr in tracks {
                    if self.is_excluded(profile, &tr.artist.name, &tr.title, tr.mbid.as_deref()) {
                        continue;
                    }
                    let key = CandidateSet::key(&tr.artist.name, &tr.title);
                    if !seen_in_playlist.insert(key.clone()) {
                        continue;
                    }
                    entries
                        .entry(key)
                        .and_modify(|e| {
                            e.creators.insert(creator_key.clone());
                            e.from_regional_query |= is_regional;
                            if e.track.mbid.is_none() && tr.mbid.is_some() {
                                e.track.mbid = tr.mbid.clone();
                            }
                            if e.track.artist.mbid.is_none() && tr.artist.mbid.is_some() {
                                e.track.artist.mbid = tr.artist.mbid.clone();
                            }
                        })
                        .or_insert_with(|| {
                            let mut creators = HashSet::new();
                            creators.insert(creator_key.clone());
                            ConsensusEntry {
                                track: tr,
                                creators,
                                from_regional_query: is_regional,
                                lb_users: 0,
                                score: 0.0,
                            }
                        });
                }
            }
        }

        // Batch-fetch ListenBrainz recording-level popularity (`POST /1/popularity/recording`).
        let rec_mbids: Vec<String> = entries
            .values()
            .filter_map(|e| e.track.mbid.clone())
            .collect();
        let popularity = if rec_mbids.is_empty() {
            HashMap::new()
        } else {
            match pool.recording_popularity(&rec_mbids).await {
                Ok(p) => p,
                Err(e) => {
                    errors.record(e.service(), e);
                    HashMap::new()
                }
            }
        };

        let mut ranked: Vec<ConsensusEntry> = entries
            .into_values()
            .map(|mut e| {
                e.lb_users = e
                    .track
                    .mbid
                    .as_deref()
                    .and_then(|m| popularity.get(m).copied())
                    .unwrap_or(0);
                let creator_count = e.creators.len() as f64;
                // Consensus across >= 2 independent creators dominates single-playlist picks,
                // weighted smoothly by log recording listener count.
                let multi_bonus = if e.creators.len() >= 2 { 2.0 } else { 1.0 };
                e.score = multi_bonus * creator_count * (1.0 + (e.lb_users as f64).ln_1p());
                e
            })
            .collect();

        ranked.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.lb_users.cmp(&a.lb_users))
        });
        ranked
    }

    async fn generate_seasonal(&self, profile: &UserMusicProfile) -> GeneratorOutput {
        let def = &self.spec.definition;
        let culture = &self.spec.culture;
        let target_count = self.spec.target_count;
        let mut errors = SourceErrors::default();

        let country = culture.effective_country();
        let base_local = def.effective_base_local_ratio(culture).unwrap_or(0.0);
        let (mut local_quota, mut intl_quota, wildcard_quota) =
            seasonal_bucket_quotas(target_count, self.spec.steering, base_local);
        if country.is_none() {
            intl_quota += local_quota;
            local_quota = 0;
        }

        let local_reason = CandidateReason::RegionalContextual {
            mix_slug: def.slug.clone(),
            country_code: country.map(str::to_string),
        };
        let intl_reason = CandidateReason::InternationalContextual {
            mix_slug: def.slug.clone(),
        };

        // 1. Build the verified ListenBrainz Cross-Playlist Consensus pool and cap before
        //    artist metadata enrichment so large seasonal queries stay bounded.
        let mut consensus = self.collect_consensus(profile, &mut errors).await;
        let consensus_cap = target_count.saturating_mul(6).max(30);
        consensus.truncate(consensus_cap);

        // 2. Batch-enrich artist areas (for leak-free Regional classification) & artist tags
        //    (for personalized Taste-Matched Wildcards from verified seasonal tracks).
        let artist_mbids: Vec<String> = {
            let mut unique: HashSet<String> = HashSet::new();
            consensus
                .iter()
                .filter_map(|e| e.track.artist.mbid.clone())
                .filter(|m| unique.insert(m.clone()))
                .collect()
        };
        let areas = if artist_mbids.is_empty() {
            HashMap::new()
        } else {
            match self.knowledge.artist_areas(&artist_mbids).await {
                Ok(a) => a,
                Err(e) => {
                    errors.record(e.service(), e);
                    HashMap::new()
                }
            }
        };
        let artist_tags = if artist_mbids.is_empty() {
            HashMap::new()
        } else {
            match self.knowledge.artist_tags(&artist_mbids).await {
                Ok(t) => t,
                Err(e) => {
                    errors.record(e.service(), e);
                    HashMap::new()
                }
            }
        };

        let user_genres = Self::user_top_genres(profile, 6);

        let mut local_pool: Vec<Candidate> = Vec::new();
        let mut wildcard_streams: HashMap<String, Vec<Candidate>> = HashMap::new();
        let mut intl_pool: Vec<Candidate> = Vec::new();
        let mut used_keys: HashSet<String> = HashSet::new();

        // Normalize consensus scores into (0.0, 1.0]
        let max_score = consensus
            .first()
            .map(|e| e.score)
            .unwrap_or(1.0)
            .max(1.0);

        for entry in &consensus {
            let key = CandidateSet::key(&entry.track.artist.name, &entry.track.title);
            let norm_score = (entry.score / max_score).clamp(0.05, 1.0);
            let area_code = entry
                .track
                .artist
                .mbid
                .as_deref()
                .and_then(|m| areas.get(m))
                .and_then(|a| a.as_deref());
            let entry_tags = entry
                .track
                .artist
                .mbid
                .as_deref()
                .and_then(|m| artist_tags.get(m));

            let matches_user_country = country.is_some_and(|c| {
                if let Some(a) = area_code {
                    a.eq_ignore_ascii_case(c)
                } else if entry.from_regional_query {
                    // Never blindly classify unknown-area tracks as Local; require artist tags
                    // to corroborate the regional scene.
                    let adj = country_local_tag(c);
                    let scene = country_scene_keywords(c);
                    entry_tags.is_some_and(|tags| {
                        tags.iter().any(|wt| {
                            adj.is_some_and(|needle| tag_matches_genre(&wt.name, needle))
                                || scene.iter().any(|kw| tag_matches_genre(&wt.name, kw))
                        })
                    })
                } else {
                    false
                }
            });

            if matches_user_country && used_keys.insert(key.clone()) {
                local_pool.push(Self::make_candidate(
                    entry.track.artist.name.clone(),
                    entry.track.title.clone(),
                    entry.track.mbid.clone(),
                    entry.track.artist.mbid.clone(),
                    norm_score,
                    "seasonal_lb_consensus_local",
                    "contextual",
                    local_reason.clone(),
                    CultureBucket::Local,
                ));
                continue;
            }

            // Check if this verified seasonal track matches one of the user's own taste genres
            // using whole-word / exact matching (no substring false positives like "ska" in "alaska").
            let matched_genre = entry_tags.and_then(|tags| {
                user_genres
                    .iter()
                    .find(|genre| tags.iter().any(|wt| tag_matches_genre(&wt.name, genre)))
                    .cloned()
            });

            if let Some(genre) = matched_genre {
                if used_keys.insert(key.clone()) {
                    wildcard_streams
                        .entry(genre.clone())
                        .or_default()
                        .push(Self::make_candidate(
                            entry.track.artist.name.clone(),
                            entry.track.title.clone(),
                            entry.track.mbid.clone(),
                            entry.track.artist.mbid.clone(),
                            norm_score,
                            "seasonal_lb_consensus_wildcard",
                            "contextual",
                            CandidateReason::TasteWildcard {
                                mix_slug: def.slug.clone(),
                                genre,
                            },
                            CultureBucket::TasteWildcard,
                        ));
                    continue;
                }
            }

            if used_keys.insert(key) {
                intl_pool.push(Self::make_candidate(
                    entry.track.artist.name.clone(),
                    entry.track.title.clone(),
                    entry.track.mbid.clone(),
                    entry.track.artist.mbid.clone(),
                    norm_score,
                    "seasonal_lb_consensus_intl",
                    "contextual",
                    intl_reason.clone(),
                    CultureBucket::International,
                ));
            }
        }

        // Deterministic wildcard interleaving in `user_genres` priority order.
        let ordered_wildcard_streams: Vec<Vec<Candidate>> = user_genres
            .iter()
            .filter_map(|g| wildcard_streams.remove(g))
            .collect();
        let mut wildcard_pool = interleave(ordered_wildcard_streams);

        // 3. Sparse custom mix fallback: ONLY when community consensus has fewer than
        //    SPARSE_CONSENSUS_FLOOR verified tracks (e.g. a niche user-created custom mix),
        //    supplement via ListenBrainz multi-tag radio — still 100% pure ListenBrainz.
        if consensus.len() < SPARSE_CONSENSUS_FLOOR {
            if let Some(primary_tag) = def.primary_tag() {
                match self
                    .knowledge
                    .multi_tag_tracks(&[primary_tag], TagOp::And, Some((60, 100)), target_count)
                    .await
                {
                    Ok(tracks) => {
                        for (idx, tr) in tracks.into_iter().enumerate() {
                            if self.is_excluded(
                                profile,
                                &tr.artist.name,
                                &tr.title,
                                tr.mbid.as_deref(),
                            ) {
                                continue;
                            }
                            let key = CandidateSet::key(&tr.artist.name, &tr.title);
                            if used_keys.insert(key) {
                                intl_pool.push(Self::make_candidate(
                                    tr.artist.name,
                                    tr.title,
                                    tr.mbid,
                                    tr.artist.mbid,
                                    0.7 / (1.0 + idx as f64 * 0.08),
                                    "seasonal_lb_tag_fallback",
                                    "contextual",
                                    intl_reason.clone(),
                                    CultureBucket::International,
                                ));
                            }
                        }
                    }
                    Err(e) => errors.record(e.service(), e),
                }
            }
            if wildcard_pool.len() < wildcard_quota {
                if let (Some(primary_tag), Some(genre)) = (def.primary_tag(), user_genres.first()) {
                    match self
                        .knowledge
                        .multi_tag_tracks(
                            &[primary_tag, genre.as_str()],
                            TagOp::And,
                            None,
                            wildcard_quota,
                        )
                        .await
                    {
                        Ok(tracks) => {
                            for (idx, tr) in tracks.into_iter().enumerate() {
                                if self.is_excluded(
                                    profile,
                                    &tr.artist.name,
                                    &tr.title,
                                    tr.mbid.as_deref(),
                                ) {
                                    continue;
                                }
                                let key = CandidateSet::key(&tr.artist.name, &tr.title);
                                if used_keys.insert(key) {
                                    wildcard_pool.push(Self::make_candidate(
                                        tr.artist.name,
                                        tr.title,
                                        tr.mbid,
                                        tr.artist.mbid,
                                        0.65 / (1.0 + idx as f64 * 0.1),
                                        "seasonal_lb_wildcard_fallback",
                                        "contextual",
                                        CandidateReason::TasteWildcard {
                                            mix_slug: def.slug.clone(),
                                            genre: genre.clone(),
                                        },
                                        CultureBucket::TasteWildcard,
                                    ));
                                }
                            }
                        }
                        Err(e) => errors.record(e.service(), e),
                    }
                }
            }
        }

        // 4. Assemble final quota-enforced sets with per-artist diversity capping, backfilling
        //    any unmet bucket quotas from the remaining overflow pools.
        let mut artist_counts: HashMap<String, usize> = HashMap::new();
        let mut local_set = CandidateSet::new();
        let mut intl_set = CandidateSet::new();
        let mut wildcard_set = CandidateSet::new();

        let (local_picked, local_leftovers) =
            take_diverse_with_remainder(local_pool, local_quota, &mut artist_counts);
        for c in local_picked {
            local_set.insert(c);
        }
        let (wildcard_picked, wildcard_leftovers) =
            take_diverse_with_remainder(wildcard_pool, wildcard_quota, &mut artist_counts);
        for c in wildcard_picked {
            wildcard_set.insert(c);
        }
        let (intl_picked, intl_leftovers) =
            take_diverse_with_remainder(intl_pool, intl_quota, &mut artist_counts);
        for c in intl_picked {
            intl_set.insert(c);
        }

        let filled = local_set.len() + wildcard_set.len() + intl_set.len();
        let shortfall = target_count.saturating_sub(filled);
        if shortfall > 0 {
            let overflow: Vec<Candidate> = intl_leftovers
                .into_iter()
                .chain(wildcard_leftovers)
                .chain(local_leftovers)
                .collect();
            for c in take_diverse(overflow, shortfall, &mut artist_counts) {
                match c.culture_bucket {
                    Some(CultureBucket::Local) => local_set.insert(c),
                    Some(CultureBucket::TasteWildcard) => wildcard_set.insert(c),
                    _ => intl_set.insert(c),
                }
            }
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

    async fn generate_moment(&self, profile: &UserMusicProfile) -> GeneratorOutput {
        let def = &self.spec.definition;
        let target_count = self.spec.target_count;
        let mut errors = SourceErrors::default();
        let top_genres = Self::user_top_genres(profile, 3);
        let mut streams: Vec<Vec<Candidate>> = Vec::new();
        let mut seen_keys: HashSet<String> = HashSet::new();

        for (i, mood_tag) in def.theme_tags.iter().enumerate() {
            let paired_genre = top_genres
                .get(i % top_genres.len().max(1))
                .map(String::as_str);
            let query_tags: Vec<&str> = match paired_genre {
                Some(g) if !g.eq_ignore_ascii_case(mood_tag) => vec![mood_tag.as_str(), g],
                _ => vec![mood_tag.as_str()],
            };
            let reason = CandidateReason::MomentMatch {
                mix_slug: def.slug.clone(),
                mood_tag: Some(mood_tag.clone()),
                genre: paired_genre.map(str::to_string),
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
        match self.spec.definition.kind {
            ContextualMixKind::Seasonal => "contextual_seasonal",
            ContextualMixKind::Moment => "contextual_moment",
            ContextualMixKind::Rolling => "contextual_rolling",
        }
    }

    fn label(&self) -> &'static str {
        match self.spec.definition.kind {
            ContextualMixKind::Seasonal => "Seasonal & Cultural Discovery",
            ContextualMixKind::Moment => "Contextual Moment Mix",
            ContextualMixKind::Rolling => "Rolling Discovery",
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
        match self.spec.definition.kind {
            ContextualMixKind::Seasonal => self.generate_seasonal(profile).await,
            ContextualMixKind::Moment | ContextualMixKind::Rolling => {
                self.generate_moment(profile).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::{balanced, played, tag, track, FakeKnowledge};
    use shared::recommendation::builtin_contextual_mixes;

    struct FakeConsensusPool {
        searches: HashMap<String, Vec<CommunityPlaylistRef>>,
        playlists: HashMap<String, Vec<TrackRef>>,
        popularity: HashMap<String, u64>,
    }

    #[async_trait]
    impl SeasonalPlaylistPool for FakeConsensusPool {
        async fn search_public_playlists(
            &self,
            query: &str,
            limit: usize,
        ) -> Result<Vec<CommunityPlaylistRef>, KnowledgeError> {
            Ok(self
                .searches
                .get(query)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(limit)
                .collect())
        }

        async fn playlist_tracks(
            &self,
            playlist_mbid: &str,
        ) -> Result<Vec<TrackRef>, KnowledgeError> {
            Ok(self
                .playlists
                .get(playlist_mbid)
                .cloned()
                .unwrap_or_default())
        }

        async fn recording_popularity(
            &self,
            recording_mbids: &[String],
        ) -> Result<HashMap<String, u64>, KnowledgeError> {
            Ok(recording_mbids
                .iter()
                .filter_map(|m| self.popularity.get(m).map(|v| (m.clone(), *v)))
                .collect())
        }
    }

    fn eclectic_profile() -> UserMusicProfile {
        UserMusicProfile {
            genre_distribution: vec![
                tag("jazz", 0.40),
                tag("indie rock", 0.35),
                tag("city pop", 0.25),
            ],
            tag_comfort_zone: vec!["jazz".into(), "indie rock".into(), "city pop".into()],
            seed_artists: vec![
                played("Tatsuro Yamashita", Some("mb-tatsuro"), 80),
                played("Mariya Takeuchi", Some("mb-mariya"), 60),
                played("Radiohead", Some("mb-radiohead"), 40),
            ],
            ..Default::default()
        }
    }

    fn winter_holiday_def() -> ContextualMixDefinition {
        builtin_contextual_mixes()
            .into_iter()
            .find(|m| m.slug == "winter_holiday")
            .unwrap()
    }

    fn build_consensus_fixtures() -> (Arc<FakeKnowledge>, Arc<FakeConsensusPool>) {
        let mut kb = FakeKnowledge::default();
        kb.areas.insert("mb-tatsuro", Some("JP".into()));
        kb.areas.insert("mb-mariya", Some("JP".into()));
        kb.areas.insert("mb-radiohead", Some("GB".into()));

        let mut searches = HashMap::new();
        let mut playlists = HashMap::new();
        let mut popularity = HashMap::new();

        // Include a trigram false positive ("Nose" / "Nope") that strict title matching MUST reject!
        searches.insert(
            "christmas".to_string(),
            vec![
                CommunityPlaylistRef {
                    mbid: "pl-1".into(),
                    title: "Christmas Classics".into(),
                    creator: "alice".into(),
                },
                CommunityPlaylistRef {
                    mbid: "pl-2".into(),
                    title: "Best Christmas Songs".into(),
                    creator: "bob".into(),
                },
                CommunityPlaylistRef {
                    mbid: "pl-junk".into(),
                    title: "Random Trigram False Positive".into(),
                    creator: "eve".into(),
                },
            ],
        );

        let mut pl1_tracks = Vec::new();
        let mut pl2_tracks = Vec::new();

        // 15 JP regional Christmas tracks (artist area = JP)
        for i in 1..=15 {
            let ambid: &'static str = Box::leak(format!("mb-jp-{i}").into_boxed_str());
            kb.areas.insert(ambid, Some("JP".into()));
            let rmbid = format!("rec-jp-{i}");
            popularity.insert(rmbid.clone(), 5_000 + i as u64 * 100);
            let tr = track(&format!("JP Artist {i}"), Some(ambid), &format!("JP Snow {i}"), Some(&rmbid));
            pl1_tracks.push(tr.clone());
            if i <= 10 {
                pl2_tracks.push(tr);
            }
        }

        // 6 Taste-matched seasonal wildcards (artist tagged jazz / indie rock)
        for i in 1..=3 {
            let ambid: &'static str = Box::leak(format!("mb-jazz-{i}").into_boxed_str());
            kb.areas.insert(ambid, Some("US".into()));
            kb.tags.insert(ambid, vec![tag("jazz", 1.0)]);
            let rmbid = format!("rec-jazz-{i}");
            popularity.insert(rmbid.clone(), 12_000);
            let tr = track(&format!("Jazz Trio {i}"), Some(ambid), &format!("Winter Swing {i}"), Some(&rmbid));
            pl1_tracks.push(tr.clone());
            pl2_tracks.push(tr);
        }
        for i in 1..=3 {
            let ambid: &'static str = Box::leak(format!("mb-indie-{i}").into_boxed_str());
            kb.areas.insert(ambid, Some("GB".into()));
            kb.tags.insert(ambid, vec![tag("indie rock", 1.0)]);
            let rmbid = format!("rec-indie-{i}");
            popularity.insert(rmbid.clone(), 9_000);
            let tr = track(&format!("Indie Band {i}"), Some(ambid), &format!("December Rain {i}"), Some(&rmbid));
            pl1_tracks.push(tr.clone());
            pl2_tracks.push(tr);
        }

        // 15 International classics + 1 junk "[no artist]" noise track that MUST be filtered out
        for i in 1..=15 {
            let ambid: &'static str = Box::leak(format!("mb-intl-{i}").into_boxed_str());
            kb.areas.insert(ambid, Some("US".into()));
            let rmbid = format!("rec-intl-{i}");
            popularity.insert(rmbid.clone(), 30_000 + i as u64 * 1000);
            let tr = track(&format!("Classic Singer {i}"), Some(ambid), &format!("Holiday Hit {i}"), Some(&rmbid));
            pl1_tracks.push(tr.clone());
            pl2_tracks.push(tr);
        }
        pl1_tracks.push(track("[no artist]", None, "Halloween Sounds", Some("rec-junk")));

        // Trigram false positive playlist containing Hotel California (MUST NEVER enter!)
        playlists.insert("pl-1".into(), pl1_tracks);
        playlists.insert("pl-2".into(), pl2_tracks);
        playlists.insert(
            "pl-junk".into(),
            vec![track("Eagles", None, "Hotel California", Some("rec-hotel"))],
        );

        (
            Arc::new(kb),
            Arc::new(FakeConsensusPool {
                searches,
                playlists,
                popularity,
            }),
        )
    }

    #[test]
    fn strict_playlist_title_filter_rejects_trigram_noise() {
        assert!(playlist_title_matches("Christmas Classics", "christmas"));
        assert!(playlist_title_matches("Christmas-Hits/2026", "christmas"));
        assert!(playlist_title_matches("Chansons de Noël", "noel"));
        assert!(playlist_title_matches("Noël/Hiver", "noel"));
        assert!(playlist_title_matches("Spooky Halloween Party", "halloween"));
        assert!(playlist_title_matches("Weihnachtslieder 2026", "weihnachten"));
        assert!(playlist_title_matches("Kerstliedjes", "kerst"));
        assert!(playlist_title_matches("Julmusik & Jullåtar", "jul"));
        assert!(playlist_title_matches("Julesange", "jul"));
        assert!(!playlist_title_matches("July Summer Vibes", "jul"));
        assert!(!playlist_title_matches("Julia's Mix", "jul"));
        assert!(playlist_title_matches("圣诞歌曲精选", "圣诞"));
        assert!(!playlist_title_matches("Nose", "noel"));
        assert!(!playlist_title_matches("nope", "noel"));
        assert!(!playlist_title_matches("Nowa", "noel"));
        assert!(is_junk_track("[no artist]", "Halloween Sounds"));
        assert!(!is_junk_track("Michael Jackson", "Thriller"));
        assert!(tag_matches_genre("ska punk", "ska"));
        assert!(tag_matches_genre("uk indie rock", "indie rock"));
        assert!(!tag_matches_genre("alaska", "ska"));
        assert!(!tag_matches_genre("nebraska", "ska"));
    }

    #[test]
    fn consensus_search_queries_fall_back_to_country_default_languages() {
        let def = winter_holiday_def();
        let culture = UserCultureProfile {
            home_country: Some("FR".into()),
            preferred_languages: vec![],
            ..Default::default()
        };
        let queries = consensus_search_queries(&def, &culture);
        assert!(
            queries.iter().any(|(q, regional)| q == "noel" && *regional),
            "expected 'noel' from FR default languages, got {queries:?}"
        );
    }

    #[tokio::test]
    async fn missing_country_shifts_local_budget_and_shortfalls_backfill_up_to_target() {
        let (kb, pool) = build_consensus_fixtures();
        let profile = eclectic_profile();
        // No country set -> local_quota = 0, shifted to intl_quota; full target_count = 15 met.
        let gen = ContextualCandidateGenerator::new(
            ContextSpec {
                definition: winter_holiday_def(),
                culture: UserCultureProfile::default(),
                steering: CultureSteering::Balanced,
                target_count: 15,
            },
            kb,
            Some(pool),
            HashSet::new(),
        );
        let out = gen.generate(&profile, &balanced()).await;
        let items = out.candidates.into_vec();
        assert_eq!(items.len(), 15);
        assert!(
            !items.iter().any(|c| c.culture_bucket == Some(CultureBucket::Local)),
            "no Local candidates when country is None"
        );
    }

    #[tokio::test]
    async fn listenbrainz_consensus_enforces_quotas_steering_and_zero_tag_leakage() {
        let (kb, pool) = build_consensus_fixtures();
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
            let pool = pool.clone();
            let profile = profile.clone();
            let culture = culture.clone();
            async move {
                let gen = ContextualCandidateGenerator::new(
                    ContextSpec {
                        definition: winter_holiday_def(),
                        culture,
                        steering,
                        target_count: 20,
                    },
                    kb,
                    Some(pool),
                    HashSet::new(),
                );
                let out = gen.generate(&profile, &balanced()).await;
                let items = out.candidates.into_vec();
                // Neither trigram-noise Hotel California nor [no artist] junk can ever enter!
                assert!(!items.iter().any(|c| c.track == "Hotel California"));
                assert!(!items.iter().any(|c| c.artist == "[no artist]"));
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
                let wildcard_genres: HashSet<String> = items
                    .iter()
                    .filter_map(|c| match &c.reason {
                        Some(CandidateReason::TasteWildcard { genre, .. }) => Some(genre.clone()),
                        _ => None,
                    })
                    .collect();
                assert!(items.iter().all(|c| c.reason.is_some()));
                (local, intl, wildcard, wildcard_genres)
            }
        };

        // Balanced (0.45 * 20 = 9 local, 0.20 * 20 = 4 wildcard, 7 intl)
        let (bal_local, bal_intl, bal_wild, wild_genres) =
            run_with(CultureSteering::Balanced).await;
        assert_eq!((bal_local, bal_intl, bal_wild), (9, 7, 4));
        assert_eq!(wild_genres.len(), 2, "expected jazz + indie rock wildcards, got {wild_genres:?}");

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
        let (kb, pool) = build_consensus_fixtures();
        let profile = eclectic_profile();
        let culture = UserCultureProfile {
            home_country: Some("JP".into()),
            ..Default::default()
        };
        let rejected = HashSet::from([CandidateSet::key("Classic Singer 15", "Holiday Hit 15")]);
        let gen = ContextualCandidateGenerator::new(
            ContextSpec {
                definition: winter_holiday_def(),
                culture,
                steering: CultureSteering::Balanced,
                target_count: 10,
            },
            kb,
            Some(pool),
            rejected,
        );
        let out = gen.generate(&profile, &balanced()).await;
        assert!(
            !out.candidates
                .candidates
                .contains_key(&CandidateSet::key("Classic Singer 15", "Holiday Hit 15"))
        );

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
                    CandidateReason::InternationalContextual {
                        mix_slug: "winter_holiday".into(),
                    },
                    CultureBucket::Local,
                )
            })
            .collect();
        let capped = take_diverse(same_artist, 5, &mut counts);
        assert_eq!(capped.len(), MAX_TRACKS_PER_ARTIST);

        let tracks = vec![("Winter Song", true), ("Regular Track", false)];
        let core_only = filter_core_taste_tracks(tracks, |(_, excluded)| *excluded);
        assert_eq!(core_only, vec![("Regular Track", false)]);
    }
}
