//! The listening profile: what the user plays and stars, read from every
//! listening history and described with the music knowledge.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::info;

use super::Resolver;
use crate::history::HistoryError;
use crate::{ListeningHistory, MusicKnowledge};
use shared::recommendation::{
    fold_name, ArtistRef, KnownMusic, Listen, MomentumArtist, PlayedArtist, PlayedTrack,
    SourceErrors, TimePeriod, UserMusicProfile, WeightedTag,
};

const TOP_ARTISTS: usize = 200;
const TOP_TRACKS: usize = 200;
const MONTH_ARTISTS: usize = 50;
const RECENT_LISTENS: usize = 200;
const RECENT_DAYS: i64 = 30;
const RESOLVED_TOP_ARTISTS: usize = 50;
const POPULARITY_ARTISTS: usize = 30;
const SEED_ARTISTS: usize = 10;
const FAVOURITE_ARTISTS: usize = 20;
const MOMENTUM_ARTISTS: usize = 10;
const MERGED_MOMENTUM_ARTISTS: usize = 15;
const RELATED_TAG_SEEDS: usize = 5;
const COMFORT_TAGS: usize = 15;
const EXPLORATION_TAGS: usize = 10;
const COMFORT_SHARE: f64 = 0.60;
const MIN_EXPLORATION_WEIGHT: f64 = 0.001;
/// Obscurity and repeat ratio when there is nothing to measure them with.
const NEUTRAL: f64 = 0.5;
/// A later history's genres count half, so the base source keeps its shape.
const MERGED_GENRE_WEIGHT: f64 = 0.5;

#[derive(Debug, Clone, Default)]
pub struct ProfileBuild {
    pub profile: UserMusicProfile,
    /// The histories whose plays or stars made it into the profile.
    pub sources: Vec<String>,
    pub errors: SourceErrors,
}

/// Builds one profile per history, in the order given, and merges those with
/// seeds into the first. Every library counts as known, seeds or not, so no
/// generator recommends what the user already has.
pub async fn build_profile(
    histories: &[Arc<dyn ListeningHistory>],
    knowledge: &dyn MusicKnowledge,
) -> ProfileBuild {
    let mut build = ProfileBuild::default();
    let mut resolver = Resolver::new(knowledge);
    let mut known = KnownMusic::default();
    for history in histories {
        let Some(profile) = profile_of(
            history.as_ref(),
            knowledge,
            &mut resolver,
            &mut build.errors,
        )
        .await
        else {
            continue;
        };
        known.extend(&profile.known);
        info!(
            "{} profile: {} seeds, {} favourite artists, {} genres, {} momentum artists",
            history.name(),
            profile.seed_artists.len(),
            profile.favourite_artists.len(),
            profile.genre_distribution.len(),
            profile.momentum_artists.len()
        );
        if !profile.has_seeds() {
            continue;
        }
        if build.sources.is_empty() {
            build.profile = profile;
        } else {
            merge(&mut build.profile, profile);
        }
        build.sources.push(history.name().to_string());
    }
    build.profile.known = known;
    build
}

/// None when the history cannot give its top artists or its library.
async fn profile_of(
    history: &dyn ListeningHistory,
    knowledge: &dyn MusicKnowledge,
    resolver: &mut Resolver<'_>,
    errors: &mut SourceErrors,
) -> Option<UserMusicProfile> {
    let mut top = required(
        history.top_artists(TimePeriod::AllTime, TOP_ARTISTS).await,
        errors,
    )?;
    let mut known = required(history.known().await, errors)?;
    let top_tracks = or_empty(
        history.top_tracks(TimePeriod::AllTime, TOP_TRACKS).await,
        errors,
    );
    let month = or_empty(
        history.top_artists(TimePeriod::Month, MONTH_ARTISTS).await,
        errors,
    );
    let favourites = or_empty(history.favourites().await, errors);

    let recent = if month.is_empty() {
        recent_artists(&or_empty(history.listens(RECENT_LISTENS).await, errors))
    } else {
        month
    };
    let mut momentum_artists = momentum_artists(&top, &recent);

    for played in top.iter_mut().take(RESOLVED_TOP_ARTISTS) {
        resolver
            .fill(&mut played.artist.mbid, &played.artist.name, errors)
            .await;
    }
    let mut favourite_artists = Vec::new();
    union(
        &mut favourite_artists,
        favourites.iter().map(|t| t.artist.clone()).collect(),
        FAVOURITE_ARTISTS,
        |a| (&a.name, a.mbid.as_deref()),
    );
    for artist in &mut favourite_artists {
        resolver.fill(&mut artist.mbid, &artist.name, errors).await;
    }
    for momentum in &mut momentum_artists {
        resolver
            .fill(&mut momentum.mbid, &momentum.name, errors)
            .await;
    }

    let genre_distribution = genre_distribution(knowledge, &top, errors).await;
    let (tag_comfort_zone, tag_exploration_zone) =
        tag_zones(knowledge, &genre_distribution, errors).await;

    for played in &top {
        known.add_artist(&played.artist);
    }
    for played in &top_tracks {
        known.add_track(&played.track);
    }
    for track in &favourites {
        known.add_track(track);
    }
    for artist in &favourite_artists {
        known.add_artist(artist);
    }

    Some(UserMusicProfile {
        obscurity_score: obscurity_score(knowledge, &top, errors).await,
        repeat_ratio: repeat_ratio(&top_tracks),
        genre_distribution,
        tag_comfort_zone,
        tag_exploration_zone,
        momentum_artists,
        seed_artists: top.into_iter().take(SEED_ARTISTS).collect(),
        favourite_artists,
        known,
    })
}

fn required<T>(answer: Result<T, HistoryError>, errors: &mut SourceErrors) -> Option<T> {
    answer.map_err(|e| errors.record(e.service(), &e)).ok()
}

fn or_empty<T: Default>(answer: Result<T, HistoryError>, errors: &mut SourceErrors) -> T {
    required(answer, errors).unwrap_or_default()
}

/// Artists played in the last 30 days, most played first.
fn recent_artists(listens: &[Listen]) -> Vec<PlayedArtist> {
    let since = chrono::Utc::now().timestamp() - RECENT_DAYS * 86_400;
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut artists: Vec<PlayedArtist> = Vec::new();
    for listen in listens.iter().filter(|l| l.timestamp >= since) {
        let i = *at.entry(fold_name(&listen.artist)).or_insert_with(|| {
            artists.push(PlayedArtist {
                artist: ArtistRef {
                    name: listen.artist.clone(),
                    mbid: None,
                },
                plays: 0,
            });
            artists.len() - 1
        });
        artists[i].plays += 1;
    }
    artists.sort_by_key(|a| std::cmp::Reverse(a.plays));
    artists.truncate(MONTH_ARTISTS);
    artists
}

/// Artists climbing from their all-time rank to a higher recent rank; one new
/// to the all-time list scores 1.
fn momentum_artists(top: &[PlayedArtist], recent: &[PlayedArtist]) -> Vec<MomentumArtist> {
    let all_time_rank: HashMap<String, usize> = top
        .iter()
        .enumerate()
        .map(|(rank, a)| (fold_name(&a.artist.name), rank))
        .rev()
        .collect();
    let mut momentum: Vec<MomentumArtist> = recent
        .iter()
        .enumerate()
        .filter_map(|(recent_rank, a)| {
            let score = match all_time_rank.get(&fold_name(&a.artist.name)) {
                Some(0) => 0.0,
                Some(&rank) => (rank as f64 - recent_rank as f64) / rank as f64,
                None => 1.0,
            };
            (score > 0.0).then(|| MomentumArtist {
                name: a.artist.name.clone(),
                momentum_score: score,
                mbid: a.artist.mbid.clone(),
            })
        })
        .collect();
    momentum.sort_by(|a, b| b.momentum_score.total_cmp(&a.momentum_score));
    momentum.truncate(MOMENTUM_ARTISTS);
    momentum
}

/// Tags of the resolved top artists, each weighted by the artist's plays.
async fn genre_distribution(
    knowledge: &dyn MusicKnowledge,
    top: &[PlayedArtist],
    errors: &mut SourceErrors,
) -> Vec<WeightedTag> {
    let resolved: Vec<(&str, u64)> = top
        .iter()
        .take(RESOLVED_TOP_ARTISTS)
        .filter_map(|a| Some((a.artist.mbid.as_deref()?, a.plays)))
        .collect();
    if resolved.is_empty() {
        return Vec::new();
    }
    let mbids: Vec<String> = resolved.iter().map(|(m, _)| m.to_string()).collect();
    let tags = match knowledge.artist_tags(&mbids).await {
        Ok(tags) => tags,
        Err(e) => {
            errors.record(e.service(), &e);
            return Vec::new();
        }
    };
    let mut weights: HashMap<String, f64> = HashMap::new();
    for (mbid, plays) in resolved {
        for tag in tags.get(mbid).into_iter().flatten() {
            *weights.entry(tag.name.to_lowercase()).or_default() += tag.weight * plays as f64;
        }
    }
    normalised(weights)
}

/// Weights scaled to sum 1, heaviest first.
fn normalised(weights: HashMap<String, f64>) -> Vec<WeightedTag> {
    let total: f64 = weights.values().sum();
    if total <= 0.0 {
        return Vec::new();
    }
    let mut tags: Vec<WeightedTag> = weights
        .into_iter()
        .map(|(name, weight)| WeightedTag {
            name,
            weight: weight / total,
        })
        .collect();
    tags.sort_by(|a, b| {
        b.weight
            .total_cmp(&a.weight)
            .then_with(|| a.name.cmp(&b.name))
    });
    tags
}

/// 1 for listeners of little-known artists, 0 for those of the most known:
/// the median user count of the resolved top artists against the reference.
async fn obscurity_score(
    knowledge: &dyn MusicKnowledge,
    top: &[PlayedArtist],
    errors: &mut SourceErrors,
) -> f64 {
    let mbids: Vec<String> = top
        .iter()
        .take(POPULARITY_ARTISTS)
        .filter_map(|a| a.artist.mbid.clone())
        .collect();
    if mbids.is_empty() {
        return NEUTRAL;
    }
    let mut users: Vec<u64> = match knowledge.artist_popularity(&mbids).await {
        Ok(users) => users.into_values().collect(),
        Err(e) => {
            errors.record(e.service(), &e);
            return NEUTRAL;
        }
    };
    if users.is_empty() {
        return NEUTRAL;
    }
    users.sort_unstable();
    let median = users[users.len() / 2];
    match knowledge.popularity_reference().await {
        Ok(0) => NEUTRAL,
        Ok(reference) => 1.0 - (median as f64 / reference as f64).clamp(0.0, 1.0),
        Err(e) => {
            errors.record(e.service(), &e);
            NEUTRAL
        }
    }
}

/// Distinct top tracks per play: 1 when every play is a different track.
fn repeat_ratio(top_tracks: &[PlayedTrack]) -> f64 {
    let plays: u64 = top_tracks.iter().map(|t| t.plays).sum();
    if plays == 0 {
        return NEUTRAL;
    }
    top_tracks.len() as f64 / plays as f64
}

/// Comfort tags cover the first 60% of the genre weight; the rest, with the
/// tags related to the first comfort tags, are there to explore.
async fn tag_zones(
    knowledge: &dyn MusicKnowledge,
    genres: &[WeightedTag],
    errors: &mut SourceErrors,
) -> (Vec<String>, Vec<String>) {
    let mut comfort = Vec::new();
    let mut exploration = Vec::new();
    let mut cumulative = 0.0;
    for tag in genres {
        if cumulative < COMFORT_SHARE {
            comfort.push(tag.name.to_lowercase());
        } else if tag.weight > MIN_EXPLORATION_WEIGHT {
            exploration.push(tag.name.to_lowercase());
        }
        cumulative += tag.weight;
    }
    for seed in comfort.iter().take(RELATED_TAG_SEEDS) {
        match knowledge.related_tags(seed).await {
            Ok(related) => {
                for tag in related {
                    let tag = tag.to_lowercase();
                    if !comfort.contains(&tag) && !exploration.contains(&tag) {
                        exploration.push(tag);
                    }
                }
            }
            Err(e) => errors.record(e.service(), &e),
        }
    }
    comfort.truncate(COMFORT_TAGS);
    exploration.truncate(EXPLORATION_TAGS);
    (comfort, exploration)
}

/// Adds a later history's profile to the base; scalar fields stay the base's.
fn merge(base: &mut UserMusicProfile, other: UserMusicProfile) {
    let mut weights: HashMap<String, f64> = std::mem::take(&mut base.genre_distribution)
        .into_iter()
        .map(|t| (t.name, t.weight))
        .collect();
    for tag in other.genre_distribution {
        *weights.entry(tag.name).or_default() += tag.weight * MERGED_GENRE_WEIGHT;
    }
    base.genre_distribution = normalised(weights);
    union(
        &mut base.momentum_artists,
        other.momentum_artists,
        MERGED_MOMENTUM_ARTISTS,
        |m| (&m.name, m.mbid.as_deref()),
    );
    // Seeds and favourites take turns, so each history holds about half of the
    // first places the generators read.
    interleave(
        &mut base.seed_artists,
        other.seed_artists,
        SEED_ARTISTS,
        |p| (&p.artist.name, p.artist.mbid.as_deref()),
    );
    interleave(
        &mut base.favourite_artists,
        other.favourite_artists,
        FAVOURITE_ARTISTS,
        |a| (&a.name, a.mbid.as_deref()),
    );
}

/// Appends the artists of `from` that `into` does not name, by folded name or
/// MBID, then keeps the first `cap`.
fn union<T>(
    into: &mut Vec<T>,
    from: Vec<T>,
    cap: usize,
    artist: impl Fn(&T) -> (&str, Option<&str>),
) {
    for item in from {
        if into.len() >= cap {
            break;
        }
        if !names(into, &item, &artist) {
            into.push(item);
        }
    }
    into.truncate(cap);
}

/// Takes from `into` and `from` in turn, skipping artists already taken by
/// folded name or MBID, up to `cap`: a full first list cannot crowd out the
/// second one's first artists.
fn interleave<T>(
    into: &mut Vec<T>,
    from: Vec<T>,
    cap: usize,
    artist: impl Fn(&T) -> (&str, Option<&str>),
) {
    let mut base = std::mem::take(into).into_iter();
    let mut from = from.into_iter();
    while into.len() < cap {
        let (a, b) = (base.next(), from.next());
        if a.is_none() && b.is_none() {
            break;
        }
        for item in a.into_iter().chain(b) {
            if into.len() < cap && !names(into, &item, &artist) {
                into.push(item);
            }
        }
    }
}

/// Whether `list` holds the artist of `item`, by folded name or MBID.
fn names<T>(list: &[T], item: &T, artist: &impl Fn(&T) -> (&str, Option<&str>)) -> bool {
    let (name, mbid) = artist(item);
    let name = fold_name(name);
    list.iter().any(|x| {
        let (n, m) = artist(x);
        fold_name(n) == name || matches!((m, mbid), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::engine::test_support::*;
    use crate::ListeningHistory;
    use shared::recommendation::{Listen, PlayedTrack, TrackRef};

    const SH: &str = "1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f";
    const VN: &str = "2e3f4a5b-6c7d-4e8f-9a0b-1c2d3e4f5a6b";

    fn names(seeds: &[shared::recommendation::PlayedArtist]) -> Vec<&str> {
        seeds.iter().map(|s| s.artist.name.as_str()).collect()
    }

    #[tokio::test]
    async fn a_navidrome_only_profile_has_seeds_genres_and_the_library_as_known() {
        let navidrome = FakeHistory {
            name: "Navidrome",
            top_artists: vec![played("Static Harbor", None, 12)],
            top_tracks: vec![PlayedTrack {
                track: track("Static Harbor", None, "Northern Line", None),
                plays: 12,
            }],
            favourites: vec![track("Vela Norte", None, "Foglight", None)],
            known: known_of(&["Marrow Choir"]),
            ..Default::default()
        };
        let k = FakeKnowledge {
            mbids: [("static harbor", SH), ("vela norte", VN)].into(),
            tags: [(SH, vec![tag("dream pop", 1.0)])].into(),
            users: [(SH, 1200)].into(),
            reference: 2400,
            ..Default::default()
        };
        let b = build_profile(&[Arc::new(navidrome) as Arc<dyn ListeningHistory>], &k).await;
        assert_eq!(b.sources, ["Navidrome"]);
        assert_eq!(
            b.profile.seed_artists[0].artist,
            artist("Static Harbor", Some(SH))
        );
        assert_eq!(
            b.profile.favourite_artists,
            [artist("Vela Norte", Some(VN))]
        );
        assert_eq!(b.profile.genre_distribution[0].name, "dream pop");
        assert!((b.profile.obscurity_score - 0.5).abs() < 1e-9);
        assert!((b.profile.repeat_ratio - 1.0 / 12.0).abs() < 1e-9);
        assert!(b.profile.known.knows_artist(&artist("Marrow Choir", None)));
        assert!(b
            .profile
            .known
            .knows_track(&track("Vela Norte", None, "Foglight", None)));
    }

    #[tokio::test]
    async fn listenbrainz_is_the_base_and_navidrome_adds_its_seeds_and_library() {
        // More of each than the caps hold, so Navidrome only gets in by taking turns.
        let tops = |prefix: &str| -> Vec<PlayedArtist> {
            (0..SEED_ARTISTS + 2)
                .map(|i| played(&format!("{prefix}{i}"), None, 100 - i as u64))
                .collect()
        };
        let loved = |prefix: &str| -> Vec<TrackRef> {
            (0..FAVOURITE_ARTISTS + 2)
                .map(|i| track(&format!("{prefix}{i}"), None, "t", None))
                .collect()
        };
        let listenbrainz = FakeHistory {
            name: "ListenBrainz",
            top_artists: tops("LB"),
            favourites: loved("LB loved "),
            ..Default::default()
        };
        let navidrome = FakeHistory {
            name: "Navidrome",
            top_artists: tops("ND"),
            favourites: loved("ND starred "),
            known: known_of(&["C"]),
            ..Default::default()
        };
        let histories: [Arc<dyn ListeningHistory>; 2] =
            [Arc::new(listenbrainz), Arc::new(navidrome)];
        let b = build_profile(&histories, &FakeKnowledge::default()).await;
        assert_eq!(b.sources, ["ListenBrainz", "Navidrome"]);
        assert_eq!(
            names(&b.profile.seed_artists),
            ["LB0", "ND0", "LB1", "ND1", "LB2", "ND2", "LB3", "ND3", "LB4", "ND4"]
        );
        let first_favourites: Vec<&str> = b
            .profile
            .favourite_artists
            .iter()
            .take(10)
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(
            first_favourites,
            [
                "LB loved 0",
                "ND starred 0",
                "LB loved 1",
                "ND starred 1",
                "LB loved 2",
                "ND starred 2",
                "LB loved 3",
                "ND starred 3",
                "LB loved 4",
                "ND starred 4"
            ]
        );
        assert_eq!(b.profile.favourite_artists.len(), FAVOURITE_ARTISTS);
        for name in ["LB11", "ND11", "C"] {
            assert!(b.profile.known.knows_artist(&artist(name, None)), "{name}");
        }
    }

    #[test]
    fn interleaving_skips_an_artist_the_other_list_already_holds() {
        let mut base = vec![artist("A", None), artist("B", Some(SH))];
        interleave(
            &mut base,
            vec![
                artist("a", None),
                artist("Other B", Some(SH)),
                artist("C", None),
            ],
            3,
            |a| (&a.name, a.mbid.as_deref()),
        );
        let got: Vec<&str> = base.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(got, ["A", "B", "C"]);
    }

    #[tokio::test]
    async fn a_library_without_plays_still_counts_as_known() {
        let listenbrainz = FakeHistory {
            name: "ListenBrainz",
            top_artists: vec![played("A", None, 5)],
            ..Default::default()
        };
        let navidrome = FakeHistory {
            name: "Navidrome",
            known: known_of(&["C"]),
            ..Default::default()
        };
        let histories: [Arc<dyn ListeningHistory>; 2] =
            [Arc::new(navidrome), Arc::new(listenbrainz)];
        let b = build_profile(&histories, &FakeKnowledge::default()).await;
        assert_eq!(b.sources, ["ListenBrainz"]);
        assert!(b.profile.known.knows_artist(&artist("C", None)));
    }

    #[tokio::test]
    async fn nothing_played_or_starred_has_no_seeds() {
        let navidrome = FakeHistory {
            name: "Navidrome",
            known: known_of(&["Marrow Choir"]),
            ..Default::default()
        };
        let b = build_profile(
            &[Arc::new(navidrome) as Arc<dyn ListeningHistory>],
            &FakeKnowledge::default(),
        )
        .await;
        assert!(!b.profile.has_seeds());
        assert!(b.sources.is_empty());
        assert!(b.errors.is_empty());
    }

    #[tokio::test]
    async fn a_failing_history_is_reported_and_the_others_still_count() {
        let listenbrainz = FakeHistory {
            name: "ListenBrainz",
            fails: true,
            ..Default::default()
        };
        let navidrome = FakeHistory {
            name: "Navidrome",
            top_artists: vec![played("B", None, 3)],
            ..Default::default()
        };
        let histories: [Arc<dyn ListeningHistory>; 2] =
            [Arc::new(listenbrainz), Arc::new(navidrome)];
        let b = build_profile(&histories, &FakeKnowledge::default()).await;
        assert_eq!(b.sources, ["Navidrome"]);
        assert_eq!(b.errors.into_vec()[0].source, "ListenBrainz");
    }

    #[tokio::test]
    async fn an_unresolvable_artist_is_reported_not_fatal() {
        let listenbrainz = FakeHistory {
            name: "ListenBrainz",
            top_artists: vec![played("Static Harbor", None, 4)],
            ..Default::default()
        };
        let k = FakeKnowledge {
            mbids: [("static harbor", SH)].into(),
            failing: ["resolve_artist".to_string()].into(),
            ..Default::default()
        };
        let b = build_profile(&[Arc::new(listenbrainz) as Arc<dyn ListeningHistory>], &k).await;
        assert_eq!(b.sources, ["ListenBrainz"]);
        assert_eq!(
            b.profile.seed_artists[0].artist,
            artist("Static Harbor", None)
        );
        let errors = b.errors.into_vec();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].source, "ListenBrainz Labs");
    }

    #[tokio::test]
    async fn momentum_comes_from_the_last_30_days_of_listens_when_the_month_is_empty() {
        let now = chrono::Utc::now().timestamp();
        let listen = |artist: &str, days_ago: i64| Listen {
            artist: artist.to_string(),
            track: "t".to_string(),
            album: None,
            timestamp: now - days_ago * 86_400,
        };
        let navidrome = FakeHistory {
            name: "Navidrome",
            top_artists: vec![played("Static Harbor", None, 12)],
            listens: vec![listen("Vela Norte", 2), listen("Marrow Choir", 40)],
            ..Default::default()
        };
        let k = FakeKnowledge {
            mbids: [("vela norte", VN)].into(),
            ..Default::default()
        };
        let b = build_profile(&[Arc::new(navidrome) as Arc<dyn ListeningHistory>], &k).await;
        let momentum: Vec<_> = b
            .profile
            .momentum_artists
            .iter()
            .map(|m| (m.name.as_str(), m.momentum_score, m.mbid.as_deref()))
            .collect();
        assert_eq!(momentum, [("Vela Norte", 1.0, Some(VN))]);
    }
}
