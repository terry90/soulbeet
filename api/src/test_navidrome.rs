//! A Navidrome stand-in for api tests.

use soulbeet::navidrome::{NavidromeClient, NavidromeClientBuilder};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// What an endpoint with no answer set says (a Subsonic `ok` with no body).
const OK: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;

type Answers = Arc<Mutex<HashMap<String, VecDeque<String>>>>;

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
        let answers: Answers = Arc::new(Mutex::new(HashMap::from([(
            "/auth/login".to_string(),
            VecDeque::from([r#"{"token":"jwt"}"#.to_string()]),
        )])));
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
        self.answers.lock().unwrap().insert(
            endpoint.to_string(),
            bodies.iter().map(|body| body.to_string()).collect(),
        );
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
    let body = match answers.lock().unwrap().get_mut(&endpoint) {
        Some(bodies) if bodies.len() > 1 => bodies.pop_front(),
        Some(bodies) => bodies.front().cloned(),
        None => None,
    }
    .unwrap_or_else(|| OK.to_string());
    asked.lock().unwrap().push(target.to_string());
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    reader.get_mut().write_all(response.as_bytes())
}
