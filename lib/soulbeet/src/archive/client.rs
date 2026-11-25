//! archive.org's advanced search, item file lists and item metadata, asked through the paced
//! client.

use super::error::ArchiveError;
use super::query;
use crate::polite::{Pace, PoliteClient, RemoteError};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Deserializer};
use shared::download::SourcePresence;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What failure texts and causes call archive.org.
pub(crate) const SERVICE: &str = "archive.org";

/// archive.org's own address.
const ARCHIVE_ORG: &str = "https://archive.org";

/// Where the backend reaches archive.org: `ARCHIVE_HOST` (`host[:port]`, reached over http, as
/// `MUSICBRAINZ_HOST` is) for a test double, else archive.org.
pub fn base_url_from_env() -> String {
    base_url_from(std::env::var("ARCHIVE_HOST").ok())
}

fn base_url_from(host: Option<String>) -> String {
    match host.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        Some(host) => format!("http://{host}"),
        None => ARCHIVE_ORG.to_string(),
    }
}

/// What a path segment keeps unencoded.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// `name` as URL path segments, its `/` kept between them.
pub(crate) fn path_of(name: &str) -> String {
    name.split('/')
        .map(|segment| utf8_percent_encode(segment, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// A field archive.org writes as a string, a number or a list: its text, the first one of a
/// list. Missing, null and anything else read as None.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Text(pub Option<String>);

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Text(Texts::deserialize(d)?.0.into_iter().next()))
    }
}

impl Text {
    pub(crate) fn get(&self) -> Option<&str> {
        self.0.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
}

/// A field archive.org writes as one string or a list of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Texts(pub Vec<String>);

impl<'de> Deserialize<'de> for Texts {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        fn text(v: serde_json::Value) -> Option<String> {
            match v {
                serde_json::Value::String(s) => Some(s),
                serde_json::Value::Number(n) => Some(n.to_string()),
                serde_json::Value::Bool(b) => Some(b.to_string()),
                _ => None,
            }
        }
        Ok(Texts(match serde_json::Value::deserialize(d)? {
            serde_json::Value::Array(items) => items.into_iter().filter_map(text).collect(),
            other => text(other).into_iter().collect(),
        }))
    }
}

/// One item of a search answer.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Doc {
    pub identifier: String,
    #[serde(default)]
    pub title: Text,
    #[serde(default)]
    pub creator: Texts,
    #[serde(default)]
    pub collection: Texts,
    #[serde(default)]
    pub format: Texts,
    #[serde(default)]
    pub downloads: Text,
    #[serde(default, rename = "external-identifier")]
    pub external: Texts,
    #[serde(default)]
    pub licenseurl: Text,
    #[serde(default, rename = "access-restricted-item")]
    pub restricted: Text,
}

impl Doc {
    /// Its collections, without the users' favourite lists (`fav-<user>`).
    pub(crate) fn collections(&self) -> Vec<String> {
        (self.collection.0.iter())
            .filter(|c| !c.starts_with("fav-"))
            .cloned()
            .collect()
    }

    /// Whether the backend never offers its files, whatever the query kept.
    pub(crate) fn withheld(&self) -> bool {
        self.restricted.get() == Some("true")
            || (self.collection.0.iter()).any(|c| c == "stream_only" || c == "etree")
    }

    /// Whether it names the MusicBrainz release `mbid`.
    pub(crate) fn names_release(&self, mbid: &str) -> bool {
        let wanted = format!("urn:mb_release_id:{}", mbid.trim().to_ascii_lowercase());
        (self.external.0.iter()).any(|e| e.trim().to_ascii_lowercase() == wanted)
    }
}

#[derive(Deserialize)]
struct SearchAnswer {
    response: SearchDocs,
}

#[derive(Deserialize)]
struct SearchDocs {
    #[serde(rename = "numFound")]
    num_found: u64,
    #[serde(default)]
    docs: Vec<Doc>,
}

/// One file of an item, as `/metadata/<id>/files` lists it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ItemFile {
    pub name: String,
    #[serde(default)]
    pub source: Text,
    #[serde(default)]
    pub format: Text,
    #[serde(default)]
    pub size: Text,
    #[serde(default)]
    pub md5: Text,
    #[serde(default)]
    pub length: Text,
    #[serde(default)]
    pub private: Text,
    #[serde(default)]
    pub title: Text,
    #[serde(default)]
    pub artist: Text,
    #[serde(default)]
    pub creator: Text,
    #[serde(default)]
    pub album: Text,
    #[serde(default)]
    pub track: Text,
}

/// `/metadata/<id>/files`: `{"error": "Couldn't locate item '<id>'"}` for an item archive.org
/// does not have.
#[derive(Deserialize)]
struct FilesAnswer {
    #[serde(default)]
    result: Vec<ItemFile>,
}

/// `/metadata/<id>/metadata`: the item's record, or `{"error": ..}` or `{}` when it is gone.
#[derive(Deserialize)]
struct MetadataAnswer {
    result: Option<ItemRecord>,
}

#[derive(Deserialize)]
struct ItemRecord {
    #[serde(default)]
    collection: Texts,
    #[serde(default, rename = "access-restricted-item")]
    restricted: Text,
    #[serde(default)]
    is_dark: Text,
}

/// How long a health check waits for archive.org, inside the 30 s the status line gives it.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(20);

/// What archive.org last answered the backend, and when. Every search, file list, presence,
/// download and health request tells it, so a health check asks archive.org only when nothing
/// else did for a while.
#[derive(Debug, Clone, Default)]
pub(crate) struct Heard(Arc<Mutex<Option<(Instant, Answer)>>>);

/// How archive.org answered: Ok when it was up.
type Answer = Result<(), RemoteError>;

impl Heard {
    /// Notes how a request went. An answer says archive.org is up, and so does a refusal of the
    /// request as written; an outage, a timeout, a rate limit or a browser check says it is not.
    /// An unreadable answer says neither.
    pub(crate) fn note<T>(&self, result: &Result<T, RemoteError>) {
        let heard = match result {
            Err(
                e @ (RemoteError::Unavailable { .. }
                | RemoteError::RateLimited { .. }
                | RemoteError::Challenged { .. }
                | RemoteError::Status { status: 408, .. }),
            ) => Err(e.clone()),
            Err(RemoteError::Unreadable { .. }) => return,
            Ok(_) | Err(RemoteError::Status { .. }) => Ok(()),
        };
        self.set(heard);
    }

    fn set(&self, heard: Answer) {
        *self.0.lock().unwrap() = Some((Instant::now(), heard));
    }

    /// What archive.org said less than `age` ago; None when it said nothing as recent.
    pub(crate) fn within(&self, age: Duration) -> Option<Answer> {
        (self.0.lock().unwrap().as_ref())
            .filter(|(at, _)| at.elapsed() < age)
            .map(|(_, heard)| heard.clone())
    }
}

/// archive.org as the backend asks it.
#[derive(Debug, Clone)]
pub(crate) struct Api {
    base: String,
    pace: Pace,
    heard: Heard,
}

impl Api {
    pub(crate) fn new(base: &str) -> Self {
        Api {
            base: base.trim_end_matches('/').to_string(),
            pace: Pace::ARCHIVE,
            heard: Heard::default(),
        }
    }

    /// What archive.org last answered any of the backend's requests.
    pub(crate) fn heard(&self) -> &Heard {
        &self.heard
    }

    /// An `Api` that does not wait between requests, for the tests.
    #[cfg(test)]
    pub(crate) fn unpaced(base: &str) -> Self {
        Api {
            pace: Pace::immediate(),
            ..Api::new(base)
        }
    }

    /// A client for one search, one presence or one health check. Every client of the host
    /// shares one pace, and gives up only for its own run.
    fn client(&self, pace: Pace) -> Result<PoliteClient, RemoteError> {
        PoliteClient::new(&self.base, SERVICE, pace)
    }

    pub(crate) fn searcher(&self) -> Result<Searcher, RemoteError> {
        Ok(Searcher {
            client: self.client(self.pace)?,
            heard: self.heard.clone(),
        })
    }

    /// Whether the item `identifier` can be fetched now.
    pub(crate) async fn presence(&self, identifier: &str) -> Result<SourcePresence, ArchiveError> {
        let client = self.client(Pace {
            retries: 1,
            ..self.pace
        })?;
        let path = format!("/metadata/{}/metadata", path_of(identifier));
        let answer = client.get::<MetadataAnswer>(&path).await;
        self.heard.note(&answer);
        let answer = answer?;
        Ok(match answer.and_then(|a| a.result) {
            None => SourcePresence::Offline,
            Some(record) if record.is_dark.get() == Some("true") => SourcePresence::Offline,
            Some(record)
                if record.restricted.get() == Some("true")
                    || (record.collection.0.iter()).any(|c| c == "stream_only") =>
            {
                SourcePresence::Blocked
            }
            Some(_) => SourcePresence::Online,
        })
    }

    /// Ok when archive.org's search answers and finds any audio item: one request, no retry.
    /// What it found is what archive.org was last heard to say.
    pub(crate) async fn health(&self) -> Result<(), RemoteError> {
        let answer = self.probe().await;
        self.heard.set(answer.clone());
        answer
    }

    async fn probe(&self) -> Result<(), RemoteError> {
        let client = self.client(Pace {
            retries: 0,
            timeout: HEALTH_TIMEOUT,
            ..self.pace
        })?;
        let answer: Option<SearchAnswer> = client
            .get(&query::search_path("mediatype:audio", 0))
            .await?;
        match answer {
            Some(a) if a.response.num_found >= 1 => Ok(()),
            _ => Err(RemoteError::Unreadable {
                service: SERVICE,
                detail: "the search for any audio item found none".into(),
            }),
        }
    }
}

/// The client of one search.
pub(crate) struct Searcher {
    client: PoliteClient,
    heard: Heard,
}

impl Searcher {
    /// The items `q` finds, at most `query::ROWS`.
    pub(crate) async fn items(&self, q: &str) -> Result<Vec<Doc>, RemoteError> {
        let answer = (self.client)
            .get::<SearchAnswer>(&query::search_path(q, query::ROWS))
            .await;
        self.heard.note(&answer);
        Ok(answer?.map(|a| a.response.docs).unwrap_or_default())
    }

    /// The files of the item `identifier`; none for an item archive.org does not have.
    pub(crate) async fn files(&self, identifier: &str) -> Result<Vec<ItemFile>, RemoteError> {
        let path = format!("/metadata/{}/files", path_of(identifier));
        let answer = self.client.get::<FilesAnswer>(&path).await;
        self.heard.note(&answer);
        Ok(answer?.map(|a| a.result).unwrap_or_default())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_http::{serve, Reply};

    pub(crate) fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    #[test]
    fn archive_host_points_the_backend_at_a_double_over_http() {
        assert_eq!(base_url_from(None), "https://archive.org");
        assert_eq!(base_url_from(Some("  ".into())), "https://archive.org");
        assert_eq!(
            base_url_from(Some("stubs:5064".into())),
            "http://stubs:5064"
        );
    }

    #[test]
    fn a_file_name_is_sent_as_path_segments() {
        assert_eq!(
            path_of("[1998] Stratosphere/01 Moon Age.mp3"),
            "%5B1998%5D%20Stratosphere/01%20Moon%20Age.mp3"
        );
        assert_eq!(path_of("a?b#c%d"), "a%3Fb%23c%25d");
    }

    #[test]
    fn a_search_answer_reads_strings_and_lists_alike() {
        let answer: SearchAnswer =
            serde_json::from_str(include_str!("fixtures/s1_duster_stratosphere.json")).unwrap();
        assert_eq!(answer.response.num_found, 5);
        let first = &answer.response.docs[0];
        assert_eq!(first.creator.0, ["Duster"]);
        assert!(first.collection.0.iter().any(|c| c.starts_with("fav-")));
        assert_eq!(first.collections(), ["hifidelity_potpourri", "hifidelity"]);
        let listed: Doc = serde_json::from_value(serde_json::json!({
            "identifier": "x", "title": ["One", "Two"], "creator": ["A", "B"],
            "downloads": 12, "access-restricted-item": true
        }))
        .unwrap();
        assert_eq!(listed.title.get(), Some("One"));
        assert_eq!(listed.creator.0, ["A", "B"]);
        assert_eq!(listed.downloads.get(), Some("12"));
        assert!(listed.withheld());
    }

    #[test]
    fn stream_only_restricted_and_live_music_archive_items_are_withheld() {
        let answer: SearchAnswer =
            serde_json::from_str(include_str!("fixtures/s8_restricted_filter.json")).unwrap();
        let withheld: Vec<&str> = (answer.response.docs.iter())
            .filter(|d| d.withheld())
            .map(|d| d.identifier.as_str())
            .collect();
        // Every one of them is an etree item, and some are stream-only too.
        assert_eq!(withheld.len(), answer.response.docs.len());
        assert!((answer.response.docs.iter()).any(|d| d
            .collection
            .0
            .iter()
            .any(|c| c == "stream_only")));
    }

    #[tokio::test]
    async fn a_missing_item_reads_as_offline_and_a_stream_only_one_as_blocked() {
        let (base, seen) = serve(vec![
            ok(include_str!("fixtures/missing_meta.json")),
            ok("{}"),
            ok(include_str!("fixtures/gd_meta_only.json")),
            ok(r#"{"result": {"identifier": "dmp003", "collection": ["dmp", "netlabels"]}}"#),
            ok(r#"{"result": {"identifier": "d", "is_dark": "true"}}"#),
        ]);
        let api = Api::unpaced(&base);
        for want in [
            SourcePresence::Offline,
            SourcePresence::Offline,
            SourcePresence::Blocked,
            SourcePresence::Online,
            SourcePresence::Offline,
        ] {
            assert_eq!(api.presence("dmp003").await.unwrap(), want);
        }
        assert_eq!(seen.lock().unwrap()[0].target, "/metadata/dmp003/metadata");
    }

    #[tokio::test]
    async fn what_archive_org_answers_any_request_is_heard() {
        let refused = |status| Reply {
            status,
            body: String::new(),
        };
        let mut replies = vec![
            ok(r#"{"response":{"numFound":0,"start":0,"docs":[]}}"#),
            ok(include_str!("fixtures/missing_files.json")),
            refused(400),
        ];
        replies.extend((0..4).map(|_| refused(429)));
        let (base, _) = serve(replies);
        let api = Api::unpaced(&base);
        let heard = || api.heard().within(Duration::from_secs(60));
        assert_eq!(heard(), None);
        let searcher = api.searcher().unwrap();

        searcher.items("x").await.unwrap();
        assert_eq!(heard(), Some(Ok(())));
        // A missing item has no files.
        assert!(searcher.files("gone").await.unwrap().is_empty());
        // A refusal of the request as written is an answer too.
        assert!(searcher.items("x").await.is_err());
        assert_eq!(heard(), Some(Ok(())));
        assert!(searcher.items("x").await.is_err());
        assert_eq!(
            heard(),
            Some(Err(RemoteError::RateLimited { service: SERVICE }))
        );
        assert_eq!(api.heard().within(Duration::ZERO), None);
    }

    #[tokio::test]
    async fn the_health_check_asks_once_for_any_audio_item() {
        let (base, seen) = serve(vec![
            ok(
                r#"{"responseHeader":{"status":0},"response":{"numFound":14339488,"start":0,"docs":[]}}"#,
            ),
            ok(r#"{"responseHeader":{"status":0},"response":{"numFound":0,"start":0,"docs":[]}}"#),
            Reply {
                status: 503,
                body: "down".into(),
            },
        ]);
        let api = Api::unpaced(&base);
        api.health().await.unwrap();
        assert_eq!(api.heard().within(Duration::from_secs(60)), Some(Ok(())));
        assert!(matches!(
            api.health().await,
            Err(RemoteError::Unreadable { .. })
        ));
        assert!(matches!(
            api.heard().within(Duration::from_secs(60)),
            Some(Err(RemoteError::Unreadable { .. }))
        ));
        assert!(matches!(
            api.health().await,
            Err(RemoteError::Unavailable { .. })
        ));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "a failed check is not retried");
        assert!(
            seen[0].target.contains("q=mediatype%3Aaudio"),
            "{}",
            seen[0].target
        );
        assert!(seen[0].target.contains("rows=0"), "{}", seen[0].target);
        assert_eq!(
            seen[0].user_agent.as_deref(),
            Some(crate::polite::USER_AGENT)
        );
    }
}
