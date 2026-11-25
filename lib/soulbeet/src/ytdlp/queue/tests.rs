use super::*;
use crate::ytdlp::matching::tests::{spiderland, track};
use crate::ytdlp::matching::{self, Wanted};
use crate::ytdlp::protocol::tests::gone;
use crate::ytdlp::protocol::Tools;
use crate::ytdlp::{Engine, EngineSettings};
use lofty::tag::ItemKey;

/// The CRC of an Ogg page (poly 0x04c11db7, not reflected).
fn ogg_crc(bytes: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in bytes {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn ogg_page(kind: u8, granule: u64, sequence: u32, packets: &[&[u8]]) -> Vec<u8> {
    let mut page = b"OggS".to_vec();
    page.extend([0, kind]);
    page.extend(granule.to_le_bytes());
    page.extend(1u32.to_le_bytes());
    page.extend(sequence.to_le_bytes());
    page.extend([0; 4]);
    page.push(packets.len() as u8);
    for packet in packets {
        page.push(packet.len() as u8);
    }
    for packet in packets {
        page.extend(*packet);
    }
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// A mono Ogg Opus of `seconds` of silence, as lofty reads its length: its last granule.
pub(crate) fn opus_of(seconds: u64) -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.extend([1, 1]);
    head.extend(312u16.to_le_bytes());
    head.extend(48_000u32.to_le_bytes());
    head.extend([0, 0, 0]);
    let mut tags = b"OpusTags".to_vec();
    tags.extend(4u32.to_le_bytes());
    tags.extend(b"test");
    tags.extend(0u32.to_le_bytes());
    let audio: Vec<&[u8]> = vec![&[0xF8]; 50];
    let mut out = ogg_page(0x02, 0, 0, &[&head]);
    out.extend(ogg_page(0x00, 0, 1, &[&tags]));
    out.extend(ogg_page(0x04, 312 + 48_000 * seconds, 2, &audio));
    out
}

/// The helper the engine tests run: it answers `version`, the searches from the fixtures,
/// `probe`, and `fetch` as the video id asks, logging each command and video id in `log`.
const FAKE: &str = r#"
import json, os, shutil, sys, time
request = json.loads(sys.stdin.readline())
command = request["command"]
with open(LOG, "a") as f:
    f.write(json.dumps({"command": command, "video_id": request.get("video_id")}) + "\n")
def say(answer):
    print(json.dumps(answer), flush=True)
def fail(kind, message):
    say({"error": {"kind": kind, "message": message}})
    sys.exit(0)
if command == "version":
    say({"version": json.load(open(VERSIONS))})
    sys.exit(0)
if command == "probe":
    if os.path.exists(PROBE_FAILS):
        fail("network", "timed out")
    say({"probe": "ok"})
    sys.exit(0)
if command != "fetch":
    fixtures = FIXTURES
    names = {"search_albums": "slint-albums", "album": "slint-spiderland"}
    sys.stdout.write(open(f"{fixtures}/{names[command]}.ndjson").read())
    sys.exit(0)
video, staging = request["video_id"], request["staging_dir"]
if video == "RATELIMIT01":
    fail("rate_limited", "This content isn't available, try again later")
if video == "FORBIDDEN01":
    fail("forbidden", "HTTP Error 403: Forbidden at https://rr1---sn-x.googlevideo.com/videoplayback?ip=203.0.113.9")
if video == "BOTCHECK001":
    fail("bot_check", "Sign in to confirm you are not a bot")
if video == "HANGHANGHAN":
    time.sleep(60)
if video == "DIEDIEDIE01":
    os._exit(9)
if video == "GARBAGE0001":
    print("[download]  42.0% of 2.03MiB", flush=True)
    sys.exit(0)
if video == "SLOWSLOWSLO":
    open(os.path.join(staging, "pid"), "w").write(str(os.getpid()))
    say({"progress": {"downloaded": 10, "total": 100, "speed": 5.0}})
    time.sleep(60)
if video == "NETWORK0001" and not os.path.exists(os.path.join(staging, "tried")):
    open(os.path.join(staging, "tried"), "w").write("1")
    fail("network", "Connection reset by peer")
path = os.path.join(staging, video + ".opus")
if video == "OUTSIDE0001":
    path = OUTSIDE
else:
    shutil.copy(SILENCE, path)
listed = 365 if video == "SHORTFILE01" else 356
say({"progress": {"downloaded": 1024, "total": 2048, "speed": 100.0}})
say({"done": {"path": path, "format_id": "251", "acodec": "opus", "abr": 136.374, "asr": 48000, "duration": listed}})
"#;

/// A downloads folder, the fake helper, and what it reads and writes.
pub(crate) struct Rig {
    pub root: tempfile::TempDir,
}

/// The pace of tests: no waits.
pub(crate) const QUICK: Pace = Pace {
    gap: (Duration::ZERO, Duration::ZERO),
    per_hour: 1000,
    network_waits: [Duration::ZERO, Duration::ZERO],
    per_user: 200,
    search_gap: Duration::ZERO,
    fetch_idle: Duration::from_secs(20),
    fetch_total: Duration::from_secs(40),
};

impl Rig {
    pub(crate) fn new() -> Rig {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        // Breadcrumb Trail's length, as MusicBrainz and the fake's file have it.
        std::fs::write(root.path().join("silence.opus"), opus_of(355)).unwrap();
        std::fs::write(root.path().join("outside.opus"), opus_of(355)).unwrap();
        let rig = Rig { root };
        rig.set_versions(serde_json::json!({
            "python": "3.11.2", "yt_dlp": "2026.11.23", "ejs": "0.8.0",
            "ytmusicapi": "1.12.4", "deno": "2.9.8", "ffmpeg": "8.0.1"
        }));
        rig
    }

    pub(crate) fn set_versions(&self, versions: serde_json::Value) {
        std::fs::write(self.path("versions.json"), versions.to_string()).unwrap();
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    pub(crate) fn downloads(&self) -> PathBuf {
        self.path("downloads")
    }

    pub(crate) fn settings(&self) -> EngineSettings {
        EngineSettings {
            download_path: self.downloads(),
            instance: "test-install".into(),
            tools: Tools {
                python: crate::beets::resolve_on_path("python3").expect("python3 on PATH"),
                site_packages: self.path("site-packages"),
                deno: self.path("deno"),
                ffmpeg: None,
            },
        }
    }

    fn script(&self) -> String {
        let quoted = |name: &str| format!("{:?}", self.path(name).display().to_string());
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ytdlp/fixtures");
        let header = [
            ("LOG", quoted("log")),
            ("VERSIONS", quoted("versions.json")),
            ("PROBE_FAILS", quoted("probe-fails")),
            ("SILENCE", quoted("silence.opus")),
            ("OUTSIDE", quoted("outside.opus")),
            ("FIXTURES", format!("{:?}", fixtures.display().to_string())),
        ]
        .map(|(name, value)| format!("{name} = {value}\n"))
        .concat();
        format!("{header}{FAKE}")
    }

    pub(crate) async fn engine(&self, pace: Pace) -> Arc<Engine> {
        Engine::start_with(self.settings(), pace, self.script().into())
            .await
            .unwrap()
    }

    /// The commands the helper ran, and the video ids it fetched, in order.
    pub(crate) fn log(&self) -> Vec<(String, Option<String>)> {
        std::fs::read_to_string(self.path("log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                (
                    v["command"].as_str().unwrap().to_string(),
                    v["video_id"].as_str().map(str::to_string),
                )
            })
            .collect()
    }

    pub(crate) fn fetched(&self) -> Vec<String> {
        (self.log().into_iter())
            .filter(|(c, _)| c == "fetch")
            .filter_map(|(_, v)| v)
            .collect()
    }

    pub(crate) fn partial(&self) -> PathBuf {
        self.downloads()
            .join(crate::partial::PARTIAL_FOLDER)
            .join("test-install")
            .join(BACKEND_ID)
    }

    /// The transfer staging folders left in the partial folder.
    pub(crate) fn staging(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.partial())
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !["home", "cache"].contains(&n.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

/// An item of the Spiderland track `index` as video `video_id`, as a search offers it.
pub(crate) fn item(index: usize, video_id: &str) -> DownloadableItem {
    let wanted = spiderland();
    let page = crate::ytdlp::protocol::AlbumPage {
        title: "Spiderland".into(),
        artists: vec!["Slint".into()],
        year: Some("1991".into()),
        tracks: (0..wanted.tracks.len())
            .map(|i| crate::ytdlp::protocol::AlbumTrack {
                video_id: Some(if i == index {
                    video_id.to_string()
                } else {
                    format!("other{i:06}")
                }),
                title: wanted.tracks[i].title.clone(),
                track_number: Some(i as u32 + 1),
                duration_s: wanted.tracks[i].duration_secs(),
                available: true,
                video_type: Some("MUSIC_VIDEO_TYPE_ATV".into()),
                artists: vec!["Slint".into()],
                explicit: false,
            })
            .collect(),
    };
    let hit = crate::ytdlp::protocol::AlbumHit {
        browse_id: "MPREb_ur8iGYISD11".into(),
        title: "Spiderland".into(),
        artists: vec!["Slint".into()],
        year: None,
        explicit: false,
    };
    let (group, _) = matching::album_group(&hit, &page, &wanted, false, 0);
    group.unwrap().items.remove(index)
}

/// Waits until the transfer `id` ended: its row.
pub(crate) async fn ended(engine: &Engine, source: &str, id: &str) -> DownloadProgress {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let row = engine
            .queue
            .one(source, id)
            .expect("the transfer is listed");
        if matches!(
            row.state,
            DownloadState::Completed | DownloadState::Failed(_) | DownloadState::Cancelled
        ) {
            return row;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{id} never ended: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn enqueued(answers: &[QueuedDownload]) -> Vec<String> {
    answers
        .iter()
        .map(|a| {
            assert_eq!(a.status, EnqueueStatus::Enqueued, "{a:?}");
            a.id.clone()
        })
        .collect()
}

fn failure_of(row: &DownloadProgress) -> &Failure {
    row.failure.as_ref().expect("a failed row has a failure")
}

#[tokio::test]
async fn a_fetched_track_is_tagged_as_the_wanted_one_and_lands_in_its_batch_folder() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let wanted = item(0, "MdzF0rFOC9Y");
    let ids = enqueued(
        &engine
            .queue
            .download(0, vec![wanted.clone()], "Spiderland [1]", "r1", "u1")
            .unwrap(),
    );
    let row = ended(&engine, &wanted.source, &ids[0]).await;
    assert_eq!(row.state, DownloadState::Completed, "{row:?}");
    assert_eq!(row.request.as_deref(), Some("r1"));
    assert_eq!(row.backend.as_deref(), Some(BACKEND_ID));

    // Where the monitor looks for it: the plain landing name of the item id.
    let landed = rig
        .downloads()
        .join("Spiderland [1]")
        .join(&shared::download::landing_names(&wanted.id)[0]);
    let file = lofty::read_from_path(&landed).unwrap();
    let tag = file.primary_tag().unwrap();
    for (key, value) in [
        (ItemKey::TrackTitle, "Breadcrumb Trail"),
        (ItemKey::TrackArtist, "Slint"),
        (ItemKey::AlbumTitle, "Spiderland"),
        (ItemKey::TrackNumber, "1"),
        (ItemKey::RecordingDate, "1991-03-27"),
        (ItemKey::MusicBrainzRecordingId, "rec-Breadcrumb Trail"),
        (ItemKey::MusicBrainzReleaseId, "rel-spiderland"),
        (
            ItemKey::Comment,
            "Lossy: YouTube Music, Opus 136 kbps, video MdzF0rFOC9Y",
        ),
    ] {
        assert_eq!(tag.get_string(key), Some(value), "{key:?}");
    }
    assert!(rig.staging().is_empty(), "{:?}", rig.staging());
}

#[tokio::test]
async fn a_taken_name_lands_as_a_copy_and_the_file_there_is_left_alone() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let wanted = item(0, "MdzF0rFOC9Y");
    let folder = rig.downloads().join("Spiderland [1]");
    std::fs::create_dir_all(&folder).unwrap();
    let taken = folder.join(&wanted.id);
    std::fs::write(&taken, b"already here").unwrap();

    let ids = enqueued(
        &engine
            .queue
            .download(0, vec![wanted.clone()], "Spiderland [1]", "r1", "u1")
            .unwrap(),
    );
    assert_eq!(
        ended(&engine, &wanted.source, &ids[0]).await.state,
        DownloadState::Completed
    );
    assert_eq!(std::fs::read(&taken).unwrap(), b"already here");
    let copies: Vec<String> = std::fs::read_dir(&folder)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| shared::download::is_ticks_copy(n, &wanted.id))
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
}

#[tokio::test]
async fn titles_with_separators_and_long_ones_land_under_the_name_the_monitor_reads() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    for (n, title) in ["AC/DC: What?", &"Long title ".repeat(40)]
        .iter()
        .enumerate()
    {
        let mut wanted = item(0, "MdzF0rFOC9Y");
        let mut data: YtTrack =
            serde_json::from_str(wanted.backend_data.as_deref().unwrap()).unwrap();
        data.wanted.title = title.to_string();
        wanted.id = item_name(&data.wanted.artist, title, &data.video_id);
        wanted.backend_data = Some(serde_json::to_string(&data).unwrap());
        let folder = format!("Batch [{n}]");
        let ids = enqueued(
            &engine
                .queue
                .download(0, vec![wanted.clone()], &folder, "r", "u1")
                .unwrap(),
        );
        assert_eq!(
            ended(&engine, &wanted.source, &ids[0]).await.state,
            DownloadState::Completed
        );
        let landed = rig.downloads().join(&folder).join(&wanted.id);
        assert!(landed.is_file(), "{}", landed.display());
        assert_eq!(
            shared::download::landing_names(&wanted.id),
            [wanted.id.clone()]
        );
    }
}

#[tokio::test]
async fn two_users_take_turns() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    // Held up by the first one, so the turns decide the rest.
    let first = enqueued(
        &engine
            .queue
            .download(0, vec![item(0, "SLOWSLOWSLO")], "A [1]", "r", "alice")
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while rig.fetched().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the first fetch never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let alice = (1..3).map(|i| item(i, &format!("alice{i:06}"))).collect();
    let bob = (3..5).map(|i| item(i, &format!("bob{i:08}"))).collect();
    enqueued(
        &engine
            .queue
            .download(0, alice, "A [1]", "r", "alice")
            .unwrap(),
    );
    enqueued(&engine.queue.download(0, bob, "B [1]", "r", "bob").unwrap());
    engine
        .queue
        .cancel(&item(0, "SLOWSLOWSLO").source, &first[0], false)
        .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while rig.fetched().len() < 5 {
        assert!(std::time::Instant::now() < deadline, "{:?}", rig.fetched());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        rig.fetched(),
        [
            "SLOWSLOWSLO",
            "bob00000003",
            "alice000001",
            "bob00000004",
            "alice000002"
        ]
    );
}

#[test]
fn the_next_transfer_is_of_the_user_served_longest_ago() {
    let mut state = State::default();
    let add = |state: &mut State, id: &str, owner: &str| {
        let seq = state.next_seq;
        state.next_seq += 1;
        state.records.insert(
            id.into(),
            Record {
                id: id.into(),
                source: "s".into(),
                item: id.into(),
                request: "r".into(),
                destination: "D".into(),
                track: serde_json::from_str(
                    item(0, "MdzF0rFOC9Y").backend_data.as_deref().unwrap(),
                )
                .unwrap(),
                owner: owner.into(),
                phase: Phase::Queued,
                size: 0,
                transferred: 0,
                speed: 0.0,
                attempts: 0,
                seq,
                ended_at: None,
                task: None,
            },
        );
    };
    for (id, owner) in [
        ("a1", "alice"),
        ("a2", "alice"),
        ("a3", "alice"),
        ("b1", "bob"),
    ] {
        add(&mut state, id, owner);
    }
    let mut order = Vec::new();
    while let Some(id) = state.next() {
        state.turn += 1;
        let record = state.records.get_mut(&id).unwrap();
        record.phase = Phase::Completed;
        let owner = record.owner.clone();
        state.served.insert(owner, state.turn);
        order.push(id);
    }
    assert_eq!(order, ["a1", "b1", "a2", "a3"]);
}

#[tokio::test(start_paused = true)]
async fn videos_start_ten_to_twenty_seconds_apart_and_at_most_120_an_hour() {
    let pace = Pace::YOUTUBE;
    let mut pacer = Pacer::default();
    let start = Instant::now();
    assert_eq!(pacer.wait(start, pace.per_hour), Duration::ZERO);
    pacer.started(start, pace.gap());
    let wait = pacer.wait(start, pace.per_hour);
    assert!(
        (Duration::from_secs(10)..=Duration::from_secs(20)).contains(&wait),
        "{wait:?}"
    );
    // 120 videos in the hour: the 121st waits for the first one's hour to pass.
    let mut pacer = Pacer::default();
    for n in 0..120 {
        pacer.started(start + Duration::from_secs(n * 20), Duration::from_secs(10));
    }
    let now = start + Duration::from_secs(119 * 20 + 15);
    assert_eq!(
        pacer.wait(now, pace.per_hour),
        (start + HOUR).duration_since(now)
    );
    tokio::time::advance(HOUR).await;
    assert_eq!(pacer.wait(start + HOUR, pace.per_hour), Duration::ZERO);
}

#[tokio::test]
async fn a_rate_limit_pauses_the_queue_and_fails_what_waits_with_the_wait() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let limited = item(0, "RATELIMIT01");
    let waiting = item(1, "MdzF0rFOC9Y");
    let ids = enqueued(
        &engine
            .queue
            .download(
                0,
                vec![limited.clone(), waiting.clone()],
                "S [1]",
                "r",
                "u1",
            )
            .unwrap(),
    );
    for (source, id) in [(&limited.source, &ids[0]), (&waiting.source, &ids[1])] {
        let row = ended(&engine, source, id).await;
        let failure = failure_of(&row);
        assert_eq!(failure.code, FailureCode::RateLimited, "{failure:?}");
        let wait = failure.retry_after_seconds.unwrap();
        assert!((3500..=3600).contains(&wait), "{wait}");
    }
    assert_eq!(rig.fetched(), ["RATELIMIT01"]);
    // Refused while it lasts, without asking YouTube.
    let refused = engine
        .queue
        .download(0, vec![item(2, "MdzF0rFOC9Y")], "S [1]", "r", "u2")
        .unwrap();
    assert!(
        matches!(&refused[0].status, EnqueueStatus::Failed(f) if f.code == FailureCode::RateLimited)
    );
    assert_eq!(rig.fetched(), ["RATELIMIT01"]);
}

#[tokio::test]
async fn three_refused_downloads_in_an_hour_are_a_bot_check() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let items: Vec<DownloadableItem> = (0..3).map(|i| item(i, "FORBIDDEN01")).collect();
    let ids = enqueued(
        &engine
            .queue
            .download(0, items.clone(), "S [1]", "r", "u1")
            .unwrap(),
    );
    for (it, id) in items.iter().zip(&ids) {
        let row = ended(&engine, &it.source, id).await;
        let cause = failure_of(&row).cause.as_ref().unwrap();
        // The signed media URL carries the server's address: it never reaches a failure.
        assert!(!cause.raw.contains("googlevideo"), "{}", cause.raw);
        assert!(!cause.raw.contains("203.0.113.9"), "{}", cause.raw);
    }
    let pause = engine.standing.paused().unwrap();
    assert_eq!(pause.why, crate::ytdlp::error::Hold::BotCheck);
}

#[tokio::test]
async fn a_cancelled_fetch_has_its_helper_killed_and_only_its_staging_folder_removed() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let folder = rig.downloads().join("S [1]");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("kept.flac"), b"flac").unwrap();
    let slow = item(0, "SLOWSLOWSLO");
    let ids = enqueued(
        &engine
            .queue
            .download(0, vec![slow.clone()], "S [1]", "r", "u1")
            .unwrap(),
    );
    let pid_file = rig.partial().join(&ids[0]).join("pid");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let pid: i32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|p| p.parse().ok())
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fetch never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(engine.queue.queue_position(&slow.source, &ids[0]), Some(0));

    engine.queue.cancel(&slow.source, &ids[0], false).await;

    assert!(gone(pid).await, "the helper survived the cancel");
    assert_eq!(
        engine.queue.one(&slow.source, &ids[0]).unwrap().state,
        DownloadState::Cancelled
    );
    assert!(rig.staging().is_empty(), "{:?}", rig.staging());
    assert_eq!(std::fs::read(folder.join("kept.flac")).unwrap(), b"flac");
    engine.queue.cancel(&slow.source, &ids[0], true).await;
    assert!(engine.queue.one(&slow.source, &ids[0]).is_none());
}

#[tokio::test]
async fn a_file_shorter_than_youtube_lists_is_truncated_and_never_lands() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let short = item(0, "SHORTFILE01");
    let ids = enqueued(
        &engine
            .queue
            .download(0, vec![short.clone()], "S [1]", "r", "u1")
            .unwrap(),
    );
    let row = ended(&engine, &short.source, &ids[0]).await;
    assert_eq!(failure_of(&row).code, FailureCode::TransferSizeMismatch);
    assert!(!rig.downloads().join("S [1]").exists());
    assert!(rig.staging().is_empty());
}

#[tokio::test]
async fn a_helper_that_hangs_dies_or_prints_garbage_fails_its_transfer() {
    let rig = Rig::new();
    let pace = Pace {
        fetch_idle: Duration::from_secs(2),
        ..QUICK
    };
    let engine = rig.engine(pace).await;
    for (video, code) in [
        ("HANGHANGHAN", FailureCode::TransferTimedOut),
        ("DIEDIEDIE01", FailureCode::BackendResponseUnreadable),
        ("GARBAGE0001", FailureCode::BackendResponseUnreadable),
    ] {
        let it = item(0, video);
        let ids = enqueued(
            &engine
                .queue
                .download(0, vec![it.clone()], video, "r", "u1")
                .unwrap(),
        );
        let row = ended(&engine, &it.source, &ids[0]).await;
        assert_eq!(failure_of(&row).code, code, "{video}");
        assert!(!rig.downloads().join(video).exists(), "{video}");
    }
    assert!(rig.staging().is_empty(), "{:?}", rig.staging());
}

#[tokio::test]
async fn a_file_the_helper_names_outside_its_folder_is_never_landed() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let before = std::fs::read(rig.path("outside.opus")).unwrap();
    let it = item(0, "OUTSIDE0001");
    let ids = enqueued(
        &engine
            .queue
            .download(0, vec![it.clone()], "S [1]", "r", "u1")
            .unwrap(),
    );
    let row = ended(&engine, &it.source, &ids[0]).await;
    assert_eq!(
        failure_of(&row).code,
        FailureCode::BackendResponseUnreadable
    );
    assert_eq!(std::fs::read(rig.path("outside.opus")).unwrap(), before);
    assert!(!rig.downloads().join("S [1]").exists());
}

#[tokio::test]
async fn a_failed_connection_is_tried_again() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let it = item(0, "NETWORK0001");
    let ids = enqueued(
        &engine
            .queue
            .download(0, vec![it.clone()], "S [1]", "r", "u1")
            .unwrap(),
    );
    let row = ended(&engine, &it.source, &ids[0]).await;
    assert_eq!(row.state, DownloadState::Completed, "{row:?}");
    assert_eq!(row.attempts, 2);
}

#[tokio::test]
async fn an_item_the_backend_never_offered_gets_no_answer() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let mut renamed = item(0, "MdzF0rFOC9Y");
    renamed.id = "../../escape.opus".into();
    let mut other_video = item(0, "MdzF0rFOC9Y");
    let mut data: YtTrack =
        serde_json::from_str(other_video.backend_data.as_deref().unwrap()).unwrap();
    data.video_id = "../etc/pwd".into();
    other_video.backend_data = Some(serde_json::to_string(&data).unwrap());
    let mut no_data = item(0, "MdzF0rFOC9Y");
    no_data.backend_data = None;
    let answers = engine
        .queue
        .download(0, vec![renamed, other_video, no_data], "S [1]", "r", "u1")
        .unwrap();
    assert!(answers.is_empty(), "{answers:?}");
    // Nor does a folder that is not one folder down.
    assert!(engine
        .queue
        .download(0, vec![item(0, "MdzF0rFOC9Y")], "../up", "r", "u1")
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn one_user_cannot_fill_the_queue() {
    let rig = Rig::new();
    let engine = rig
        .engine(Pace {
            per_user: 1,
            ..QUICK
        })
        .await;
    // Held up, so the rest wait.
    enqueued(
        &engine
            .queue
            .download(0, vec![item(0, "SLOWSLOWSLO")], "S [1]", "r", "u1")
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while rig.fetched().is_empty() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let answers = engine
        .queue
        .download(
            0,
            vec![item(1, "alice000001"), item(2, "alice000002")],
            "S [1]",
            "r",
            "u1",
        )
        .unwrap();
    assert_eq!(answers[0].status, EnqueueStatus::Enqueued);
    assert!(
        matches!(&answers[1].status, EnqueueStatus::Failed(f) if f.code == FailureCode::EnqueueFailed)
    );
    let other = engine
        .queue
        .download(0, vec![item(3, "bob00000003")], "B [1]", "r", "u2")
        .unwrap();
    assert_eq!(other[0].status, EnqueueStatus::Enqueued);
    engine.stop().await;
}

#[tokio::test]
async fn starting_clears_what_a_previous_run_left_and_nothing_else() {
    let rig = Rig::new();
    let partial = rig.partial();
    std::fs::create_dir_all(partial.join("old-transfer")).unwrap();
    std::fs::write(partial.join("old-transfer").join("x.webm.part"), b"x").unwrap();
    std::fs::create_dir_all(partial.join("cache")).unwrap();
    std::fs::write(partial.join("cache").join("player.js"), b"js").unwrap();
    let batch = rig.downloads().join("Kept [1]");
    std::fs::create_dir_all(&batch).unwrap();
    std::fs::write(batch.join("01.opus"), b"kept").unwrap();
    let other = rig
        .downloads()
        .join(crate::partial::PARTIAL_FOLDER)
        .join("test-install")
        .join("archive");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("t1"), b"archive part").unwrap();

    let _engine = rig.engine(QUICK).await;

    assert!(!partial.join("old-transfer").exists());
    assert_eq!(
        std::fs::read(partial.join("cache").join("player.js")).unwrap(),
        b"js"
    );
    assert_eq!(std::fs::read(batch.join("01.opus")).unwrap(), b"kept");
    assert_eq!(std::fs::read(other.join("t1")).unwrap(), b"archive part");
}

#[tokio::test]
async fn a_track_a_song_search_offers_is_one_the_queue_takes() {
    let one = Wanted {
        album: None,
        tracks: vec![track(
            "Breadcrumb Trail",
            "Slint",
            Some("Spiderland"),
            "5:56",
        )],
    };
    let songs = crate::ytdlp::matching::tests::songs("slint-songs");
    let (groups, _) = matching::song_groups(&songs, 0, &one, false, 0);
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let answers = engine
        .queue
        .download(0, vec![groups[0].items[0].clone()], "S [1]", "r", "u1")
        .unwrap();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].status, EnqueueStatus::Enqueued);
}

#[tokio::test]
async fn a_loose_match_lands_without_musicbrainz_ids() {
    let rig = Rig::new();
    let engine = rig.engine(QUICK).await;
    let mut loose = item(0, "MdzF0rFOC9Y");
    let mut data: YtTrack = serde_json::from_str(loose.backend_data.as_deref().unwrap()).unwrap();
    data.title_score = 0.7;
    loose.backend_data = Some(serde_json::to_string(&data).unwrap());
    let ids = enqueued(
        &engine
            .queue
            .download(0, vec![loose.clone()], "S [1]", "r", "u1")
            .unwrap(),
    );
    assert_eq!(
        ended(&engine, &loose.source, &ids[0]).await.state,
        DownloadState::Completed
    );
    let file = lofty::read_from_path(rig.downloads().join("S [1]").join(&loose.id)).unwrap();
    let tag = file.primary_tag().unwrap();
    assert_eq!(
        tag.get_string(ItemKey::TrackTitle),
        Some("Breadcrumb Trail")
    );
    assert_eq!(tag.get_string(ItemKey::MusicBrainzRecordingId), None);
}
