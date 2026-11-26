//! An in-process HTTP server that answers with scripted replies, for the
//! crate's client tests.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One scripted answer.
pub(crate) struct Reply {
    pub status: u16,
    pub body: String,
}

/// One request the server received.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Seen {
    pub method: String,
    pub target: String,
    pub content_type: Option<String>,
    pub user_agent: Option<String>,
    pub authorization: Option<String>,
    /// The `Range` header, for `serve_file`.
    pub range: Option<String>,
    pub if_range: Option<String>,
    pub body: String,
    /// When its request line arrived.
    pub at: std::time::Instant,
}

/// Answers the n-th connection with replies[n] (`Connection: close`, JSON
/// content type) and records each request; base URL `http://127.0.0.1:{port}`.
pub(crate) fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    serve_with_headers(replies.into_iter().map(|r| (r, vec![])).collect())
}

/// serve, each reply sent with its own extra headers (name, value).
pub(crate) fn serve_with_headers(
    replies: Vec<(Reply, Vec<(&'static str, String)>)>,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    std::thread::spawn(move || {
        for (reply, headers) in replies {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let request = read_request(&mut reader);
            recorded.lock().unwrap().push(request);
            let reason = reqwest::StatusCode::from_u16(reply.status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            let extra: String = (headers.iter())
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            let response = format!(
                "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\n\
                 {extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.status,
                reply.body.len(),
                reply.body
            );
            reader.get_mut().write_all(response.as_bytes()).unwrap();
        }
    });
    (url, seen)
}

/// Answers every connection with what `answer` gives for its request (`Connection: close`, JSON
/// content type), each on its own thread, and records each request; base URL
/// `http://127.0.0.1:{port}`. For a double that answers by route, as a service does.
pub(crate) fn serve_fn(
    answer: impl Fn(&Seen) -> Reply + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let answer = Arc::new(answer);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (answer, recorded) = (answer.clone(), recorded.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let request = read_request(&mut reader);
                let reply = answer(&request);
                recorded.lock().unwrap().push(request);
                let response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reason(reply.status),
                    reply.body.len(),
                    reply.body
                );
                // The client may hang up first, as on a timeout.
                let _ = reader.get_mut().write_all(response.as_bytes());
            });
        }
    });
    (url, seen)
}

/// The request line, the headers and `Content-Length` bytes of body.
fn read_request(reader: &mut impl BufRead) -> Seen {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let at = std::time::Instant::now();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut content_type = None;
    let mut user_agent = None;
    let mut authorization = None;
    let mut range = None;
    let mut if_range = None;
    let mut content_length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap();
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("user-agent") {
            user_agent = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("range") {
            range = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("if-range") {
            if_range = Some(value.to_string());
        }
    }

    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).unwrap();
    Seen {
        method,
        target,
        content_type,
        user_agent,
        authorization,
        range,
        if_range,
        body: String::from_utf8(body).unwrap(),
        at,
    }
}

/// A file `serve_file` serves, as a download server does: its bytes, and the ETag it sends.
pub(crate) struct ServedFile {
    pub body: Vec<u8>,
    pub etag: Option<String>,
}

/// How `serve_file` answers one connection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum FileAnswer {
    /// The file, or the part a `Range` asks for (206) unless an `If-Range` names another ETag.
    Whole,
    /// As `Whole`, the connection closed after this many bytes of the body.
    Cut(usize),
    /// As `Whole`, then nothing after this many bytes of the body, the connection held open.
    Stall(usize),
    /// The whole file with 200 whatever `Range` asks, as a server that ignores it, the
    /// connection closed after this many bytes of the body.
    NoRange(usize),
    /// This status with an empty body.
    Status(u16),
}

/// Answers the n-th connection to `file` as answers[n] says (`Connection: close`) and records
/// each request; base URL `http://127.0.0.1:{port}`. Reads `Range: bytes=<start>-[<end>]`.
pub(crate) fn serve_file(
    file: ServedFile,
    answers: Vec<FileAnswer>,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let file = Arc::new(file);
    std::thread::spawn(move || {
        for answer in answers {
            let (stream, _) = listener.accept().unwrap();
            let file = file.clone();
            let recorded = recorded.clone();
            // Its own thread, so a stalled answer does not hold up the next connection.
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream);
                let request = read_request(&mut reader);
                let response = file_response(&file, &request, answer);
                recorded.lock().unwrap().push(request);
                let stream = reader.get_mut();
                // The client may hang up first, as on a cancel.
                let _ = stream.write_all(&response);
                let _ = stream.flush();
                if let FileAnswer::Stall(_) = answer {
                    std::thread::sleep(std::time::Duration::from_secs(600));
                }
            });
        }
    });
    (url, seen)
}

/// The bytes `serve_file` sends for `request`: head and body, the body cut as `answer` says.
fn file_response(file: &ServedFile, request: &Seen, answer: FileAnswer) -> Vec<u8> {
    let total = file.body.len();
    let etag = file
        .etag
        .as_ref()
        .map(|e| format!("ETag: {e}\r\n"))
        .unwrap_or_default();
    if let FileAnswer::Status(status) = answer {
        return format!(
            "HTTP/1.1 {status} {}\r\n{etag}Content-Length: 0\r\nConnection: close\r\n\r\n",
            reason(status)
        )
        .into_bytes();
    }
    let same_file = match (&request.if_range, &file.etag) {
        (Some(asked), Some(etag)) => asked == etag,
        (Some(_), None) => false,
        (None, _) => true,
    };
    let range = request
        .range
        .as_deref()
        .filter(|_| same_file && !matches!(answer, FileAnswer::NoRange(_)))
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'))
        .and_then(|(start, end)| {
            let start: usize = start.parse().ok()?;
            let end: usize = if end.is_empty() {
                total.checked_sub(1)?
            } else {
                end.parse::<usize>().ok()?.min(total.checked_sub(1)?)
            };
            (start <= end).then_some((start, end))
        });
    let (status, extra, body) = match range {
        Some((start, end)) => (
            206,
            format!("Content-Range: bytes {start}-{end}/{total}\r\n"),
            &file.body[start..=end],
        ),
        None => (200, String::new(), &file.body[..]),
    };
    let mut out = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/octet-stream\r\n\
         Accept-Ranges: bytes\r\n{etag}{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    )
    .into_bytes();
    let sent = match answer {
        FileAnswer::Cut(n) | FileAnswer::Stall(n) | FileAnswer::NoRange(n) => {
            &body[..n.min(body.len())]
        }
        _ => body,
    };
    out.extend_from_slice(sent);
    out
}

fn reason(status: u16) -> &'static str {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> ServedFile {
        ServedFile {
            body: (0..=255u8).collect(),
            etag: Some("\"v1\"".into()),
        }
    }

    async fn get(url: &str, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut request = reqwest::Client::new().get(format!("{url}/f"));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.unwrap()
    }

    #[tokio::test]
    async fn a_file_is_served_whole_with_its_etag_and_binary_bytes() {
        let (url, seen) = serve_file(file(), vec![FileAnswer::Whole]);
        let answer = get(&url, &[]).await;
        assert_eq!(answer.status(), 200);
        assert_eq!(answer.headers()["etag"], "\"v1\"");
        assert_eq!(answer.bytes().await.unwrap().to_vec(), file().body);
        assert_eq!(seen.lock().unwrap()[0].target, "/f");
    }

    #[tokio::test]
    async fn a_range_gets_its_part_unless_the_file_changed() {
        let (url, seen) = serve_file(
            file(),
            vec![FileAnswer::Whole, FileAnswer::Whole, FileAnswer::Whole],
        );
        let part = get(&url, &[("Range", "bytes=250-"), ("If-Range", "\"v1\"")]).await;
        assert_eq!(part.status(), 206);
        assert_eq!(part.headers()["content-range"], "bytes 250-255/256");
        assert_eq!(
            part.bytes().await.unwrap().to_vec(),
            vec![250, 251, 252, 253, 254, 255]
        );

        let bounded = get(&url, &[("Range", "bytes=1-2")]).await;
        assert_eq!(bounded.bytes().await.unwrap().to_vec(), vec![1, 2]);

        let changed = get(&url, &[("Range", "bytes=250-"), ("If-Range", "\"v0\"")]).await;
        assert_eq!(changed.status(), 200);
        assert_eq!(changed.bytes().await.unwrap().len(), 256);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[2].range.as_deref(), Some("bytes=250-"));
        assert_eq!(seen[2].if_range.as_deref(), Some("\"v0\""));
    }

    #[tokio::test]
    async fn a_cut_answer_ends_early_and_a_stalled_one_sends_nothing_more() {
        let (url, _) = serve_file(
            file(),
            vec![
                FileAnswer::Cut(10),
                FileAnswer::Stall(20),
                FileAnswer::Status(403),
            ],
        );
        let mut cut = get(&url, &[]).await;
        let mut got = Vec::new();
        let ended = loop {
            match cut.chunk().await {
                Ok(Some(bytes)) => got.extend_from_slice(&bytes),
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        assert!(ended.is_err(), "a body shorter than its length is an error");
        assert_eq!(got, (0..10u8).collect::<Vec<_>>());

        let mut stalled = get(&url, &[]).await;
        let mut got = Vec::new();
        let waited = tokio::time::timeout(std::time::Duration::from_millis(300), async {
            while let Ok(Some(bytes)) = stalled.chunk().await {
                got.extend_from_slice(&bytes);
            }
        })
        .await;
        assert!(waited.is_err(), "the stalled body never ends");
        assert_eq!(got.len(), 20);

        assert_eq!(get(&url, &[]).await.status(), 403);
    }
}
