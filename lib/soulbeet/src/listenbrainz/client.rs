use std::collections::HashMap;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use shared::recommendation::TimePeriod;

use crate::polite::{Pace, PoliteClient, RemoteError};

use super::models::*;

/// The ListenBrainz API. Most reads need no token; the token is sent on
/// every request when the user saved one.
pub struct ListenBrainzClient {
    http: PoliteClient,
}

impl ListenBrainzClient {
    pub fn new(base_url: &str, token: Option<String>, pace: Pace) -> Result<Self, RemoteError> {
        let http = PoliteClient::new(base_url, "ListenBrainz", pace)?.with_token(token);
        Ok(Self { http })
    }

    pub fn has_token(&self) -> bool {
        self.http.has_token()
    }

    /// False only when ListenBrainz answers that it does not know the saved token. Asked with
    /// one saved: without, ListenBrainz answers 400.
    pub async fn token_is_valid(&self) -> Result<bool, RemoteError> {
        let answer: Option<TokenValidation> = self.http.get("/1/validate-token").await?;
        Ok(answer.is_none_or(|a| a.valid))
    }

    /// 204 means ListenBrainz has not computed the user's statistics yet.
    pub async fn user_top_artists(
        &self,
        user: &str,
        period: TimePeriod,
        count: usize,
    ) -> Result<Vec<LbArtist>, RemoteError> {
        let path = format!(
            "/1/stats/user/{}/artists?range={}&count={count}",
            path_segment(user),
            range(period)
        );
        let page: Option<Payload<ArtistsPayload<LbArtist>>> = self.http.get(&path).await?;
        Ok(page.map(|p| p.payload.artists).unwrap_or_default())
    }

    pub async fn user_top_recordings(
        &self,
        user: &str,
        period: TimePeriod,
        count: usize,
    ) -> Result<Vec<LbRecording>, RemoteError> {
        let path = format!(
            "/1/stats/user/{}/recordings?range={}&count={count}",
            path_segment(user),
            range(period)
        );
        let page: Option<Payload<RecordingsPayload>> = self.http.get(&path).await?;
        Ok(page.map(|p| p.payload.recordings).unwrap_or_default())
    }

    /// ListenBrainz serves at most 1000 listens per request.
    pub async fn user_listens(
        &self,
        user: &str,
        count: usize,
    ) -> Result<Vec<LbListen>, RemoteError> {
        let path = format!(
            "/1/user/{}/listens?count={}",
            path_segment(user),
            count.min(1000)
        );
        let page: Option<Payload<ListensPayload>> = self.http.get(&path).await?;
        Ok(page.map(|p| p.payload.listens).unwrap_or_default())
    }

    pub async fn user_loved(&self, user: &str) -> Result<Vec<LbFeedback>, RemoteError> {
        let path = format!(
            "/1/feedback/user/{}/get-feedback?score=1&metadata=true&count=1000",
            path_segment(user)
        );
        let page: Option<FeedbackPage> = self.http.get(&path).await?;
        Ok(page.map(|p| p.feedback).unwrap_or_default())
    }

    pub async fn similar_users(&self, user: &str) -> Result<Vec<SimilarUser>, RemoteError> {
        let path = format!("/1/user/{}/similar-users", path_segment(user));
        let page: Option<Payload<Vec<SimilarUser>>> = self.http.get(&path).await?;
        Ok(page.map(|p| p.payload).unwrap_or_default())
    }

    /// The listing carries no tracks; `playlist` reads them.
    pub async fn recommendation_playlists(
        &self,
        user: &str,
    ) -> Result<Vec<PlaylistSummary>, RemoteError> {
        let path = format!("/1/user/{}/playlists/recommendations", path_segment(user));
        let listing: Option<PlaylistListing> = self.http.get(&path).await?;
        Ok(listing
            .map(|l| l.playlists)
            .unwrap_or_default()
            .into_iter()
            .map(|p| PlaylistSummary {
                mbid: last_segment(&p.playlist.identifier).to_string(),
                source_patch: p.playlist.source_patch().map(str::to_string),
                date: p.playlist.date,
            })
            .collect())
    }

    /// Searches public ListenBrainz community playlists by title/description (`GET /1/playlist/search`).
    /// Works without a token; ListenBrainz requires queries of at least 3 characters.
    pub async fn search_playlists(
        &self,
        query: &str,
        count: usize,
    ) -> Result<Vec<PlaylistSearchHit>, RemoteError> {
        let trimmed = query.trim();
        if trimmed.chars().count() < 3 || count == 0 {
            return Ok(Vec::new());
        }
        let path = format!(
            "/1/playlist/search?query={}&count={count}",
            encode(trimmed)
        );
        let listing: Option<PlaylistListing> = self.http.get(&path).await?;
        Ok(listing
            .map(|l| l.playlists)
            .unwrap_or_default()
            .into_iter()
            .map(|p| PlaylistSearchHit {
                mbid: last_segment(&p.playlist.identifier).to_string(),
                title: p.playlist.title,
                creator: p.playlist.creator,
            })
            .collect())
    }

    /// A track missing its title or artist name is left out, since a
    /// candidate needs both.
    pub async fn playlist(&self, playlist_mbid: &str) -> Result<Vec<PlaylistTrack>, RemoteError> {
        let path = format!("/1/playlist/{playlist_mbid}");
        let page: Option<PlaylistEnvelope<JspfPlaylist>> = self.http.get(&path).await?;
        Ok(page
            .map(|p| p.playlist.track)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|t| {
                Some(PlaylistTrack {
                    recording_mbid: t.identifier.first().map(|u| last_segment(u).to_string()),
                    artist_mbid: t
                        .extension
                        .as_ref()
                        .and_then(|ext| ext.musicbrainz.as_ref())
                        .and_then(|mb| mb.artist_identifiers.first())
                        .map(|u| last_segment(u).to_string()),
                    title: t.title?,
                    creator: t.creator?,
                })
            })
            .collect())
    }

    /// Reads a JSPF playlist with its title, annotation, and full track metadata
    /// (including album, duration, and recording MBID).
    pub async fn playlist_full(
        &self,
        playlist_mbid: &str,
    ) -> Result<Option<FullPlaylist>, RemoteError> {
        let path = format!("/1/playlist/{playlist_mbid}");
        let Some(page) = self
            .http
            .get::<PlaylistEnvelope<JspfPlaylist>>(&path)
            .await?
        else {
            return Ok(None);
        };
        let p = page.playlist;
        let title = p
            .title
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let description = p
            .annotation
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let tracks = p
            .track
            .into_iter()
            .filter_map(|t| {
                let title = t.title?.trim().to_string();
                let creator = t.creator?.trim().to_string();
                if title.is_empty() || creator.is_empty() {
                    return None;
                }
                let album = t
                    .album
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let duration_s = t
                    .duration
                    .filter(|&ms| ms > 0)
                    .and_then(|ms| u32::try_from(ms / 1000).ok())
                    .filter(|&s| s > 0);
                let recording_mbid = t
                    .identifier
                    .first()
                    .map(|u| last_segment(u).trim().to_string())
                    .filter(|id| !id.is_empty());
                let artist_mbid = t
                    .extension
                    .as_ref()
                    .and_then(|ext| ext.musicbrainz.as_ref())
                    .and_then(|mb| mb.artist_identifiers.first())
                    .map(|u| last_segment(u).trim().to_string())
                    .filter(|id| !id.is_empty());
                Some(FullPlaylistTrack {
                    title,
                    creator,
                    album,
                    duration_s,
                    recording_mbid,
                    artist_mbid,
                })
            })
            .collect();
        Ok(Some(FullPlaylist {
            title,
            description,
            tracks,
        }))
    }

    /// Every recording of the artist alone, at any popularity.
    pub async fn artist_radio(
        &self,
        artist_mbid: &str,
    ) -> Result<ArtistRadioResponse, RemoteError> {
        let path = format!(
            "/1/lb-radio/artist/{artist_mbid}?mode=easy&max_similar_artists=0\
             &max_recordings_per_artist=1000&pop_begin=0&pop_end=100"
        );
        Ok(self.http.get(&path).await?.unwrap_or_default())
    }

    pub async fn tag_radio(
        &self,
        tag: &str,
        count: usize,
    ) -> Result<Vec<TagRadioRecording>, RemoteError> {
        let path = format!(
            "/1/lb-radio/tags?tag={}&pop_begin=0&pop_end=100&count={count}",
            encode(tag)
        );
        Ok(self.http.get(&path).await?.unwrap_or_default())
    }

    /// Multi-tag LB Radio query (`GET /1/lb-radio/tags?tag=a&tag=b&operator=AND&pop_begin=...`).
    pub async fn multi_tag_radio(
        &self,
        tags: &[&str],
        operator: &str,
        pop_begin: u8,
        pop_end: u8,
        count: usize,
    ) -> Result<Vec<TagRadioRecording>, RemoteError> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let mut query: Vec<String> = tags
            .iter()
            .map(|t| format!("tag={}", encode(t)))
            .collect();
        query.push(format!("operator={}", encode(operator)));
        query.push(format!("pop_begin={}", pop_begin.min(100)));
        query.push(format!("pop_end={}", pop_end.min(100)));
        query.push(format!("count={count}"));
        let path = format!("/1/lb-radio/tags?{}", query.join("&"));
        Ok(self.http.get(&path).await?.unwrap_or_default())
    }

    pub async fn artist_popularity(
        &self,
        artist_mbids: &[String],
    ) -> Result<Vec<ArtistPopularityEntry>, RemoteError> {
        let body = ArtistMbids { artist_mbids };
        Ok(self
            .http
            .post("/1/popularity/artist", &body)
            .await?
            .unwrap_or_default())
    }

    /// Recording-level listen/user counts (`POST /1/popularity/recording`), batched 100 at a time.
    /// Works without a token.
    pub async fn recording_popularity(
        &self,
        recording_mbids: &[String],
    ) -> Result<Vec<RecordingPopularityEntry>, RemoteError> {
        let mut all = Vec::new();
        for chunk in recording_mbids.chunks(100) {
            let body = RecordingMbids {
                recording_mbids: chunk,
            };
            let entries: Vec<RecordingPopularityEntry> = self
                .http
                .post("/1/popularity/recording", &body)
                .await?
                .unwrap_or_default();
            all.extend(entries);
        }
        Ok(all)
    }

    pub async fn sitewide_artists(&self, count: usize) -> Result<Vec<SitewideArtist>, RemoteError> {
        let path = format!("/1/stats/sitewide/artists?range=all_time&count={count}");
        let page: Option<Payload<ArtistsPayload<SitewideArtist>>> = self.http.get(&path).await?;
        Ok(page.map(|p| p.payload.artists).unwrap_or_default())
    }

    /// Answers 401 without a token.
    pub async fn top_recordings_for_artist(
        &self,
        artist_mbid: &str,
    ) -> Result<Vec<TopRecordingForArtist>, RemoteError> {
        let path = format!("/1/popularity/top-recordings-for-artist/{artist_mbid}");
        Ok(self.http.get(&path).await?.unwrap_or_default())
    }

    /// One request for every artist, the MBIDs comma separated.
    pub async fn artist_metadata(
        &self,
        artist_mbids: &[String],
    ) -> Result<Vec<ArtistMetadataEntry>, RemoteError> {
        let path = format!(
            "/1/metadata/artist/?artist_mbids={}&inc=tag",
            artist_mbids.join(",")
        );
        Ok(self.http.get(&path).await?.unwrap_or_default())
    }

    /// Keyed by recording MBID; recordings ListenBrainz does not know are absent.
    pub async fn recording_metadata(
        &self,
        recording_mbids: &[String],
    ) -> Result<HashMap<String, RecordingMetadata>, RemoteError> {
        let body = RecordingMetadataQuery {
            recording_mbids,
            inc: "artist",
        };
        Ok(self
            .http
            .post("/1/metadata/recording/", &body)
            .await?
            .unwrap_or_default())
    }
}

fn range(period: TimePeriod) -> &'static str {
    match period {
        TimePeriod::Week => "this_week",
        TimePeriod::Month => "this_month",
        TimePeriod::Quarter => "quarter",
        TimePeriod::HalfYear => "half_yearly",
        TimePeriod::Year => "this_year",
        TimePeriod::AllTime => "all_time",
    }
}

/// The MBID at the end of a MusicBrainz or ListenBrainz URL.
fn last_segment(url: &str) -> &str {
    url.rsplit_once('/').map_or(url, |(_, id)| id)
}

/// Form encoding for tags in a query string.
pub(super) fn encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'~');

/// User names sit in a path, which ListenBrainz only percent-decodes, so a
/// space must be %20 and never '+'.
fn path_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{serve, Reply};

    fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn user_top_artists_read_the_stats_endpoint() {
        let (base, seen) = serve(vec![ok(include_str!("fixtures/user_top_artists_rob.json"))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let artists = lb
            .user_top_artists("rob", TimePeriod::Month, 3)
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/1/stats/user/rob/artists?range=this_month&count=3"
        );
        assert_eq!(
            (artists[0].artist_name.as_str(), artists[0].listen_count),
            ("Wax Tailor", 4298)
        );
        assert_eq!(
            artists[0].artist_mbid.as_deref(),
            Some("c24b47a1-2f52-44f3-a3c6-374dca844731")
        );
        assert_eq!(seen.lock().unwrap()[0].authorization, None);
    }

    #[tokio::test]
    async fn stats_not_computed_yet_read_as_none() {
        let (base, _) = serve(vec![Reply {
            status: 204,
            body: String::new(),
        }]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let artists = lb
            .user_top_artists("rob", TimePeriod::AllTime, 3)
            .await
            .unwrap();
        assert!(artists.is_empty());
    }

    #[tokio::test]
    async fn loved_tracks_come_with_their_metadata() {
        let (base, seen) = serve(vec![ok(include_str!("fixtures/loved_rob.json"))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let loved = lb.user_loved("rob").await.unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/1/feedback/user/rob/get-feedback?score=1&metadata=true&count=1000"
        );
        assert_eq!(loved.len(), 2);
        assert_eq!(
            loved[0].recording_mbid.as_deref(),
            Some("40313583-d07e-4ed3-a2db-1a6d3f63906e")
        );
        let track = loved[0].track_metadata.as_ref().unwrap();
        assert_eq!(track.artist_name, "Kaskade & Punctual feat. Poppy Baskcomb");
        let mapping = track.mbid_mapping.as_ref().unwrap();
        assert_eq!(
            mapping.artist_mbids[0],
            "29ed4a49-fb99-4a5c-8713-609cabe6f34a"
        );
        assert_eq!(
            mapping.recording_mbid.as_deref(),
            Some("40313583-d07e-4ed3-a2db-1a6d3f63906e")
        );
    }

    #[tokio::test]
    async fn the_artist_radio_asks_for_the_artist_alone() {
        let (base, seen) = serve(vec![ok(include_str!(
            "fixtures/artist_radio_wonder_who.json"
        ))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let radio = lb
            .artist_radio("75874408-e5a8-4bdd-94cf-3dfc8ce1acdc")
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/1/lb-radio/artist/75874408-e5a8-4bdd-94cf-3dfc8ce1acdc?mode=easy&max_similar_artists=0&max_recordings_per_artist=1000&pop_begin=0&pop_end=100"
        );
        let recordings = &radio["75874408-e5a8-4bdd-94cf-3dfc8ce1acdc"];
        assert_eq!(recordings.len(), 5);
        let popular = recordings
            .iter()
            .find(|r| r.total_listen_count == 904)
            .unwrap();
        assert_eq!(
            popular.recording_mbid,
            "45f17c9f-41d0-42b7-82d4-f2be545aadbb"
        );
    }

    #[tokio::test]
    async fn recording_metadata_is_posted_and_keyed_by_recording() {
        let (base, seen) = serve(vec![ok(include_str!(
            "fixtures/recording_metadata_wonder_who.json"
        ))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let ids = vec!["45f17c9f-41d0-42b7-82d4-f2be545aadbb".to_string()];
        let map = lb.recording_metadata(&ids).await.unwrap();
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            (sent.method.as_str(), sent.target.as_str()),
            ("POST", "/1/metadata/recording/")
        );
        assert_eq!(
            sent.body,
            r#"{"recording_mbids":["45f17c9f-41d0-42b7-82d4-f2be545aadbb"],"inc":"artist"}"#
        );
        let flowers = &map["45f17c9f-41d0-42b7-82d4-f2be545aadbb"];
        assert_eq!(flowers.recording.name, "Watch the Flowers Grow");
        assert_eq!(
            flowers.recording.first_release_date.as_deref(),
            Some("2012-03-27")
        );
        assert_eq!(flowers.artist.name, "The Wonder Who");
        assert_eq!(
            flowers.artist.artists[0].artist_mbid,
            "75874408-e5a8-4bdd-94cf-3dfc8ce1acdc"
        );
        let lonesome = &map["7f3593da-7abc-4ae1-943e-cda2e4a00771"];
        assert_eq!(lonesome.recording.first_release_date, None);
    }

    #[tokio::test]
    async fn artist_metadata_names_every_artist_in_one_request() {
        let (base, seen) = serve(vec![ok(include_str!("fixtures/artist_metadata.json"))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let ids = vec![
            "5c98fc12-be83-4246-ae5b-2184192913b9".to_string(),
            "ed3f4831-e3e0-4dc0-9381-f5649e9df221".to_string(),
        ];
        let entries = lb.artist_metadata(&ids).await.unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/1/metadata/artist/?artist_mbids=5c98fc12-be83-4246-ae5b-2184192913b9,ed3f4831-e3e0-4dc0-9381-f5649e9df221&inc=tag"
        );
        let tinariwen = entries
            .iter()
            .find(|e| e.artist_mbid == "5c98fc12-be83-4246-ae5b-2184192913b9")
            .unwrap();
        let tags = &tinariwen.tag.as_ref().unwrap().artist;
        assert!(tags
            .iter()
            .any(|t| (t.tag.as_str(), t.count) == ("desert blues", 7)));
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn sitewide_artists_keep_an_artist_without_an_mbid() {
        let (base, seen) = serve(vec![ok(include_str!("fixtures/sitewide_artists.json"))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let artists = lb.sitewide_artists(200).await.unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/1/stats/sitewide/artists?range=all_time&count=200"
        );
        assert_eq!(artists.len(), 3);
        assert_eq!(
            artists[0].artist_mbid.as_deref(),
            Some("a74b1b7f-71a5-4011-9441-d0b5e4122711")
        );
        assert_eq!(
            (
                artists[2].artist_name.as_str(),
                artists[2].artist_mbid.as_deref()
            ),
            ("The Beatles", None)
        );
    }

    #[tokio::test]
    async fn popularity_is_posted_by_artist() {
        let (base, seen) = serve(vec![ok(include_str!("fixtures/artist_popularity.json"))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let ids: Vec<String> = [
            "5c98fc12-be83-4246-ae5b-2184192913b9",
            "d0f25f16-7782-42e8-8047-c16a1cb2b450",
            "ed3f4831-e3e0-4dc0-9381-f5649e9df221",
            "616699bf-d176-4368-a30a-1c4a6dd526db",
            "5b1b9c77-dfee-42ce-a4ae-abee4d40d407",
            "ae0b2424-d4c5-4c54-82ac-fe3be5453270",
        ]
        .map(String::from)
        .into();
        let entries = lb.artist_popularity(&ids).await.unwrap();
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            (sent.method.as_str(), sent.target.as_str()),
            ("POST", "/1/popularity/artist")
        );
        assert_eq!(
            sent.body,
            format!(r#"{{"artist_mbids":["{}"]}}"#, ids.join(r#"",""#))
        );
        let fishmans = entries
            .iter()
            .find(|e| e.artist_mbid == "d0f25f16-7782-42e8-8047-c16a1cb2b450")
            .unwrap();
        assert_eq!(fishmans.total_user_count, Some(4836));
        assert_eq!(entries.len(), 6);
    }

    #[tokio::test]
    async fn top_recordings_need_the_token() {
        let (base, seen) = serve(vec![
            ok(include_str!("fixtures/top_recordings_radiohead.json")),
            Reply {
                status: 401,
                body: include_str!("fixtures/token_required.json").into(),
            },
        ]);
        let with_token =
            ListenBrainzClient::new(&base, Some("t".into()), Pace::immediate()).unwrap();
        assert!(with_token.has_token());
        let top = with_token
            .top_recordings_for_artist("a74b1b7f-71a5-4011-9441-d0b5e4122711")
            .await
            .unwrap();
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            sent.target,
            "/1/popularity/top-recordings-for-artist/a74b1b7f-71a5-4011-9441-d0b5e4122711"
        );
        assert_eq!(sent.authorization.as_deref(), Some("Token t"));
        assert_eq!(top.len(), 3);
        assert_eq!(
            (top[0].recording_name.as_str(), top[0].total_listen_count),
            ("Karma Police", 3890683)
        );
        assert_eq!(top[0].artist_name, "Radiohead");
        assert_eq!(
            top[0].artist_mbids,
            vec!["a74b1b7f-71a5-4011-9441-d0b5e4122711".to_string()]
        );

        let without = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        assert!(!without.has_token());
        let refused = without
            .top_recordings_for_artist("a74b1b7f-71a5-4011-9441-d0b5e4122711")
            .await;
        assert!(matches!(
            refused,
            Err(RemoteError::Status {
                service: "ListenBrainz",
                status: 401,
                ..
            })
        ));
        assert_eq!(seen.lock().unwrap()[1].authorization, None);
    }

    #[tokio::test]
    async fn the_saved_token_is_checked_with_validate_token() {
        // A known token's answer, as api.py's validate_token documents it.
        let valid = r#"{"code":200,"message":"Token valid.","valid":true,"user_name":"rob"}"#;
        let (base, seen) = serve(vec![
            ok(valid),
            ok(include_str!("fixtures/token_invalid.json")),
        ]);
        let lb = ListenBrainzClient::new(&base, Some("t".into()), Pace::immediate()).unwrap();
        assert!(lb.token_is_valid().await.unwrap());
        assert!(!lb.token_is_valid().await.unwrap());
        let sent = seen.lock().unwrap()[0].clone();
        assert_eq!(
            (sent.target.as_str(), sent.authorization.as_deref()),
            ("/1/validate-token", Some("Token t"))
        );
    }

    #[tokio::test]
    async fn tag_radio_and_user_reads_parse_their_captures() {
        let (base, seen) = serve(vec![
            ok(include_str!("fixtures/tag_radio_desert_blues.json")),
            ok(include_str!("fixtures/user_top_recordings_rob.json")),
            ok(include_str!("fixtures/listens_rob.json")),
        ]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();

        let radio = lb.tag_radio("desert blues", 20).await.unwrap();
        assert_eq!(radio.len(), 4);
        assert_eq!(
            (radio[0].recording_mbid.as_str(), radio[0].percent),
            ("0671db54-477b-4a41-81bd-b8cc4f9927e7", 100.0)
        );

        let (mbase, mseen) = serve(vec![ok(include_str!("fixtures/tag_radio_desert_blues.json"))]);
        let mlb = ListenBrainzClient::new(&mbase, None, Pace::immediate()).unwrap();
        let multi = mlb
            .multi_tag_radio(&["christmas", "french"], "AND", 65, 100, 15)
            .await
            .unwrap();
        assert_eq!(multi.len(), 4);
        assert_eq!(
            mseen.lock().unwrap()[0].target,
            "/1/lb-radio/tags?tag=christmas&tag=french&operator=AND&pop_begin=65&pop_end=100&count=15"
        );

        let top = lb
            .user_top_recordings("rob", TimePeriod::AllTime, 2)
            .await
            .unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(
            (
                top[0].artist_name.as_str(),
                top[0].track_name.as_str(),
                top[0].listen_count
            ),
            ("TM Juke", "Just for a Day (Sunday)", 243)
        );
        assert_eq!(
            top[0].recording_mbid.as_deref(),
            Some("586a6fc2-804e-4e60-a115-217d8ae9e90d")
        );
        assert_eq!(
            top[0].artist_mbids,
            vec!["3fa17b15-b8d1-46c4-a0e3-6e0f2a064a1d".to_string()]
        );

        let listens = lb.user_listens("rob", 2).await.unwrap();
        assert_eq!(listens.len(), 2);
        assert_eq!(listens[0].listened_at, 1771414109);
        assert_eq!(
            (
                listens[0].track_metadata.artist_name.as_str(),
                listens[0].track_metadata.track_name.as_str(),
                listens[0].track_metadata.release_name.as_deref()
            ),
            ("Röyksopp", "Some Resolve", Some("Profound Mysteries II"))
        );

        let targets: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.target.clone())
            .collect();
        assert_eq!(
            targets,
            [
                "/1/lb-radio/tags?tag=desert+blues&pop_begin=0&pop_end=100&count=20",
                "/1/stats/user/rob/recordings?range=all_time&count=2",
                "/1/user/rob/listens?count=2",
            ]
        );
    }

    #[tokio::test]
    async fn similar_users_read_their_payload() {
        let (base, seen) = serve(vec![ok(include_str!("fixtures/similar_users_rob.json"))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let users = lb.similar_users("rob").await.unwrap();
        assert_eq!(seen.lock().unwrap()[0].target, "/1/user/rob/similar-users");
        assert_eq!(users.len(), 2);
        assert_eq!(
            (users[0].user_name.as_str(), users[0].similarity),
            ("Violet&Red", 0.5391550135659047)
        );
    }

    #[tokio::test]
    async fn recommendation_playlists_are_named_by_their_mbid() {
        let (base, seen) = serve(vec![ok(include_str!(
            "fixtures/recommendation_playlists_rob.json"
        ))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let playlists = lb.recommendation_playlists("rob").await.unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/1/user/rob/playlists/recommendations"
        );
        assert_eq!(
            playlists,
            [
                summary(
                    "48f2eb97-33e7-496c-8da1-25ead7280a81",
                    "daily-jams",
                    "2026-11-18T23:03:34.548019+00:00"
                ),
                summary(
                    "55f2a01f-3974-46ca-8461-a8a84886eba4",
                    "weekly-exploration",
                    "2026-08-23T22:13:00.700111+00:00"
                ),
                summary(
                    "54cc305b-f68e-4ab8-a2b9-3188727a3b5b",
                    "weekly-jams",
                    "2026-08-23T22:07:21.126678+00:00"
                ),
                summary(
                    "865982ba-7d21-48f5-81ce-893a3f341faa",
                    "weekly-exploration",
                    "2026-07-28T12:24:12.184152+00:00"
                ),
                summary(
                    "3c8f40f3-0e59-4275-8b86-8f3d3588b4d8",
                    "top-discoveries-of-2025",
                    "2026-02-22T10:42:29.813561+00:00"
                ),
                summary(
                    "b0a1c836-9635-4080-a676-a48a9b74b853",
                    "top-missed-recordings-of-2025",
                    "2026-02-22T10:26:09.147289+00:00"
                ),
                summary(
                    "e3f1ab53-3ac4-4e9f-a74d-42bddbd4a2f1",
                    "top-missed-recordings-of-2024",
                    "2026-02-22T08:34:02.254698+00:00"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_playlist_without_extension_metadata_has_no_source_patch() {
        let body = r#"{"playlists":[{"playlist":{
            "identifier":"https://listenbrainz.org/playlist/48f2eb97-33e7-496c-8da1-25ead7280a81",
            "date":"2026-11-18T23:03:34.548019+00:00","title":"Untagged"}}]}"#;
        let (base, _) = serve(vec![ok(body)]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let playlists = lb.recommendation_playlists("rob").await.unwrap();
        assert_eq!(
            playlists,
            [PlaylistSummary {
                mbid: "48f2eb97-33e7-496c-8da1-25ead7280a81".into(),
                source_patch: None,
                date: "2026-11-18T23:03:34.548019+00:00".into(),
            }]
        );
    }

    fn summary(mbid: &str, source_patch: &str, date: &str) -> PlaylistSummary {
        PlaylistSummary {
            mbid: mbid.into(),
            source_patch: Some(source_patch.into()),
            date: date.into(),
        }
    }

    #[tokio::test]
    async fn a_playlist_reads_its_tracks_and_their_mbids() {
        let (base, seen) = serve(vec![ok(include_str!("fixtures/playlist.json"))]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let tracks = lb
            .playlist("48f2eb97-33e7-496c-8da1-25ead7280a81")
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/1/playlist/48f2eb97-33e7-496c-8da1-25ead7280a81"
        );
        assert_eq!(tracks.len(), 3);
        assert_eq!(
            tracks[0],
            PlaylistTrack {
                title: "Fauna".into(),
                creator: "Carbon Based Lifeforms".into(),
                recording_mbid: Some("22c7fa7f-2404-4103-8267-6a98b4b1f8ee".into()),
                artist_mbid: Some("8229a8f1-b315-4fae-af57-b3eb71efdaf4".into()),
            }
        );
    }

    /// ListenBrainz writes `creator`, `title` and `artist_identifiers` only
    /// when it has them, so one bare recording must not cost the playlist.
    #[tokio::test]
    async fn a_track_without_names_is_left_out_and_one_without_artists_kept() {
        let body = r#"{"playlist":{"track":[
            {"identifier":["https://musicbrainz.org/recording/22c7fa7f-2404-4103-8267-6a98b4b1f8ee"],
             "extension":{"https://musicbrainz.org/doc/jspf#track":{"added_by":"troi-bot"}}},
            {"identifier":["https://musicbrainz.org/recording/418bce6c-1195-4cf2-b5c0-4cf163acbd46"],
             "creator":"Bicep","title":"Lido",
             "extension":{"https://musicbrainz.org/doc/jspf#track":{"added_by":"troi-bot"}}},
            {"identifier":["https://musicbrainz.org/recording/f1ff69d8-0ee1-4978-a194-2725659ea543"],
             "creator":"Rival Consoles","title":"Memory Arc",
             "extension":{"https://musicbrainz.org/doc/jspf#track":{"added_by":"troi-bot",
               "artist_identifiers":["https://musicbrainz.org/artist/803f2853-7e3e-4dff-96a7-9f77b5f56de5"]}}}
        ]}}"#;
        let (base, _) = serve(vec![ok(body)]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        let tracks = lb
            .playlist("48f2eb97-33e7-496c-8da1-25ead7280a81")
            .await
            .unwrap();
        assert_eq!(
            tracks,
            [
                PlaylistTrack {
                    title: "Lido".into(),
                    creator: "Bicep".into(),
                    recording_mbid: Some("418bce6c-1195-4cf2-b5c0-4cf163acbd46".into()),
                    artist_mbid: None,
                },
                PlaylistTrack {
                    title: "Memory Arc".into(),
                    creator: "Rival Consoles".into(),
                    recording_mbid: Some("f1ff69d8-0ee1-4978-a194-2725659ea543".into()),
                    artist_mbid: Some("803f2853-7e3e-4dff-96a7-9f77b5f56de5".into()),
                },
            ]
        );
    }

    #[tokio::test]
    async fn listens_are_asked_at_most_a_thousand_at_a_time_and_names_are_encoded() {
        let (base, seen) = serve(vec![ok(r#"{"payload":{"listens":[]}}"#)]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        assert!(lb.user_listens("a b&c", 5000).await.unwrap().is_empty());
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/1/user/a%20b%26c/listens?count=1000"
        );
    }

    #[tokio::test]
    async fn search_playlists_and_recording_popularity_round_trip() {
        let search_body = r#"{"playlists":[{"playlist":{"identifier":"https://listenbrainz.org/playlist/00403793-6e93-49ae-aed3-4b2e7ba5fb26","title":"Christmas","creator":"Gronald"}}]}"#;
        let pop_body = r#"[{"recording_mbid":"r1","total_listen_count":100000,"total_user_count":75283}]"#;
        let (base, seen) = serve(vec![ok(search_body), ok(pop_body)]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();

        let hits = lb.search_playlists("christmas", 5).await.unwrap();
        assert_eq!(
            hits,
            vec![PlaylistSearchHit {
                mbid: "00403793-6e93-49ae-aed3-4b2e7ba5fb26".into(),
                title: "Christmas".into(),
                creator: "Gronald".into(),
            }]
        );
        // Queries shorter than 3 chars are skipped without calling ListenBrainz.
        assert!(lb.search_playlists("ab", 5).await.unwrap().is_empty());

        let pop = lb.recording_popularity(&["r1".into()]).await.unwrap();
        assert_eq!(pop.len(), 1);
        assert_eq!(pop[0].total_user_count, Some(75_283));

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs[0].target, "/1/playlist/search?query=christmas&count=5");
        assert_eq!(reqs[1].target, "/1/popularity/recording");
    }

    #[tokio::test]
    async fn community_playlists_tolerate_null_headers_and_minimal_jspf_tracks() {
        let search_body = r#"{"playlists":[{"playlist":{"identifier":"https://listenbrainz.org/playlist/00403793-6e93-49ae-aed3-4b2e7ba5fb26","title":null,"creator":null,"date":null}}]}"#;
        let playlist_body = r#"{"playlist":{"track":[
            {"creator":"Bare Artist","title":"Bare Track"},
            {"creator":"Empty Ext","title":"Track Two","identifier":null,"extension":{}}
        ]}}"#;
        let (base, _) = serve(vec![ok(search_body), ok(playlist_body)]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();

        let hits = lb.search_playlists("christmas", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "");
        assert_eq!(hits[0].creator, "");

        let tracks = lb
            .playlist("00403793-6e93-49ae-aed3-4b2e7ba5fb26")
            .await
            .unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "Bare Track");
        assert_eq!(tracks[0].recording_mbid, None);
        assert_eq!(tracks[0].artist_mbid, None);
        assert_eq!(tracks[1].title, "Track Two");
    }
}
