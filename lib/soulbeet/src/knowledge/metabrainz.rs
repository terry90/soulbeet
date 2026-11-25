//! Music knowledge from ListenBrainz, ListenBrainz Labs and MusicBrainz,
//! cached for every user of the install.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use shared::recommendation::{
    fold_name, ArtistRef, Neighbour, RecordingFacts, TrackRef, WeightedTag,
};

use super::{
    cached, read, store, KnowledgeError, TTL_ARTIST_TAGS, TTL_GENRES, TTL_MBID, TTL_POPULARITY,
    TTL_RECORDINGS, TTL_SIMILAR_ARTISTS, TTL_TAG_SIMILARITY, TTL_TAG_TRACKS, TTL_TOP_TRACKS,
};
use crate::listenbrainz::client::ListenBrainzClient;
use crate::listenbrainz::labs::LabsClient;
use crate::listenbrainz::models::{ArtistTags, RecordingMetadata};
use crate::musicbrainz::exact_artist;
use crate::polite::RemoteError;
use crate::traits::{KnowledgeCache, MusicBrainzLookup, MusicKnowledge, TagOp};

pub const SIMILAR_ARTISTS_ALGORITHM: &str =
    "session_based_days_7500_session_300_contribution_5_threshold_10_limit_100_filter_True_skip_30";

/// Top tracks kept per artist, whatever limit a caller asks for.
const TOP_TRACKS_KEPT: usize = 10;
/// The artist metadata MBIDs ride in the query string, so fewer per request.
const ARTIST_METADATA_BATCH: usize = 25;
const POSTED_BATCH: usize = 100;
const SITEWIDE_ARTISTS: usize = 200;
const ARTIST_SEARCH_LIMIT: u8 = 10;

pub struct MetaBrainzKnowledge {
    listenbrainz: Arc<ListenBrainzClient>,
    labs: LabsClient,
    musicbrainz: Arc<dyn MusicBrainzLookup>,
    cache: Arc<dyn KnowledgeCache>,
}

impl MetaBrainzKnowledge {
    pub fn new(
        listenbrainz: Arc<ListenBrainzClient>,
        labs: LabsClient,
        musicbrainz: Arc<dyn MusicBrainzLookup>,
        cache: Arc<dyn KnowledgeCache>,
    ) -> Self {
        Self {
            listenbrainz,
            labs,
            musicbrainz,
            cache,
        }
    }

    /// ListenBrainz's own ranking of the artist's recordings, which needs a token.
    async fn ranked_top_tracks(&self, artist_mbid: &str) -> Result<Vec<TrackRef>, KnowledgeError> {
        let mut top = self
            .listenbrainz
            .top_recordings_for_artist(artist_mbid)
            .await
            .map_err(|e| on_unauthorized(e, KnowledgeError::TokenRefused))?;
        top.sort_by_key(|r| std::cmp::Reverse(r.total_listen_count));
        Ok(top
            .into_iter()
            .take(TOP_TRACKS_KEPT)
            .map(|r| TrackRef {
                artist: ArtistRef {
                    name: r.artist_name,
                    mbid: Some(artist_mbid.to_string()),
                },
                title: r.recording_name,
                mbid: Some(r.recording_mbid),
            })
            .collect())
    }

    /// The artist radio ranked by listens, for installs without a token.
    async fn radio_top_tracks(&self, artist_mbid: &str) -> Result<Vec<TrackRef>, KnowledgeError> {
        let mut radio = self
            .listenbrainz
            .artist_radio(artist_mbid)
            .await
            .map_err(|e| on_unauthorized(e, KnowledgeError::TokenRequired))?;
        let mut recordings = radio.remove(artist_mbid).unwrap_or_default();
        recordings.sort_by(|a, b| {
            b.total_listen_count
                .cmp(&a.total_listen_count)
                .then_with(|| a.recording_mbid.cmp(&b.recording_mbid))
        });
        let ids: Vec<String> = recordings
            .into_iter()
            .take(TOP_TRACKS_KEPT)
            .map(|r| r.recording_mbid)
            .collect();
        let facts = self.recordings(&ids).await?;
        Ok(ids
            .into_iter()
            .filter_map(|id| {
                let f = facts.get(&id)?;
                Some(TrackRef {
                    artist: ArtistRef {
                        name: f.artist.name.clone(),
                        mbid: Some(artist_mbid.to_string()),
                    },
                    title: f.title.clone(),
                    mbid: Some(id),
                })
            })
            .collect())
    }
}

#[async_trait]
impl MusicKnowledge for MetaBrainzKnowledge {
    fn name(&self) -> &'static str {
        "MetaBrainz"
    }

    async fn resolve_artist(&self, name: &str) -> Result<Option<String>, KnowledgeError> {
        let key = format!("kc1:mb:artist:{}", fold_name(name));
        cached(self.cache.as_ref(), &key, TTL_MBID, async {
            let found = self
                .musicbrainz
                .search_artists(name, ARTIST_SEARCH_LIMIT)
                .await
                .map_err(KnowledgeError::MusicBrainz)?;
            Ok(exact_artist(name, &found))
        })
        .await
    }

    async fn similar_artists(
        &self,
        seed_name: &str,
        seed_mbid: &str,
        limit: usize,
    ) -> Result<Vec<Neighbour>, KnowledgeError> {
        let key = format!("kc1:labs:similar-artists:{seed_mbid}");
        // Stored with Labs' raw scores; scaled once the seed's other credits are out.
        let stored: Vec<Neighbour> =
            cached(self.cache.as_ref(), &key, TTL_SIMILAR_ARTISTS, async {
                let found = self
                    .labs
                    .similar_artists(seed_mbid, SIMILAR_ARTISTS_ALGORITHM)
                    .await
                    .map_err(|e| match e {
                        RemoteError::Status { status: 400, .. } => {
                            KnowledgeError::AlgorithmRetired {
                                algorithm: SIMILAR_ARTISTS_ALGORITHM.to_string(),
                            }
                        }
                        other => other.into(),
                    })?;
                Ok(found
                    .into_iter()
                    .filter(|n| !n.artist_mbid.eq_ignore_ascii_case(seed_mbid))
                    .filter_map(|n| {
                        let name = n.name.filter(|name| !name.trim().is_empty())?;
                        Some(Neighbour {
                            artist: ArtistRef {
                                name,
                                mbid: Some(n.artist_mbid),
                            },
                            score: n.score as f64,
                        })
                    })
                    .collect())
            })
            .await?;
        let kept: Vec<Neighbour> = stored
            .into_iter()
            .filter(|n| !names_the_seed(&n.artist.name, seed_name))
            .collect();
        let best = kept.iter().map(|n| n.score).fold(0.0, f64::max);
        Ok(kept
            .into_iter()
            .take(limit)
            .map(|n| Neighbour {
                score: n.score / best,
                ..n
            })
            .collect())
    }

    async fn artist_top_tracks(
        &self,
        artist_mbid: &str,
        limit: usize,
    ) -> Result<Vec<TrackRef>, KnowledgeError> {
        let cache = self.cache.as_ref();
        let tracks = if self.listenbrainz.has_token() {
            let key = format!("kc1:lb:top-tracks:token:{artist_mbid}");
            cached(
                cache,
                &key,
                TTL_TOP_TRACKS,
                self.ranked_top_tracks(artist_mbid),
            )
            .await?
        } else {
            let key = format!("kc1:lb:top-tracks:radio:{artist_mbid}");
            cached(
                cache,
                &key,
                TTL_TOP_TRACKS,
                self.radio_top_tracks(artist_mbid),
            )
            .await?
        };
        Ok(tracks.into_iter().take(limit).collect())
    }

    async fn artist_tags(
        &self,
        artist_mbids: &[String],
    ) -> Result<HashMap<String, Vec<WeightedTag>>, KnowledgeError> {
        let lb = &self.listenbrainz;
        cached_each(
            self.cache.as_ref(),
            "kc1:lb:artist-tags:",
            artist_mbids,
            TTL_ARTIST_TAGS,
            ARTIST_METADATA_BATCH,
            |batch| async move {
                Ok(lb
                    .artist_metadata(&batch)
                    .await?
                    .into_iter()
                    .map(|e| (e.artist_mbid, scaled_tags(e.tag)))
                    .collect())
            },
        )
        .await
    }

    async fn artist_areas(
        &self,
        artist_mbids: &[String],
    ) -> Result<HashMap<String, Option<String>>, KnowledgeError> {
        let lb = &self.listenbrainz;
        cached_each(
            self.cache.as_ref(),
            "kc1:lb:artist-area:",
            artist_mbids,
            TTL_ARTIST_TAGS,
            ARTIST_METADATA_BATCH,
            |batch| async move {
                Ok(lb
                    .artist_metadata(&batch)
                    .await?
                    .into_iter()
                    .map(|e| {
                        let code = e.area.as_deref().and_then(normalize_area_code);
                        (e.artist_mbid, code)
                    })
                    .collect())
            },
        )
        .await
    }

    async fn artist_popularity(
        &self,
        artist_mbids: &[String],
    ) -> Result<HashMap<String, u64>, KnowledgeError> {
        let lb = &self.listenbrainz;
        let users: HashMap<String, Option<u64>> = cached_each(
            self.cache.as_ref(),
            "kc1:lb:artist-users:",
            artist_mbids,
            TTL_POPULARITY,
            POSTED_BATCH,
            |batch| async move {
                Ok(lb
                    .artist_popularity(&batch)
                    .await?
                    .into_iter()
                    .map(|e| (e.artist_mbid, e.total_user_count))
                    .collect())
            },
        )
        .await?;
        Ok(users
            .into_iter()
            .filter_map(|(mbid, count)| Some((mbid, count?)))
            .collect())
    }

    async fn popularity_reference(&self) -> Result<u64, KnowledgeError> {
        cached(
            self.cache.as_ref(),
            "kc1:lb:popularity-reference",
            TTL_POPULARITY,
            async {
                let top = self.listenbrainz.sitewide_artists(SITEWIDE_ARTISTS).await?;
                let mbids: Vec<String> = top.into_iter().filter_map(|a| a.artist_mbid).collect();
                let mut counts: Vec<u64> = self
                    .artist_popularity(&mbids)
                    .await?
                    .into_values()
                    .collect();
                counts.sort_unstable();
                let median = counts.get(counts.len() / 2).copied();
                median.ok_or_else(|| {
                    RemoteError::Unreadable {
                        service: "ListenBrainz",
                        detail: "no user counts for the sitewide top artists".to_string(),
                    }
                    .into()
                })
            },
        )
        .await
    }

    async fn related_tags(&self, tag: &str) -> Result<Vec<String>, KnowledgeError> {
        let cache = self.cache.as_ref();
        let key = format!("kc1:labs:tag-similarity:{tag}");
        let similar: Vec<String> = cached(cache, &key, TTL_TAG_SIMILARITY, async {
            Ok(self
                .labs
                .tag_similarity(tag)
                .await?
                .into_iter()
                .map(|t| t.similar_tag)
                .collect())
        })
        .await?;
        let genres: Vec<String> = cached(cache, "kc1:mb:genres", TTL_GENRES, async {
            self.musicbrainz
                .genre_names()
                .await
                .map_err(KnowledgeError::MusicBrainz)
        })
        .await?;
        let genres: HashSet<&str> = genres.iter().map(String::as_str).collect();
        Ok(similar
            .into_iter()
            .filter(|t| t != tag && genres.contains(t.as_str()))
            .collect())
    }

    async fn tag_tracks(&self, tag: &str, count: usize) -> Result<Vec<TrackRef>, KnowledgeError> {
        let key = format!("kc1:lb:tag-tracks:{tag}:{count}");
        cached(self.cache.as_ref(), &key, TTL_TAG_TRACKS, async {
            let ids: Vec<String> = self
                .listenbrainz
                .tag_radio(tag, count)
                .await?
                .into_iter()
                .map(|r| r.recording_mbid)
                .collect();
            let mut facts = self.recordings(&ids).await?;
            Ok(ids
                .into_iter()
                .filter_map(|id| {
                    let f = facts.remove(&id)?;
                    Some(TrackRef {
                        artist: f.artist,
                        title: f.title,
                        mbid: Some(id),
                    })
                })
                .collect())
        })
        .await
    }

    async fn multi_tag_tracks(
        &self,
        tags: &[&str],
        operator: TagOp,
        pop_range: Option<(u8, u8)>,
        count: usize,
    ) -> Result<Vec<TrackRef>, KnowledgeError> {
        if tags.is_empty() || count == 0 {
            return Ok(Vec::new());
        }
        let (pop_begin, pop_end) = pop_range.unwrap_or((0, 100));
        let normalized_tags: Vec<String> = tags
            .iter()
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if normalized_tags.is_empty() {
            return Ok(Vec::new());
        }
        let key = format!(
            "kc1:lb:multi-tag:{}:{}-{}:{}:{}",
            operator.as_str(),
            pop_begin,
            pop_end,
            normalized_tags.join("+"),
            count
        );
        cached(self.cache.as_ref(), &key, TTL_TAG_TRACKS, async {
            let refs: Vec<&str> = normalized_tags.iter().map(String::as_str).collect();
            let ids: Vec<String> = self
                .listenbrainz
                .multi_tag_radio(&refs, operator.as_str(), pop_begin, pop_end, count)
                .await?
                .into_iter()
                .map(|r| r.recording_mbid)
                .collect();
            let mut facts = self.recordings(&ids).await?;
            Ok(ids
                .into_iter()
                .filter_map(|id| {
                    let f = facts.remove(&id)?;
                    Some(TrackRef {
                        artist: f.artist,
                        title: f.title,
                        mbid: Some(id),
                    })
                })
                .collect())
        })
        .await
    }

    async fn recordings(
        &self,
        recording_mbids: &[String],
    ) -> Result<HashMap<String, RecordingFacts>, KnowledgeError> {
        let lb = &self.listenbrainz;
        let facts: HashMap<String, Option<RecordingFacts>> = cached_each(
            self.cache.as_ref(),
            "kc1:lb:recording:",
            recording_mbids,
            TTL_RECORDINGS,
            POSTED_BATCH,
            |batch| async move {
                Ok(lb
                    .recording_metadata(&batch)
                    .await?
                    .into_iter()
                    .map(|(mbid, m)| (mbid, Some(recording_facts(m))))
                    .collect())
            },
        )
        .await?;
        Ok(facts
            .into_iter()
            .filter_map(|(mbid, f)| Some((mbid, f?)))
            .collect())
    }
}

/// Normalizes a MusicBrainz / ListenBrainz artist `area` string into a 2-letter ISO-3166
/// country code (e.g. `"France"` -> `"FR"`, `"fr"` -> `"FR"`).
pub(crate) fn normalize_area_code(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() == 2 && trimmed.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Some(trimmed.to_ascii_uppercase());
    }
    let code = match trimmed.to_ascii_lowercase().as_str() {
        "france" => "FR",
        "united states" | "united states of america" | "usa" => "US",
        "united kingdom" | "england" | "scotland" | "wales" | "northern ireland" | "uk" => "GB",
        "canada" => "CA",
        "germany" => "DE",
        "japan" => "JP",
        "spain" => "ES",
        "italy" => "IT",
        "belgium" => "BE",
        "switzerland" => "CH",
        "netherlands" => "NL",
        "sweden" => "SE",
        "norway" => "NO",
        "denmark" => "DK",
        "finland" => "FI",
        "ireland" => "IE",
        "australia" => "AU",
        "new zealand" => "NZ",
        "brazil" => "BR",
        "mexico" => "MX",
        "argentina" => "AR",
        "south korea" | "korea, republic of" => "KR",
        "portugal" => "PT",
        "poland" => "PL",
        "austria" => "AT",
        "iceland" => "IS",
        "mali" => "ML",
        "india" => "IN",
        _ => return None,
    };
    Some(code.to_string())
}

/// True when the seed's folded name sits in the neighbour's as a whole word,
/// as in "Fishmans+" for Fishmans: the seed under another credit.
fn names_the_seed(neighbour: &str, seed: &str) -> bool {
    let (neighbour, seed) = (fold_name(neighbour), fold_name(seed));
    if seed.is_empty() {
        return false;
    }
    neighbour.match_indices(&seed).any(|(at, _)| {
        let before = neighbour[..at].chars().next_back();
        let after = neighbour[at + seed.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// A 401 is the token's fault: `token_error` says which way.
fn on_unauthorized(e: RemoteError, token_error: KnowledgeError) -> KnowledgeError {
    match e {
        RemoteError::Status { status: 401, .. } => token_error,
        other => other.into(),
    }
}

/// Tag weights relative to the artist's most voted tag. MusicBrainz votes can
/// net to zero or below, a tag editors reject, so only positive counts count.
fn scaled_tags(tags: Option<ArtistTags>) -> Vec<WeightedTag> {
    let entries: Vec<_> = tags
        .map(|t| t.artist)
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.count > 0)
        .collect();
    let Some(max) = entries.iter().map(|t| t.count).max() else {
        return Vec::new();
    };
    let mut weighted: Vec<WeightedTag> = entries
        .into_iter()
        .map(|t| WeightedTag {
            weight: t.count as f64 / max as f64,
            name: t.tag,
        })
        .collect();
    weighted.sort_by(|a, b| {
        b.weight
            .total_cmp(&a.weight)
            .then_with(|| a.name.cmp(&b.name))
    });
    weighted
}

fn recording_facts(m: RecordingMetadata) -> RecordingFacts {
    RecordingFacts {
        title: m.recording.name,
        artist: ArtistRef {
            name: m.artist.name,
            mbid: m.artist.artists.into_iter().next().map(|a| a.artist_mbid),
        },
        first_release_year: m
            .recording
            .first_release_date
            .as_deref()
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse().ok()),
    }
}

/// Reads each id under `prefix`, fetches the misses `batch` at a time and
/// stores what each fetched id got; an id the answer leaves out is stored as
/// `T::default()`, so it is not asked again before `ttl` runs out.
async fn cached_each<T, F, Fut>(
    cache: &dyn KnowledgeCache,
    prefix: &str,
    ids: &[String],
    ttl: Duration,
    batch: usize,
    fetch: F,
) -> Result<HashMap<String, T>, KnowledgeError>
where
    T: Serialize + DeserializeOwned + Default,
    F: Fn(Vec<String>) -> Fut,
    Fut: Future<Output = Result<HashMap<String, T>, KnowledgeError>>,
{
    let mut found = HashMap::new();
    let mut asked = HashSet::new();
    let mut misses = Vec::new();
    for id in ids {
        if !asked.insert(id) {
            continue;
        }
        match read(cache, &format!("{prefix}{id}")).await? {
            Some(value) => {
                found.insert(id.clone(), value);
            }
            None => misses.push(id.clone()),
        }
    }
    for chunk in misses.chunks(batch) {
        let mut fetched = fetch(chunk.to_vec()).await?;
        for id in chunk {
            let value = fetched.remove(id).unwrap_or_default();
            store(cache, &format!("{prefix}{id}"), &value, ttl).await?;
            found.insert(id.clone(), value);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use shared::failure::Service;

    use super::*;
    use crate::knowledge::MemoryCache;
    use crate::listenbrainz::models::ArtistTagEntry;
    use crate::musicbrainz::ArtistMatch;
    use crate::polite::Pace;
    use crate::test_http::{serve, Reply};
    use crate::MetadataError;

    const UNUSED: &str = "http://127.0.0.1:1";
    const FISHMANS: &str = "d0f25f16-7782-42e8-8047-c16a1cb2b450";
    const WONDER_WHO: &str = "75874408-e5a8-4bdd-94cf-3dfc8ce1acdc";
    const RADIOHEAD: &str = "a74b1b7f-71a5-4011-9441-d0b5e4122711";
    const TINARIWEN: &str = "5c98fc12-be83-4246-ae5b-2184192913b9";

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    /// Answers searches from a queue and counts them; the genre list is the fixture's.
    #[derive(Default)]
    struct FakeMusicBrainz {
        answers: Mutex<VecDeque<Result<Vec<ArtistMatch>, MetadataError>>>,
        searches: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl MusicBrainzLookup for FakeMusicBrainz {
        async fn search_artists(
            &self,
            _name: &str,
            _limit: u8,
        ) -> Result<Vec<ArtistMatch>, MetadataError> {
            self.searches.fetch_add(1, Ordering::SeqCst);
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("a queued search answer")
        }

        async fn genre_names(&self) -> Result<Vec<String>, MetadataError> {
            Ok(include_str!("../fixtures/musicbrainz/genres.txt")
                .lines()
                .map(String::from)
                .collect())
        }
    }

    fn knowledge(
        api: &str,
        labs: &str,
        mb: FakeMusicBrainz,
        token: Option<&str>,
    ) -> (MetaBrainzKnowledge, Arc<MemoryCache>) {
        let cache = Arc::new(MemoryCache::default());
        let listenbrainz = Arc::new(
            ListenBrainzClient::new(api, token.map(String::from), Pace::immediate()).unwrap(),
        );
        let labs = LabsClient::new(labs, Pace::immediate()).unwrap();
        let k = MetaBrainzKnowledge::new(listenbrainz, labs, Arc::new(mb), cache.clone());
        (k, cache)
    }

    fn titles(tracks: &[TrackRef]) -> Vec<&str> {
        tracks.iter().map(|t| t.title.as_str()).collect()
    }

    #[tokio::test]
    async fn a_neighbour_naming_the_seed_or_unnamed_is_left_out() {
        let (labs, _) = serve(vec![
            ok(include_str!(
                "../listenbrainz/fixtures/similar_artists_fishmans.json"
            )),
            ok(include_str!(
                "../listenbrainz/fixtures/similar_artists_null_name.json"
            )),
        ]);
        let (k, _) = knowledge(UNUSED, &labs, FakeMusicBrainz::default(), None);
        let n = k.similar_artists("Fishmans", FISHMANS, 3).await.unwrap();
        assert_eq!(
            n.iter().map(|x| x.artist.name.as_str()).collect::<Vec<_>>(),
            ["Death Grips", "Sweet Trip", "Beach House"]
        );
        assert_eq!(n[0].score, 1.0);
        assert!((n[1].score - 168.0 / 181.0).abs() < 1e-9);
        assert_eq!(
            n[0].artist.mbid.as_deref(),
            Some("f9133036-ab3d-4e97-bd11-7a2c98ad148a")
        );
        let m = k
            .similar_artists(
                "Benjamin Francis Leftwich",
                "b9e82c61-c1ec-4b42-9233-ca3567d2f2da",
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            m.iter().map(|x| x.artist.name.as_str()).collect::<Vec<_>>(),
            ["Ben Howard", "José González"]
        );
        assert!((m[1].score - 1140.0 / 1284.0).abs() < 1e-9);
    }

    #[test]
    fn a_seed_name_counts_only_as_a_whole_word() {
        assert!(names_the_seed("Fishmans+", "Fishmans"));
        assert!(!names_the_seed("Airbag", "Air"));
        assert!(names_the_seed(
            "Arijit Singh & Shreya Ghoshal",
            "Arijit Singh"
        ));
    }

    #[tokio::test]
    async fn a_retired_algorithm_is_a_typed_error() {
        let (labs, _) = serve(vec![Reply {
            status: 400,
            body: include_str!("../listenbrainz/fixtures/similar_artists_retired.html").into(),
        }]);
        let (k, cache) = knowledge(UNUSED, &labs, FakeMusicBrainz::default(), None);
        match k.similar_artists("Fishmans", FISHMANS, 10).await {
            Err(KnowledgeError::AlgorithmRetired { algorithm }) => {
                assert_eq!(algorithm, SIMILAR_ARTISTS_ALGORITHM)
            }
            other => panic!("expected a retired algorithm, got {other:?}"),
        }
        assert!(cache.ttls().is_empty());
    }

    #[tokio::test]
    async fn similar_artists_are_kept_30_days() {
        let seed = "5b1b9c77-dfee-42ce-a4ae-abee4d40d407";
        let (labs, seen) = serve(vec![ok(include_str!(
            "../listenbrainz/fixtures/similar_artists_fishmans.json"
        ))]);
        let (k, cache) = knowledge(UNUSED, &labs, FakeMusicBrainz::default(), None);
        let first = k.similar_artists("Seed", seed, 10).await.unwrap();
        let second = k.similar_artists("Seed", seed, 10).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(cache.ttls().contains(&(
            format!("kc1:labs:similar-artists:{seed}"),
            Duration::from_secs(30 * 86_400)
        )));
    }

    #[tokio::test]
    async fn top_tracks_without_a_token_come_from_the_radio_by_listens() {
        let (api, seen) = serve(vec![
            ok(include_str!(
                "../listenbrainz/fixtures/artist_radio_wonder_who.json"
            )),
            ok(include_str!(
                "../listenbrainz/fixtures/recording_metadata_wonder_who.json"
            )),
        ]);
        let (k, cache) = knowledge(&api, UNUSED, FakeMusicBrainz::default(), None);
        let tracks = k.artist_top_tracks(WONDER_WHO, 3).await.unwrap();
        assert_eq!(
            titles(&tracks),
            [
                "Watch the Flowers Grow",
                "Lonesome Road",
                "Don't Think Twice"
            ]
        );
        assert_eq!(
            tracks[0],
            TrackRef {
                artist: ArtistRef {
                    name: "The Wonder Who".into(),
                    mbid: Some(WONDER_WHO.into())
                },
                title: "Watch the Flowers Grow".into(),
                mbid: Some("45f17c9f-41d0-42b7-82d4-f2be545aadbb".into()),
            }
        );
        let sent = seen.lock().unwrap()[1].clone();
        assert_eq!(
            (sent.method.as_str(), sent.target.as_str()),
            ("POST", "/1/metadata/recording/")
        );
        assert!(cache.ttls().contains(&(
            format!("kc1:lb:top-tracks:radio:{WONDER_WHO}"),
            Duration::from_secs(14 * 86_400)
        )));
    }

    #[tokio::test]
    async fn top_tracks_with_a_token_come_from_listenbrainzs_ranking() {
        let (api, seen) = serve(vec![ok(include_str!(
            "../listenbrainz/fixtures/top_recordings_radiohead.json"
        ))]);
        let (k, cache) = knowledge(&api, UNUSED, FakeMusicBrainz::default(), Some("t"));
        let tracks = k.artist_top_tracks(RADIOHEAD, 2).await.unwrap();
        assert_eq!(titles(&tracks), ["Karma Police", "Paranoid Android"]);
        assert_eq!(
            tracks[0].artist,
            ArtistRef {
                name: "Radiohead".into(),
                mbid: Some(RADIOHEAD.into())
            }
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].authorization.as_deref(), Some("Token t"));
        assert!(cache.ttls().contains(&(
            format!("kc1:lb:top-tracks:token:{RADIOHEAD}"),
            Duration::from_secs(14 * 86_400)
        )));
    }

    #[tokio::test]
    async fn a_refused_or_missing_token_says_so() {
        let refused = || Reply {
            status: 401,
            body: include_str!("../listenbrainz/fixtures/token_required.json").into(),
        };
        let (api, seen) = serve(vec![refused(), refused()]);
        let (with_token, _) = knowledge(&api, UNUSED, FakeMusicBrainz::default(), Some("t"));
        assert!(matches!(
            with_token.artist_top_tracks(RADIOHEAD, 3).await,
            Err(KnowledgeError::TokenRefused)
        ));
        let (without, _) = knowledge(&api, UNUSED, FakeMusicBrainz::default(), None);
        assert!(matches!(
            without.artist_top_tracks(RADIOHEAD, 3).await,
            Err(KnowledgeError::TokenRequired)
        ));
        assert!(seen.lock().unwrap()[1]
            .target
            .starts_with("/1/lb-radio/artist/"));
    }

    #[tokio::test]
    async fn tags_are_scaled_per_artist() {
        let (api, seen) = serve(vec![ok(include_str!(
            "../listenbrainz/fixtures/artist_metadata.json"
        ))]);
        let (k, cache) = knowledge(&api, UNUSED, FakeMusicBrainz::default(), None);
        let ids: Vec<String> = [TINARIWEN, "ed3f4831-e3e0-4dc0-9381-f5649e9df221", FISHMANS]
            .map(String::from)
            .into();
        let tags = k.artist_tags(&ids).await.unwrap();
        let named: Vec<(&str, f64)> = tags[TINARIWEN]
            .iter()
            .map(|t| (t.name.as_str(), t.weight))
            .collect();
        assert_eq!(named[0], ("desert blues", 1.0));
        assert_eq!(named[1], ("african", 4.0 / 7.0));
        assert_eq!(named[2], ("touareg", 4.0 / 7.0));
        assert!(tags[FISHMANS].is_empty());
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(cache.ttls().contains(&(
            format!("kc1:lb:artist-tags:{FISHMANS}"),
            Duration::from_secs(30 * 86_400)
        )));
        let again = k.artist_tags(&ids[..1]).await.unwrap();
        assert_eq!(again[TINARIWEN].len(), named.len());
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    // Built by hand: no real artist_metadata answer has a count at zero or below.
    #[test]
    fn only_tags_with_positive_votes_are_weighted() {
        let tags = |counts: &[(&str, i64)]| {
            Some(ArtistTags {
                artist: counts
                    .iter()
                    .map(|&(tag, count)| ArtistTagEntry {
                        tag: tag.into(),
                        count,
                    })
                    .collect(),
            })
        };
        let kept: Vec<(String, f64)> = scaled_tags(tags(&[("rock", 3), ("pop", 0), ("noise", -1)]))
            .into_iter()
            .map(|t| (t.name, t.weight))
            .collect();
        assert_eq!(kept, [("rock".to_string(), 1.0)]);
        assert!(scaled_tags(tags(&[("pop", 0), ("noise", -1)])).is_empty());
    }

    #[tokio::test]
    async fn popularity_reference_is_the_median_user_count_of_the_sitewide_top() {
        let (api, seen) = serve(vec![
            ok(include_str!(
                "../listenbrainz/fixtures/sitewide_artists.json"
            )),
            ok(include_str!(
                "../listenbrainz/fixtures/artist_popularity_radiohead.json"
            )),
        ]);
        let (k, cache) = knowledge(&api, UNUSED, FakeMusicBrainz::default(), None);
        assert_eq!(k.popularity_reference().await.unwrap(), 461120);
        assert_eq!(
            seen.lock().unwrap()[1].body,
            r#"{"artist_mbids":["a74b1b7f-71a5-4011-9441-d0b5e4122711","f59c5520-5f46-4d2c-b2c4-822eabf53419"]}"#
        );
        assert!(cache.ttls().contains(&(
            "kc1:lb:popularity-reference".to_string(),
            Duration::from_secs(30 * 86_400)
        )));
    }

    #[tokio::test]
    async fn popularity_reads_user_counts() {
        let (api, _) = serve(vec![ok(include_str!(
            "../listenbrainz/fixtures/artist_popularity.json"
        ))]);
        let (k, _) = knowledge(&api, UNUSED, FakeMusicBrainz::default(), None);
        let ids: Vec<String> = [
            TINARIWEN,
            FISHMANS,
            "ed3f4831-e3e0-4dc0-9381-f5649e9df221",
            "616699bf-d176-4368-a30a-1c4a6dd526db",
            "5b1b9c77-dfee-42ce-a4ae-abee4d40d407",
            "ae0b2424-d4c5-4c54-82ac-fe3be5453270",
        ]
        .map(String::from)
        .into();
        let users = k.artist_popularity(&ids).await.unwrap();
        assert_eq!(users[FISHMANS], 4836);
        assert_eq!(users.len(), 6);
    }

    #[tokio::test]
    async fn related_tags_keep_musicbrainz_genres_only() {
        let (labs, _) = serve(vec![ok(include_str!(
            "../listenbrainz/fixtures/tag_similarity_desert_blues.json"
        ))]);
        let (k, cache) = knowledge(UNUSED, &labs, FakeMusicBrainz::default(), None);
        assert_eq!(
            k.related_tags("desert blues").await.unwrap(),
            ["desert rock", "afrobeat", "griot"]
        );
        let ttls = cache.ttls();
        assert!(ttls.contains(&(
            "kc1:labs:tag-similarity:desert blues".to_string(),
            Duration::from_secs(30 * 86_400)
        )));
        assert!(ttls.contains(&(
            "kc1:mb:genres".to_string(),
            Duration::from_secs(30 * 86_400)
        )));
    }

    #[tokio::test]
    async fn tag_tracks_take_names_and_years_from_recording_metadata() {
        let (api, seen) = serve(vec![
            ok(include_str!(
                "../listenbrainz/fixtures/tag_radio_desert_blues.json"
            )),
            ok(include_str!(
                "../listenbrainz/fixtures/recording_metadata_desert_blues.json"
            )),
        ]);
        let (k, cache) = knowledge(&api, UNUSED, FakeMusicBrainz::default(), None);
        let tracks = k.tag_tracks("desert blues", 4).await.unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(
            (tracks[0].artist.name.as_str(), tracks[0].title.as_str()),
            ("Tinariwen", "Sastanàqqàm")
        );
        assert_eq!(
            tracks[2].artist,
            ArtistRef {
                name: "Vieux Farka Touré et Khruangbin".into(),
                mbid: Some("7682d624-9e0b-45b2-b7d0-d1b18b3ebd05".into())
            }
        );
        assert!(cache.ttls().contains(&(
            "kc1:lb:tag-tracks:desert blues:4".to_string(),
            Duration::from_secs(14 * 86_400)
        )));

        let ids: Vec<String> = [
            "0671db54-477b-4a41-81bd-b8cc4f9927e7",
            "b0d3e48f-4e05-4bfe-929b-4bb580c38840",
        ]
        .map(String::from)
        .into();
        let facts = k.recordings(&ids).await.unwrap();
        assert_eq!(facts[&ids[0]].first_release_year, Some(2017));
        assert_eq!(facts[&ids[1]].first_release_year, None);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_failed_lookup_is_asked_again_and_no_match_is_remembered() {
        let mb = FakeMusicBrainz::default();
        let searches = mb.searches.clone();
        mb.answers.lock().unwrap().extend([
            Err(MetadataError::RateLimited {
                service: Service::Musicbrainz,
                status: Some(503),
                raw: String::new(),
            }),
            Ok(vec![]),
        ]);
        let (k, cache) = knowledge(UNUSED, UNUSED, mb, None);
        assert!(matches!(
            k.resolve_artist("Fishmans").await,
            Err(KnowledgeError::MusicBrainz(_))
        ));
        assert_eq!(k.resolve_artist("Fishmans").await.unwrap(), None);
        assert_eq!(k.resolve_artist("Fishmans").await.unwrap(), None);
        assert_eq!(searches.load(Ordering::SeqCst), 2);
        assert_eq!(
            cache.ttls(),
            vec![(
                "kc1:mb:artist:fishmans".to_string(),
                Duration::from_secs(30 * 86_400)
            )]
        );
    }
}
