//! A Navidrome stand-in for api tests, the answers it is given, and a
//! `NavidromeOf` that hands out one client.

use crate::auth::UserId;
use crate::core::index::NavidromeOf;
use crate::services::NavidromeLookupError;
use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::json;
use soulbeet::navidrome::{NavidromeClient, NavidromeClientBuilder};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// What an endpoint with no answer set says (a Subsonic `ok` with no body).
const OK: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;

type Answers = Arc<Mutex<Book>>;

/// What the stand-in answers: the `when` rules first, then the bodies set per endpoint.
#[derive(Default)]
struct Book {
    bodies: HashMap<String, VecDeque<String>>,
    when: Vec<When>,
}

/// An answer for the requests to `endpoint` whose query contains `query_part`.
struct When {
    endpoint: String,
    query_part: String,
    status: u16,
    body: String,
}

/// A Navidrome stand-in on 127.0.0.1: answers each endpoint path from the bodies set for it, in
/// order, the last one repeating; an endpoint with none answers `{"subsonic-response":{"status":"ok","version":"1.16.1"}}`.
/// `/auth/login` answers `{"token":"jwt"}` unless set. Records each request's path and query.
pub(crate) struct NavidromeStub {
    pub url: String,
    answers: Answers,
    asked: Arc<Mutex<Vec<String>>>,
}

impl NavidromeStub {
    pub fn start() -> NavidromeStub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let answers: Answers = Arc::new(Mutex::new(Book {
            bodies: HashMap::from([(
                "/auth/login".to_string(),
                VecDeque::from([r#"{"token":"jwt"}"#.to_string()]),
            )]),
            when: Vec::new(),
        }));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (bodies, paths) = (answers.clone(), asked.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                // A client that hung up mid-request gets no answer.
                let _ = serve(stream, &bodies, &paths);
            }
        });
        NavidromeStub {
            url,
            answers,
            asked,
        }
    }

    pub fn answer(&self, endpoint: &str, bodies: &[&str]) {
        self.answers.lock().unwrap().bodies.insert(
            endpoint.to_string(),
            bodies.iter().map(|body| body.to_string()).collect(),
        );
    }

    /// Answers `endpoint` requests whose query contains `query_part` with `status` and `body`,
    /// every time and before any `answer`.
    pub fn answer_when(&self, endpoint: &str, query_part: &str, status: u16, body: &str) {
        self.answers.lock().unwrap().when.push(When {
            endpoint: endpoint.to_string(),
            query_part: query_part.to_string(),
            status,
            body: body.to_string(),
        });
    }

    /// The path of each request, in order.
    pub fn asked(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .map(|target| target.split('?').next().unwrap_or_default().to_string())
            .collect()
    }

    /// The path and query of each request, in order.
    pub fn asked_with_queries(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }

    /// A client signed in as alice / secret.
    pub fn client(&self) -> Arc<NavidromeClient> {
        Arc::new(
            NavidromeClientBuilder::new()
                .base_url(&self.url)
                .username("alice")
                .password("secret")
                .build()
                .unwrap(),
        )
    }
}

/// Reads one request and answers it with the next body set for its path.
fn serve(stream: TcpStream, answers: &Answers, asked: &Mutex<Vec<String>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let target = line.split_whitespace().nth(1).unwrap_or_default();
    let endpoint = target.split('?').next().unwrap_or_default().to_string();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    reader.read_exact(&mut vec![0; length])?;
    let (status, body) = answers.lock().unwrap().answer(&endpoint, target);
    asked.lock().unwrap().push(target.to_string());
    let status = StatusCode::from_u16(status).expect("a test sets a valid status");
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        status.as_u16(),
        status.canonical_reason().unwrap_or_default(),
        body.len()
    );
    reader.get_mut().write_all(response.as_bytes())
}

impl Book {
    /// The status and body for a request to `endpoint` with path and query `target`.
    fn answer(&mut self, endpoint: &str, target: &str) -> (u16, String) {
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
        if let Some(when) = self
            .when
            .iter()
            .find(|w| w.endpoint == endpoint && query.contains(&w.query_part))
        {
            return (when.status, when.body.clone());
        }
        let body = match self.bodies.get_mut(endpoint) {
            Some(bodies) if bodies.len() > 1 => bodies.pop_front(),
            Some(bodies) => bodies.front().cloned(),
            None => None,
        };
        (200, body.unwrap_or_else(|| OK.to_string()))
    }
}

/// Hands every user `self.0`: the client, or the lookup error the function makes.
pub(crate) struct NavidromeFor(pub Result<Arc<NavidromeClient>, fn() -> NavidromeLookupError>);

#[async_trait]
impl NavidromeOf for NavidromeFor {
    async fn navidrome_of(&self, _: &UserId) -> Result<Arc<NavidromeClient>, NavidromeLookupError> {
        self.0.clone().map_err(|e| e())
    }
}

/// Subsonic's `ok` envelope as Navidrome 0.64.2 sends it, around `body` (one or more
/// `"name":value` members).
pub(crate) fn subsonic_ok(body: &str) -> String {
    format!(
        r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true,{body}}}}}"#
    )
}

/// Subsonic's `failed` envelope as Navidrome 0.64.2 sends it, with error `code` and `message`.
pub(crate) fn subsonic_failed(code: u32, message: &str) -> String {
    let error = json!({"code": code, "message": message});
    format!(
        r#"{{"subsonic-response":{{"status":"failed","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true,"error":{error}}}}}"#
    )
}

/// A native `GET /api/song` record (the NATIVE_SONG shape of the soulbeet client tests) with
/// these values: library 1 at `/music`, album "Winter Songs" by `artist`, rated 4, starred, played 3
/// times. `mbzRecordingID` only when `mbid` is given.
pub(crate) fn native_song(
    id: &str,
    artist: &str,
    title: &str,
    duration: f64,
    mbid: Option<&str>,
) -> String {
    let mut song = json!({
        "id": id, "libraryId": 1, "libraryPath": "/music", "libraryName": "Music Library",
        "path": format!("{artist}/Winter Songs/03 - {title}.flac"), "title": title,
        "album": "Winter Songs", "artist": artist, "albumArtist": artist,
        "albumId": "7hQ2nLr5VxZ0cYbM8wTkAf", "trackNumber": 3, "discNumber": 1, "year": 2019,
        "size": 28734112, "suffix": "flac", "duration": duration, "bitRate": 912,
        "missing": false, "updatedAt": "2026-11-01T18:20:03.114208655+01:00",
        "rating": 4, "starred": true, "starredAt": "2026-11-02T18:04:51.112Z", "playCount": 3,
    });
    if let Some(mbid) = mbid {
        song["mbzRecordingID"] = json!(mbid);
    }
    song.to_string()
}

/// A native `GET /api/playlist/{id}/tracks` row (model.PlaylistTrack, model/playlist.go:224-229):
/// `native_song`'s record whose own `id` is the track's 1-based `place`, the song's id being
/// `mediaFileId`, and `missing` as given.
pub(crate) fn playlist_row(
    place: u32,
    song_id: &str,
    artist: &str,
    title: &str,
    missing: bool,
) -> String {
    let mut row: serde_json::Value =
        serde_json::from_str(&native_song(song_id, artist, title, 263.4, None))
            .expect("native_song writes JSON");
    row["id"] = json!(place.to_string());
    row["mediaFileId"] = json!(song_id);
    row["missing"] = json!(missing);
    row.to_string()
}

/// A Subsonic song child (the SONG shape of the soulbeet client tests) with these values: track 3
/// of "Winter Songs", rated 4 and starred by this user, no genre (`"genres":[]`).
pub(crate) fn child(id: &str, artist: &str, title: &str) -> String {
    let path = format!("{artist}/Winter Songs/01-03 - {title}.flac");
    let (id, artist, title, path) = (json!(id), json!(artist), json!(title), json!(path));
    format!(
        r#"{{"id":{id},"parent":"7hQ2nLr5VxZ0cYbM8wTkAf","isDir":false,"title":{title},"album":"Winter Songs","artist":{artist},"track":3,"size":28734112,"contentType":"audio/flac","suffix":"flac","starred":"2026-11-02T18:04:51.112Z","duration":263,"bitRate":912,"path":{path},"discNumber":1,"albumId":"7hQ2nLr5VxZ0cYbM8wTkAf","artistId":"2kD9pWq4ZxC1vBnM6yTjLs","type":"music","userRating":4,"averageRating":3.5,"bpm":0,"comment":"","sortName":"","mediaType":"song","musicBrainzId":"0a2f7e4b-6c1d-4e8a-9b3f-5d7c1e2a4b6c","isrc":[],"genres":[],"replayGain":{{}},"channelCount":2,"samplingRate":44100}}"#
    )
}
