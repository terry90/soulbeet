pub mod client;
pub mod models;

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use tracing::warn;

use crate::error::Result;
use crate::traits::ScrobbleProvider;
use client::ListenBrainzClient;
use shared::recommendation::{
    ArtistPopularity, Listen, RankedArtist, RankedTrack, SimilarArtist, SimilarTrack, TimePeriod,
    WeightedTag,
};

pub struct ListenBrainzProvider {
    client: ListenBrainzClient,
    mbid_failures: AtomicUsize,
}

impl ListenBrainzProvider {
    pub fn new(username: impl Into<String>, token: Option<String>) -> Self {
        Self {
            client: ListenBrainzClient::new(username, token),
            mbid_failures: AtomicUsize::new(0),
        }
    }

    /// Access the underlying client for direct API calls (used by the engine).
    pub fn client(&self) -> &ListenBrainzClient {
        &self.client
    }

    /// Reset the MBID failure counter and return the accumulated count.
    pub fn take_mbid_failures(&self) -> usize {
        self.mbid_failures.swap(0, Ordering::Relaxed)
    }
}

#[async_trait]
impl ScrobbleProvider for ListenBrainzProvider {
    fn id(&self) -> &str {
        "listenbrainz"
    }

    fn name(&self) -> &str {
        "ListenBrainz"
    }

    async fn get_listens(&self, count: u32) -> Result<Vec<Listen>> {
        let resp = self.client.get_listens(count).await?;
        Ok(resp
            .payload
            .listens
            .into_iter()
            .map(|l| Listen {
                artist: l.track_metadata.artist_name,
                track: l.track_metadata.track_name,
                album: l.track_metadata.release_name,
                timestamp: l.listened_at,
            })
            .collect())
    }

    async fn get_top_artists(&self, period: TimePeriod, count: u32) -> Result<Vec<RankedArtist>> {
        let resp = self.client.get_top_artists(period, count).await?;
        Ok(resp
            .payload
            .artists
            .into_iter()
            .map(|a| RankedArtist {
                name: a.artist_name,
                mbid: a.artist_mbid,
                play_count: a.listen_count,
            })
            .collect())
    }

    async fn get_top_tracks(&self, period: TimePeriod, count: u32) -> Result<Vec<RankedTrack>> {
        let resp = self.client.get_top_recordings(period, count).await?;
        Ok(resp
            .payload
            .recordings
            .into_iter()
            .map(|r| RankedTrack {
                artist: r.artist_name,
                track: r.track_name,
                mbid: r.recording_mbid,
                play_count: r.listen_count,
            })
            .collect())
    }

    async fn get_artist_tags(&self, artist: &str) -> Result<Vec<WeightedTag>> {
        // We need an MBID to call the metadata endpoint
        let mbid = match self.client.lookup_artist_mbid(artist).await? {
            Some(id) => id,
            None => {
                self.mbid_failures.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "Could not resolve MBID for artist '{}', returning no tags",
                    artist
                );
                return Ok(vec![]);
            }
        };

        let resp = self.client.get_artist_metadata(&[&mbid]).await?;
        let entry = match resp.0.into_iter().next() {
            Some(e) => e,
            None => return Ok(vec![]),
        };

        let tags = match entry.tag {
            Some(t) => t.artist,
            None => return Ok(vec![]),
        };

        if tags.is_empty() {
            return Ok(vec![]);
        }

        // Normalize weights: use the count values, scale to 0.0-1.0 range
        let max_count = tags.iter().map(|t| t.count).max().unwrap_or(1).max(1) as f64;
        Ok(tags
            .into_iter()
            .map(|t| WeightedTag {
                name: t.tag,
                weight: t.count as f64 / max_count,
            })
            .collect())
    }

    async fn get_artist_popularity(&self, artist: &str) -> Result<ArtistPopularity> {
        let mbid = match self.client.lookup_artist_mbid(artist).await? {
            Some(id) => id,
            None => {
                self.mbid_failures.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "Could not resolve MBID for artist '{}', returning default popularity",
                    artist
                );
                return Ok(ArtistPopularity {
                    listener_count: 0,
                    play_count: 0,
                });
            }
        };

        let resp = self.client.get_artist_popularity(&[&mbid]).await?;
        match resp.0.into_iter().next() {
            Some(entry) => Ok(ArtistPopularity {
                listener_count: entry.total_user_count,
                play_count: entry.total_listen_count,
            }),
            None => Ok(ArtistPopularity {
                listener_count: 0,
                play_count: 0,
            }),
        }
    }

    async fn get_global_popularity_median(&self) -> Result<u64> {
        // Fetch sitewide top artists and use the artist near the middle as a
        // realistic median reference. The old approach (top artist / 2) was
        // orders of magnitude too high, making every user look maximally obscure.
        let resp = self.client.get_sitewide_artists(200).await?;
        let artists = &resp.payload.artists;
        if artists.is_empty() {
            return Ok(0);
        }
        let mid = artists.len() / 2;
        Ok(artists[mid].listen_count)
    }

    async fn get_similar_artists(&self, artist: &str, limit: u32) -> Result<Vec<SimilarArtist>> {
        let mbid = match self.client.lookup_artist_mbid(artist).await? {
            Some(id) => id,
            None => {
                self.mbid_failures.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "Could not resolve MBID for artist '{}', returning no similar artists",
                    artist
                );
                return Ok(vec![]);
            }
        };

        // Use lb-radio/artist in "easy" mode as a similarity proxy.
        // Response is { artist_mbid: [ { similar_artist_name, similar_artist_mbid, ... } ] }
        let resp = self
            .client
            .get_artist_radio(&mbid, "easy", limit, 2)
            .await?;

        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        for (_artist_mbid, recordings) in resp {
            for rec in recordings {
                if let Some(ref name) = rec.similar_artist_name {
                    if seen.insert(name.to_lowercase()) {
                        let score = 1.0 - (result.len() as f64 / (limit as f64 * 2.0).max(1.0));
                        result.push(SimilarArtist {
                            name: name.clone(),
                            mbid: rec.similar_artist_mbid.clone(),
                            score: score.max(0.1),
                        });
                        if result.len() >= limit as usize {
                            return Ok(result);
                        }
                    }
                }
            }
        }
        Ok(result)
    }

    async fn get_similar_tracks(
        &self,
        artist: &str,
        _track: &str,
        limit: u32,
    ) -> Result<Vec<SimilarTrack>> {
        // ListenBrainz doesn't have a track.getSimilar endpoint.
        // Use lb-radio/artist for the track's artist as a proxy.
        let mbid = match self.client.lookup_artist_mbid(artist).await? {
            Some(id) => id,
            None => {
                self.mbid_failures.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "Could not resolve MBID for artist '{}', returning no similar tracks",
                    artist
                );
                return Ok(vec![]);
            }
        };

        let resp = self
            .client
            .get_artist_radio(&mbid, "easy", 3, limit)
            .await?;

        let http_client = crate::http::build_client("soulful/0.1 (https://github.com/soulful)");
        let mut result = Vec::new();
        for (_artist_mbid, recordings) in resp {
            for rec in recordings {
                if result.len() >= limit as usize {
                    return Ok(result);
                }
                let score = 1.0 - (result.len() as f64 / (limit as f64).max(1.0)).min(0.9);

                // Resolve recording MBID to actual track name
                let (track_name, track_mbid) = match &rec.recording_mbid {
                    Some(rid) if !rid.is_empty() => {
                        match crate::http::cached_recording_lookup(&http_client, rid).await {
                            Ok(Some(info)) if !info.title.is_empty() => {
                                (info.title, Some(rid.clone()))
                            }
                            _ => continue,
                        }
                    }
                    _ => continue,
                };

                result.push(SimilarTrack {
                    artist: rec.similar_artist_name.unwrap_or_default(),
                    track: track_name,
                    mbid: track_mbid,
                    score,
                });
            }
        }
        Ok(result)
    }

    async fn get_tag_top_tracks(&self, tag: &str, limit: u32) -> Result<Vec<RankedTrack>> {
        let resp = self.client.get_tag_radio(tag, 0, 100, limit).await?;

        let http_client = crate::http::build_client("soulful/0.1 (https://github.com/soulful)");
        let mut tracks = Vec::new();

        for rec in resp {
            if tracks.len() >= limit as usize {
                break;
            }
            let mbid = match rec.recording_mbid {
                Some(ref id) if !id.is_empty() => id,
                _ => continue,
            };
            match crate::http::cached_recording_lookup(&http_client, mbid).await {
                Ok(Some(info)) if !info.artist.is_empty() && !info.title.is_empty() => {
                    tracks.push(RankedTrack {
                        artist: info.artist,
                        track: info.title,
                        mbid: Some(mbid.clone()),
                        play_count: rec.percent as u64,
                    });
                }
                _ => continue,
            }
        }

        Ok(tracks)
    }

    async fn get_related_tags(&self, _tag: &str) -> Result<Vec<String>> {
        // ListenBrainz doesn't have a tag.getSimilar endpoint.
        // Related tags are derived from artist metadata in the profile builder.
        Ok(vec![])
    }

    async fn get_artist_top_tracks(&self, artist: &str, limit: u32) -> Result<Vec<RankedTrack>> {
        let mbid = match self.client.lookup_artist_mbid(artist).await? {
            Some(id) => id,
            None => {
                self.mbid_failures.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "Could not resolve MBID for artist '{}', returning no top tracks",
                    artist
                );
                return Ok(vec![]);
            }
        };

        let resp = self.client.get_top_recordings_for_artist(&mbid).await?;

        Ok(resp
            .0
            .into_iter()
            .take(limit as usize)
            .map(|r| RankedTrack {
                artist: r.artist_name,
                track: r.recording_name.unwrap_or_default(),
                mbid: r.recording_mbid,
                play_count: r.total_listen_count,
            })
            .collect())
    }
}
