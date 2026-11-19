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
    pub body: String,
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

/// The request line, the headers and `Content-Length` bytes of body.
fn read_request(reader: &mut impl BufRead) -> Seen {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut content_type = None;
    let mut user_agent = None;
    let mut authorization = None;
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
        body: String::from_utf8(body).unwrap(),
    }
}
