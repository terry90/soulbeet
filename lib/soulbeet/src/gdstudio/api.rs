//! GD Studio's API as the backend asks it: every call through the install's budget and the
//! breaker, one at a time with the host's pace, following no redirect. Also where a file URL it
//! names may point, and the first bytes of a file read off NetEase's servers.

use super::audio_header::{self, Header, HEAD_BYTES};
use super::budget::Budget;
use super::error::GdError;
use super::Limits;
use crate::http::CircuitBreaker;
use crate::polite::{HostGate, Pace, PoliteClient, RemoteError, USER_AGENT};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use serde::{Deserialize, Deserializer};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What failure texts and causes call GD Studio.
pub(crate) const SERVICE: &str = "GD Studio";

/// GD Studio's documented API host, which needs no signature.
const GD_STUDIO: &str = "https://music-api.gdstudio.xyz";

/// The hosts NetEase serves its files from end with this.
const NETEASE_FILES: &str = ".music.126.net";

/// `GDSTUDIO_HOST` (`host[:port]`, reached over http, as `MUSICBRAINZ_HOST` is), for a test
/// double; unset for GD Studio itself.
pub fn test_host_from_env() -> Option<String> {
    test_host_from(std::env::var("GDSTUDIO_HOST").ok())
}

fn test_host_from(host: Option<String>) -> Option<String> {
    host.map(|h| h.trim().to_string()).filter(|h| !h.is_empty())
}

/// Where the API is: the test double `test_host` names, else GD Studio.
pub(crate) fn base_url(test_host: Option<&str>) -> String {
    match test_host {
        Some(host) => format!("http://{host}"),
        None => GD_STUDIO.to_string(),
    }
}

/// Whether a file URL GD Studio named may be fetched: https on one of NetEase's file hosts, by
/// name, with no user or port of its own. With a test double set, its host over http too, on
/// any port, as a test serves files from servers of its own.
pub(crate) fn allowed(url: &url::Url, test_host: Option<&str>) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let double = test_host.map(|h| h.rsplit_once(':').map_or(h, |(host, _)| host));
    if url.scheme() == "http" && double.is_some() && url.host_str() == double {
        return true;
    }
    let netease =
        matches!(url.host(), Some(url::Host::Domain(host)) if host.ends_with(NETEASE_FILES));
    url.scheme() == "https" && netease && url.port().is_none()
}

/// One row of a search answer.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Row {
    #[serde(deserialize_with = "text_or_number")]
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "names")]
    pub artist: Vec<String>,
    #[serde(default)]
    pub album: String,
}

fn text_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!("not a track id: {other}"))),
    }
}

fn names<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        serde_json::Value::String(s) => vec![s],
        _ => vec![],
    })
}

/// `types=url`: `size` is in bytes and `br` the file's real average kbps, whatever the
/// documentation says; an empty `url` for a track NetEase does not serve, or does not have.
#[derive(Debug, Deserialize)]
struct UrlAnswer {
    url: String,
    #[serde(default)]
    br: f64,
    #[serde(default)]
    size: f64,
}

/// Which list a search asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Listing {
    /// The tracks of the album the text names best, in album order.
    Album,
    /// Tracks across albums.
    Tracks,
}

impl Listing {
    fn source(self) -> &'static str {
        match self {
            Listing::Album => "netease_album",
            Listing::Tracks => "netease",
        }
    }
}

/// A file URL GD Studio named, read: the object it names and when it was named.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Located {
    pub url: String,
    pub host: String,
    pub kbps: u32,
    pub size: u64,
    /// "flac" or "mp3", from the file name.
    pub ext: String,
    /// The file name's stem: the MD5 of the file.
    pub md5: String,
    pub issued: Instant,
}

impl Located {
    /// The URL `answer` names, when it is one the backend fetches: on an allowed host, named
    /// `<md5>.<flac|mp3>`, with a size.
    fn of(answer: UrlAnswer, test_host: Option<&str>) -> Result<Located, Refusal> {
        let url = url::Url::parse(&answer.url)
            .map_err(|e| Refusal::Host(format!("an unreadable URL ({e})")))?;
        let host = url.host_str().unwrap_or_default().to_string();
        if !allowed(&url, test_host) {
            return Err(Refusal::Host(host));
        }
        let unnamed = |why: String| Err(Refusal::File(why));
        let name = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let (md5, ext) = name.rsplit_once('.').unwrap_or((&name, ""));
        if md5.len() != 32 || !md5.bytes().all(|b| b.is_ascii_hexdigit()) {
            return unnamed(format!("the file on {host} is not named by its MD5"));
        }
        if !matches!(ext, "flac" | "mp3") {
            return unnamed(format!(
                "the file on {host} is a .{ext}, neither FLAC nor MP3"
            ));
        }
        if !(answer.size >= 1.0 && answer.size.fract() == 0.0) {
            return unnamed(format!("the file on {host} has no size"));
        }
        Ok(Located {
            url: answer.url,
            host,
            kbps: answer.br.max(0.0).round() as u32,
            size: answer.size as u64,
            ext: ext.to_string(),
            md5: md5.to_string(),
            issued: Instant::now(),
        })
    }

    /// Whether this and `other` name the same object.
    pub(crate) fn same_object(&self, ext: &str, size: u64, md5: &str) -> bool {
        self.ext == ext && self.size == size && self.md5 == md5
    }
}

/// What GD Studio answered a `types=url` call.
#[derive(Debug)]
pub(crate) enum Resolved {
    Located(Located),
    /// No URL: NetEase does not serve the track.
    NotServed,
    /// A URL the backend does not fetch.
    Refused(Refusal),
}

/// Why a file URL is not fetched.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Refusal {
    /// It points off NetEase's servers: the host it names.
    Host(String),
    /// It names no file the backend can check: why.
    File(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Host(host) => write!(f, "the file is on {host}, not on NetEase's servers"),
            Refusal::File(why) => f.write_str(why),
        }
    }
}

/// What the API last answered, and when: the health check and `source_status` read it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Heard(Arc<Mutex<Option<(Instant, Answer)>>>);

/// How the API answered: Ok when it was up.
type Answer = Result<(), RemoteError>;

impl Heard {
    fn set(&self, answer: Answer) {
        *self.0.lock().unwrap() = Some((Instant::now(), answer));
    }

    /// What it said less than `age` ago.
    pub(crate) fn within(&self, age: Duration) -> Option<Answer> {
        (self.0.lock().unwrap().as_ref())
            .filter(|(at, _)| at.elapsed() < age)
            .map(|(_, answer)| answer.clone())
    }
}

/// A 200 answer that is not data but a refusal naming a limit, as an operator might write one.
fn names_a_limit(body: &[u8]) -> bool {
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_slice(body) else {
        return false;
    };
    let text = serde_json::Value::Object(fields).to_string().to_lowercase();
    ["频率", "限制", "limit", "too many"]
        .iter()
        .any(|word| text.contains(word))
}

/// The API, its budget, its breaker and the file URLs it named lately, one per engine.
pub(crate) struct Api {
    base: String,
    test_host: Option<String>,
    pace: Pace,
    /// The pace every call to the API keeps, and its holds.
    gate: Arc<HostGate>,
    budget: Budget,
    breaker: CircuitBreaker,
    failures: u64,
    rest_s: i64,
    heard: Heard,
    /// The last URL named for each track, reused while it is younger than `reuse`: NetEase's
    /// servers refuse a URL 25 minutes after it was named.
    located: Mutex<HashMap<String, Located>>,
    reuse: Duration,
    /// Reads the first bytes of files, never following a redirect off an allowed host.
    files: reqwest::Client,
}

impl Api {
    pub(crate) fn new(
        test_host: Option<String>,
        limits: &Limits,
        budget: Budget,
    ) -> Result<Api, GdError> {
        let files = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| unavailable(e.without_url().to_string()))?;
        let base = base_url(test_host.as_deref());
        let gate = HostGate::of(&base).map_err(unavailable)?;
        Ok(Api {
            base,
            test_host,
            gate,
            pace: Pace {
                min_interval: limits.spacing,
                backoff: limits.hold,
                retries: 0,
                timeout: Duration::from_secs(30),
            },
            budget,
            breaker: CircuitBreaker::new(limits.failures, limits.rest_s),
            failures: limits.failures,
            rest_s: limits.rest_s,
            heard: Heard::default(),
            located: Mutex::new(HashMap::new()),
            reuse: limits.reuse,
            files,
        })
    }

    pub(crate) fn heard(&self) -> &Heard {
        &self.heard
    }

    pub(crate) fn test_host(&self) -> Option<&str> {
        self.test_host.as_deref()
    }

    pub(crate) async fn resting(&self) -> bool {
        self.breaker.is_open().await
    }

    /// One call: a slot of the budget, then the host's turn, then the breaker asked; the call
    /// is counted as it goes out, and the slot is held until its answer is in.
    async fn call<T: serde::de::DeserializeOwned>(&self, query: &str) -> Result<T, GdError> {
        let resting = || GdError::Resting {
            failures: self.failures,
            rest_s: self.rest_s,
        };
        if self.breaker.is_open().await {
            return Err(resting());
        }
        let client = PoliteClient::without_redirects(&self.base, SERVICE, self.pace)?
            .with_quota_check(names_a_limit);
        let mut slot = self.budget.take().await;
        // The spacing and any hold are waited out here, so the client's own turn comes at
        // once: no other call to the API can wait for it while this one holds its slot.
        self.gate.wait_for_turn(Duration::ZERO).await;
        if self.breaker.is_open().await {
            return Err(resting());
        }
        slot.spend().await;
        let answer = client.get::<T>(&format!("/api.php?{query}")).await;
        match &answer {
            Err(RemoteError::Unavailable { .. } | RemoteError::Challenged { .. }) => {
                self.breaker.record_failure().await
            }
            _ => self.breaker.record_success().await,
        }
        self.heard
            .set(answer.as_ref().map(|_| ()).map_err(Clone::clone));
        // The next call sees this one's answer in the breaker.
        drop(slot);
        match answer? {
            Some(value) => Ok(value),
            None => Err(GdError::Remote(RemoteError::Unreadable {
                service: SERVICE,
                detail: "an empty answer".into(),
            })),
        }
    }

    /// The rows `listing` gives for `name`, at most `count`.
    pub(crate) async fn search(
        &self,
        listing: Listing,
        name: &str,
        count: u32,
    ) -> Result<Vec<Row>, GdError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("types", "search")
            .append_pair("source", listing.source())
            .append_pair("name", name)
            .append_pair("count", &count.to_string())
            .finish();
        self.call(&query).await
    }

    /// The URL last named for `track_id`, while it may still be used.
    pub(crate) fn fresh(&self, track_id: &str) -> Option<Located> {
        let located = self.located.lock().unwrap();
        (located.get(track_id))
            .filter(|l| l.issued.elapsed() < self.reuse)
            .cloned()
    }

    /// Where the track `track_id` is now, at the best quality NetEase serves it in. A URL the
    /// backend may fetch is kept for `fresh`.
    pub(crate) async fn resolve(&self, track_id: &str) -> Result<Resolved, GdError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("types", "url")
            .append_pair("source", "netease")
            .append_pair("id", track_id)
            .append_pair("br", "999")
            .finish();
        let answer: UrlAnswer = self.call(&query).await?;
        if answer.url.trim().is_empty() {
            return Ok(Resolved::NotServed);
        }
        Ok(match Located::of(answer, self.test_host()) {
            Ok(located) => {
                let mut kept = self.located.lock().unwrap();
                kept.retain(|_, l| l.issued.elapsed() < self.reuse);
                kept.insert(track_id.to_string(), located.clone());
                Resolved::Located(located)
            }
            Err(why) => Resolved::Refused(why),
        })
    }

    /// Keeps a URL naming the FLAC of `md5` and `size` on the server at `base` for `track_id`,
    /// as a search's resolve does.
    #[cfg(test)]
    pub(crate) fn remember(&self, track_id: &str, base: &str, md5: &str, size: usize) {
        let located = Located {
            url: format!("{base}/cdn/{md5}.flac"),
            host: "127.0.0.1".into(),
            kbps: 900,
            size: size as u64,
            ext: "flac".into(),
            md5: md5.into(),
            issued: Instant::now(),
        };
        self.located
            .lock()
            .unwrap()
            .insert(track_id.into(), located);
    }

    /// One call of the budget that any search answers.
    pub(crate) async fn health(&self) -> Result<(), GdError> {
        self.search(Listing::Tracks, "a", 1).await.map(|_| ())
    }

    /// The header of the file `located` names, read from its first bytes, once its server
    /// confirmed the size and, when it gives one, the MD5.
    pub(crate) async fn head(&self, located: &Located) -> Result<Header, String> {
        let range = format!("bytes=0-{}", HEAD_BYTES - 1);
        let mut response = (self
            .files
            .get(&located.url)
            .header(RANGE, range)
            .send()
            .await)
            .map_err(|e| e.without_url().to_string())?;
        let headers = response.headers().clone();
        let total = match response.status() {
            StatusCode::PARTIAL_CONTENT => headers.get(CONTENT_RANGE).and_then(|v| {
                v.to_str()
                    .ok()?
                    .rsplit_once('/')?
                    .1
                    .trim()
                    .parse::<u64>()
                    .ok()
            }),
            StatusCode::OK => headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok()),
            status => {
                return Err(format!(
                    "{} answered HTTP {}",
                    located.host,
                    status.as_u16()
                ))
            }
        };
        if total != Some(located.size) {
            return Err(format!(
                "{} holds {total:?} bytes, GD Studio gave {}",
                located.host, located.size
            ));
        }
        let origin = headers
            .get("x-nos-meta-origin-md5")
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase);
        if origin.as_deref().is_some_and(|md5| md5 != located.md5) {
            return Err(format!(
                "{} gives the MD5 {}, the file is named {}",
                located.host,
                origin.unwrap_or_default(),
                located.md5
            ));
        }
        let mut bytes = Vec::new();
        while (bytes.len() as u64) < HEAD_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(e) => return Err(e.without_url().to_string()),
            }
        }
        let header = audio_header::read(&bytes)?;
        if header.ext() != located.ext {
            return Err(format!(
                "the file named .{} starts as {}",
                located.ext,
                header.ext()
            ));
        }
        Ok(header)
    }
}

fn unavailable(detail: String) -> GdError {
    GdError::Remote(RemoteError::Unavailable {
        service: SERVICE,
        detail,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gdstudio::tests::fast;
    use crate::test_http::{serve, serve_file, serve_with_headers, FileAnswer, Reply, ServedFile};
    use crate::BackendFault;

    pub(crate) fn ok(body: &str) -> Reply {
        Reply {
            status: 200,
            body: body.into(),
        }
    }

    pub(crate) fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/src/gdstudio/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// The API at `base`, a test double: `host:port` of its URL.
    pub(crate) fn api_at(base: &str, limits: &Limits) -> Api {
        let budget = Budget::new(limits.calls, limits.window);
        Api::new(
            Some(base.trim_start_matches("http://").to_string()),
            limits,
            budget,
        )
        .unwrap()
    }

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn only_netease_file_hosts_over_https_are_fetched() {
        for (link, fetched) in [
            ("https://m701.music.126.net/2026/x/a.flac", true),
            ("https://m8.music.126.net/2026/x/a.mp3", true),
            ("http://m701.music.126.net/2026/x/a.flac", false),
            ("https://music.126.net.evil.example/a.flac", false),
            ("https://x@evil/a.flac", false),
            ("https://u:p@m701.music.126.net/a.flac", false),
            ("https://m701.music.126.net:8443/a.flac", false),
            ("https://203.0.113.7/a.flac", false),
            ("https://[2001:db8::1]/a.flac", false),
            ("https://evil.example/m701.music.126.net/a.flac", false),
            ("http://stubs:5063/cdn/a.flac", false),
        ] {
            assert_eq!(allowed(&url(link), None), fetched, "{link}");
        }
        let double = Some("stubs:5063");
        assert!(allowed(&url("http://stubs:5063/cdn/a.flac"), double));
        assert!(allowed(&url("http://stubs:5064/cdn/a.flac"), double));
        assert!(!allowed(&url("http://other:5063/cdn/a.flac"), double));
        assert!(!allowed(&url("http://x@stubs:5063/cdn/a.flac"), double));
        assert!(allowed(&url("https://m701.music.126.net/a.flac"), double));
    }

    #[test]
    fn gdstudio_host_points_the_backend_at_a_double_over_http() {
        assert_eq!(test_host_from(None), None);
        assert_eq!(test_host_from(Some("  ".into())), None);
        assert_eq!(
            test_host_from(Some("stubs:5063".into())).as_deref(),
            Some("stubs:5063")
        );
        assert_eq!(base_url(None), "https://music-api.gdstudio.xyz");
        assert_eq!(base_url(Some("stubs:5063")), "http://stubs:5063");
    }

    #[test]
    fn search_answers_read_their_rows() {
        for (name, rows, first, artist) in [
            ("s1_album_search.json", 14, "One More Time", "Daft Punk"),
            ("s3_pomme.json", 14, "la conquête", "Pomme"),
            ("s4_kol.json", 13, "Intro", "Kings of Leon"),
            ("s7_okc.json", 12, "Airbag", "Radiohead"),
            ("s8_lilbaby.json", 17, "Top Uh It", "Lil Baby"),
            ("s2_track_search.json", 10, "Papaoutai", "Stromae"),
            (
                "s5_midori.json",
                4,
                "Mr. Henri Rousseau's Dream",
                "高田みどり",
            ),
            ("s6_jay.json", 5, "晴天 (原唱 周杰伦)", "RyaVocal"),
        ] {
            let read: Vec<Row> = serde_json::from_str(&fixture(name)).unwrap();
            assert_eq!(read.len(), rows, "{name}");
            assert_eq!(
                (read[0].name.as_str(), read[0].artist[0].as_str()),
                (first, artist)
            );
            assert!(read.iter().all(|r| !r.id.is_empty() && !r.album.is_empty()));
        }
        let lil: Vec<Row> = serde_json::from_str(&fixture("s8_lilbaby.json")).unwrap();
        assert_eq!(lil[6].artist.len(), 4);
        let numbered: Row = serde_json::from_value(serde_json::json!({
            "id": 17455800, "name": "One More Time", "artist": "Daft Punk", "album": "Discovery"
        }))
        .unwrap();
        assert_eq!(
            (numbered.id.as_str(), numbered.artist.len()),
            ("17455800", 1)
        );
    }

    #[test]
    fn url_answers_read_size_in_bytes_and_the_real_bitrate() {
        let located = |name: &str| {
            let answer: UrlAnswer = serde_json::from_str(&fixture(name)).unwrap();
            Located::of(answer, None).unwrap()
        };
        let flac = located("u1_17455800_br999.json");
        assert_eq!(
            (
                flac.ext.as_str(),
                flac.size,
                flac.kbps,
                flac.md5.as_str(),
                flac.host.as_str()
            ),
            (
                "flac",
                40_733_003,
                1017,
                "a9c3b0d78c80477b78654c012158835f",
                "m801.music.126.net"
            )
        );
        let mp3 = located("u4_17455800_br320.json");
        assert_eq!(
            (mp3.ext.as_str(), mp3.size, mp3.kbps),
            ("mp3", 12_816_762, 320)
        );
        // The older path style names its file by its MD5 too.
        let older = located("u9_midori1_br999.json");
        assert_eq!(
            (older.ext.as_str(), older.kbps, older.md5.as_str()),
            ("mp3", 192, "1280943e812008ac715956d45a483c72")
        );
        let empty: UrlAnswer = serde_json::from_str(&fixture("u5_unknown.json")).unwrap();
        assert!(empty.url.is_empty());
        assert_eq!((empty.br, empty.size), (0.0, 0.0));

        let refused = |url: &str, size: f64| {
            Located::of(
                UrlAnswer {
                    url: url.into(),
                    br: 320.0,
                    size,
                },
                None,
            )
            .unwrap_err()
        };
        assert_eq!(
            refused(
                "https://evil.example/a9c3b0d78c80477b78654c012158835f.flac",
                1.0
            ),
            Refusal::Host("evil.example".into())
        );
        let file = |why: Refusal, has: &str| matches!(why, Refusal::File(w) if w.contains(has));
        assert!(file(
            refused("https://m7.music.126.net/x/song.flac", 1.0),
            "MD5"
        ));
        assert!(file(
            refused(
                "https://m7.music.126.net/x/a9c3b0d78c80477b78654c012158835f.m4a",
                1.0
            ),
            ".m4a"
        ));
        assert!(file(
            refused(
                "https://m7.music.126.net/x/a9c3b0d78c80477b78654c012158835f.mp3",
                0.0
            ),
            "no size"
        ));
    }

    #[tokio::test]
    async fn every_call_names_its_listing_and_carries_the_apps_user_agent() {
        let (base, seen) = serve(vec![
            ok(&fixture("s1_album_search.json")),
            ok(&fixture("u5_unknown.json")),
            ok("[]"),
        ]);
        let api = api_at(&base, &fast());
        let rows = api
            .search(Listing::Album, "Daft Punk Discovery", 50)
            .await
            .unwrap();
        assert_eq!(rows.len(), 14);
        assert!(matches!(
            api.resolve("99999999999").await.unwrap(),
            Resolved::NotServed
        ));
        api.health().await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].target,
            "/api.php?types=search&source=netease_album&name=Daft+Punk+Discovery&count=50"
        );
        assert_eq!(
            seen[1].target,
            "/api.php?types=url&source=netease&id=99999999999&br=999"
        );
        assert_eq!(
            seen[2].target,
            "/api.php?types=search&source=netease&name=a&count=1"
        );
        assert_eq!(seen[0].user_agent.as_deref(), Some(USER_AGENT));
        assert_eq!(api.heard().within(Duration::from_secs(60)), Some(Ok(())));
    }

    #[tokio::test]
    async fn refusals_read_as_their_failures_and_five_outages_rest_the_api() {
        let mut replies = vec![
            Reply {
                status: 401,
                body: r#"{"detail":"Invalid request."}"#.into(),
            },
            ok(r#"{"error":"5分钟内不超50次请求，访问频率限制"}"#),
            ok(r#"{"url": 3}"#),
        ];
        replies.extend((0..5).map(|_| Reply {
            status: 522,
            body: "<html>origin down</html>".into(),
        }));
        let (base, seen) = serve(replies);
        let api = api_at(&base, &fast());
        let unsigned = api.health().await.unwrap_err();
        assert!(unsigned.answer_unreadable(), "{unsigned:?}");
        assert!(matches!(
            api.health().await,
            Err(GdError::Remote(RemoteError::RateLimited { .. }))
        ));
        assert!(matches!(
            api.resolve("1").await,
            Err(GdError::Remote(RemoteError::Unreadable { .. }))
        ));
        for _ in 0..5 {
            assert!(matches!(
                api.health().await,
                Err(GdError::Remote(RemoteError::Unavailable { .. }))
            ));
        }
        assert!(matches!(api.health().await, Err(GdError::Resting { .. })));
        assert!(api.resting().await);
        assert_eq!(seen.lock().unwrap().len(), 8, "a resting API is not asked");
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let (elsewhere, reached) = serve(vec![ok("[]")]);
        let (base, _) = serve_with_headers(vec![(
            Reply {
                status: 302,
                body: String::new(),
            },
            vec![("Location", format!("{elsewhere}/api.php?types=search"))],
        )]);
        let api = api_at(&base, &fast());
        assert!(matches!(
            api.health().await,
            Err(GdError::Remote(RemoteError::Status { status: 302, .. }))
        ));
        assert!(reached.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_call_counts_when_it_is_sent_so_a_hold_lets_no_more_out_in_a_window() {
        let mut replies = vec![Reply {
            status: 429,
            body: String::new(),
        }];
        replies.extend((0..5).map(|_| ok("[]")));
        let (base, seen) = serve(replies);
        let window = Duration::from_millis(1000);
        let limits = Limits {
            calls: 3,
            window,
            hold: Duration::from_millis(400),
            ..fast()
        };
        let api = Arc::new(api_at(&base, &limits));
        assert!(matches!(
            api.health().await,
            Err(GdError::Remote(RemoteError::RateLimited { .. }))
        ));
        // Five callers come while the 429 holds every call.
        let callers: Vec<_> = (0..5)
            .map(|_| {
                let api = api.clone();
                tokio::spawn(async move { api.health().await })
            })
            .collect();
        for caller in callers {
            caller.await.unwrap().unwrap();
        }
        let sent: Vec<Instant> = seen.lock().unwrap().iter().map(|s| s.at).collect();
        assert_eq!(sent.len(), 6);
        // A request reaches the server a moment after it is counted.
        let span = window - Duration::from_millis(50);
        for (i, first) in sent.iter().enumerate() {
            let within = sent[i..]
                .iter()
                .filter(|at| at.duration_since(*first) < span)
                .count();
            assert!(within <= limits.calls, "{within} calls within a window");
        }
    }

    #[tokio::test]
    async fn a_files_head_is_read_once_its_server_confirms_size_and_md5() {
        let file = std::fs::read(format!(
            "{}/src/gdstudio/fixtures/r1_flac.meta",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let md5 = format!("{:x}", md5::compute(&file));
        let located = |base: &str, size: u64| Located {
            url: format!("{base}/cdn/{md5}.flac"),
            host: "127.0.0.1".into(),
            kbps: 1017,
            size,
            ext: "flac".into(),
            md5: md5.clone(),
            issued: Instant::now(),
        };
        let (base, seen) = serve_file(
            ServedFile {
                body: file.clone(),
                etag: None,
            },
            vec![FileAnswer::Whole],
        );
        let api = api_at(&base, &fast());
        let header = api.head(&located(&base, file.len() as u64)).await.unwrap();
        assert!(matches!(
            header,
            Header::Flac {
                bits: 16,
                rate: 44_100,
                ..
            }
        ));
        assert_eq!(
            seen.lock().unwrap()[0].range.as_deref(),
            Some("bytes=0-4095")
        );

        let range = |total: u64| ("Content-Range", format!("bytes 0-0/{total}"));
        let one = |status| Reply {
            status,
            body: "x".into(),
        };
        let (base, _) = serve_with_headers(vec![
            (one(206), vec![range(999)]),
            (
                one(206),
                vec![range(40_733_003), ("x-nos-meta-origin-md5", "0".repeat(32))],
            ),
            (one(403), vec![]),
        ]);
        let api = api_at(&base, &fast());
        let named = located(&base, 40_733_003);
        assert!(api.head(&named).await.unwrap_err().contains("999"));
        assert!(api.head(&named).await.unwrap_err().contains("MD5"));
        assert!(api.head(&named).await.unwrap_err().contains("403"));
    }
}
