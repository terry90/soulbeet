//! archive.org's advanced search, item file lists and item metadata, asked through the paced
//! client.

use super::error::ArchiveError;
use super::query;
use crate::polite::{Pace, PoliteClient, RemoteError};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Deserializer};
use shared::download::SourcePresence;
use std::time::Duration;

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

/// `/metadata/<id>/files`: `{}` for an item archive.org does not have.
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

/// archive.org as the backend asks it.
#[derive(Debug, Clone)]
pub(crate) struct Api {
    base: String,
    pace: Pace,
}

impl Api {
    pub(crate) fn new(base: &str) -> Self {
        Api {
            base: base.trim_end_matches('/').to_string(),
            pace: Pace::ARCHIVE,
        }
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
        Ok(Searcher(self.client(self.pace)?))
    }

    /// Whether the item `identifier` can be fetched now.
    pub(crate) async fn presence(&self, identifier: &str) -> Result<SourcePresence, ArchiveError> {
        let client = self.client(Pace {
            retries: 1,
            ..self.pace
        })?;
        let path = format!("/metadata/{}/metadata", path_of(identifier));
        let answer: Option<MetadataAnswer> = client.get(&path).await?;
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
    pub(crate) async fn health(&self) -> Result<(), ArchiveError> {
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
            }
            .into()),
        }
    }
}

/// The client of one search.
pub(crate) struct Searcher(PoliteClient);

impl Searcher {
    /// The items `q` finds, at most `query::ROWS`.
    pub(crate) async fn items(&self, q: &str) -> Result<Vec<Doc>, RemoteError> {
        let answer: Option<SearchAnswer> = self.0.get(&query::search_path(q, query::ROWS)).await?;
        Ok(answer.map(|a| a.response.docs).unwrap_or_default())
    }

    /// The files of the item `identifier`; none for an item archive.org does not have.
    pub(crate) async fn files(&self, identifier: &str) -> Result<Vec<ItemFile>, RemoteError> {
        let path = format!("/metadata/{}/files", path_of(identifier));
        let answer: Option<FilesAnswer> = self.0.get(&path).await?;
        Ok(answer.map(|a| a.result).unwrap_or_default())
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
        assert!(matches!(
            api.health().await,
            Err(ArchiveError::Remote(RemoteError::Unreadable { .. }))
        ));
        assert!(matches!(
            api.health().await,
            Err(ArchiveError::Remote(RemoteError::Unavailable { .. }))
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
