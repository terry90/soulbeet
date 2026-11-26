//! Searches of NetEase through GD Studio: each runs as a task that lists an album's tracks or a
//! track's rows, resolves the rows that are wanted tracks and reads their files' headers, and
//! is polled for its groups once it ends.

use super::api::{Api, Listing, Resolved, Row};
use super::error::GdError;
use super::matching::{self, best_album, closest_albums, AlbumFit, Fit, Read, Wanted};
use super::Limits;
use shared::download::{DownloadableGroup, SearchResult, SearchSpec, SearchState, SearchStats};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::{info, warn};

/// How long an ended search can still be polled.
const KEPT: Duration = Duration::from_secs(30 * 60);

/// Rows an album listing asks for: more than any album has tracks.
const ALBUM_ROWS: u32 = 50;
/// Rows a track listing asks for.
const TRACK_ROWS: u32 = 20;
/// Rows of a track search resolved, the best first.
const TRACK_PICKS: usize = 3;

#[derive(Debug, Clone)]
enum Phase {
    Running,
    Ended(Instant, SearchState),
    /// Ended without any answer: why.
    Failed(Instant, GdError),
}

struct Progress {
    groups: Vec<DownloadableGroup>,
    stats: SearchStats,
    phase: Phase,
}

/// One search, as its task fills it.
struct Run {
    progress: Mutex<Progress>,
    stop: watch::Sender<bool>,
}

impl Run {
    fn ended_at(&self) -> Option<Instant> {
        match self.progress.lock().unwrap().phase {
            Phase::Running => None,
            Phase::Ended(at, _) | Phase::Failed(at, _) => Some(at),
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

    /// Starts a search for `spec`: its id.
    pub(crate) fn start(
        &self,
        api: &Arc<Api>,
        limits: &Limits,
        spec: &SearchSpec,
    ) -> Result<String, GdError> {
        let wanted = Wanted::of(spec)?;
        let id = format!("gd-{}", uuid::Uuid::new_v4());
        let (stop, stopped) = watch::channel(false);
        let run = Arc::new(Run {
            progress: Mutex::new(Progress {
                groups: vec![],
                stats: SearchStats::default(),
                phase: Phase::Running,
            }),
            stop,
        });
        {
            let mut runs = self.runs.lock().unwrap();
            Self::prune(&mut runs);
            runs.insert(id.clone(), run.clone());
        }
        let task = Task {
            api: api.clone(),
            wanted,
            text: spec.text.clone().filter(|t| !t.trim().is_empty()),
            keep_below_floor: spec.keep_below_floor,
            deadline: tokio::time::Instant::now() + limits.search_deadline,
            stopped,
            budget: (limits.calls, limits.window, limits.search_deadline),
            stats: SearchStats::default(),
            groups: vec![],
            answered: false,
            failed: None,
            cut: None,
        };
        tokio::spawn(async move {
            let ended = task.run().await;
            let mut progress = run.progress.lock().unwrap();
            progress.groups = ended.groups;
            // Stable: equal groups keep the order NetEase listed them in.
            progress.groups.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            progress.stats = ended.stats;
            progress.phase = match ended.failed {
                Some(e) if !ended.answered => Phase::Failed(Instant::now(), e),
                _ => Phase::Ended(Instant::now(), ended.state),
            };
        });
        Ok(id)
    }

    /// In progress with no groups while the search runs; once it ended, what it found.
    pub(crate) fn poll(&self, search_id: &str) -> Result<SearchResult, GdError> {
        let run = {
            let mut runs = self.runs.lock().unwrap();
            Self::prune(&mut runs);
            runs.get(search_id).cloned()
        };
        let run = run.ok_or_else(|| GdError::UnknownSearch {
            search_id: search_id.to_string(),
        })?;
        let progress = run.progress.lock().unwrap();
        let (state, groups, stats) = match &progress.phase {
            Phase::Running => (SearchState::InProgress, vec![], SearchStats::default()),
            Phase::Ended(_, state) => (
                state.clone(),
                progress.groups.clone(),
                progress.stats.clone(),
            ),
            Phase::Failed(_, e) => return Err(e.clone()),
        };
        Ok(SearchResult {
            search_id: search_id.to_string(),
            groups,
            has_more: state == SearchState::InProgress,
            state,
            stats,
        })
    }

    /// Ends a search at its next wait; its poll then reads what it resolved. One the backend
    /// does not run is no error.
    pub(crate) fn stop(&self, search_id: &str) {
        if let Some(run) = self.runs.lock().unwrap().get(search_id) {
            run.stop.send_replace(true);
        }
    }

    pub(crate) fn stop_all(&self) {
        for run in self.runs.lock().unwrap().values() {
            run.stop.send_replace(true);
        }
    }
}

/// Why a search ended before it checked every track.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Cut {
    Deadline,
    Stopped,
}

/// What a search's task ended with.
struct Ended {
    groups: Vec<DownloadableGroup>,
    stats: SearchStats,
    state: SearchState,
    answered: bool,
    failed: Option<GdError>,
}

/// How a row's file was read.
enum Step {
    Read(Box<Read>),
    Skipped,
    Cut(Cut),
    Failed(GdError),
}

struct Task {
    api: Arc<Api>,
    wanted: Wanted,
    text: Option<String>,
    keep_below_floor: bool,
    deadline: tokio::time::Instant,
    stopped: watch::Receiver<bool>,
    /// Calls, window and deadline, for the note of a search cut short.
    budget: (usize, Duration, Duration),
    stats: SearchStats,
    groups: Vec<DownloadableGroup>,
    /// Whether any call was answered.
    answered: bool,
    failed: Option<GdError>,
    cut: Option<Cut>,
}

impl Task {
    async fn run(mut self) -> Ended {
        if self.wanted.is_album {
            self.album().await;
        } else {
            self.track().await;
        }
        Ended {
            groups: self.groups,
            stats: self.stats,
            state: match self.cut {
                Some(Cut::Deadline) => SearchState::TimedOut,
                _ => SearchState::Completed,
            },
            answered: self.answered,
            failed: self.failed,
        }
    }

    /// `fut`, unless the search is stopped or its time runs out first.
    async fn bounded<T>(&mut self, fut: impl Future<Output = T>) -> Result<T, Cut> {
        let mut stopped = self.stopped.clone();
        let deadline = self.deadline;
        tokio::select! {
            biased;
            _ = stopped.wait_for(|stop| *stop) => Err(Cut::Stopped),
            _ = tokio::time::sleep_until(deadline) => Err(Cut::Deadline),
            out = fut => Ok(out),
        }
    }

    /// The rows `listing` gives for `text`; None when the search ends here.
    async fn list(&mut self, listing: Listing, text: &str, count: u32) -> Option<Vec<Row>> {
        let api = self.api.clone();
        match self.bounded(api.search(listing, text, count)).await {
            Err(cut) => {
                self.cut = Some(cut);
                None
            }
            Ok(Err(e)) => {
                warn!("GD Studio search failed: {e}");
                self.failed = Some(e);
                None
            }
            Ok(Ok(rows)) => {
                self.answered = true;
                Some(rows)
            }
        }
    }

    async fn album(&mut self) {
        let wanted = self.wanted.clone();
        let keep = self.keep_below_floor;
        let text = self.text.clone().unwrap_or_else(|| wanted.text());
        self.stats.query_text = text.clone();
        let Some(rows) = self.list(Listing::Album, &text, ALBUM_ROWS).await else {
            return;
        };
        self.stats.response_count = rows.len() as u32;
        let mut best = best_album(&rows, &wanted, keep);
        let half = |taken: &[Option<(usize, Fit)>]| taken.iter().flatten().count() * 2;
        let short = |best: &Option<AlbumFit>| {
            best.as_ref()
                .is_none_or(|(_, _, taken)| half(taken) < wanted.tracks.len())
        };
        if short(&best) {
            // The album listing names the album NetEase finds first for the text; the tracks
            // across albums can name a closer one.
            let Some(across) = self.list(Listing::Tracks, &text, ALBUM_ROWS).await else {
                return self.end_album(best).await;
            };
            let tried = best.as_ref().map(|(name, _, _)| name.to_lowercase());
            let retry = closest_albums(&across, wanted.album.as_deref().unwrap_or_default())
                .into_iter()
                .find(|name| Some(name.to_lowercase()) != tried)
                .map(|name| format!("{} {name}", wanted.artist).trim().to_string())
                // The same text lists the same album again.
                .filter(|retry| retry.to_lowercase() != text.to_lowercase());
            if let Some(retry) = retry {
                info!("GD Studio found less than half of {text}; asking for {retry}");
                self.stats.retried_texts.push(retry.clone());
                if let Some(rows) = self.list(Listing::Album, &retry, ALBUM_ROWS).await {
                    self.stats.response_count = rows.len() as u32;
                    let found = best_album(&rows, &wanted, keep);
                    let count =
                        |b: &Option<AlbumFit>| b.as_ref().map_or(0, |(_, _, taken)| half(taken));
                    if count(&found) > count(&best) {
                        best = found;
                    }
                }
            }
        }
        self.end_album(best).await;
    }

    /// Resolves the rows of `best` that are wanted tracks and offers them as one group.
    async fn end_album(&mut self, best: Option<AlbumFit>) {
        let Some((name, rows, taken)) = best else {
            return;
        };
        self.stats.file_count = rows.len() as u32;
        let picked: Vec<(usize, usize)> = (taken.iter().enumerate())
            .filter_map(|(t, took)| took.map(|(r, _)| (t, r)))
            .collect();
        if picked.len() * 2 < self.wanted.tracks.len() && !self.keep_below_floor {
            return;
        }
        let mut reads = Vec::new();
        for (done, (t, r)) in picked.iter().enumerate() {
            match self.read(&rows[*r]).await {
                Step::Read(read) => reads.push((*t, *read)),
                Step::Skipped => {}
                Step::Cut(cut) => {
                    self.note_unchecked(picked.len() - done, cut);
                    break;
                }
                Step::Failed(e) => {
                    self.failed = Some(e);
                    break;
                }
            }
        }
        let wanted = self.wanted.clone();
        let (group, below) = matching::group(
            &name,
            &reads,
            &wanted,
            rows.len() as u32,
            self.keep_below_floor,
        );
        self.stats.below_floor += below;
        self.groups.extend(group);
    }

    async fn track(&mut self) {
        let wanted = self.wanted.clone();
        let text = self.text.clone().unwrap_or_else(|| wanted.text());
        self.stats.query_text = text.clone();
        let Some(rows) = self.list(Listing::Tracks, &text, TRACK_ROWS).await else {
            return;
        };
        self.stats.response_count = rows.len() as u32;
        self.stats.file_count = rows.len() as u32;
        let track = &wanted.tracks[0];
        let mut fitting: Vec<(f64, &Row)> = rows
            .iter()
            .filter_map(|row| matching::fit(row, track, &wanted, None).map(|f| (f.score, row)))
            .collect();
        let under = fitting
            .iter()
            .filter(|(score, _)| *score < crate::matching::score::MIN_SCORE_THRESHOLD)
            .count();
        self.stats.below_floor += under as u32;
        if !self.keep_below_floor {
            fitting.retain(|(score, _)| *score >= crate::matching::score::MIN_SCORE_THRESHOLD);
        }
        // Stable: equal rows keep NetEase's order.
        fitting.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let picks: Vec<Row> = (fitting.into_iter().take(TRACK_PICKS))
            .map(|(_, row)| row.clone())
            .collect();
        for (done, row) in picks.iter().enumerate() {
            match self.read(row).await {
                Step::Read(read) => {
                    let listed = rows.iter().filter(|r| r.album == row.album).count() as u32;
                    let (group, _) = matching::group(
                        &row.album,
                        &[(0, *read)],
                        &wanted,
                        listed,
                        self.keep_below_floor,
                    );
                    self.groups.extend(group);
                }
                Step::Skipped => {}
                Step::Cut(cut) => {
                    self.note_unchecked(picks.len() - done, cut);
                    break;
                }
                Step::Failed(e) => {
                    self.failed = Some(e);
                    break;
                }
            }
        }
    }

    /// The row's file: where it is now and what its first bytes say. A file NetEase does not
    /// serve, or that is not what its URL names, is skipped.
    async fn read(&mut self, row: &Row) -> Step {
        let api = self.api.clone();
        let located = match api.fresh(&row.id) {
            Some(located) => located,
            None => match self.bounded(api.resolve(&row.id)).await {
                Err(cut) => return Step::Cut(cut),
                Ok(Err(e)) => {
                    warn!("GD Studio could not resolve NetEase track {}: {e}", row.id);
                    return Step::Failed(e);
                }
                Ok(Ok(Resolved::NotServed)) => {
                    self.answered = true;
                    self.stats.locked_file_count += 1;
                    return Step::Skipped;
                }
                Ok(Ok(Resolved::Refused(why))) => {
                    self.answered = true;
                    warn!("Not offering NetEase track {}: {why}", row.id);
                    return Step::Skipped;
                }
                Ok(Ok(Resolved::Located(located))) => {
                    self.answered = true;
                    located
                }
            },
        };
        match self.bounded(api.head(&located)).await {
            Err(cut) => Step::Cut(cut),
            Ok(Err(why)) => {
                warn!(
                    "Not offering NetEase track {} ({} on {}): {why}",
                    row.id, located.md5, located.host
                );
                Step::Skipped
            }
            Ok(Ok(header)) => Step::Read(Box::new(Read {
                row: row.clone(),
                located,
                header,
            })),
        }
    }

    fn note_unchecked(&mut self, tracks: usize, cut: Cut) {
        self.cut = Some(cut);
        let (calls, window, deadline) = self.budget;
        self.stats.unanswered_note = Some(match cut {
            Cut::Deadline => format!(
                "{tracks} tracks were not checked within the search's {} s: Soulbeet sends GD \
                 Studio at most {calls} calls per {} minutes for every user together",
                deadline.as_secs(),
                window.as_secs() / 60
            ),
            Cut::Stopped => format!("{tracks} tracks were not checked: the search was stopped"),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gdstudio::api::tests::{api_at, fixture};
    use crate::gdstudio::matching::tests::{album_spec, mb_track};
    use crate::gdstudio::tests::fast;
    use crate::test_http::Reply;
    use std::io::{BufRead, BufReader, Write};

    /// GD Studio and NetEase's servers as a test answers them: each API call gets the next of
    /// `answers` (`None`: a URL answer naming `files`' next file on this server), each file
    /// request its file's first bytes. Records the API calls.
    fn double(
        answers: Vec<Option<String>>,
        files: Vec<Vec<u8>>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let here = base.clone();
        std::thread::spawn(move || {
            let mut answers = answers.into_iter();
            let mut named = files.iter();
            let by_md5: HashMap<String, Vec<u8>> = files
                .iter()
                .map(|f| (format!("{:x}", md5::compute(f)), f.clone()))
                .collect();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let target = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    header.clear();
                }
                let (status, extra, body): (u16, String, Vec<u8>) =
                    if let Some(name) = target.strip_prefix("/cdn/") {
                        let md5 = name.split('.').next().unwrap_or_default();
                        let file = by_md5.get(md5).cloned().unwrap_or_default();
                        let head = file[..file.len().min(4096)].to_vec();
                        let range = format!(
                            "Content-Range: bytes 0-{}/{}\r\n",
                            head.len().saturating_sub(1),
                            file.len()
                        );
                        (206, range, head)
                    } else {
                        seen.lock().unwrap().push(target.clone());
                        match answers.next() {
                            Some(Some(body)) => (200, String::new(), body.into_bytes()),
                            Some(None) => {
                                let file = named.next().unwrap();
                                let md5 = format!("{:x}", md5::compute(file));
                                let ext = if file.starts_with(b"fLaC") {
                                    "flac"
                                } else {
                                    "mp3"
                                };
                                let body = serde_json::json!({
                                    "url": format!("{here}/cdn/{md5}.{ext}"),
                                    "br": 900, "size": file.len(), "from": "music.gdstudio.xyz"
                                });
                                (200, String::new(), body.to_string().into_bytes())
                            }
                            None => continue,
                        }
                    };
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        (base, calls)
    }

    /// A FLAC header for `seconds` of 44.1 kHz audio, then `pad` bytes, so each file differs.
    fn flac_file(seconds: u64, pad: u8) -> Vec<u8> {
        let samples = seconds * 44_100;
        let mut out = b"fLaC".to_vec();
        out.extend([0x80, 0, 0, 34]);
        out.extend(4096u16.to_be_bytes());
        out.extend(4096u16.to_be_bytes());
        out.extend([0; 6]);
        let packed: u64 = (44_100u64 << 44) | (1 << 41) | (15 << 36) | samples;
        out.extend(packed.to_be_bytes());
        out.extend([0; 16]);
        out.extend(vec![pad; 100]);
        out
    }

    async fn ended(searches: &Searches, id: &str) -> SearchResult {
        for _ in 0..500 {
            let result = searches.poll(id).unwrap();
            if result.state != SearchState::InProgress {
                return result;
            }
            assert!(result.groups.is_empty(), "no groups while it runs");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the search never ended");
    }

    #[tokio::test]
    async fn an_album_search_resolves_each_wanted_track_and_offers_one_group() {
        let titles = ["One More Time", "Aerodynamic", "Digital Love"];
        let files = vec![flac_file(320, 1), flac_file(212, 2), flac_file(301, 3)];
        let listing = || Some(fixture("s1_album_search.json"));
        let (base, calls) = double(vec![listing(), None, None, None, listing()], files);
        let api = Arc::new(api_at(&base, &fast()));
        let searches = Searches::default();
        let id = searches
            .start(
                &api,
                &fast(),
                &album_spec("Daft Punk", "Discovery", &titles),
            )
            .unwrap();
        let result = ended(&searches, &id).await;
        assert_eq!(result.state, SearchState::Completed);
        assert!(id.starts_with("gd-"));
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 4, "{calls:?}");
            assert!(calls[0].contains("source=netease_album&name=Daft+Punk+Discovery&count=50"));
            assert!(calls[1].ends_with("id=17455800&br=999"));
        }
        assert_eq!(result.groups.len(), 1);
        let group = &result.groups[0];
        assert_eq!((group.title.as_str(), group.items.len()), ("Discovery", 3));
        assert_eq!(
            group.items[1].id,
            "17455801/02 - Aerodynamic [17455801].flac"
        );
        assert_eq!(group.items[0].duration, Some(320));
        assert_eq!(result.stats.query_text, "Daft Punk Discovery");
        assert_eq!(
            (result.stats.response_count, result.stats.file_count),
            (14, 14)
        );
        // A second search within 20 minutes reuses the URLs the first was given.
        let again = searches
            .start(
                &api,
                &fast(),
                &album_spec("Daft Punk", "Discovery", &titles),
            )
            .unwrap();
        assert_eq!(ended(&searches, &again).await.groups[0].items.len(), 3);
        assert_eq!(
            calls.lock().unwrap().len(),
            5,
            "one listing, no URL asked again"
        );
    }

    #[tokio::test]
    async fn an_album_found_short_is_asked_again_by_its_closest_name() {
        // The first listing is another album; the tracks across albums name the wanted one.
        let other = serde_json::json!([
            {"id": "1", "name": "Intro", "artist": ["Kings of Leon"], "album": "Live in Paris"}
        ])
        .to_string();
        let across = serde_json::json!([
            {"id": "3457893145", "name": "Intro", "artist": ["Kings of Leon"], "album": "O My Beloved"},
            {"id": "9", "name": "Intro", "artist": ["Kings of Leon"], "album": "Live in Paris"}
        ])
        .to_string();
        let (base, calls) = double(
            vec![
                Some(other),
                Some(across),
                Some(fixture("s4_kol.json")),
                None,
                None,
                None,
            ],
            vec![flac_file(60, 1), flac_file(200, 2), flac_file(230, 3)],
        );
        let api = Arc::new(api_at(&base, &fast()));
        let searches = Searches::default();
        let spec = album_spec(
            "Kings of Leon",
            "O My Beloved (Deluxe)",
            &["Intro", "My Whole World", "Cold Blue Dawn"],
        );
        let id = searches.start(&api, &fast(), &spec).unwrap();
        let result = ended(&searches, &id).await;
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 6, "{calls:?}");
        assert!(calls[1].contains("source=netease&name=Kings+of+Leon+O+My+Beloved+%28Deluxe%29"));
        assert!(calls[2].contains("source=netease_album&name=Kings+of+Leon+O+My+Beloved&"));
        assert_eq!(result.stats.retried_texts, ["Kings of Leon O My Beloved"]);
        assert_eq!(result.groups[0].title, "O My Beloved");
        assert_eq!(result.groups[0].items.len(), 3);
    }

    #[tokio::test]
    async fn a_track_search_resolves_its_best_three_rows() {
        let files = vec![flac_file(233, 1), flac_file(233, 2), flac_file(233, 3)];
        let (base, calls) = double(
            vec![Some(fixture("s2_track_search.json")), None, None, None],
            files,
        );
        let api = Arc::new(api_at(&base, &fast()));
        let searches = Searches::default();
        let mut track = mb_track("Stromae", "Papaoutai", Some("3:52"));
        track.album_title = Some("Racine carrée".into());
        let id = searches
            .start(&api, &fast(), &SearchSpec::of(None, vec![track]))
            .unwrap();
        let result = ended(&searches, &id).await;
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls[0].contains("source=netease&name=Stromae+Papaoutai&count=20"));
        // The row on the wanted album first.
        assert!(calls[1].ends_with("id=27587484&br=999"), "{calls:?}");
        assert_eq!(result.groups.len(), 3);
        assert_eq!(result.groups[0].title, "racine carrée");
        assert!(result.groups.iter().all(|g| g.items.len() == 1));
        assert_eq!(
            result.groups[0].items[0].id,
            "27587484/Papaoutai [27587484].flac"
        );
    }

    #[tokio::test]
    async fn a_track_netease_does_not_serve_is_counted_and_left_out() {
        let (base, _) = double(
            vec![
                Some(fixture("s7_okc.json")),
                Some(fixture("u5_unknown.json")),
                None,
            ],
            vec![flac_file(380, 1)],
        );
        let api = Arc::new(api_at(&base, &fast()));
        let searches = Searches::default();
        let spec = album_spec("Radiohead", "OK Computer", &["Airbag", "Paranoid Android"]);
        let id = searches.start(&api, &fast(), &spec).unwrap();
        let result = ended(&searches, &id).await;
        assert_eq!(result.stats.locked_file_count, 1);
        assert_eq!(result.groups[0].items.len(), 1);
        assert_eq!(result.groups[0].items[0].title, "Paranoid Android");
    }

    #[tokio::test]
    async fn a_search_whose_budget_is_spent_times_out_with_what_it_resolved() {
        let limits = Limits {
            calls: 2,
            search_deadline: Duration::from_millis(400),
            ..fast()
        };
        let (base, calls) = double(
            vec![Some(fixture("s1_album_search.json")), None, None],
            vec![flac_file(320, 1), flac_file(212, 2)],
        );
        let api = Arc::new(api_at(&base, &limits));
        let searches = Searches::default();
        let spec = album_spec("Daft Punk", "Discovery", &["One More Time", "Aerodynamic"]);
        let id = searches.start(&api, &limits, &spec).unwrap();
        let result = ended(&searches, &id).await;
        assert_eq!(result.state, SearchState::TimedOut);
        assert_eq!(
            calls.lock().unwrap().len(),
            2,
            "the third call waits for the budget"
        );
        assert_eq!(result.groups[0].items.len(), 1);
        let note = result.stats.unanswered_note.unwrap();
        assert!(
            note.starts_with("1 tracks were not checked within the search's 0 s"),
            "{note}"
        );
        assert!(note.contains("at most 2 calls per 5 minutes"), "{note}");
    }

    #[tokio::test]
    async fn a_stopped_search_ends_with_what_it_resolved_and_an_unknown_one_is_lost() {
        let limits = Limits { calls: 1, ..fast() };
        let (base, _) = double(vec![Some(fixture("s1_album_search.json"))], vec![]);
        let api = Arc::new(api_at(&base, &limits));
        let searches = Searches::default();
        let spec = album_spec("Daft Punk", "Discovery", &["One More Time", "Aerodynamic"]);
        let id = searches.start(&api, &limits, &spec).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        searches.stop(&id);
        searches.stop("never-started");
        let result = ended(&searches, &id).await;
        assert_eq!(result.state, SearchState::Completed);
        assert!(result.groups.is_empty());
        assert_eq!(
            result.stats.unanswered_note.as_deref(),
            Some("2 tracks were not checked: the search was stopped")
        );
        assert!(matches!(
            searches.poll("never-started"),
            Err(GdError::UnknownSearch { .. })
        ));
    }

    #[tokio::test]
    async fn a_search_nothing_answers_fails_with_gd_studios_error() {
        let (base, _) = crate::test_http::serve(vec![Reply {
            status: 401,
            body: r#"{"detail":"Invalid request."}"#.into(),
        }]);
        let api = Arc::new(api_at(&base, &fast()));
        let searches = Searches::default();
        let id = searches
            .start(
                &api,
                &fast(),
                &SearchSpec::of(None, vec![mb_track("a", "b", None)]),
            )
            .unwrap();
        let mut polled = searches.poll(&id);
        for _ in 0..500 {
            if !matches!(&polled, Ok(r) if r.state == SearchState::InProgress) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            polled = searches.poll(&id);
        }
        assert!(
            matches!(
                polled,
                Err(GdError::Remote(crate::polite::RemoteError::Status {
                    status: 401,
                    ..
                }))
            ),
            "{polled:?}"
        );
    }
}
