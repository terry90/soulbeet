//! A sidecar for the tests: the Web API subset the backend uses, answering from the saved live
//! answers in `fixtures/` and from state a test changes as it goes.

use crate::test_http::{serve_fn, Reply, Seen};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(crate) fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/src/airdcpp/fixtures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// How a peer's file list answers.
#[derive(Debug, Clone)]
pub(crate) enum List {
    /// Loads at its second look, its items answered with this.
    Loads(Value),
    /// Never loads.
    Pending,
    /// The sidecar's user has one open already.
    Open,
}

/// What a queue request is answered with.
#[derive(Debug, Clone)]
pub(crate) enum QueueAnswer {
    /// A new bundle, waiting.
    Bundle,
    Refused(u16, &'static str),
    /// A new bundle, its answer replaced by a proxy's page with this status.
    BundleBehindProxy(u16),
}

pub(crate) struct State {
    pub settings: Value,
    pub session: Value,
    pub hubs: Value,
    pub system: Value,
    pub hub_search: (u16, Value),
    pub results: Value,
    pub children: Value,
    /// By CID; a CID left out loads nothing.
    pub lists: HashMap<String, List>,
    /// The lists opened, by CID, and how often each was looked at.
    pub opened: HashMap<String, (String, u32)>,
    pub queue: Vec<QueueAnswer>,
    pub bundles: Vec<Value>,
    pub transfers: Vec<Value>,
    pub next_bundle: u64,
    /// Where a folder the sidecar makes appears for Soulful: None when the folders differ.
    pub shared_folder: Option<PathBuf>,
    /// The folder made holds a folder named `probe`, where Soulful then cannot write its file.
    pub probe_blocked: bool,
    pub hinted: (u16, Value),
    /// Every request is refused with a 401.
    pub unauthorized: bool,
    /// The owner of each search instance made, by id.
    pub instances: HashMap<u64, String>,
    /// AirDC++ restarted after the search was made: its id names another client's search.
    pub restarted: bool,
    /// What a file list session answers once loaded, instead of the folder asked for.
    pub list_moved_to: Option<String>,
    /// Bundles left out of the listing, as a page of an unordered map can: still answered by
    /// id and by path.
    pub unlisted: Vec<u64>,
}

impl State {
    pub(crate) fn new(download_directory: &str) -> State {
        let mut settings = fixture("settings_get");
        settings["download_directory"] = download_directory.into();
        // Sent at once; the saved answer waited 45 s for the sidecar's search interval.
        let mut hub_search = fixture("hub_search_queued");
        hub_search["queue_time"] = 0.into();
        State {
            settings,
            session: fixture("sessions_self"),
            hubs: fixture("hubs"),
            system: fixture("system_info"),
            hub_search: (200, hub_search),
            results: json!([]),
            children: json!({}),
            lists: HashMap::new(),
            opened: HashMap::new(),
            queue: vec![],
            bundles: vec![],
            transfers: vec![],
            next_bundle: 1000,
            shared_folder: None,
            probe_blocked: false,
            hinted: (200, fixture("hinted_user_away")),
            unauthorized: false,
            instances: HashMap::new(),
            restarted: false,
            list_moved_to: None,
            unlisted: vec![],
        }
    }

    /// A bundle as AirDC++ lists one, `state` its status id and `text` its status text.
    pub(crate) fn bundle(
        id: u64,
        target: &str,
        state: &str,
        text: &str,
        bytes: u64,
        speed: u64,
    ) -> Value {
        let mut bundle = fixture("bundle_waiting");
        bundle["id"] = id.into();
        bundle["target"] = target.into();
        bundle["status"]["id"] = state.into();
        bundle["status"]["str"] = text.into();
        bundle["downloaded_bytes"] = (bytes as f64).into();
        bundle["speed"] = (speed as f64).into();
        bundle
    }

    /// A download transfer of `target` from `cid`, as `GET /transfers` lists one.
    pub(crate) fn transfer(target: &str, cid: &str, state: &str, text: &str) -> Value {
        let mut transfer = fixture("transfer_timeout");
        transfer["target"] = target.into();
        transfer["user"]["cid"] = cid.into();
        transfer["status"]["id"] = state.into();
        transfer["status"]["str"] = text.into();
        transfer
    }

    fn answer(&mut self, seen: &Seen) -> Reply {
        let path = seen.target.strip_prefix("/api/v1/").unwrap_or(&seen.target);
        let parts: Vec<&str> = path.split('/').collect();
        let body: Value = serde_json::from_str(&seen.body).unwrap_or(Value::Null);
        let ok = |v: &Value| Reply {
            status: 200,
            body: v.to_string(),
        };
        let status = |status: u16, message: &str| Reply {
            status,
            body: json!({ "message": message }).to_string(),
        };
        let none = Reply {
            status: 204,
            body: String::new(),
        };
        if self.unauthorized {
            return Reply {
                status: 401,
                body: "Invalid username or password".into(),
            };
        }
        match (seen.method.as_str(), parts.as_slice()) {
            ("GET", ["sessions", "self"]) => ok(&self.session),
            ("GET", ["system", "system_info"]) => ok(&self.system),
            ("POST", ["settings", "get"]) => ok(&self.settings),
            ("GET", ["hubs"]) => ok(&self.hubs),
            ("GET", ["share", "stats"]) => none,
            ("POST", ["search"]) => {
                let id = self.instances.len() as u64 + 1;
                let owner = match body["owner_suffix"].as_str() {
                    Some(suffix) => format!("basic_auth:{suffix}"),
                    None => "basic_auth".to_string(),
                };
                self.instances.insert(id, owner.clone());
                ok(&json!({"id": id, "owner": owner}))
            }
            ("GET", ["search", id]) => {
                let owner = id.parse().ok().and_then(|id| self.instances.get(&id));
                match (owner, self.restarted) {
                    (Some(_), true) => ok(&json!({"id": 1, "owner": "session:7"})),
                    (Some(owner), false) => ok(&json!({"id": 1, "owner": owner})),
                    (None, _) => status(404, &format!("Entity {id} was not found")),
                }
            }
            ("POST", ["search", _, "hub_search"]) => {
                let (code, answer) = self.hub_search.clone();
                Reply {
                    status: code,
                    body: answer.to_string(),
                }
            }
            ("GET", ["search", _, "results", "0", _]) => ok(&self.results),
            ("GET", ["search", _, "results", id, "children"]) => {
                ok(self.children.get(*id).unwrap_or(&json!([])))
            }
            ("DELETE", ["search", _]) => none,
            ("GET", ["filelists", cid]) => {
                let opened = self.opened.get_mut(*cid);
                match (self.lists.get(*cid), opened) {
                    (Some(List::Open), _) => ok(&fixture("filelist_album_session_loaded")),
                    (_, None) => status(404, "Entity was not found"),
                    (Some(List::Loads(_)), Some((dir, looks))) => {
                        *looks += 1;
                        let mut session = match *looks {
                            1 => fixture("filelist_album_session_pending"),
                            _ => fixture("filelist_album_session_loaded"),
                        };
                        if *looks > 1 {
                            session["location"]["path"] = dir.clone().into();
                        }
                        // The sidecar's user moved it once Soulbeet read it.
                        if let (Some(moved), true) = (&self.list_moved_to, *looks > 2) {
                            session["location"]["path"] = moved.clone().into();
                        }
                        ok(&session)
                    }
                    (_, Some(_)) => ok(&fixture("filelist_album_session_pending")),
                }
            }
            ("POST", ["filelists"]) => {
                let cid = body["user"]["cid"].as_str().unwrap_or_default().to_string();
                let dir = body["directory"].as_str().unwrap_or_default().to_string();
                self.opened.insert(cid, (dir, 0));
                ok(&fixture("filelist_album_session_pending"))
            }
            ("GET", ["filelists", cid, "items", ..]) => match self.lists.get(*cid) {
                Some(List::Loads(items)) => ok(items),
                _ => status(404, "Entity was not found"),
            },
            ("DELETE", ["filelists", cid]) => {
                self.opened.remove(*cid);
                none
            }
            ("POST", ["queue", "bundles", "file"]) => {
                let answer = match self.queue.is_empty() {
                    true => QueueAnswer::Bundle,
                    false => self.queue.remove(0),
                };
                let proxied = match answer {
                    QueueAnswer::BundleBehindProxy(code) => Some(code),
                    _ => None,
                };
                match answer {
                    QueueAnswer::Refused(code, message) => status(code, message),
                    QueueAnswer::Bundle | QueueAnswer::BundleBehindProxy(_) => {
                        let id = self.next_bundle;
                        self.next_bundle += 1;
                        let dir = body["target_directory"].as_str().unwrap_or_default();
                        let name = body["target_name"].as_str().unwrap_or_default();
                        // As AirDC++ formats it: %% back to %.
                        let target = format!("{}{name}", dir.replace("%%", "%"));
                        self.bundles.push(State::bundle(
                            id,
                            &target,
                            "queued",
                            "Waiting (0.0%)",
                            0,
                            0,
                        ));
                        match proxied {
                            Some(code) => Reply {
                                status: code,
                                body: "<html><body>Gateway Time-out</body></html>".into(),
                            },
                            None => ok(&json!({"id": id, "merged": false})),
                        }
                    }
                }
            }
            ("GET", ["queue", "bundles", start, count]) => {
                let (start, count): (usize, usize) =
                    (start.parse().unwrap(), count.parse().unwrap());
                let listed: Vec<Value> = (self.bundles.iter())
                    .filter(|b| !self.unlisted.iter().any(|id| b["id"] == *id))
                    .cloned()
                    .collect();
                match listed.len() {
                    0 => ok(&json!([])),
                    n if start >= n => status(400, "Invalid range"),
                    n => ok(&Value::Array(listed[start..n.min(start + count)].to_vec())),
                }
            }
            ("GET", ["queue", "bundles", id]) => {
                let number: Option<u64> = id.parse().ok();
                match self.bundles.iter().find(|b| b["id"].as_u64() == number) {
                    Some(bundle) => ok(bundle),
                    None => status(404, &format!("Bundle {id} was not found")),
                }
            }
            ("POST", ["queue", "check_path_queued"]) => {
                let path = body["path"].as_str().unwrap_or_default();
                let found = self.bundles.iter().find(|b| b["target"] == path);
                let completed = |b: &Value| b["status"]["id"] == "completed";
                ok(
                    &json!({"bundle": found.map(|b| json!({"id": b["id"], "completed": completed(b)}))}),
                )
            }
            ("GET", ["transfers"]) => ok(&Value::Array(self.transfers.clone())),
            ("POST", ["queue", "bundles", id, "remove"]) => {
                let before = self.bundles.len();
                let number: Option<u64> = id.parse().ok();
                self.bundles.retain(|b| b["id"].as_u64() != number);
                match self.bundles.len() < before {
                    true => none,
                    false => status(404, &format!("Bundle {id} was not found")),
                }
            }
            ("POST", ["queue", "bundles", _, "search"]) => ok(&json!({"sent": 1})),
            ("POST", ["users", "search_hinted_user"]) => {
                let (code, answer) = self.hinted.clone();
                Reply {
                    status: code,
                    body: answer.to_string(),
                }
            }
            ("POST", ["filesystem", "directory"]) => {
                let made = body["path"].as_str().unwrap_or_default();
                if let Some(shared) = &self.shared_folder {
                    let name = made.trim_end_matches('/').rsplit('/').next().unwrap();
                    std::fs::create_dir_all(shared.join(name)).unwrap();
                    if self.probe_blocked {
                        std::fs::create_dir(shared.join(name).join("probe")).unwrap();
                    }
                }
                none
            }
            _ => status(
                400,
                "The supplied URL doesn't match any method in this API module",
            ),
        }
    }
}

/// A fake sidecar: its base URL, its state, and the requests it got.
pub(crate) struct Fake {
    pub url: String,
    pub state: Arc<Mutex<State>>,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    pub(crate) fn start(state: State) -> Fake {
        let state = Arc::new(Mutex::new(state));
        let answering = state.clone();
        let (url, seen) = serve_fn(move |seen| answering.lock().unwrap().answer(seen));
        Fake { url, state, seen }
    }

    /// The requests of `method` whose path, after `/api/v1/`, starts with `prefix`.
    pub(crate) fn asked(&self, method: &str, prefix: &str) -> Vec<Seen> {
        (self.seen.lock().unwrap().iter())
            .filter(|s| s.method == method && s.target.starts_with(&format!("/api/v1/{prefix}")))
            .cloned()
            .collect()
    }
}
