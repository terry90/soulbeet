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
    async fn listens_are_asked_at_most_a_thousand_at_a_time_and_names_are_encoded() {
        let (base, seen) = serve(vec![ok(r#"{"payload":{"listens":[]}}"#)]);
        let lb = ListenBrainzClient::new(&base, None, Pace::immediate()).unwrap();
        assert!(lb.user_listens("a b&c", 5000).await.unwrap().is_empty());
        assert_eq!(
            seen.lock().unwrap()[0].target,
            "/1/user/a%20b%26c/listens?count=1000"
        );
    }
}
