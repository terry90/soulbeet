//! Searches of the hubs the sidecar is connected to. Each runs as a task: it waits for the
//! install's search turn, sends one hub search, reads the results for 20 s, loads the file lists
//! of the best album folders and groups the files by peer and folder, scored as slskd's are.

use super::error::AirdcppError;
use super::http::Sidecar;
use super::models::{
    whole, FilelistItems, FilelistSession, GroupedResult, HubSearchQueued, ListItem,
    SearchInstance, SidecarSettings, UserResult,
};
use super::Timing;
use crate::audio::is_audio_path;
use crate::backend_error::BackendError;
use crate::matching::assign::assign_tracks;
use crate::matching::path::{is_disc_folder, rank_match, Ranked, WantedTrack};
use crate::matching::score::{group_score, MIN_SCORE_THRESHOLD};
use itertools::Itertools;
use reqwest::Method;
use serde_json::json;
use shared::airdcpp::{is_tiger_hash, ItemData, Source, BACKEND_ID};
use shared::download::{
    format_score, DownloadableGroup, DownloadableItem, FileFacts, SearchResult, SearchSpec,
    SearchState, SearchStats,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::{sleep, Instant};
use tracing::{info, warn};

/// How long an ended search can still be polled.
const KEPT: Duration = Duration::from_secs(600);
/// Album folders whose file lists one search loads, and of one nick.
const LISTS_PER_SEARCH: usize = 4;
const LISTS_PER_NICK: usize = 2;
/// File lists loading at once for the install.
pub(crate) const LISTS_AT_ONCE: usize = 2;
/// Disc folders of one release whose lists are loaded.
const DISCS: usize = 4;
/// Results read, and results whose users are read one by one.
const RESULTS: usize = 200;
const CHILDREN: usize = 50;
/// Search priority 4: normal, which AirDC++ sends to each hub at most every 10 s.
const PRIORITY: u32 = 4;
/// Words in brackets that name an edition, which hub searches match nothing with.
const EDITION_WORDS: &[&str] = &[
    "deluxe",
    "edition",
    "remaster",
    "remastered",
    "expanded",
    "anniversary",
    "bonus",
    "special",
];
/// Searches no hub takes: some hubs refuse anything shorter.
const MIN_TEXT_CHARS: usize = 5;

/// The install's turn to search: one search at a time, first come first served, each at least
/// the sidecar's interval after the one before. Alternate searches of stalled files take it
/// only when no search waits for it.
#[derive(Default)]
pub(crate) struct Turn {
    last: tokio::sync::Mutex<Option<Instant>>,
    /// Searches waiting for the turn.
    waiting: AtomicUsize,
    /// Told when a search got the turn.
    took: tokio::sync::Notify,
}

/// A search's place in the wait for the turn, left when dropped.
struct Waiting<'a>(&'a Turn);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::SeqCst);
        self.0.took.notify_waiters();
    }
}

impl Turn {
    /// Waits for the turn and `interval` after the last search, then runs `send` in it. The turn
    /// counts as used when `send` says a search went out.
    pub(crate) async fn take<T>(
        &self,
        interval: Duration,
        send: impl std::future::Future<Output = (bool, T)>,
    ) -> T {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let waiting = Waiting(self);
        let mut last = self.last.lock().await;
        drop(waiting);
        Self::run(&mut last, interval, send).await
    }

    /// `take` for an alternate search: it goes only when no search waits for the turn, and
    /// lets one that came meanwhile go first.
    pub(crate) async fn take_after_searches<T>(
        &self,
        interval: Duration,
        send: impl std::future::Future<Output = (bool, T)>,
    ) -> T {
        loop {
            let took = self.took.notified();
            tokio::pin!(took);
            // Registered before the look, so a search taking the turn after it still wakes this.
            took.as_mut().enable();
            if self.waiting.load(Ordering::SeqCst) == 0 {
                let mut last = self.last.lock().await;
                if let Some(at) = *last {
                    tokio::time::sleep_until(at + interval).await;
                }
                if self.waiting.load(Ordering::SeqCst) == 0 {
                    return Self::run(&mut last, interval, send).await;
                }
            }
            took.await;
        }
    }

    async fn run<T>(
        last: &mut Option<Instant>,
        interval: Duration,
        send: impl std::future::Future<Output = (bool, T)>,
    ) -> T {
        if let Some(at) = *last {
            tokio::time::sleep_until(at + interval).await;
        }
        let (sent, answer) = send.await;
        if sent {
            *last = Some(Instant::now());
        }
        answer
    }
}

/// How file lists are loaded: a few at once for the install, one per peer at a time.
pub(crate) struct ListSlots {
    permits: tokio::sync::Semaphore,
    peers: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    timing: Timing,
}

impl ListSlots {
    fn new(timing: Timing) -> Self {
        ListSlots {
            permits: tokio::sync::Semaphore::new(LISTS_AT_ONCE),
            peers: Mutex::new(HashMap::new()),
            timing,
        }
    }
}

impl ListSlots {
    fn peer(&self, cid: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut peers = self.peers.lock().unwrap();
        peers.retain(|_, lock| Arc::strong_count(lock) > 1);
        peers.entry(cid.to_string()).or_default().clone()
    }
}

/// What a search is for.
#[derive(Debug, Clone)]
pub(crate) struct Wanted {
    pub artist: String,
    pub album: Option<String>,
    pub tracks: Vec<WantedTrack>,
    pub keep_below_floor: bool,
}

impl Wanted {
    fn of(spec: &SearchSpec) -> Result<Wanted, AirdcppError> {
        let artist = match (&spec.album, spec.tracks.as_slice()) {
            (Some(album), _) => album.artist.clone(),
            (None, [track]) => track.artist.clone(),
            (None, tracks) => {
                return Err(AirdcppError::NoSearchTarget {
                    tracks: tracks.len(),
                })
            }
        };
        Ok(Wanted {
            artist,
            album: spec.album.as_ref().map(|a| a.title.clone()),
            tracks: (spec.tracks.iter())
                .map(|t| WantedTrack {
                    title: t.title.clone(),
                    duration_s: t.duration_secs(),
                })
                .collect(),
            keep_below_floor: spec.keep_below_floor,
        })
    }

    /// An album search loads its folders' file lists; a track search reads the files found.
    fn is_album(&self) -> bool {
        self.album.is_some() && self.tracks.len() != 1
    }
}

/// Whether `c` is a Latin letter with a diacritic, which hub searches match nothing with.
fn accented_latin(c: char) -> bool {
    c.is_alphabetic() && !c.is_ascii() && matches!(u32::from(c), 0x00C0..=0x024F | 0x1E00..=0x1EFF)
}

/// `text` without the bracketed parts that name an edition, as `(Deluxe Edition)`.
fn without_editions(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find(['(', '[']) {
        let close_char = if rest[open..].starts_with('(') {
            ')'
        } else {
            ']'
        };
        let Some(len) = rest[open..].find(close_char) else {
            break;
        };
        let inner = rest[open + 1..open + len].to_lowercase();
        let edition = inner
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| EDITION_WORDS.contains(&w));
        out.push_str(&rest[..open]);
        if !edition {
            out.push_str(&rest[open..=open + len]);
        }
        rest = &rest[open + len + 1..];
    }
    out.push_str(rest);
    out
}

/// The text a search sends: the spec's own, or the artist and the album (or the track), without
/// edition brackets and without words holding an accented Latin letter. One too short to search
/// is refused.
pub(crate) fn query_text(spec: &SearchSpec) -> Result<String, AirdcppError> {
    let base = match spec.text.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => match (&spec.album, spec.tracks.as_slice()) {
            (Some(album), [track]) => format!("{} {}", album.artist, track.title),
            (Some(album), _) => format!("{} {}", album.artist, album.title),
            (None, [track]) => format!("{} {}", track.artist, track.title),
            (None, tracks) => {
                return Err(AirdcppError::NoSearchTarget {
                    tracks: tracks.len(),
                })
            }
        },
    };
    let text = without_editions(&base)
        .split_whitespace()
        .filter(|word| !word.chars().any(accented_latin))
        .join(" ");
    if text.chars().count() < MIN_TEXT_CHARS {
        return Err(AirdcppError::QueryRejected(format!(
            "Direct Connect hubs take no search shorter than {MIN_TEXT_CHARS} characters, \
             and \"{base}\" leaves \"{text}\""
        )));
    }
    Ok(text)
}

/// One file or folder one peer has, as the search or its file list gave it.
#[derive(Debug, Clone)]
struct Hit {
    source: Source,
    nick: String,
    hub_name: String,
    /// The ADC path: a folder ends with `/`.
    path: String,
    size: u64,
    tth: String,
    directory: bool,
    free_slot: Option<bool>,
    same_tth: u32,
}

/// Whether a path holds a control byte, which AirDC++ would replace in the name it saves.
fn has_control(path: &str) -> bool {
    path.bytes().any(|b| (1..=31).contains(&b))
}

/// What the search answers say of the free slots: unknown for the `0/255` NMDC placeholder.
pub(super) fn free_slot(free: f64, total: f64) -> Option<bool> {
    let (free, total) = (whole(free), whole(total));
    match (free, total) {
        (0, 255) => None,
        (0, _) => Some(false),
        _ => Some(true),
    }
}

/// The release folder a hit is in, without its trailing `/`: a file's folder, or the one above
/// when that is one disc of the release; for a folder hit, the folder or the one above.
fn release_of(hit: &Hit) -> String {
    let folder = match hit.directory {
        true => hit.path.trim_end_matches('/'),
        false => hit.path.rsplit_once('/').map_or("", |(folder, _)| folder),
    };
    match folder.rsplit_once('/') {
        Some((above, name)) if is_disc_folder(name) => above.to_string(),
        _ => folder.to_string(),
    }
}

/// Why results were dropped, counted for the search's stats.
#[derive(Debug, Default)]
struct Dropped {
    /// From peers that, as this sidecar, take no incoming connections.
    passive: u32,
}

/// The hits of the grouped results and their users, without the ones Soulful cannot get.
fn hits(
    results: &[GroupedResult],
    children: &HashMap<String, Vec<UserResult>>,
    passive_here: bool,
    dropped: &mut Dropped,
) -> Vec<Hit> {
    let mut out = Vec::new();
    for result in results {
        let users: Vec<UserResult> = match children.get(&result.id) {
            Some(users) if whole(result.users.count) > 1 => users.clone(),
            _ => vec![UserResult {
                path: result.path.clone(),
                size: result.size,
                tth: result.tth.clone(),
                kind: result.kind.clone(),
                slots: result.slots.clone(),
                user: result.users.user.clone(),
            }],
        };
        for found in users {
            let user = &found.user;
            // No NMDC connection is made between two passive clients.
            if passive_here && user.has("nmdc") && (user.has("passive") || user.has("noconnect")) {
                dropped.passive += 1;
                continue;
            }
            // Known fake answers come from bots with operator rights.
            if user.has("bot") && user.has("op") {
                continue;
            }
            if has_control(&found.path) {
                continue;
            }
            let Some(source) = Source::parse(&format!("{}@{}", user.cid, user.hub_url)) else {
                continue;
            };
            out.push(Hit {
                source,
                nick: user.nicks.clone(),
                hub_name: user.hub_names.clone(),
                path: found.path.clone(),
                size: whole(found.size),
                tth: found.tth.clone(),
                directory: found.kind.id == "directory",
                free_slot: free_slot(found.slots.free, found.slots.total),
                same_tth: whole(result.hits) as u32,
            });
        }
    }
    out
}

/// A surround mix: its folder is left out, since hubs often label one as the album.
fn surround(path: &str) -> bool {
    path.to_lowercase().contains("{surround}") || path.to_lowercase().ends_with(".ec3")
}

/// The files one peer has in one release folder.
#[derive(Debug, Clone)]
struct Folder {
    source: Source,
    nick: String,
    hub_name: String,
    /// The release folder's ADC path, without its trailing `/`.
    path: String,
    files: Vec<Hit>,
    free_slot: Option<bool>,
    /// Audio files its file list holds; None while only the search's files are known.
    listed_audio: Option<u32>,
}

fn folders(hits: Vec<Hit>) -> Vec<Folder> {
    let mut by_key: Vec<((Source, String), Folder)> = Vec::new();
    let surround_folders: HashSet<String> = hits
        .iter()
        .filter(|h| surround(&h.path))
        .map(release_of)
        .collect();
    for hit in hits {
        let path = release_of(&hit);
        if surround_folders.contains(&path) {
            continue;
        }
        let key = (hit.source.clone(), path.clone());
        let folder = match by_key.iter_mut().find(|(k, _)| *k == key) {
            Some((_, folder)) => folder,
            None => {
                by_key.push((
                    key,
                    Folder {
                        source: hit.source.clone(),
                        nick: hit.nick.clone(),
                        hub_name: hit.hub_name.clone(),
                        path,
                        files: vec![],
                        free_slot: hit.free_slot,
                        listed_audio: None,
                    },
                ));
                &mut by_key.last_mut().unwrap().1
            }
        };
        if !hit.directory && !folder.files.iter().any(|f| f.path == hit.path) {
            folder.free_slot = folder.free_slot.or(hit.free_slot);
            folder.files.push(hit);
        }
    }
    by_key.into_iter().map(|(_, f)| f).collect()
}

/// How well a folder's path names the artist and the album.
fn folder_score(folder: &Folder, wanted: &Wanted) -> f64 {
    let ranked = rank_match(
        &format!("{}/x", folder.path),
        Some(&wanted.artist),
        wanted.album.as_deref(),
        &[],
        None,
    );
    ranked[0].result.total_score
}

/// The folders whose file lists are loaded: the best named first, a few per nick.
fn to_list(folders: &[Folder], wanted: &Wanted) -> Vec<usize> {
    let mut order: Vec<(usize, f64)> = (folders.iter().enumerate())
        .map(|(i, f)| (i, folder_score(f, wanted)))
        .filter(|(_, score)| *score > 0.0)
        .collect();
    order.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut per_nick: HashMap<&str, usize> = HashMap::new();
    let mut chosen = Vec::new();
    for (i, _) in order {
        let taken = per_nick.entry(folders[i].nick.as_str()).or_default();
        if *taken < LISTS_PER_NICK {
            *taken += 1;
            chosen.push(i);
        }
        if chosen.len() == LISTS_PER_SEARCH {
            break;
        }
    }
    chosen
}

const LOSSLESS: &[&str] = &["flac", "wav"];

fn extension_of(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
}

/// A lossy file's bitrate as its size over the length of the track it matched.
fn estimated_kbps(size: u64, extension: &str, duration_s: Option<u32>) -> Option<u32> {
    if LOSSLESS.contains(&extension) {
        return None;
    }
    let seconds = u64::from(duration_s.filter(|d| *d > 0)?);
    u32::try_from(size * 8 / 1000 / seconds).ok()
}

/// A folder's files scored as the wanted tracks: its group, and how many audio files scored
/// below the floor. None when no file is a wanted track.
fn group(folder: &Folder, wanted: &Wanted) -> (Option<DownloadableGroup>, u32) {
    struct Candidate {
        track: usize,
        ranked: Ranked,
        below_floor: bool,
    }
    let under = |r: &Ranked| {
        r.result.total_score < MIN_SCORE_THRESHOLD
            || (r.track.is_some() && r.result.track_score == 0.0)
    };
    let mut below = 0;
    let mut scored: Vec<(Vec<Candidate>, &Hit)> = Vec::new();
    for file in &folder.files {
        if !is_audio_path(Path::new(&file.path)) || !is_tiger_hash(&file.tth) || file.size == 0 {
            continue;
        }
        let ranked = rank_match(
            &file.path,
            Some(&wanted.artist),
            wanted.album.as_deref(),
            &wanted.tracks,
            None,
        );
        if under(&ranked[0]) {
            below += 1;
            if !wanted.keep_below_floor {
                continue;
            }
        }
        let candidates = ranked
            .into_iter()
            .filter_map(|r| {
                Some(Candidate {
                    below_floor: under(&r),
                    track: r.track?,
                    ranked: r,
                })
            })
            .filter(|c| wanted.keep_below_floor || !c.below_floor)
            .collect();
        scored.push((candidates, file));
    }
    let quality = |file: &Hit, kbps: Option<u32>| {
        let ext = extension_of(&file.path).unwrap_or_default();
        let slot = if folder.free_slot == Some(true) {
            0.1
        } else {
            0.0
        };
        (format_score(&ext, kbps) + slot).min(1.0)
    };
    let kbps_of = |file: &Hit, track: usize| {
        let ext = extension_of(&file.path).unwrap_or_default();
        estimated_kbps(file.size, &ext, wanted.tracks[track].duration_s)
    };
    let taken = assign_tracks(
        &wanted.tracks,
        &scored,
        |(candidates, _), track, _| {
            (candidates.iter())
                .find(|c| c.track == track)
                .map(|c| c.ranked.result.total_score)
        },
        // Ties go to the better file, its bitrate estimated as the track it best matches.
        |(candidates, file)| {
            let kbps = candidates.first().and_then(|c| kbps_of(file, c.track));
            quality(file, kbps)
        },
    );
    let items: Vec<DownloadableItem> = taken
        .into_iter()
        .enumerate()
        .filter_map(|(track, took)| {
            let (index, _) = took?;
            let (candidates, file) = &scored[index];
            let candidate = candidates.iter().find(|c| c.track == track)?;
            let kbps = kbps_of(file, track);
            Some(item(
                folder,
                file,
                candidate.ranked.clone(),
                candidate.below_floor,
                kbps,
                quality(file, kbps),
            ))
        })
        .collect();
    if items.is_empty() {
        return (None, below);
    }
    let completeness = items.len() as f64 / wanted.tracks.len().max(1) as f64;
    let mean = |f: &dyn Fn(&DownloadableItem) -> f64| {
        items.iter().map(f).sum::<f64>() / items.len() as f64
    };
    let score = group_score(
        mean(&|i| i.match_score),
        completeness,
        mean(&|i| i.quality_score),
    );
    let quality = (items.iter())
        .map(|i| i.quality.clone())
        .counts()
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(q, _)| q)
        .unwrap_or_default();
    let group = DownloadableGroup {
        source: folder.source.to_string(),
        group_id: folder.path.clone(),
        title: items[0].album.clone(),
        artist: Some(items[0].artist.clone()),
        item_count: items.len(),
        total_size: items.iter().filter_map(|i| i.size).sum(),
        items,
        quality,
        score,
    };
    (Some(group), below)
}

fn item(
    folder: &Folder,
    file: &Hit,
    ranked: Ranked,
    below_floor: bool,
    kbps: Option<u32>,
    quality_score: f64,
) -> DownloadableItem {
    let extension = extension_of(&file.path);
    let data = ItemData {
        cid: folder.source.cid.clone(),
        hub_url: folder.source.hub_url.clone(),
        nick: folder.nick.clone(),
        hub_name: folder.hub_name.clone(),
        tth: file.tth.clone(),
        size: file.size,
        listed: folder.listed_audio.is_some(),
    };
    let mut facts = serde_json::Map::new();
    facts.insert("nick".into(), folder.nick.clone().into());
    facts.insert("hub_url".into(), folder.source.hub_url.clone().into());
    facts.insert("hub_name".into(), folder.hub_name.clone().into());
    facts.insert("tth".into(), file.tth.clone().into());
    facts.insert("listed".into(), data.listed.into());
    facts.insert("same_tth_users".into(), file.same_tth.into());
    if let Some(kbps) = kbps {
        facts.insert("estimated_kbps".into(), kbps.into());
    }
    DownloadableItem {
        id: file.path.clone(),
        backend: BACKEND_ID.into(),
        source: folder.source.to_string(),
        title: ranked.result.matched_track,
        artist: ranked.result.guessed_artist,
        album: ranked.result.guessed_album,
        size: Some(file.size),
        duration: None,
        quality: extension.clone().unwrap_or_else(|| "unknown".into()),
        quality_score,
        backend_data: serde_json::to_string(&data).ok(),
        match_score: ranked.result.total_score,
        below_floor,
        facts: FileFacts {
            extension,
            folder_audio_files: folder.listed_audio,
            source_has_free_slot: folder.free_slot,
            backend_facts: facts,
            ..FileFacts::default()
        },
    }
}

/// The groups of `folders`, best first, and the files that scored below the floor.
fn groups(folders: &[Folder], wanted: &Wanted) -> (Vec<DownloadableGroup>, u32) {
    let mut below = 0;
    let mut out = Vec::new();
    for folder in folders {
        let (group, under) = group(folder, wanted);
        below += under;
        out.extend(group);
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (out, below)
}

#[derive(Clone)]
enum Phase {
    Running,
    Ended(Instant),
    Failed(Instant, BackendError),
}

struct Progress {
    groups: Vec<DownloadableGroup>,
    stats: SearchStats,
    phase: Phase,
}

struct Run {
    progress: Mutex<Progress>,
    stop: AtomicBool,
}

impl Run {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn show(&self, groups: Vec<DownloadableGroup>, stats: SearchStats) {
        let mut progress = self.progress.lock().unwrap();
        progress.groups = groups;
        progress.stats = stats;
    }

    fn end(&self, failed: Option<BackendError>) {
        let mut progress = self.progress.lock().unwrap();
        progress.phase = match failed {
            Some(e) => Phase::Failed(Instant::now(), e),
            None => Phase::Ended(Instant::now()),
        };
    }

    fn ended_at(&self) -> Option<Instant> {
        match self.progress.lock().unwrap().phase {
            Phase::Running => None,
            Phase::Ended(at) | Phase::Failed(at, _) => Some(at),
        }
    }
}

/// What every search of the process shares.
pub(crate) struct Shared {
    pub turn: Turn,
    pub lists: ListSlots,
    pub timing: Timing,
}

impl Shared {
    pub(crate) fn new(timing: Timing) -> Shared {
        Shared {
            turn: Turn::default(),
            lists: ListSlots::new(timing),
            timing,
        }
    }
}

/// The searches of the process, by id.
#[derive(Default)]
pub(crate) struct Searches {
    runs: Mutex<HashMap<String, Arc<Run>>>,
}

impl Searches {
    fn prune(runs: &mut HashMap<String, Arc<Run>>) {
        runs.retain(|_, run| run.ended_at().is_none_or(|at| at.elapsed() < KEPT));
    }

    /// Starts a search for `spec` on `sidecar`: its id, a Soulful uuid.
    pub(crate) fn start(
        &self,
        sidecar: Arc<Sidecar>,
        shared: Arc<Shared>,
        spec: &SearchSpec,
    ) -> Result<String, AirdcppError> {
        let wanted = Wanted::of(spec)?;
        let text = query_text(spec)?;
        let id = uuid::Uuid::new_v4().to_string();
        let run = Arc::new(Run {
            progress: Mutex::new(Progress {
                groups: vec![],
                stats: SearchStats {
                    query_text: text.clone(),
                    ..SearchStats::default()
                },
                phase: Phase::Running,
            }),
            stop: AtomicBool::new(false),
        });
        {
            let mut runs = self.runs.lock().unwrap();
            Self::prune(&mut runs);
            runs.insert(id.clone(), run.clone());
        }
        tokio::spawn(async move {
            let failed = search(&sidecar, &shared, &wanted, &text, &run).await.err();
            if let Some(e) = &failed {
                warn!("Direct Connect search for {text} failed: {e}");
            }
            run.end(failed.map(BackendError::from));
        });
        Ok(id)
    }

    pub(crate) fn poll(&self, search_id: &str) -> Result<SearchResult, BackendError> {
        let run = {
            let mut runs = self.runs.lock().unwrap();
            Self::prune(&mut runs);
            runs.get(search_id).cloned()
        };
        let run = run.ok_or_else(|| AirdcppError::UnknownSearch {
            search_id: search_id.to_string(),
        })?;
        let progress = run.progress.lock().unwrap();
        let state = match &progress.phase {
            // A stopped search reads what it found so far.
            Phase::Running if !run.stopped() => SearchState::InProgress,
            Phase::Running | Phase::Ended(_) => SearchState::Completed,
            Phase::Failed(_, e) => return Err(e.clone()),
        };
        Ok(SearchResult {
            search_id: search_id.to_string(),
            groups: progress.groups.clone(),
            has_more: false,
            stats: match state {
                SearchState::InProgress => SearchStats::default(),
                _ => progress.stats.clone(),
            },
            state,
        })
    }

    /// Stops a search: it loads no more lists. One the backend does not run is no error.
    pub(crate) fn stop(&self, search_id: &str) {
        if let Some(run) = self.runs.lock().unwrap().get(search_id) {
            run.stop.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) fn stop_all(&self) {
        for run in self.runs.lock().unwrap().values() {
            run.stop.store(true, Ordering::SeqCst);
        }
    }
}

/// Sleeps `wait` in short steps: false once the search was stopped.
async fn wait_unless_stopped(run: &Run, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        if run.stopped() {
            return false;
        }
        sleep((until - Instant::now()).min(Duration::from_millis(250))).await;
    }
    !run.stopped()
}

/// Makes a search instance and sends the hub search of `text` in it, both in the install's turn,
/// so the instance's 10 minutes start when the search goes out: the instance, and when the
/// search goes out to the last hub, from now. None when the search was stopped before its turn
/// came.
async fn send(
    sidecar: &Sidecar,
    shared: &Shared,
    interval: Duration,
    text: &str,
    run: &Run,
) -> Result<Option<(SearchInstance, Result<Duration, AirdcppError>)>, AirdcppError> {
    let body = json!({
        "query": {"pattern": text, "file_type": "any"},
        "priority": PRIORITY,
    });
    // The owner AirDC++ gives the instance names this run, so a restart of AirDC++ that hands
    // its id to another instance is noticed.
    let made = json!({
        "expiration": 10,
        "owner_suffix": format!("soulbeet-{}", uuid::Uuid::new_v4()),
    });
    let sent = shared
        .turn
        .take(interval, async {
            if run.stopped() {
                return (false, None);
            }
            let instance: SearchInstance = match sidecar.post("search", &made).await {
                Ok(instance) => instance,
                Err(e) => return (false, Some(Err(e))),
            };
            let path = format!("search/{}/hub_search", instance.id);
            let answer = sidecar.send(Method::POST, &path, Some(&body)).await;
            let sent = matches!(&answer, Ok(a) if a.ok());
            (sent, Some(Ok((instance, answer))))
        })
        .await;
    let Some(made) = sent else {
        return Ok(None);
    };
    let (instance, answer) = made?;
    let queued = answer.and_then(queue_time);
    Ok(Some((instance, queued)))
}

/// When a hub search goes out to the last hub, from its answer.
fn queue_time(sent: super::http::Answer) -> Result<Duration, AirdcppError> {
    match sent.status {
        503 => Err(AirdcppError::SearchOverflow),
        400 => {
            let message = sent.message();
            match message.to_lowercase().contains("hub") {
                true => Err(AirdcppError::NoHub),
                false => Err(AirdcppError::QueryRejected(format!(
                    "AirDC++ refused the search: {message}"
                ))),
            }
        }
        _ => {
            let queued: HubSearchQueued = sent.json()?;
            if queued.queued_count == 0 {
                return Err(AirdcppError::NoHub);
            }
            Ok(Duration::from_millis(queued.queue_time))
        }
    }
}

/// Whether the instance AirDC++ holds under `instance`'s id is still this run's. A restart of
/// AirDC++ starts its ids at 1 again, so the id may name another client's search by now.
async fn ours(sidecar: &Sidecar, instance: &SearchInstance) -> Result<bool, AirdcppError> {
    let answer = sidecar
        .send(Method::GET, &format!("search/{}", instance.id), None)
        .await?;
    if answer.status == 404 {
        return Ok(false);
    }
    let held: SearchInstance = answer.json()?;
    Ok(held.owner == instance.owner)
}

/// Runs a search into `run`, deleting its instance in the sidecar at the end while it is still
/// this run's.
async fn search(
    sidecar: &Sidecar,
    shared: &Shared,
    wanted: &Wanted,
    text: &str,
    run: &Run,
) -> Result<(), AirdcppError> {
    let settings: SidecarSettings = sidecar
        .post("settings/get", &json!({ "keys": SidecarSettings::KEYS }))
        .await?;
    let interval =
        Duration::from_secs(settings.min_search_interval).max(shared.timing.min_hub_interval);
    let Some((instance, queued)) = send(sidecar, shared, interval, text, run).await? else {
        return Ok(());
    };
    let found = match queued {
        Ok(queued) => {
            collect(
                sidecar, shared, &settings, &instance, queued, wanted, text, run,
            )
            .await
        }
        Err(e) => Err(e),
    };
    match ours(sidecar, &instance).await {
        Ok(true) => {
            let deleted = sidecar
                .send(Method::DELETE, &format!("search/{}", instance.id), None)
                .await;
            if let Err(e) = deleted.and_then(|a| a.done()) {
                warn!("Could not delete AirDC++ search {}: {e}", instance.id);
            }
        }
        // AirDC++ drops it within 10 minutes by itself.
        Ok(false) => info!(
            "AirDC++ search {} is no longer this one's; left alone",
            instance.id
        ),
        Err(e) => warn!("Could not read AirDC++ search {}: {e}", instance.id),
    }
    found
}

#[allow(clippy::too_many_arguments)]
async fn collect(
    sidecar: &Sidecar,
    shared: &Shared,
    settings: &SidecarSettings,
    instance: &SearchInstance,
    queued: Duration,
    wanted: &Wanted,
    text: &str,
    run: &Run,
) -> Result<(), AirdcppError> {
    let started = Instant::now() + queued;
    // A search stopped meanwhile still reads what came, and loads no list.
    wait_unless_stopped(run, queued + shared.timing.collect).await;
    if !ours(sidecar, instance).await? {
        return Err(AirdcppError::Restarted);
    }
    let instance = instance.id;
    let results: Vec<GroupedResult> = sidecar
        .get(&format!("search/{instance}/results/0/{RESULTS}"))
        .await?;
    let mut children = HashMap::new();
    for result in (results.iter())
        .filter(|r| whole(r.users.count) > 1)
        .take(CHILDREN)
    {
        let path = format!("search/{instance}/results/{}/children", result.id);
        match sidecar.get::<Vec<UserResult>>(&path).await {
            Ok(users) => {
                children.insert(result.id.clone(), users);
            }
            Err(e) => warn!("AirDC++ users of a search result: {e}"),
        }
    }
    let mut dropped = Dropped::default();
    let hits = hits(&results, &children, settings.passive(), &mut dropped);
    let not_audio = (hits.iter())
        .filter(|h| !h.directory && !is_audio_path(Path::new(&h.path)))
        .count() as u32;
    let audio = (hits.iter())
        .filter(|h| !h.directory && is_audio_path(Path::new(&h.path)))
        .count() as u32;
    let responders = (hits.iter()).map(|h| &h.source).unique().count() as u32;
    let mut folders = folders(hits);
    let note = match (results.is_empty(), responders, dropped.passive) {
        (true, _, _) => Some("No Direct Connect hub answered.".to_string()),
        (false, 0, n) if n > 0 => Some(format!(
            "Every answer ({n}) came from a peer that, as this AirDC++, takes no incoming \
             connections, so none of them can send files to it."
        )),
        _ => None,
    };
    let stats = |below_floor, file_count| SearchStats {
        query_text: text.to_string(),
        response_count: responders,
        file_count,
        not_audio,
        below_floor,
        unanswered_note: note.clone(),
        ..SearchStats::default()
    };
    let (found, below) = groups(&folders, wanted);
    run.show(found, stats(below, audio));
    if dropped.passive > 0 {
        info!(
            "Direct Connect search for {text}: {} answers from passive peers left out",
            dropped.passive
        );
    }
    if !wanted.is_album() {
        return Ok(());
    }
    let mut listed_files = 0;
    for index in to_list(&folders, wanted) {
        if run.stopped() || Instant::now() > started + shared.timing.deadline {
            break;
        }
        let folder = &folders[index];
        let Some(items) = listed(sidecar, &shared.lists, &folder.source, &folder.path, run).await
        else {
            continue;
        };
        let listed = &mut folders[index];
        let audio_items: Vec<&ListItem> = (items.iter())
            .filter(|i| i.kind.id == "file" && is_audio_path(Path::new(&i.path)))
            .collect();
        listed_files += audio_items.len() as u32;
        listed.listed_audio = Some(audio_items.len() as u32);
        listed.files = (audio_items.into_iter())
            .filter(|i| !has_control(&i.path))
            .map(|i| Hit {
                source: listed.source.clone(),
                nick: listed.nick.clone(),
                hub_name: listed.hub_name.clone(),
                path: i.path.clone(),
                size: whole(i.size),
                tth: i.tth.clone(),
                directory: false,
                free_slot: listed.free_slot,
                same_tth: 1,
            })
            .collect();
        let (found, below) = groups(&folders, wanted);
        run.show(found, stats(below, audio + listed_files));
    }
    Ok(())
}

/// The states of a file list session still loading.
const LIST_LOADING: [&str; 2] = ["download_pending", "downloading"];

/// Closes the list `session` this search opened, unless the sidecar's user has moved it to a
/// folder other than those the search asked for, `asked`: then it is theirs now and stays open.
async fn close_list(sidecar: &Sidecar, session: &str, asked: &[String]) {
    let state = match sidecar.send(Method::GET, session, None).await {
        Ok(answer) if answer.status == 404 => return,
        Ok(answer) => answer.json::<FilelistSession>(),
        Err(e) => Err(e),
    };
    let ours = match &state {
        Ok(state) => {
            LIST_LOADING.contains(&state.state.id.as_str())
                || (state.location.as_ref()).is_none_or(|l| asked.contains(&l.path))
        }
        Err(e) => {
            warn!("Could not read an AirDC++ file list before closing it: {e}");
            return;
        }
    };
    if !ours {
        info!("An AirDC++ file list this search opened was moved by its user; left open");
        return;
    }
    let closed = sidecar.send(Method::DELETE, session, None).await;
    if let Err(e) = closed.and_then(|a| a.done()) {
        warn!("Could not close an AirDC++ file list: {e}");
    }
}

/// The files of `folder` and of its disc folders in the file list of `source`, loaded for this
/// search and closed after; None when the list did not load, or one is open already, which is
/// the sidecar user's own and left alone.
async fn listed(
    sidecar: &Sidecar,
    slots: &ListSlots,
    source: &Source,
    folder: &str,
    run: &Run,
) -> Option<Vec<ListItem>> {
    let peer = slots.peer(&source.cid);
    let _peer = peer.lock().await;
    let _permit = slots.permits.acquire().await.ok()?;
    let session = format!("filelists/{}", source.cid);
    match sidecar.send(Method::GET, &session, None).await {
        Ok(a) if a.status == 404 => {}
        Ok(_) => return None,
        Err(e) => {
            warn!("AirDC++ file list session: {e}");
            return None;
        }
    }
    let directory = format!("{folder}/");
    let body = json!({
        "user": {"cid": source.cid, "hub_url": source.hub_url},
        "directory": directory,
    });
    match sidecar.send(Method::POST, "filelists", Some(&body)).await {
        Ok(a) if a.ok() => {}
        Ok(a) => {
            info!("AirDC++ did not open a file list: {}", a.message());
            return None;
        }
        Err(e) => {
            // The request may have opened the list before its answer was lost.
            warn!("AirDC++ file list: {e}");
            close_list(sidecar, &session, &[directory]).await;
            return None;
        }
    }
    let (items, asked) = read_list(sidecar, &session, &directory, run, &slots.timing).await;
    close_list(sidecar, &session, &asked).await;
    items
}

/// Waits until the list `session` shows `directory` loaded.
async fn loaded(
    sidecar: &Sidecar,
    session: &str,
    directory: &str,
    run: &Run,
    wait: Duration,
    poll: Duration,
) -> bool {
    let until = Instant::now() + wait;
    loop {
        if let Ok(state) = sidecar.get::<FilelistSession>(session).await {
            let here = state.location.as_ref().is_some_and(|l| l.path == directory);
            if state.state.id == "loaded" && here {
                return true;
            }
        }
        if Instant::now() >= until || !wait_unless_stopped(run, poll).await {
            return false;
        }
    }
}

/// The items of `directory` and of its disc folders in the list `session`, and every folder the
/// list was asked to show.
async fn read_list(
    sidecar: &Sidecar,
    session: &str,
    directory: &str,
    run: &Run,
    timing: &Timing,
) -> (Option<Vec<ListItem>>, Vec<String>) {
    let mut asked = vec![directory.to_string()];
    if !loaded(
        sidecar,
        session,
        directory,
        run,
        timing.list_wait,
        timing.list_poll,
    )
    .await
    {
        return (None, asked);
    }
    let items_path = format!("{session}/items/0/500");
    let Ok(listed) = sidecar.get::<FilelistItems>(&items_path).await else {
        return (None, asked);
    };
    let mut items = listed.items;
    let discs: Vec<String> = (items.iter())
        .filter(|i| i.kind.id == "directory" && is_disc_folder(&i.name))
        .map(|i| i.path.clone())
        .take(DISCS)
        .collect();
    for disc in discs {
        let body = json!({"list_path": disc, "reload": false});
        asked.push(disc.clone());
        let changed = sidecar
            .send(Method::POST, &format!("{session}/directory"), Some(&body))
            .await;
        if !matches!(changed, Ok(a) if a.ok()) {
            continue;
        }
        if loaded(
            sidecar,
            session,
            &disc,
            run,
            timing.list_wait / 2,
            timing.list_poll,
        )
        .await
        {
            if let Ok(listed) = sidecar.get::<FilelistItems>(&items_path).await {
                items.extend(listed.items);
            }
        }
    }
    (Some(items), asked)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTH: &str = "CNPPZX6TSGAVDPCOAE776LTWHPEI2RET3ZMJJ2A";

    #[test]
    fn of_two_files_that_match_a_track_alike_the_better_bitrate_is_taken() {
        let source = Source {
            cid: "HDHIYJMY23IYPTC75NB6OHBKAU3UTCU5HRMXYPQ".into(),
            hub_url: "nmdcs://hub-one.example:411".into(),
        };
        // 430 s at 128 and at 320 kbps.
        let file = |disc: &str, size: u64| Hit {
            source: source.clone(),
            nick: "peer01".into(),
            hub_name: "Hub One".into(),
            path: format!("/music/Air/Moon Safari/{disc}/01 - La Femme d'Argent.mp3"),
            size,
            tth: TTH.into(),
            directory: false,
            free_slot: Some(true),
            same_tth: 1,
        };
        let (low, high) = (file("CD1", 6_880_000), file("CD2", 17_200_000));
        let wanted = Wanted {
            artist: "Air".into(),
            album: Some("Moon Safari".into()),
            tracks: vec![WantedTrack {
                title: "La Femme d'Argent".into(),
                duration_s: Some(430),
            }],
            keep_below_floor: false,
        };
        let score = |hit: &Hit| {
            let ranked = rank_match(
                &hit.path,
                Some("Air"),
                Some("Moon Safari"),
                &wanted.tracks,
                None,
            );
            ranked[0].result.total_score
        };
        assert_eq!(score(&low), score(&high), "the premise: both match alike");
        let folder = Folder {
            source: source.clone(),
            nick: "peer01".into(),
            hub_name: "Hub One".into(),
            path: "/music/Air/Moon Safari".into(),
            files: vec![low, high.clone()],
            free_slot: Some(true),
            listed_audio: Some(2),
        };
        let (group, _) = group(&folder, &wanted);
        let group = group.unwrap();
        assert_eq!(group.items[0].id, high.path);
        assert_eq!(group.items[0].facts.backend_facts["estimated_kbps"], 320);
    }

    #[tokio::test(start_paused = true)]
    async fn an_alternate_search_lets_every_search_waiting_for_the_turn_go_first() {
        let turn = Arc::new(Turn::default());
        let order = Arc::new(Mutex::new(Vec::new()));
        let interval = Duration::from_secs(10);
        turn.take(interval, async { (true, ()) }).await;
        let go = |name: &'static str, alternate: bool| {
            let (turn, order) = (turn.clone(), order.clone());
            tokio::spawn(async move {
                let send = async {
                    order.lock().unwrap().push(name);
                    (true, ())
                };
                match alternate {
                    true => turn.take_after_searches(interval, send).await,
                    false => turn.take(interval, send).await,
                }
            })
        };
        let alternate = go("alternate", true);
        tokio::time::sleep(Duration::from_millis(1)).await;
        let search = go("search", false);
        let (alternate, search) = tokio::join!(alternate, search);
        alternate.unwrap();
        search.unwrap();
        assert_eq!(*order.lock().unwrap(), ["search", "alternate"]);
    }
}
