use reqwest::Client;
use serde_json::json;
use tracing::debug;

use crate::error::{Result, SoulseekError};
use crate::http::resilient_send;
use shared::recommendation::TimePeriod;

use super::models::*;

const LB_API_BASE: &str = "https://api.listenbrainz.org";

pub struct ListenBrainzClient {
    client: Client,
    username: String,
    token: Option<String>,
}

impl ListenBrainzClient {
    pub fn new(username: impl Into<String>, token: Option<String>) -> Self {
        Self {
            client: crate::http::build_client("soulful/0.1 (https://github.com/soulful)"),
            username: username.into(),
            token,
        }
    }

    fn auth_header(&self) -> Option<(String, String)> {
        self.token
            .as_ref()
            .map(|t| ("Authorization".to_string(), format!("Token {}", t)))
    }

    /// GET with retries and rate limiting. Returns None for 204 No Content.
    async fn get(&self, path: &str) -> Result<Option<reqwest::Response>> {
        let url = format!("{}{}", LB_API_BASE, path);
        debug!("ListenBrainz GET {}", url);

        crate::http::lb_rate_limit().await;
        let auth = self.auth_header();
        let resp = resilient_send(
            || {
                let mut req = self.client.get(&url);
                if let Some((ref key, ref val)) = auth {
                    req = req.header(key.as_str(), val.as_str());
                }
                req
            },
            &format!("LB GET {}", path),
        )
        .await?;

        if resp.status().as_u16() == 204 {
            return Ok(None);
        }

        Ok(Some(resp))
    }

    async fn post_json(&self, path: &str, body: serde_json::Value) -> Result<reqwest::Response> {
        let url = format!("{}{}", LB_API_BASE, path);
        debug!("ListenBrainz POST {}", url);

        crate::http::lb_rate_limit().await;
        let auth = self.auth_header();
        let body_clone = body.clone();
        resilient_send(
            || {
                let mut req = self.client.post(&url).json(&body_clone);
                if let Some((ref key, ref val)) = auth {
                    req = req.header(key.as_str(), val.as_str());
                }
                req
            },
            &format!("LB POST {}", path),
        )
        .await
    }

    async fn get_json<T: serde::de::DeserializeOwned + Default>(
        &self,
        path: &str,
        context: &str,
    ) -> Result<T> {
        match self.get(path).await? {
            Some(resp) => resp.json().await.map_err(|e| SoulseekError::Api {
                status: 500,
                message: format!("Failed to parse {} response: {}", context, e),
            }),
            None => Ok(T::default()),
        }
    }

    // --- Listens ---

    fn encoded_username(&self) -> String {
        urlencoding(&self.username)
    }

    pub async fn get_listens(&self, count: u32) -> Result<ListensResponse> {
        let path = format!(
            "/1/user/{}/listens?count={}",
            self.encoded_username(),
            count
        );
        self.get_json(&path, "listens").await
    }

    /// Fetch top recordings for an arbitrary user (not just self.username).
    /// Used by the recommendation engine for collaborative filtering.
    pub async fn get_user_top_recordings(
        &self,
        username: &str,
        period: TimePeriod,
        count: u32,
    ) -> Result<TopRecordingsResponse> {
        let range = time_period_to_range(period);
        let path = format!(
            "/1/stats/user/{}/recordings?range={}&count={}",
            urlencoding(username),
            range,
            count
        );
        self.get_json(&path, "top recordings").await
    }

    // --- Stats ---

    pub async fn get_top_artists(
        &self,
        period: TimePeriod,
        count: u32,
    ) -> Result<TopArtistsResponse> {
        let range = time_period_to_range(period);
        let path = format!(
            "/1/stats/user/{}/artists?range={}&count={}",
            self.encoded_username(),
            range,
            count
        );
        self.get_json(&path, "top artists").await
    }

    pub async fn get_top_recordings(
        &self,
        period: TimePeriod,
        count: u32,
    ) -> Result<TopRecordingsResponse> {
        let range = time_period_to_range(period);
        let path = format!(
            "/1/stats/user/{}/recordings?range={}&count={}",
            self.encoded_username(),
            range,
            count
        );
        self.get_json(&path, "top recordings").await
    }

    // --- Similar Users ---

    pub async fn get_similar_users(&self) -> Result<Vec<SimilarUser>> {
        let path = format!("/1/user/{}/similar-users", self.encoded_username());
        let raw: SimilarUsersRaw = self.get_json(&path, "similar users").await?;
        Ok(raw.into_users())
    }

    // --- Recommendation Playlists ---

    pub async fn get_recommendation_playlists(&self) -> Result<RecommendationPlaylistsResponse> {
        let path = format!(
            "/1/user/{}/playlists/recommendations",
            self.encoded_username()
        );
        self.get_json(&path, "recommendation playlists").await
    }

    // --- LB Radio: Artist ---

    pub async fn get_artist_radio(
        &self,
        mbid: &str,
        mode: &str,
        max_similar_artists: u32,
        max_recordings_per_artist: u32,
    ) -> Result<ArtistRadioResponse> {
        let path = format!(
            "/1/lb-radio/artist/{}?mode={}&max_similar_artists={}&max_recordings_per_artist={}&pop_begin=0&pop_end=100",
            mbid, mode, max_similar_artists, max_recordings_per_artist
        );
        self.get_json(&path, "artist radio").await
    }

    // --- LB Radio: Tags ---

    pub async fn get_tag_radio(
        &self,
        tag: &str,
        pop_begin: u32,
        pop_end: u32,
        count: u32,
    ) -> Result<TagRadioResponse> {
        let path = format!(
            "/1/lb-radio/tags?tag={}&pop_begin={}&pop_end={}&count={}",
            urlencoding(tag),
            pop_begin,
            pop_end,
            count
        );
        self.get_json(&path, "tag radio").await
    }

    // --- Popularity: Artist ---

    pub async fn get_artist_popularity(
        &self,
        artist_mbids: &[&str],
    ) -> Result<ArtistPopularityResponse> {
        let body = json!({
            "artist_mbids": artist_mbids,
        });
        let resp = self.post_json("/1/popularity/artist", body).await?;
        resp.json().await.map_err(|e| SoulseekError::Api {
            status: 500,
            message: format!("Failed to parse artist popularity response: {}", e),
        })
    }

    // --- Sitewide Stats ---

    pub async fn get_sitewide_artists(&self, count: u32) -> Result<SitewideArtistsResponse> {
        let path = format!("/1/stats/sitewide/artists?count={}", count);
        self.get_json(&path, "sitewide artists").await
    }

    // --- Popularity: Top Recordings for Artist ---

    pub async fn get_top_recordings_for_artist(
        &self,
        mbid: &str,
    ) -> Result<TopRecordingsForArtistResponse> {
        let path = format!("/1/popularity/top-recordings-for-artist/{}", mbid);
        self.get_json(&path, "top recordings for artist").await
    }

    // --- Metadata: Artist ---

    pub async fn get_artist_metadata(
        &self,
        artist_mbids: &[&str],
    ) -> Result<ArtistMetadataResponse> {
        let mbids_param = artist_mbids.join(",");
        let path = format!(
            "/1/metadata/artist/?artist_mbids={}&inc=tag",
            urlencoding(&mbids_param)
        );
        self.get_json(&path, "artist metadata").await
    }

    // --- MusicBrainz artist lookup (cached + rate-limited) ---

    pub async fn lookup_artist_mbid(&self, artist: &str) -> Result<Option<String>> {
        crate::http::cached_mbid_lookup(&self.client, artist).await
    }
}

fn time_period_to_range(period: TimePeriod) -> &'static str {
    match period {
        TimePeriod::Week => "this_week",
        TimePeriod::Month => "this_month",
        TimePeriod::Quarter => "quarter",
        TimePeriod::HalfYear => "half_yearly",
        TimePeriod::Year => "this_year",
        TimePeriod::AllTime => "all_time",
    }
}

/// Simple percent-encoding for URL path/query segments.
fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
