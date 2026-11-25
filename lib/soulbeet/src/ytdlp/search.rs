//! Searches of YouTube Music: each runs as a task that asks the helper for albums and their
//! tracklists, or for songs, and is polled for what it found so far. One helper search runs at
//! a time in the process, a few seconds apart, whoever asks.

use super::error::{Fault, Kind, YtDlpError};
use super::matching::{self, Counts, Wanted, ALBUM_HITS, SONG_HITS};
use super::protocol::{Answer, Helper, Limits, Request};
use super::standing::Standing;
use shared::download::{DownloadableGroup, SearchResult, SearchSpec, SearchState, SearchStats};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::AbortHandle;
use tokio::time::Instant;

/// How long an ended search can still be polled.
const KEPT: Duration = Duration::from_secs(30 * 60);

/// What a search that found nothing because YouTube Music had nothing says.
const NOTHING_FOUND: &str = "YouTube Music returned no songs for this query";

#[derive(Debug, Clone)]
enum Phase {
    Running,
    Ended(Instant),
    /// Ended without any answer: why.
    Failed(Instant, Fault),
}

struct Progress {
    groups: Vec<DownloadableGroup>,
    stats: SearchStats,
    phase: Phase,
}

/// One search, as its task fills it.
struct Run {
    progress: Mutex<Progress>,
    task: Mutex<Option<AbortHandle>>,
}

impl Run {
    fn add(&self, groups: impl IntoIterator<Item = DownloadableGroup>) {
        let mut progress = self.progress.lock().unwrap();
        progress.groups.extend(groups);
        // Stable: equal groups keep the order they were found in.
        progress.groups.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    fn count(&self, counts: Counts, responses: u32) {
        let mut progress = self.progress.lock().unwrap();
        let stats = &mut progress.stats;
        stats.response_count += responses;
        stats.file_count += counts.weighed;
        stats.not_audio += counts.not_audio;
        stats.below_floor += counts.below_floor;
    }

    fn asked(&self, query: &str) {
        let mut progress = self.progress.lock().unwrap();
        if progress.stats.query_text.is_empty() {
            progress.stats.query_text = query.to_string();
        }
    }

    /// Ends the run, unless it ended already.
    fn end(&self, failed: Option<Fault>) {
        let mut progress = self.progress.lock().unwrap();
        if !matches!(progress.phase, Phase::Running) {
            return;
        }
        if progress.stats.response_count == 0 && failed.is_none() {
            progress.stats.unanswered_note = Some(NOTHING_FOUND.to_string());
        }
        progress.phase = match failed {
            Some(fault) => Phase::Failed(Instant::now(), fault),
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

/// What a search task asks YouTube Music through.
#[derive(Clone)]
pub(crate) struct Asker {
    pub helper: Helper,
    pub standing: Arc<Standing>,
    /// Held while one helper search runs: when the last one started.
    pub turn: Arc<tokio::sync::Mutex<Option<Instant>>>,
    /// The space between the starts of two helper searches.
    pub gap: Duration,
}

impl Asker {
    /// Asks `request` in the process's turn, unless YouTube paused this server.
    async fn ask(&self, request: Request) -> Result<Answer, Fault> {
        let mut last = self.turn.lock().await;
        if let Some(at) = *last {
            tokio::time::sleep_until(at + self.gap).await;
        }
        if let Some(pause) = self.standing.paused() {
            return Err(pause.fault());
        }
        *last = Some(Instant::now());
        let answer = self
            .helper
            .call(&request, Limits::SEARCH, &mut |_| {})
            .await;
        self.standing.heard_search(answer.as_ref().map(|_| ()));
        answer
    }
}

/// The searches of the process, by id.
#[derive(Default)]
pub(crate) struct Searches {
    runs: Mutex<HashMap<String, Arc<Run>>>,
}

impl Searches {
    /// Forgets the searches that ended more than `KEPT` ago.
    fn prune(runs: &mut HashMap<String, Arc<Run>>) {
        runs.retain(|_, run| run.ended_at().is_none_or(|at| at.elapsed() < KEPT));
    }

    /// Starts a search for `spec` through `asker`: its id. `queued` is how many transfers wait
    /// in the backend's queue now, which each file it offers names.
    pub(crate) fn start(
        &self,
        asker: &Asker,
        spec: &SearchSpec,
        queued: u64,
    ) -> Result<String, YtDlpError> {
        let wanted = Wanted::of(spec)?;
        if let Some(pause) = asker.standing.paused() {
            return Err(YtDlpError::Paused(pause));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let run = Arc::new(Run {
            progress: Mutex::new(Progress {
                groups: vec![],
                stats: SearchStats::default(),
                phase: Phase::Running,
            }),
            task: Mutex::new(None),
        });
        {
            let mut runs = self.runs.lock().unwrap();
            Self::prune(&mut runs);
            runs.insert(id.clone(), run.clone());
        }
        let (asker, text, keep) = (asker.clone(), spec.text.clone(), spec.keep_below_floor);
        let running = run.clone();
        let task = tokio::spawn(async move {
            let failed = search(&asker, &wanted, text.as_deref(), keep, queued, &running).await;
            running.end(failed);
        });
        *run.task.lock().unwrap() = Some(task.abort_handle());
        Ok(id)
    }

    pub(crate) fn poll(&self, search_id: &str) -> Result<SearchResult, YtDlpError> {
        let run = {
            let mut runs = self.runs.lock().unwrap();
            Self::prune(&mut runs);
            runs.get(search_id).cloned()
        };
        let run = run.ok_or_else(|| YtDlpError::UnknownSearch {
            search_id: search_id.to_string(),
        })?;
        let progress = run.progress.lock().unwrap();
        let (state, stats) = match &progress.phase {
            Phase::Running => (SearchState::InProgress, SearchStats::default()),
            Phase::Ended(_) => (SearchState::Completed, progress.stats.clone()),
            Phase::Failed(_, fault) => return Err(YtDlpError::Helper(fault.clone())),
        };
        Ok(SearchResult {
            search_id: search_id.to_string(),
            groups: progress.groups.clone(),
            has_more: state == SearchState::InProgress,
            state,
            stats,
        })
    }

    /// Stops a search: its helper is killed, and it ends with what it found so far. One the
    /// backend does not run is no error.
    pub(crate) fn stop(&self, search_id: &str) {
        if let Some(run) = self.runs.lock().unwrap().get(search_id) {
            Self::halt(run);
        }
    }

    pub(crate) fn stop_all(&self) {
        for run in self.runs.lock().unwrap().values() {
            Self::halt(run);
        }
    }

    fn halt(run: &Run) {
        if let Some(task) = run.task.lock().unwrap().take() {
            task.abort();
        }
        run.end(None);
    }
}

/// Runs a search into `run`: why it read nothing, when no call was answered.
async fn search(
    asker: &Asker,
    wanted: &Wanted,
    text: Option<&str>,
    keep_below_floor: bool,
    queued: u64,
    run: &Run,
) -> Option<Fault> {
    let mut answered = false;
    let mut last = None;
    let mut note = |answer: Result<Answer, Fault>, last: &mut Option<Fault>| match answer {
        Ok(answer) => {
            answered = true;
            Some(answer)
        }
        Err(fault) => {
            *last = Some(fault);
            None
        }
    };
    if let Some(query) = wanted.album_query(text) {
        run.asked(&query);
        let request = Request::SearchAlbums {
            query,
            limit: ALBUM_HITS,
        };
        let hits = match note(asker.ask(request).await, &mut last) {
            Some(Answer::Albums(hits)) => hits,
            Some(other) => return Some(unexpected(&other)),
            None => return last,
        };
        run.count(Counts::default(), hits.len() as u32);
        for hit in matching::shortlist(&hits, wanted) {
            let request = Request::Album {
                browse_id: hit.browse_id.clone(),
            };
            let page = match note(asker.ask(request).await, &mut last) {
                Some(Answer::Album(page)) => page,
                Some(other) => {
                    last = Some(unexpected(&other));
                    continue;
                }
                None => continue,
            };
            let (group, counts) =
                matching::album_group(hit, &page, wanted, keep_below_floor, queued);
            run.count(counts, 0);
            run.add(group);
        }
    } else {
        for index in 0..wanted.tracks.len() {
            let query = wanted.song_query(index, text);
            run.asked(&query);
            let request = Request::SearchSongs {
                query,
                limit: SONG_HITS,
            };
            let songs = match note(asker.ask(request).await, &mut last) {
                Some(Answer::Songs(songs)) => songs,
                Some(other) => {
                    last = Some(unexpected(&other));
                    continue;
                }
                None => continue,
            };
            let (groups, counts) =
                matching::song_groups(&songs, index, wanted, keep_below_floor, queued);
            run.count(counts, songs.len() as u32);
            run.add(groups);
        }
    }
    (!answered).then_some(last).flatten()
}

/// The helper answered a search with something no search answers.
fn unexpected(answer: &Answer) -> Fault {
    let shown: String = format!("{answer:?}").chars().take(120).collect();
    Fault::new(
        Kind::Unreadable,
        format!("The helper answered a search with {shown}"),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ytdlp::matching::tests::{spiderland, track};
    use crate::ytdlp::protocol::tests::helper_of;
    use std::path::Path;

    /// A helper that answers each search command from the fixtures and records what it was
    /// asked in `asked`.
    pub(crate) fn fixture_helper(dir: &Path) -> Helper {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ytdlp/fixtures");
        let asked = dir.join("asked");
        let script = format!(
            r#"
import json, sys
request = json.loads(sys.stdin.readline())
with open({asked:?}, "a") as f:
    f.write(json.dumps(request) + "\n")
fixtures = {fixtures:?}
answers = {{
    ("search_albums", "Slint Spiderland"): "slint-albums",
    ("album", "MPREb_ur8iGYISD11"): "slint-spiderland",
    ("search_songs", "Kevin MacLeod Monkeys Spinning Monkeys"): "monkeys-songs",
}}
key = (request["command"], request.get("query") or request.get("browse_id"))
if key in answers:
    sys.stdout.write(open(f"{{fixtures}}/{{answers[key]}}.ndjson").read())
elif request.get("query") == "rate limited":
    print(json.dumps({{"error": {{"kind": "rate_limited", "message": "This content isn't available, try again later"}}}}))
else:
    print(json.dumps({{"albums" if request["command"] == "search_albums" else "songs": []}}))
"#,
            asked = asked.display().to_string(),
            fixtures = fixtures.display().to_string(),
        );
        helper_of(&script, dir)
    }

    pub(crate) fn asked(dir: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(dir.join("asked"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn asker(helper: Helper, standing: Arc<Standing>) -> Asker {
        Asker {
            helper,
            standing,
            turn: Arc::default(),
            gap: Duration::ZERO,
        }
    }

    async fn finished(searches: &Searches, id: &str) -> Result<SearchResult, YtDlpError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match searches.poll(id) {
                Ok(r) if r.state == SearchState::InProgress => {}
                done => return done,
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the search never ended"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn album_spec() -> SearchSpec {
        let wanted = spiderland();
        SearchSpec::of(wanted.album, wanted.tracks)
    }

    #[tokio::test]
    async fn an_album_search_reads_the_album_and_its_tracklist_only() {
        let dir = tempfile::tempdir().unwrap();
        let searches = Searches::default();
        let asker = asker(fixture_helper(dir.path()), Arc::default());
        let id = searches.start(&asker, &album_spec(), 0).unwrap();
        let result = finished(&searches, &id).await.unwrap();
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].items.len(), 6);
        assert_eq!(result.stats.query_text, "Slint Spiderland");
        assert_eq!(result.stats.response_count, 2);
        assert_eq!(result.stats.file_count, 6);
        let commands: Vec<String> = asked(dir.path())
            .iter()
            .map(|r| r["command"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(commands, ["search_albums", "album"]);
    }

    #[tokio::test]
    async fn a_search_youtube_music_has_nothing_for_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let searches = Searches::default();
        let asker = asker(fixture_helper(dir.path()), Arc::default());
        let spec = SearchSpec::of(None, vec![track("Nothing", "Nobody", None, "3:00")]);
        let id = searches.start(&asker, &spec, 0).unwrap();
        let result = finished(&searches, &id).await.unwrap();
        assert!(result.groups.is_empty());
        assert_eq!(result.stats.unanswered_note.as_deref(), Some(NOTHING_FOUND));
    }

    #[tokio::test]
    async fn a_rate_limited_search_fails_and_pauses_every_later_one() {
        let dir = tempfile::tempdir().unwrap();
        let searches = Searches::default();
        let standing = Arc::new(Standing::default());
        let asker = asker(fixture_helper(dir.path()), standing.clone());
        let spec = SearchSpec {
            text: Some("rate limited".into()),
            ..SearchSpec::of(None, vec![track("x", "y", None, "3:00")])
        };
        let id = searches.start(&asker, &spec, 0).unwrap();
        let failure = finished(&searches, &id)
            .await
            .unwrap_err()
            .failure(shared::failure::Stage::SourceSearch, "m");
        assert_eq!(
            failure.code,
            shared::failure::FailureCode::SearchRateLimited
        );
        assert!(standing.paused().is_some());
        // The next one asks YouTube nothing.
        let refused = searches.start(&asker, &album_spec(), 0).unwrap_err();
        assert!(matches!(refused, YtDlpError::Paused(_)), "{refused}");
        assert_eq!(asked(dir.path()).len(), 1);
    }

    #[tokio::test]
    async fn a_stopped_search_kills_its_helper_and_keeps_what_it_found() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let script = format!(
            "import os, sys, time\nsys.stdin.readline()\nopen({:?}, 'w').write(str(os.getpid()))\ntime.sleep(60)\n",
            pid.display().to_string()
        );
        let searches = Searches::default();
        let asker = asker(helper_of(&script, dir.path()), Arc::default());
        let id = searches.start(&asker, &album_spec(), 0).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let pid: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pid)
                .ok()
                .and_then(|p| p.parse().ok())
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the helper never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        searches.stop(&id);
        let result = searches.poll(&id).unwrap();
        assert_eq!(result.state, SearchState::Completed);
        assert!(crate::ytdlp::protocol::tests::gone(pid).await);
    }

    #[tokio::test]
    async fn an_unknown_search_is_lost() {
        let searches = Searches::default();
        let e = searches.poll("nope").unwrap_err();
        assert_eq!(
            e.failure(shared::failure::Stage::SourceSearch, "m").code,
            shared::failure::FailureCode::SearchLost
        );
    }
}
