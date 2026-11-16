//! A Navidrome stand-in for api tests, the answers it is given, and a
//! `NavidromeOf` that hands out one client.

use crate::auth::UserId;
use crate::core::index::NavidromeOf;
use crate::services::NavidromeLookupError;
use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::json;
use soulbeet::navidrome::models::SubsonicSong;
use soulbeet::navidrome::{NavidromeClient, NavidromeClientBuilder};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// What an endpoint with no answer set says (a Subsonic `ok` with no body).
pub(crate) const OK: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;

/// A playlist id as Navidrome makes one, and three songs (id, title) of the tests' library.
pub(crate) const ADVENT: &str = "6vN2bQ8rT4yH1kL9mX3zPa";
pub(crate) const HOLY: (&str, &str) = ("3sG8kPq0WmZ1xYbN4vTcDe", "O Holy Night");
pub(crate) const SILENT: (&str, &str) = ("0bF4hJ7kL2zX9cV5nM1qWe", "Silent Night");
pub(crate) const AWAY: (&str, &str) = ("9pL1kJ3hG5fD7sA2qW4eRt", "Away in a Manger");

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
/// `/auth/login` answers `{"token":"jwt"}` unless set. Records each request's path and query, and
/// its body.
pub(crate) struct NavidromeStub {
    pub url: String,
    answers: Answers,
    asked: Arc<Mutex<Vec<Asked>>>,
}

/// A request's path and query, and its body.
type Asked = (String, String);

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
            .map(|(target, _)| target.split('?').next().unwrap_or_default().to_string())
            .collect()
    }

    /// The path and query of each request, in order.
    pub fn asked_with_queries(&self) -> Vec<String> {
        let asked = self.asked.lock().unwrap();
        asked.iter().map(|(target, _)| target.clone()).collect()
    }

    /// The form of each request to `endpoint`, in order, without the sign-in and format fields
    /// every Subsonic request carries (u, t, s, v, c, f).
    pub fn posted(&self, endpoint: &str) -> Vec<String> {
        let asked = self.asked.lock().unwrap();
        (asked.iter())
            .filter(|(target, _)| target.split('?').next() == Some(endpoint))
            .map(|(_, body)| {
                let fields: Vec<&str> = (body.split('&'))
                    .filter(|field| {
                        let key = field.split('=').next().unwrap_or_default();
                        !["u", "t", "s", "v", "c", "f"].contains(&key)
                    })
                    .collect();
                fields.join("&")
            })
            .collect()
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
fn serve(stream: TcpStream, answers: &Answers, asked: &Mutex<Vec<Asked>>) -> std::io::Result<()> {
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
    let mut posted = vec![0; length];
    reader.read_exact(&mut posted)?;
    let (status, body) = answers.lock().unwrap().answer(&endpoint, target);
    let posted = String::from_utf8_lossy(&posted).into_owned();
    asked.lock().unwrap().push((target.to_string(), posted));
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

/// The native song reads `navi` was asked, with their queries, in order.
pub(crate) fn song_requests(navi: &NavidromeStub) -> Vec<String> {
    navi.asked_with_queries()
        .into_iter()
        .filter(|t| t.starts_with("/api/song"))
        .collect()
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

/// The `/api/playlist/{id}/tracks` answer of `playlist_row`s, each (place, song id, title, missing)
/// by "Marrow Choir".
pub(crate) fn playlist_rows(rows: &[(u32, &str, &str, bool)]) -> String {
    let rows: Vec<String> = rows
        .iter()
        .map(|(place, id, title, missing)| {
            playlist_row(*place, id, "Marrow Choir", title, *missing)
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// The `GET /api/song` answer of `native_song(id, "Marrow Choir", id, 263.4, None)` records, each
/// with `missing` as given.
pub(crate) fn native_songs(songs: &[(&str, bool)]) -> String {
    let songs: Vec<serde_json::Value> = songs
        .iter()
        .map(|(id, missing)| {
            let mut song: serde_json::Value =
                serde_json::from_str(&native_song(id, "Marrow Choir", id, 263.4, None))
                    .expect("native_song writes JSON");
            song["missing"] = json!(missing);
            song
        })
        .collect();
    json!(songs).to_string()
}

/// The rules of Soulful's discovery playlist for the Balanced profile, as Navidrome writes them
/// back (`create_smart_playlist`, lib/soulbeet/src/navidrome/client.rs).
pub(crate) const DISCOVERY_RULES: &str = r#"{"all":[{"contains":{"filepath":"Discovery/Balanced"}}],"sort":"-dateAdded","order":"desc"}"#;

/// alice's native `GET /api/playlist/{id}` record as Navidrome 0.64.2 writes model.Playlist
/// (model/playlist.go:18-41): `song_count` rows, synced from a playlist file when `sync`, smart
/// with `rules` (a JSON object) when given.
pub(crate) fn native_playlist(
    id: &str,
    name: &str,
    song_count: u32,
    sync: bool,
    rules: Option<&str>,
) -> String {
    let rules: serde_json::Value = rules.map_or(serde_json::Value::Null, |r| {
        serde_json::from_str(r).expect("rules are JSON")
    });
    let path = if sync {
        "/music/Playlists/Advent.m3u"
    } else {
        ""
    };
    json!({
        "id": id, "name": name, "comment": "", "duration": song_count * 263,
        "size": u64::from(song_count) * 28734112, "songCount": song_count, "ownerName": "alice",
        "ownerId": "8uY3tR6eW9qA2sD5fG8hJ1", "public": false, "path": path, "sync": sync,
        "uploadedImage": "", "createdAt": "2026-11-15T09:00:00.123456Z",
        "updatedAt": "2026-11-15T09:00:04.5Z", "rules": rules, "evaluatedAt": null,
    })
    .to_string()
}

/// A playlist as getPlaylists lists it and getPlaylist answers it, in responses.Playlist's key
/// order (server/subsonic/responses/responses.go:305-327): `comment` and `public` left out when
/// empty or false, 263 seconds a song.
fn playlist_json(
    id: &str,
    name: &str,
    owner: &str,
    public: bool,
    comment: &str,
    songs: usize,
) -> String {
    let mut p = format!(r#""id":{},"name":{}"#, json!(id), json!(name));
    if !comment.is_empty() {
        p.push_str(&format!(r#","comment":{}"#, json!(comment)));
    }
    p.push_str(&format!(
        r#","songCount":{songs},"duration":{}"#,
        songs * 263
    ));
    if public {
        p.push_str(r#","public":true"#);
    }
    p.push_str(&format!(
        r#","owner":{},"created":"2026-11-15T09:00:00.123456Z","changed":"2026-11-15T09:00:04.5Z","readonly":false"#,
        json!(owner)
    ));
    p
}

/// getPlaylist's or createPlaylist's answer (server/subsonic/playlists.go:42-75): the playlist
/// with one `child(id, "Marrow Choir", title)` entry per song, `entry` left out when there is
/// none (responses.go:333-336).
pub(crate) fn playlist_body(
    id: &str,
    name: &str,
    owner: &str,
    public: bool,
    comment: &str,
    songs: &[(&str, &str)],
) -> String {
    let mut p = playlist_json(id, name, owner, public, comment, songs.len());
    if !songs.is_empty() {
        let entries: Vec<String> = songs
            .iter()
            .map(|(id, title)| child(id, "Marrow Choir", title))
            .collect();
        p.push_str(&format!(r#","entry":[{}]"#, entries.join(",")));
    }
    subsonic_ok(&format!(r#""playlist":{{{p}}}"#))
}

/// getPlaylists' answer of (id, name, owner, public) playlists, each with no song; an empty list
/// is left out (responses.go:329-331).
pub(crate) fn playlists_body(list: &[(&str, &str, &str, bool)]) -> String {
    if list.is_empty() {
        return subsonic_ok(r#""playlists":{}"#);
    }
    let playlists: Vec<String> = list
        .iter()
        .map(|(id, name, owner, public)| {
            format!("{{{}}}", playlist_json(id, name, owner, *public, "", 0))
        })
        .collect();
    subsonic_ok(&format!(
        r#""playlists":{{"playlist":[{}]}}"#,
        playlists.join(",")
    ))
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

/// getSong's answer (server/subsonic/browsing.go GetSong): `child(id, "Marrow Choir", title)` rated
/// `rating` by this user, `userRating` left out for None (responses.go:99, omitempty), and starred
/// or not.
pub(crate) fn song_body(id: &str, title: &str, rating: Option<u8>, starred: bool) -> String {
    let mut song: serde_json::Value =
        serde_json::from_str(&child(id, "Marrow Choir", title)).expect("child writes JSON");
    let fields = song.as_object_mut().expect("child writes an object");
    match rating {
        Some(rating) => fields.insert("userRating".into(), json!(rating)),
        None => fields.remove("userRating"),
    };
    if !starred {
        fields.remove("starred");
    }
    subsonic_ok(&format!(r#""song":{song}"#))
}

/// getAlbum's answer for album `id` named `name` by "Marrow Choir", starred and rated `rating` by
/// this user, without songs. AlbumID3 always sends `userRating`, 0 when unrated (responses.go:269).
pub(crate) fn album_body(id: &str, name: &str, rating: u8) -> String {
    let album = json!({
        "id": id, "name": name, "artist": "Marrow Choir", "artistId": "2kD9pWq4ZxC1vBnM6yTjLs",
        "songCount": 12, "duration": 2841, "starred": "2026-11-03T09:12:40.004Z", "year": 2019,
        "userRating": rating, "genres": [], "musicBrainzId": "", "isCompilation": false,
        "sortName": "", "discTitles": [], "song": [],
    });
    subsonic_ok(&format!(r#""album":{album}"#))
}

/// The song search3 lists for alice: `id` titled `title` by "Marrow Choir" on album "Winter
/// Songs" ("al1"), rated `rating`, no path.
pub(crate) fn subsonic_song(id: &str, title: &str, rating: Option<u8>) -> SubsonicSong {
    SubsonicSong {
        id: id.into(),
        title: title.into(),
        artist: Some("Marrow Choir".into()),
        album: Some("Winter Songs".into()),
        album_id: Some("al1".into()),
        genre: None,
        duration: None,
        path: None,
        user_rating: rating,
        play_count: None,
        starred: None,
        music_brainz_id: None,
        suffix: None,
        bit_rate: None,
        size: None,
        year: None,
        track: None,
    }
}
