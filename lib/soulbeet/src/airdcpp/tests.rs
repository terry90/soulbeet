//! The backend against a fake sidecar answering from the saved live answers.

use super::fake::{fixture, Fake, List, QueueAnswer, State};
use super::progress::Data;
use super::*;
use crate::transfer_book::tests::MemoryBook;
use crate::{BookEntry, DownloadBackend};
use serde_json::{json, Value};
use shared::airdcpp::{ItemData, Source, BACKEND_ID};
use shared::download::{DownloadState, EnqueueStatus, FileFacts, SearchState};
use shared::failure::FailureCode;
use shared::metadata::{Album, Track};

const ROOT: &str = "/downloads/";
const TTH: &str = "CNPPZX6TSGAVDPCOAE776LTWHPEI2RET3ZMJJ2A";
const DESTINATION: &str = "Spiderland [ab12cd34]";

fn fast() -> Timing {
    Timing {
        collect: Duration::from_millis(30),
        deadline: Duration::from_secs(10),
        list_wait: Duration::from_millis(300),
        list_poll: Duration::from_millis(10),
        give_up: Duration::from_millis(600),
        alternates_after: Duration::from_millis(100),
        enqueue_spacing: Duration::ZERO,
        min_hub_interval: Duration::ZERO,
        listing_kept: Duration::ZERO,
    }
}

struct Rig {
    root: tempfile::TempDir,
    book: Arc<MemoryBook>,
    engine: Arc<Engine>,
    fake: Fake,
}

impl Rig {
    fn new(change: impl FnOnce(&mut State)) -> Rig {
        Rig::with_timing(fast(), change)
    }

    fn with_timing(timing: Timing, change: impl FnOnce(&mut State)) -> Rig {
        let mut state = State::new(ROOT);
        state.settings["min_search_interval"] = 0.into();
        change(&mut state);
        let root = tempfile::tempdir().unwrap();
        let book = Arc::new(MemoryBook::default());
        let engine = Engine::new(
            EngineSettings {
                download_path: root.path().to_path_buf(),
                book: book.clone(),
            },
            timing,
        );
        Rig {
            root,
            book,
            engine,
            fake: Fake::start(state),
        }
    }

    fn connection(&self) -> Connection {
        Connection {
            url: self.fake.url.clone(),
            username: "soulbeet".into(),
            password: "pass-n0t-logged".into(),
        }
    }

    fn backend(&self) -> AirdcppBackend {
        AirdcppBackend::new(
            self.engine.clone(),
            Sidecar::new(self.connection()).unwrap(),
        )
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.fake.state.lock().unwrap()
    }
}

fn track(artist: &str, title: &str) -> Track {
    Track {
        id: title.into(),
        title: title.into(),
        artist: artist.into(),
        album_id: None,
        album_title: None,
        release_date: None,
        duration: None,
        mbid: None,
        release_mbid: None,
    }
}

fn album(artist: &str, title: &str) -> Album {
    Album {
        id: "a".into(),
        title: title.into(),
        artist: artist.into(),
        release_date: None,
        mbid: None,
        cover_url: None,
        release_types: None,
        first_release_date: None,
    }
}

fn spiderland() -> SearchSpec {
    let titles = [
        "Breadcrumb Trail",
        "Nosferatu Man",
        "Don, Aman",
        "Washer",
        "For Dinner...",
        "Good Morning, Captain",
    ];
    SearchSpec::of(
        Some(album("Slint", "Spiderland")),
        titles.iter().map(|t| track("Slint", t)).collect(),
    )
}

/// The users of the saved album search: (nick, hub URL, CID) of each.
fn album_users() -> Vec<(String, String, String)> {
    let children = fixture("search_album_children");
    let mut users = Vec::new();
    for list in children.as_object().unwrap().values() {
        for child in list.as_array().unwrap() {
            let user = &child["user"];
            let entry = (
                user["nicks"].as_str().unwrap().to_string(),
                user["hub_url"].as_str().unwrap().to_string(),
                user["cid"].as_str().unwrap().to_string(),
            );
            if !users.contains(&entry) {
                users.push(entry);
            }
        }
    }
    users
}

fn cid_of(nick: &str, hub: &str) -> String {
    (album_users().into_iter())
        .find(|(n, h, _)| n == nick && h == hub)
        .unwrap()
        .2
}

async fn ended(backend: &AirdcppBackend, id: &str) -> SearchResult {
    for _ in 0..500 {
        let result = backend.poll_search(id).await.unwrap();
        if result.state != SearchState::InProgress {
            return result;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the search never ended");
}

fn body(seen: &crate::test_http::Seen) -> Value {
    serde_json::from_str(&seen.body).unwrap()
}

#[tokio::test]
async fn the_backend_goes_by_its_meta_and_a_rebuilt_handle_shares_the_engine() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let meta = shared::backends::meta("airdcpp").unwrap();
    assert_eq!((backend.id(), backend.name()), (meta.id, meta.name));
    assert_eq!(backend.id(), BACKEND_ID);
    let cell = EngineCell::new();
    let settings = EngineSettings {
        download_path: rig.root.path().to_path_buf(),
        book: rig.book.clone(),
    };
    let first = cell.get_or_start(settings.clone()).await;
    let again = cell.get_or_start(settings).await;
    assert!(Arc::ptr_eq(&first, &again));
}

#[tokio::test]
async fn an_album_search_reads_the_folders_file_list_and_scores_it_as_slskd_does() {
    let rig = Rig::new(|state| {
        state.results = fixture("search_album_results");
        state.children = fixture("search_album_children");
        let lister = cid_of("peer01", "nmdcs://hub-one.example:411");
        state
            .lists
            .insert(lister, List::Loads(fixture("filelist_album_items")));
    });
    let backend = rig.backend();
    let id = backend.start_search(&spiderland()).await.unwrap();
    let result = ended(&backend, &id).await;

    let sent = rig.fake.asked("POST", "search/1/hub_search");
    assert_eq!(
        body(&sent[0]),
        json!({"query": {"pattern": "Slint Spiderland", "file_type": "any"}, "priority": 4})
    );
    let best = &result.groups[0];
    let lister = cid_of("peer01", "nmdcs://hub-one.example:411");
    assert_eq!(best.source, format!("{lister}@nmdcs://hub-one.example:411"));
    assert_eq!(best.group_id, "/music/Slint/1991 – Spiderland");
    assert_eq!(best.items.len(), 6, "{best:?}");
    assert_eq!(best.quality, "flac");
    assert!(best.score > 0.7, "{}", best.score);
    for item in &best.items {
        assert_eq!(item.backend, BACKEND_ID);
        assert_eq!(item.source, best.source);
        assert_eq!(item.facts.folder_audio_files, Some(6));
        assert_eq!(item.facts.backend_facts["listed"], true);
        assert_eq!(item.facts.backend_facts["nick"], "peer01");
        let data: ItemData = serde_json::from_str(item.backend_data.as_deref().unwrap()).unwrap();
        assert!(shared::airdcpp::is_tiger_hash(&data.tth));
        assert!(item.id.starts_with("/music/Slint/1991 – Spiderland/"));
    }
    // The cover, the cue, the log and the scans folder are no track.
    assert!(!best
        .items
        .iter()
        .any(|i| i.id.ends_with(".jpg") || i.id.contains("Scans")));
    // Every list it opened, it closed.
    let opened = rig.fake.asked("POST", "filelists");
    assert!(!opened.is_empty() && opened.len() <= 4, "{opened:?}");
    assert_eq!(rig.fake.asked("DELETE", "filelists/").len(), opened.len());
    assert_eq!(rig.fake.asked("DELETE", "search/1").len(), 1);
    assert_eq!(result.stats.query_text, "Slint Spiderland");
    let made = body(&rig.fake.asked("POST", "search")[0]);
    assert_eq!(made["expiration"], 10);
    assert!(made["owner_suffix"]
        .as_str()
        .unwrap()
        .starts_with("soulbeet-"));
}

#[tokio::test]
async fn a_search_whose_id_another_search_took_in_a_restart_is_left_alone() {
    let rig = Rig::new(|state| {
        state.results = fixture("search_track_results");
        state.restarted = true;
    });
    let backend = rig.backend();
    let id = backend.start_search(&glory_box()).await.unwrap();
    let mut polled = backend.poll_search(&id).await;
    for _ in 0..300 {
        if !matches!(&polled, Ok(r) if r.state == SearchState::InProgress) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        polled = backend.poll_search(&id).await;
    }
    let failure = polled.unwrap_err().failure(Stage::SourceSearch, "m");
    assert_eq!(failure.code, FailureCode::SearchLost);
    assert!(rig.fake.asked("GET", "search/1/results").is_empty());
    assert!(rig.fake.asked("DELETE", "search/").is_empty());
}

#[tokio::test]
async fn a_list_its_user_moved_meanwhile_stays_open() {
    let lister = cid_of("peer01", "nmdcs://hub-one.example:411");
    let rig = Rig::new(|state| {
        state.results = fixture("search_album_results");
        state.children = fixture("search_album_children");
        state
            .lists
            .insert(lister.clone(), List::Loads(fixture("filelist_album_items")));
        state.list_moved_to = Some("/music/".into());
    });
    let backend = rig.backend();
    let id = backend.start_search(&spiderland()).await.unwrap();
    let result = ended(&backend, &id).await;
    assert!(!result.groups.is_empty());
    assert!(rig
        .fake
        .asked("DELETE", &format!("filelists/{lister}"))
        .is_empty());
}

#[tokio::test]
async fn a_list_that_never_loads_leaves_the_search_files_and_an_open_one_is_left_alone() {
    let lister = cid_of("peer01", "nmdcs://hub-one.example:411");
    let rig = Rig::new(|state| {
        state.results = fixture("search_album_results");
        state.children = fixture("search_album_children");
        state.lists.insert(lister.clone(), List::Pending);
    });
    let backend = rig.backend();
    let id = backend.start_search(&spiderland()).await.unwrap();
    let result = ended(&backend, &id).await;
    let source = format!("{lister}@nmdcs://hub-one.example:411");
    let group = result.groups.iter().find(|g| g.source == source).unwrap();
    assert_eq!(group.items.len(), 4, "the four files the search found");
    for item in &group.items {
        assert_eq!(item.facts.folder_audio_files, None);
        assert_eq!(item.facts.backend_facts["listed"], false);
    }
    let closed = rig.fake.asked("DELETE", &format!("filelists/{lister}"));
    assert_eq!(closed.len(), 1, "the list Soulbeet opened is closed");

    let rig = Rig::new(|state| {
        state.results = fixture("search_album_results");
        state.children = fixture("search_album_children");
        state.lists.insert(lister.clone(), List::Open);
    });
    let backend = rig.backend();
    let id = backend.start_search(&spiderland()).await.unwrap();
    ended(&backend, &id).await;
    let mine = format!("filelists/{lister}");
    assert!(rig
        .fake
        .asked("POST", "filelists")
        .iter()
        .all(|s| !s.body.contains(&lister)));
    assert!(
        rig.fake.asked("DELETE", &mine).is_empty(),
        "the user's own list stays"
    );
}

/// The sources of the saved track search whose peer takes no incoming connection.
fn passive_sources() -> Vec<String> {
    let mut users: Vec<Value> = (fixture("search_track_results").as_array().unwrap().iter())
        .map(|r| r["users"]["user"].clone())
        .collect();
    for list in fixture("search_track_children")
        .as_object()
        .unwrap()
        .values()
    {
        users.extend(list.as_array().unwrap().iter().map(|c| c["user"].clone()));
    }
    (users.iter())
        .filter(|u| {
            u["flags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "passive")
        })
        .map(|u| {
            format!(
                "{}@{}",
                u["cid"].as_str().unwrap(),
                u["hub_url"].as_str().unwrap()
            )
        })
        .collect()
}

fn glory_box() -> SearchSpec {
    SearchSpec::of(None, vec![track("Portishead", "Glory Box")])
}

#[tokio::test]
async fn a_track_search_reads_no_list_and_leaves_out_passive_peers_while_passive() {
    let passive = passive_sources();
    assert!(!passive.is_empty());
    for (mode, passive_here) in [(2, true), (0, false)] {
        let rig = Rig::new(|state| {
            state.settings["connection_mode_v4"] = mode.into();
            state.results = fixture("search_track_results");
            state.children = fixture("search_track_children");
        });
        let backend = rig.backend();
        let id = backend.start_search(&glory_box()).await.unwrap();
        let result = ended(&backend, &id).await;
        assert!(!result.groups.is_empty());
        assert!(rig.fake.asked("POST", "filelists").is_empty());
        let offered_passive = (result.groups.iter()).any(|g| passive.contains(&g.source));
        assert_eq!(offered_passive, !passive_here, "mode {mode}");
        for group in &result.groups {
            for item in &group.items {
                assert!(item.size.is_some_and(|s| s > 0));
                assert_eq!(item.title, "Glory Box");
                assert!(item.facts.backend_facts["tth"].is_string());
                assert_eq!(item.duration, None);
            }
        }
    }
}

#[test]
fn the_nmdc_slot_placeholder_says_nothing_of_free_slots() {
    assert_eq!(search::free_slot(0.0, 255.0), None);
    assert_eq!(search::free_slot(15.0, 270.0), Some(true));
    assert_eq!(search::free_slot(0.0, 21.0), Some(false));
}

#[tokio::test]
async fn a_search_no_hub_takes_or_the_sidecar_refuses_fails_with_its_code() {
    for (answer, code) in [
        (
            (503, json!({"message": "Search queue overflow"})),
            FailureCode::SearchRateLimited,
        ),
        (
            (
                200,
                json!({"queue_time": 0, "queued_count": 0, "search_id": "1", "query": {}}),
            ),
            FailureCode::BackendUnreachable,
        ),
        (
            (400, json!({"message": "Hub was not found"})),
            FailureCode::BackendUnreachable,
        ),
    ] {
        let rig = Rig::new(|state| state.hub_search = answer);
        let backend = rig.backend();
        let id = backend.start_search(&glory_box()).await.unwrap();
        let mut polled = backend.poll_search(&id).await;
        for _ in 0..300 {
            if !matches!(&polled, Ok(r) if r.state == SearchState::InProgress) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            polled = backend.poll_search(&id).await;
        }
        let failure = polled.unwrap_err().failure(Stage::SourceSearch, "m");
        assert_eq!(failure.code, code);
    }
}

#[test]
fn the_text_drops_accented_words_and_edition_brackets_and_refuses_a_short_one() {
    let spec = |artist: &str, title: &str| {
        SearchSpec::of(
            Some(album(artist, title)),
            vec![track(artist, "a"), track(artist, "b")],
        )
    };
    assert_eq!(
        search::query_text(&spec("Sigur Rós", "Ágætis byrjun (Deluxe Edition)")).unwrap(),
        "Sigur byrjun"
    );
    assert_eq!(
        search::query_text(&spec("Кино", "Группа крови")).unwrap(),
        "Кино Группа крови"
    );
    assert_eq!(
        search::query_text(&spec("Air", "Moon Safari (Live)")).unwrap(),
        "Air Moon Safari (Live)"
    );
    let short = search::query_text(&SearchSpec::of(None, vec![track("PNL", "Á")]));
    assert!(
        matches!(short, Err(AirdcppError::QueryRejected(_))),
        "{short:?}"
    );
    let mut own = spec("x", "y");
    own.text = Some("Slint Spiderland".into());
    assert_eq!(search::query_text(&own).unwrap(), "Slint Spiderland");
}

fn source() -> Source {
    Source {
        cid: cid_of("peer01", "nmdcs://hub-one.example:411"),
        hub_url: "nmdcs://hub-one.example:411".into(),
    }
}

fn item(path: &str, size: u64) -> DownloadableItem {
    let source = source();
    let data = ItemData {
        cid: source.cid.clone(),
        hub_url: source.hub_url.clone(),
        nick: "peer01".into(),
        hub_name: "Hub One".into(),
        tth: TTH.into(),
        size,
        listed: true,
    };
    DownloadableItem {
        id: path.into(),
        backend: BACKEND_ID.into(),
        source: source.to_string(),
        title: "Washer".into(),
        artist: "Slint".into(),
        album: "Spiderland".into(),
        size: Some(size),
        duration: None,
        quality: "flac".into(),
        quality_score: 1.0,
        backend_data: Some(serde_json::to_string(&data).unwrap()),
        match_score: 1.0,
        below_floor: false,
        facts: FileFacts::default(),
    }
}

async fn queued(
    backend: &AirdcppBackend,
    items: Vec<DownloadableItem>,
    destination: &str,
) -> Vec<QueuedDownload> {
    backend
        .download(items, destination, "request-1", "user-1")
        .await
        .unwrap()
}

#[tokio::test]
async fn a_file_is_booked_then_queued_under_the_name_soulful_reads() {
    let rig = Rig::new(|state| {
        state.queue = vec![
            QueueAnswer::Bundle,
            QueueAnswer::Refused(
                400,
                "A file with a different size already exists in the queue",
            ),
        ]
    });
    let backend = rig.backend();
    let answers = queued(
        &backend,
        vec![
            item("/music/Slint/1991 – Spiderland/04 - Washer.flac", 43314622),
            item(
                "/music/Slint/1991 – Spiderland/05 - For Dinner....flac",
                21057449,
            ),
        ],
        DESTINATION,
    )
    .await;
    assert_eq!(answers[0].status, EnqueueStatus::Enqueued);
    assert_eq!(answers[0].id, "1000");
    let EnqueueStatus::Failed(failure) = &answers[1].status else {
        panic!("{answers:?}");
    };
    assert_eq!(failure.code, FailureCode::EnqueueFailed);

    let sent = rig.fake.asked("POST", "queue/bundles/file");
    let first = body(&sent[0]);
    assert_eq!(first["target_directory"], format!("{ROOT}{DESTINATION}/"));
    assert_eq!(first["target_name"], "04 - Washer.flac");
    assert_eq!(first["size"], 43314622);
    assert_eq!(first["tth"], TTH);
    assert_eq!(first["user"]["cid"], source().cid);
    assert_eq!(body(&sent[1])["target_name"], "05 - For Dinner....flac");

    // The refused file's entry is gone; the queued one is bound to its bundle.
    let entries = rig.book.0.lock().unwrap().clone();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].transfer_id.as_deref(), Some("1000"));
    assert_eq!(entries[0].request, "request-1");
    let data = Data::of(&entries[0]).unwrap();
    assert_eq!(
        data.sidecar_target,
        format!("{ROOT}{DESTINATION}/04 - Washer.flac")
    );
}

/// AirDC++'s `PathUtil::cleanPathChars` on Linux for a file name, as 2.14.0 writes it.
fn validate_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if (1..=31).contains(&(c as u32)) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while let Some(i) = s.find("\\.\\") {
        s.replace_range(i + 1..i + 3, "");
    }
    while let Some(i) = s.find("/./") {
        s.replace_range(i + 1..i + 3, "");
    }
    while let Some(i) = s.find("\\\\") {
        s.replace_range(i + 1..i + 2, "");
    }
    while let Some(i) = s.find("//") {
        s.replace_range(i + 1..i + 2, "");
    }
    s = s.replace("\\..\\", "\\___").replace("/../", "/___");
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let before_separator = matches!(chars.get(i + 1), Some('\\') | Some('/'));
        out.push(if *c == '.' && before_separator && i != 0 {
            '_'
        } else {
            *c
        });
    }
    out.replace(['/', '\\'], "_")
}

#[test]
fn a_sanitized_name_is_one_airdcpp_saves_unchanged() {
    for remote in [
        "01 - Breadcrumb Trail.flac",
        "05 - For Dinner....flac",
        "Grupa krwi: live?.flac",
        "01 - Группа крови.flac",
        "trailing dots...",
        "a\\b/c.mp3",
        "\u{4e2d}\u{6587} * <x>.mp3",
    ] {
        let name = shared::download::sanitize_filename(remote);
        assert_eq!(validate_name(&name), name, "{remote}");
    }
}

#[tokio::test]
async fn a_name_taken_in_the_folder_or_by_the_call_is_queued_as_a_ticks_copy() {
    let rig = Rig::new(|_| {});
    std::fs::create_dir_all(rig.root.path().join(DESTINATION)).unwrap();
    std::fs::write(
        rig.root
            .path()
            .join(DESTINATION)
            .join("06 - Good Morning, Captain.flac"),
        b"x",
    )
    .unwrap();
    let backend = rig.backend();
    let answers = queued(
        &backend,
        vec![
            item("/a/04 - Washer.flac", 10),
            item("/b/04 - Washer.flac", 11),
            item("/a/06 - Good Morning, Captain.flac", 12),
        ],
        DESTINATION,
    )
    .await;
    assert!(
        answers.iter().all(|a| a.status == EnqueueStatus::Enqueued),
        "{answers:?}"
    );
    let names: Vec<String> = (rig.fake.asked("POST", "queue/bundles/file").iter())
        .map(|s| body(s)["target_name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names[0], "04 - Washer.flac");
    assert!(
        shared::download::is_ticks_copy(&names[1], "04 - Washer.flac"),
        "{names:?}"
    );
    assert!(
        shared::download::is_ticks_copy(&names[2], "06 - Good Morning, Captain.flac"),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_percent_is_sent_doubled_and_a_parameter_folder_is_refused_unsent() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let answers = queued(
        &backend,
        vec![item("/a/01.flac", 10)],
        "100% Hits [ab12cd34]",
    )
    .await;
    assert_eq!(answers[0].status, EnqueueStatus::Enqueued);
    let sent = body(&rig.fake.asked("POST", "queue/bundles/file")[0]);
    assert_eq!(
        sent["target_directory"],
        format!("{ROOT}100%% Hits [ab12cd34]/")
    );
    let entry = rig.book.0.lock().unwrap()[0].clone();
    assert_eq!(
        Data::of(&entry).unwrap().sidecar_target,
        format!("{ROOT}100% Hits [ab12cd34]/01.flac")
    );
    // Listed under the target AirDC++ reports, with one %.
    let rows = backend
        .get_downloads_for(&[source().to_string()])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let answers = queued(&backend, vec![item("/a/01.flac", 10)], "%[x] [ab12cd34]").await;
    let EnqueueStatus::Failed(failure) = &answers[0].status else {
        panic!("{answers:?}");
    };
    assert_eq!(failure.code, FailureCode::InvalidInput);
    assert!(rig.fake.asked("POST", "queue/bundles/file").is_empty());
    assert!(rig.book.0.lock().unwrap().is_empty());
}

/// Queues one file and returns its bundle's target as AirDC++ names it.
async fn one_queued(backend: &AirdcppBackend) -> String {
    let answers = queued(
        backend,
        vec![item("/a/04 - Washer.flac", 1000)],
        DESTINATION,
    )
    .await;
    assert_eq!(answers[0].status, EnqueueStatus::Enqueued);
    format!("{ROOT}{DESTINATION}/04 - Washer.flac")
}

fn set_bundle(rig: &Rig, target: &str, state: &str, text: &str, bytes: u64, speed: u64) {
    rig.state().bundles = vec![State::bundle(1000, target, state, text, bytes, speed)];
}

async fn row(backend: &AirdcppBackend) -> DownloadProgress {
    backend
        .get_download(&source().to_string(), "1000")
        .await
        .unwrap()
        .expect("the bundle is listed")
}

/// A saved bundle answer, as the bundle 1000 of `target`.
fn saved_bundle(name: &str, target: &str) -> Value {
    let mut bundle = fixture(name);
    bundle["id"] = 1000.into();
    bundle["target"] = target.into();
    bundle
}

#[tokio::test]
async fn a_bundles_state_reads_as_its_row() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let answers = queued(
        &backend,
        vec![item("/a/04 - Washer.flac", 3441572)],
        DESTINATION,
    )
    .await;
    assert_eq!(answers[0].status, EnqueueStatus::Enqueued);
    let target = format!("{ROOT}{DESTINATION}/04 - Washer.flac");

    rig.state().bundles = vec![saved_bundle("bundle_running", &target)];
    let r = row(&backend).await;
    assert_eq!(r.state, DownloadState::InProgress);
    assert_eq!((r.transferred, r.size), (1429215, 3441572));
    assert_eq!(r.request.as_deref(), Some("request-1"));
    assert_eq!(r.item, "/a/04 - Washer.flac");

    set_bundle(&rig, &target, "downloaded", "Downloaded", 3441572, 0);
    let r = row(&backend).await;
    assert_eq!((r.state, r.percent), (DownloadState::InProgress, 100.0));

    rig.state().bundles = vec![saved_bundle("bundle_completed", &target)];
    assert_eq!(row(&backend).await.state, DownloadState::Completed);

    // A bundle the book does not hold is never listed.
    rig.state().bundles.push(State::bundle(
        5,
        "/downloads/own/x.flac",
        "queued",
        "",
        0,
        0,
    ));
    let rows = backend
        .get_downloads_for(&[source().to_string()])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn a_slot_wait_is_queued_at_the_source_and_failed_connections_count_as_attempts() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let target = one_queued(&backend).await;
    let cid = source().cid;
    set_bundle(&rig, &target, "queued", "Waiting (0.0%)", 0, 0);
    let mut slot_wait = fixture("transfer_no_slots");
    slot_wait["target"] = target.clone().into();
    slot_wait["user"]["cid"] = cid.clone().into();
    rig.state().transfers = vec![slot_wait];
    let r = row(&backend).await;
    assert_eq!(r.state, DownloadState::Queued);
    assert!(r.queued_at_source);
    assert_eq!(r.queue_position, Some(7));
    assert_eq!(
        backend
            .queue_position(&source().to_string(), "1000")
            .await
            .unwrap(),
        Some(7)
    );

    for (state, text) in [
        ("failed", "Connection timeout"),
        ("waiting", "Connecting..."),
        ("failed", "Connection timeout"),
    ] {
        rig.state().transfers = vec![State::transfer(&target, &cid, state, text)];
        row(&backend).await;
    }
    let r = row(&backend).await;
    assert_eq!(r.attempts, 2);
    assert!(!r.queued_at_source);
}

#[tokio::test]
async fn a_file_that_gets_no_byte_is_given_up_and_removed_unless_its_peer_sends_another() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let target = one_queued(&backend).await;
    let cid = source().cid;
    set_bundle(&rig, &target, "queued", "Waiting (0.0%)", 0, 0);
    let timeout = State::transfer(&target, &cid, "failed", "Connection timeout");

    // A file of the same peer running keeps the clock stopped.
    let mut sibling = State::transfer("/downloads/x/03.flac", &cid, "running", "Running (5%)");
    sibling["download"] = true.into();
    rig.state().transfers = vec![timeout.clone(), sibling];
    for _ in 0..4 {
        let r = row(&backend).await;
        assert!(r.queued_at_source);
        assert_eq!(r.state, DownloadState::Queued);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // A byte resets it.
    rig.state().transfers = vec![timeout.clone()];
    row(&backend).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    set_bundle(&rig, &target, "queued", "Waiting (0.1%)", 1, 0);
    assert_eq!(row(&backend).await.state, DownloadState::Queued);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(row(&backend).await.state, DownloadState::Queued);
    // Alternates were asked once.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rig.fake.asked("POST", "queue/bundles/1000/search").len(), 1);

    tokio::time::sleep(Duration::from_millis(400)).await;
    let r = row(&backend).await;
    let Some(failure) = r.failure.clone() else {
        panic!("{r:?}");
    };
    assert_eq!(failure.code, FailureCode::TransferConnectionFailed);
    let removed = rig.fake.asked("POST", "queue/bundles/1000/remove");
    assert_eq!(removed.len(), 1);
    assert_eq!(body(&removed[0]), json!({"remove_finished": false}));
    // Listed as failed until the monitor removes it.
    assert!(matches!(
        row(&backend).await.state,
        DownloadState::Failed(_)
    ));
    backend
        .cancel_download(&source().to_string(), "1000", true)
        .await
        .unwrap();
    assert!(backend
        .get_download(&source().to_string(), "1000")
        .await
        .unwrap()
        .is_none());
}

/// A saved transfer answer, of `target` from `cid`.
fn saved_transfer(name: &str, target: &str, cid: &str) -> Value {
    let mut transfer = fixture(name);
    transfer["target"] = target.into();
    transfer["user"]["cid"] = cid.into();
    transfer
}

#[tokio::test]
async fn a_slot_wait_with_no_place_and_airdcpps_own_limits_never_give_the_file_up() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let target = one_queued(&backend).await;
    let cid = source().cid;
    set_bundle(&rig, &target, "queued", "Waiting (0.0%)", 0, 0);
    for (wait, at_source) in [
        ("transfer_no_slots_unplaced", true),
        ("transfer_own_slots_taken", false),
    ] {
        rig.state().transfers = vec![saved_transfer(wait, &target, &cid)];
        for _ in 0..4 {
            let r = row(&backend).await;
            assert_eq!(r.state, DownloadState::Queued, "{wait}");
            assert_eq!(r.queued_at_source, at_source, "{wait}");
            assert_eq!(r.attempts, 0, "{wait}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    rig.state().transfers = vec![saved_transfer("transfer_no_slots_unplaced", &target, &cid)];
    let r = row(&backend).await;
    assert_eq!(r.queue_position, None);
    assert!(rig
        .fake
        .asked("POST", "queue/bundles/1000/remove")
        .is_empty());
}

#[tokio::test]
async fn an_offline_peers_file_is_given_up_while_other_files_and_its_peers_list_run() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let target = one_queued(&backend).await;
    set_bundle(&rig, &target, "queued", "Waiting (0.0%)", 0, 0);
    rig.state().bundles[0]["sources"]["online"] = 0.into();
    let mut other = State::transfer(
        "/downloads/own/01.flac",
        "OTHERPEERCID",
        "running",
        "Running",
    );
    other["download"] = true.into();
    let list = saved_transfer("transfer_filelist", "", &source().cid);
    rig.state().transfers = vec![other, list];
    row(&backend).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let failure = row(&backend).await.failure.expect("given up");
    assert_eq!(failure.code, FailureCode::PeerOffline);
}

#[tokio::test]
async fn a_tth_mismatch_fails_the_file_as_a_checksum_mismatch() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let target = one_queued(&backend).await;
    set_bundle(&rig, &target, "download_error", "TTH inconsistency", 400, 0);
    let r = row(&backend).await;
    assert_eq!(
        r.failure.unwrap().code,
        FailureCode::TransferChecksumMismatch
    );
    assert_eq!(rig.fake.asked("POST", "queue/bundles/1000/remove").len(), 1);
}

#[tokio::test]
async fn every_cancel_and_remove_keeps_finished_files_and_a_cancel_lists_until_removed() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let target = one_queued(&backend).await;
    let source = source().to_string();

    // A finished file: nothing to cancel, nothing sent.
    set_bundle(&rig, &target, "completed", "Finished", 1000, 0);
    backend
        .cancel_download(&source, "1000", false)
        .await
        .unwrap();
    assert!(rig
        .fake
        .asked("POST", "queue/bundles/1000/remove")
        .is_empty());
    assert_eq!(row(&backend).await.state, DownloadState::Completed);

    // Running: removed, then listed as cancelled until remove.
    set_bundle(&rig, &target, "queued", "Running (5%)", 50, 10);
    backend
        .cancel_download(&source, "1000", false)
        .await
        .unwrap();
    assert_eq!(row(&backend).await.state, DownloadState::Cancelled);
    backend
        .cancel_download(&source, "1000", true)
        .await
        .unwrap();
    assert!(backend
        .get_download(&source, "1000")
        .await
        .unwrap()
        .is_none());
    assert!(rig.book.0.lock().unwrap().is_empty());

    // A finished file removed: the bundle goes, the file stays.
    one_queued(&backend).await;
    let id = rig.book.0.lock().unwrap()[0].transfer_id.clone().unwrap();
    let target = format!("{ROOT}{DESTINATION}/04 - Washer.flac");
    rig.state().bundles = vec![State::bundle(
        id.parse().unwrap(),
        &target,
        "completed",
        "Finished",
        1000,
        0,
    )];
    backend.cancel_download(&source, &id, true).await.unwrap();

    let removals: Vec<Value> = (rig.fake.seen.lock().unwrap().iter())
        .filter(|s| s.target.ends_with("/remove"))
        .map(body)
        .collect();
    assert_eq!(removals.len(), 2);
    assert!(removals
        .iter()
        .all(|b| *b == json!({"remove_finished": false})));
    // Another user's source cannot cancel it.
    one_queued(&backend).await;
    let id = rig.book.0.lock().unwrap()[0].transfer_id.clone().unwrap();
    backend
        .cancel_download("someone-else", &id, true)
        .await
        .unwrap();
    assert_eq!(rig.book.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_queue_answer_a_proxy_replaced_keeps_the_file_so_its_bundle_is_removed() {
    let rig = Rig::new(|state| state.queue = vec![QueueAnswer::BundleBehindProxy(504)]);
    let backend = rig.backend();
    let answers = queued(
        &backend,
        vec![item("/a/04 - Washer.flac", 1000)],
        DESTINATION,
    )
    .await;
    let EnqueueStatus::Failed(failure) = &answers[0].status else {
        panic!("{answers:?}");
    };
    assert_eq!(failure.code, FailureCode::BackendResponseUnreadable);
    let entry = rig.book.0.lock().unwrap()[0].clone();
    assert!(Data::of(&entry).unwrap().abandoned);

    // A retry is not refused as already fetching, and queues under a copy's name.
    let again = queued(
        &backend,
        vec![item("/a/04 - Washer.flac", 1000)],
        DESTINATION,
    )
    .await;
    assert_eq!(again[0].status, EnqueueStatus::Enqueued, "{again:?}");
    let names: Vec<String> = (rig.fake.asked("POST", "queue/bundles/file").iter())
        .map(|s| body(s)["target_name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        shared::download::is_ticks_copy(&names[1], &names[0]),
        "{names:?}"
    );

    // The next listing finds the first bundle at its path and removes it.
    backend
        .get_downloads_for(&[source().to_string()])
        .await
        .unwrap();
    assert_eq!(rig.fake.asked("POST", "queue/bundles/1000/remove").len(), 1);
    let entries = rig.book.0.lock().unwrap().clone();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].transfer_id.as_deref(), Some("1001"));
}

#[tokio::test]
async fn a_cancel_of_a_bundle_whose_id_was_not_recorded_finds_it_by_its_path() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    // The book takes the entry but not its bundle id.
    rig.book.1.store(true, std::sync::atomic::Ordering::SeqCst);
    let target = one_queued(&backend).await;
    rig.book.1.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(rig.book.0.lock().unwrap()[0].transfer_id, None);
    set_bundle(&rig, &target, "queued", "Running (5%)", 50, 10);
    backend
        .cancel_download(&source().to_string(), "1000", false)
        .await
        .unwrap();
    assert_eq!(rig.fake.asked("POST", "queue/bundles/1000/remove").len(), 1);
    let entry = rig.book.0.lock().unwrap()[0].clone();
    assert_eq!(entry.transfer_id.as_deref(), Some("1000"));
    assert_eq!(row(&backend).await.state, DownloadState::Cancelled);
}

fn pending_entry(target: &str, created_at: i64) -> BookEntry {
    let data = Data {
        name: "04 - Washer.flac".into(),
        sidecar_target: target.into(),
        size: 1000,
        tth: TTH.into(),
        ended: None,
        abandoned: false,
    };
    BookEntry {
        backend: BACKEND_ID.into(),
        key: format!("k-{created_at}"),
        transfer_id: None,
        file_id: None,
        source: source().to_string(),
        item: "/a/04 - Washer.flac".into(),
        request: "request-lost".into(),
        target: DESTINATION.into(),
        data: data.value(),
        created_at,
    }
}

#[tokio::test]
async fn a_pending_file_whose_bundle_appears_is_bound_and_one_that_never_does_is_dropped() {
    let rig = Rig::new(|_| {});
    let now = chrono::Utc::now().timestamp();
    let target = format!("{ROOT}{DESTINATION}/04 - Washer.flac");
    rig.book.0.lock().unwrap().push(pending_entry(&target, now));
    rig.book
        .0
        .lock()
        .unwrap()
        .push(pending_entry("/downloads/never/x.flac", now - 3600));
    set_bundle(&rig, &target, "queued", "Waiting (0.0%)", 0, 0);
    let backend = rig.backend();
    let rows = backend
        .get_downloads_for(&[source().to_string()])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "1000");
    assert_eq!(rows[0].request.as_deref(), Some("request-lost"));
    let entries = rig.book.0.lock().unwrap().clone();
    assert_eq!(entries.len(), 1, "the one that never appeared is dropped");
    assert_eq!(entries[0].transfer_id.as_deref(), Some("1000"));
    assert_eq!(rig.fake.asked("POST", "queue/check_path_queued").len(), 1);
}

#[tokio::test]
async fn a_bundle_a_listing_page_missed_is_asked_for_before_its_file_is_forgotten() {
    let rig = Rig::new(|_| {});
    let now = chrono::Utc::now().timestamp();
    let target = format!("{ROOT}{DESTINATION}/04 - Washer.flac");
    let mut bound = pending_entry(&target, now - 8 * 24 * 3600);
    bound.transfer_id = Some("1000".into());
    let lost = format!("{ROOT}{DESTINATION}/05.flac");
    let mut pending = pending_entry(&lost, now - 3600);
    pending.key = "k-pending".into();
    rig.book.0.lock().unwrap().extend([bound, pending]);
    {
        let mut state = rig.state();
        state.bundles = vec![
            State::bundle(1000, &target, "queued", "Waiting (2.0%)", 20, 0),
            State::bundle(1001, &lost, "queued", "Waiting (0.0%)", 0, 0),
        ];
        state.unlisted = vec![1000, 1001];
    }
    let backend = rig.backend();
    // Neither is listed, so neither row shows; neither is forgotten.
    let rows = backend
        .get_downloads_for(&[source().to_string()])
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "the pending one is bound by its path: {rows:?}"
    );
    assert_eq!(rig.book.0.lock().unwrap().len(), 2);

    // A stop removes both, though the listing shows neither.
    rig.engine
        .stop(Some(&Sidecar::new(rig.connection()).unwrap()))
        .await;
    assert_eq!(rig.fake.asked("POST", "queue/bundles/1000/remove").len(), 1);
    assert_eq!(rig.fake.asked("POST", "queue/bundles/1001/remove").len(), 1);
    assert!(rig.book.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_listing_reads_every_page_of_a_long_queue() {
    // With Soulbeet's, two full pages: the third start is past the end.
    let rig = Rig::new(|state| {
        state.bundles = (5001..=6999)
            .map(|id| State::bundle(id, &format!("/downloads/own/{id}.flac"), "queued", "", 0, 0))
            .collect();
    });
    let backend = rig.backend();
    one_queued(&backend).await;
    let r = row(&backend).await;
    assert_eq!(r.id, "1000");
    let pages: Vec<String> = (rig.fake.asked("GET", "queue/bundles/").iter())
        .map(|s| s.target.clone())
        .filter(|t| t.ends_with("/1000"))
        .collect();
    assert!(
        pages.contains(&"/api/v1/queue/bundles/2000/1000".to_string()),
        "{pages:?}"
    );
}

#[tokio::test]
async fn a_stop_during_a_download_call_removes_every_file_it_queued_and_refuses_the_rest() {
    let timing = Timing {
        enqueue_spacing: Duration::from_millis(150),
        ..fast()
    };
    let rig = Arc::new(Rig::with_timing(timing, |_| {}));
    let backend = Arc::new(rig.backend());
    let call = {
        let backend = backend.clone();
        tokio::spawn(async move {
            queued(
                &backend,
                (1..=6)
                    .map(|n| item(&format!("/a/0{n}.flac"), 10))
                    .collect(),
                DESTINATION,
            )
            .await
        })
    };
    while rig.fake.asked("POST", "queue/bundles/file").is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    rig.engine
        .stop(Some(&Sidecar::new(rig.connection()).unwrap()))
        .await;
    let answers = call.await.unwrap();
    let sent = rig.fake.asked("POST", "queue/bundles/file").len();
    assert!(sent < 6, "the call went on after the stop");
    let refused = (answers.iter())
        .filter(|a| matches!(&a.status, EnqueueStatus::Failed(f) if f.code == FailureCode::BackendNotConfigured))
        .count();
    assert_eq!(refused, 6 - sent, "{answers:?}");
    // Every bundle the call made is gone, and so is its entry.
    assert!(rig.state().bundles.is_empty());
    assert!(rig.book.0.lock().unwrap().is_empty());
    let seen = rig.fake.seen.lock().unwrap();
    let last_queue = seen
        .iter()
        .rposition(|s| s.target.ends_with("queue/bundles/file"))
        .unwrap();
    let first_remove = seen
        .iter()
        .position(|s| s.target.ends_with("/remove"))
        .unwrap();
    assert!(
        last_queue < first_remove,
        "a file was queued after the stop removed the others"
    );
}

#[tokio::test]
async fn turning_off_removes_unfinished_bundles_and_refuses_the_handles_built_before() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    one_queued(&backend).await;
    let answers = queued(&backend, vec![item("/a/05.flac", 10)], DESTINATION).await;
    let done_id: u64 = answers[0].id.parse().unwrap();
    let target = format!("{ROOT}{DESTINATION}/05.flac");
    {
        let mut state = rig.state();
        let running = State::bundle(
            1000,
            &format!("{ROOT}{DESTINATION}/04 - Washer.flac"),
            "queued",
            "Running (5%)",
            5,
            9,
        );
        let done = State::bundle(done_id, &target, "completed", "Finished", 10, 0);
        state.bundles = vec![running, done];
    }
    let sidecar = Sidecar::new(rig.connection()).unwrap();
    rig.engine.stop(Some(&sidecar)).await;

    let removed = rig.fake.asked("POST", "queue/bundles/");
    let removed: Vec<&str> = removed
        .iter()
        .filter(|s| s.target.ends_with("/remove"))
        .map(|s| s.target.as_str())
        .collect();
    assert_eq!(removed, ["/api/v1/queue/bundles/1000/remove"]);
    let entries = rig.book.0.lock().unwrap().clone();
    assert_eq!(entries.len(), 1, "the finished file's entry stays");
    assert_eq!(entries[0].transfer_id, Some(done_id.to_string()));

    let refused = backend.start_search(&glory_box()).await.unwrap_err();
    assert_eq!(
        refused.failure(Stage::SourceSearch, "m").code,
        FailureCode::BackendNotConfigured
    );
    assert!(backend
        .download(vec![item("/a/x.flac", 1)], DESTINATION, "r", "u")
        .await
        .is_err());
    let after = rig.backend();
    let id = after.start_search(&glory_box()).await.unwrap();
    after.stop_search(&id).await.unwrap();
}

#[tokio::test]
async fn stopping_with_no_engine_removes_what_a_previous_run_left_unfinished() {
    let rig = Rig::new(|_| {});
    let target = format!("{ROOT}{DESTINATION}/04 - Washer.flac");
    let mut entry = pending_entry(&target, chrono::Utc::now().timestamp());
    entry.transfer_id = Some("1000".into());
    rig.book.0.lock().unwrap().push(entry);
    set_bundle(&rig, &target, "queued", "Waiting (2.0%)", 20, 0);
    let cell = EngineCell::new();
    let settings = EngineSettings {
        download_path: rig.root.path().to_path_buf(),
        book: rig.book.clone(),
    };
    cell.stop(settings, Some(&Sidecar::new(rig.connection()).unwrap()))
        .await;
    assert_eq!(rig.fake.asked("POST", "queue/bundles/1000/remove").len(), 1);
    assert!(rig.book.0.lock().unwrap().is_empty());
}

async fn health(rig: &Rig) -> Result<(), Failure> {
    rig.backend().health_check().await
}

#[tokio::test]
async fn the_health_check_names_what_to_fix_in_the_sidecar() {
    use shared::failure::UserActionCode::*;
    let action = |f: &Failure| f.user_action.as_ref().map(|a| a.code);

    let rig = Rig::new(|_| {});
    rig.state().shared_folder = Some(rig.root.path().to_path_buf());
    health(&rig).await.unwrap();
    // The probe folder was made, found and deleted.
    assert_eq!(rig.fake.asked("POST", "filesystem/directory").len(), 1);
    assert_eq!(std::fs::read_dir(rig.root.path()).unwrap().count(), 0);

    let rig = Rig::new(|_| {});
    let failure = health(&rig).await.unwrap_err();
    assert_eq!(
        (failure.code, action(&failure)),
        (FailureCode::BackendNotConfigured, Some(FixDownloadPath))
    );
    assert!(
        failure.message.contains("not Soulbeet's DOWNLOAD_PATH"),
        "{}",
        failure.message
    );

    let rig = Rig::new(|state| state.settings["language_file"] = "ru-RU.xml".into());
    let failure = health(&rig).await.unwrap_err();
    assert_eq!(failure.code, FailureCode::BackendNotConfigured);
    assert!(failure.message.contains("English"));

    let rig = Rig::new(|state| {
        state.hubs = json!([{"id": 1, "hub_url": "x", "connect_state": {"id": "disconnected"}}])
    });
    let failure = health(&rig).await.unwrap_err();
    assert_eq!(failure.code, FailureCode::BackendUnreachable);
    assert_eq!(failure.message, "AirDC++ is connected to no hub");

    let rig =
        Rig::new(|state| state.session["user"]["permissions"] = json!(["search", "download"]));
    let failure = health(&rig).await.unwrap_err();
    assert_eq!(
        (failure.code, action(&failure)),
        (FailureCode::BackendNotConfigured, Some(ConfigureBackend))
    );
    assert!(
        failure.message.contains("queue_edit"),
        "{}",
        failure.message
    );

    let rig = Rig::new(|state| state.session["user"]["permissions"] = json!(["admin"]));
    rig.state().shared_folder = Some(rig.root.path().to_path_buf());
    health(&rig).await.unwrap();

    let rig = Rig::new(|_| {});
    let default = Connection {
        url: rig.fake.url.clone(),
        username: "admin".into(),
        password: "password".into(),
    };
    let failure = AirdcppBackend::new(rig.engine.clone(), Sidecar::new(default).unwrap())
        .health_check()
        .await
        .unwrap_err();
    assert_eq!(failure.code, FailureCode::BackendNotConfigured);
    assert!(
        rig.fake.seen.lock().unwrap().is_empty(),
        "nothing sent with the default login"
    );

    let down = Connection {
        url: "http://127.0.0.1:1".into(),
        ..rig.connection()
    };
    let failure = AirdcppBackend::new(rig.engine.clone(), Sidecar::new(down).unwrap())
        .health_check()
        .await
        .unwrap_err();
    assert_eq!(failure.code, FailureCode::BackendUnreachable);
}

#[tokio::test]
async fn a_failed_folder_probe_runs_again_only_after_ten_minutes_or_on_a_check() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    for _ in 0..3 {
        let failed = health::check(&rig.engine, &backend.link).await;
        assert!(
            matches!(failed, Err(AirdcppError::OtherFolder { .. })),
            "{failed:?}"
        );
    }
    assert_eq!(rig.fake.asked("POST", "filesystem/directory").len(), 1);
    let report = backend.check().await;
    assert_eq!(report.same_folder, Some(false));
    assert_eq!(rig.fake.asked("POST", "filesystem/directory").len(), 2);
}

#[tokio::test]
async fn a_folder_the_sidecar_makes_that_soulful_cannot_write_in_is_named() {
    let rig = Rig::new(|_| {});
    {
        let mut state = rig.state();
        state.shared_folder = Some(rig.root.path().to_path_buf());
        state.probe_blocked = true;
    }
    let failure = health(&rig).await.unwrap_err();
    assert_eq!(failure.code, FailureCode::BackendNotConfigured);
    assert!(failure.message.contains("PUID"), "{}", failure.message);
}

#[tokio::test]
async fn a_peers_presence_is_read_from_its_flags() {
    let rig = Rig::new(|_| {});
    let backend = rig.backend();
    let source = source().to_string();
    assert_eq!(
        backend.source_status(&source).await.unwrap(),
        SourcePresence::Away
    );
    rig.state().hinted = (400, json!({"message": "User not found"}));
    assert_eq!(
        backend.source_status(&source).await.unwrap(),
        SourcePresence::Offline
    );
    rig.state().hinted = (200, json!({"cid": "x", "hub_url": "y", "flags": ["nmdc"]}));
    assert_eq!(
        backend.source_status(&source).await.unwrap(),
        SourcePresence::Online
    );
    rig.state().hinted = (
        200,
        json!({"cid": "x", "hub_url": "y", "flags": ["offline"]}),
    );
    assert_eq!(
        backend.source_status(&source).await.unwrap(),
        SourcePresence::Offline
    );
}

/// Every field of every event and span logged, with the thread it was logged on. Installed as
/// the binary's global subscriber: a scoped one misses events whose callsites another test
/// thread registered first.
mod logs {
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread::ThreadId;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Clone, Default)]
    pub(super) struct Lines(Arc<Mutex<Vec<(ThreadId, String)>>>);

    impl Lines {
        /// The lines logged on this thread.
        pub(super) fn here(&self) -> Vec<String> {
            let me = std::thread::current().id();
            (self.0.lock().unwrap().iter())
                .filter(|(thread, _)| *thread == me)
                .map(|(_, line)| line.clone())
                .collect()
        }

        fn push(&self, line: String) {
            (self.0.lock().unwrap()).push((std::thread::current().id(), line));
        }
    }

    pub(super) fn installed() -> Lines {
        static LINES: OnceLock<Lines> = OnceLock::new();
        LINES
            .get_or_init(|| {
                let lines = Lines::default();
                tracing::subscriber::set_global_default(lines.clone()).unwrap();
                lines
            })
            .clone()
    }

    struct Text<'a>(&'a mut String);

    impl Visit for Text<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(&format!("{}={:?} ", field.name(), value));
        }
    }

    impl Subscriber for Lines {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, attrs: &Attributes<'_>) -> Id {
            let mut line = String::new();
            attrs.record(&mut Text(&mut line));
            self.push(line);
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, values: &Record<'_>) {
            let mut line = String::new();
            values.record(&mut Text(&mut line));
            self.push(line);
        }
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut line = String::new();
            event.record(&mut Text(&mut line));
            self.push(line);
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }
}

#[tokio::test]
async fn no_log_line_holds_the_password_or_the_authorization() {
    let lines = logs::installed();
    let rig = Rig::new(|state| {
        state.results = fixture("search_track_results");
        state.children = fixture("search_track_children");
    });
    let backend = rig.backend();
    let id = backend.start_search(&glory_box()).await.unwrap();
    ended(&backend, &id).await;
    let _ = backend.health_check().await;
    one_queued(&backend).await;
    let _ = backend.get_downloads_for(&[source().to_string()]).await;
    // A 401 is logged too.
    let mut refusing = State::new(ROOT);
    refusing.unauthorized = true;
    let refused = Fake::start(refusing);
    let wrong = Connection {
        url: refused.url.clone(),
        ..rig.connection()
    };
    let _ = AirdcppBackend::new(rig.engine.clone(), Sidecar::new(wrong).unwrap())
        .health_check()
        .await;
    let lines = lines.here();
    assert!(!lines.is_empty());
    let auth = rig.fake.seen.lock().unwrap()[0]
        .authorization
        .clone()
        .unwrap();
    for line in lines.iter() {
        assert!(!line.contains("pass-n0t-logged"), "{line}");
        assert!(!line.contains(auth.trim_start_matches("Basic ")), "{line}");
        assert!(!line.contains("Authorization"), "{line}");
    }
}
