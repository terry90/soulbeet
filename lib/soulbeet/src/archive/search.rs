//! Searches of archive.org: each runs as a task that asks for items, reads the files of the best
//! ones and groups them by item, and is polled for what it found so far.

use super::client::Api;
use super::error::ArchiveError;
use super::matching_ia::{self, Found, Wanted};
use super::query;
use crate::polite::RemoteError;
use shared::download::{DownloadableGroup, SearchResult, SearchSpec, SearchState, SearchStats};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long an ended search can still be polled.
const KEPT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
enum Phase {
    Running,
    Ended(Instant),
    /// Ended without reading anything: why.
    Failed(Instant, RemoteError),
}

struct Progress {
    groups: Vec<DownloadableGroup>,
    stats: SearchStats,
    phase: Phase,
}

/// One search, as its task fills it.
struct Run {
    progress: Mutex<Progress>,
    stop: AtomicBool,
}

impl Run {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn add(&self, group: DownloadableGroup) {
        let mut progress = self.progress.lock().unwrap();
        progress.groups.push(group);
        // Stable: equal groups keep the order their items were read in.
        progress.groups.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    fn end(&self, stats: SearchStats, failed: Option<RemoteError>) {
        let mut progress = self.progress.lock().unwrap();
        progress.stats = stats;
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

    /// Starts a search for `spec` on `api`: its id.
    pub(crate) fn start(&self, api: &Api, spec: &SearchSpec) -> Result<String, ArchiveError> {
        let wanted = Wanted::of(spec)?;
        let id = uuid::Uuid::new_v4().to_string();
        let run = Arc::new(Run {
            progress: Mutex::new(Progress {
                groups: vec![],
                stats: SearchStats::default(),
                phase: Phase::Running,
            }),
            stop: AtomicBool::new(false),
        });
        {
            let mut runs = self.runs.lock().unwrap();
            Self::prune(&mut runs);
            runs.insert(id.clone(), run.clone());
        }
        let (api, text, keep) = (api.clone(), spec.text.clone(), spec.keep_below_floor);
        tokio::spawn(async move {
            let (stats, failed) = search(&api, &wanted, text.as_deref(), keep, &run).await;
            run.end(stats, failed);
        });
        Ok(id)
    }

    pub(crate) fn poll(&self, search_id: &str) -> Result<SearchResult, ArchiveError> {
        let run = {
            let mut runs = self.runs.lock().unwrap();
            Self::prune(&mut runs);
            runs.get(search_id).cloned()
        };
        let run = run.ok_or_else(|| ArchiveError::UnknownSearch {
            search_id: search_id.to_string(),
        })?;
        let progress = run.progress.lock().unwrap();
        let (state, stats) = match &progress.phase {
            Phase::Running => (SearchState::InProgress, SearchStats::default()),
            Phase::Ended(_) => (SearchState::Completed, progress.stats.clone()),
            Phase::Failed(_, e) => return Err(e.clone().into()),
        };
        Ok(SearchResult {
            search_id: search_id.to_string(),
            groups: progress.groups.clone(),
            has_more: state == SearchState::InProgress,
            state,
            stats,
        })
    }

    /// Stops a search before its next request; one the backend does not run is no error.
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

/// Runs a search into `run`: what it read, and why it read nothing when no request was
/// answered.
async fn search(
    api: &Api,
    wanted: &Wanted,
    text: Option<&str>,
    keep_below_floor: bool,
    run: &Run,
) -> (SearchStats, Option<RemoteError>) {
    let mut stats = SearchStats::default();
    let searcher = match api.searcher() {
        Ok(searcher) => searcher,
        Err(e) => return (stats, Some(e)),
    };
    let mut last_error = None;
    let mut answered = false;
    let mut found: Vec<Found> = Vec::new();
    let mut note = |result: Result<_, RemoteError>, last: &mut Option<RemoteError>| match result {
        Ok(docs) => {
            answered = true;
            Some(docs)
        }
        Err(e) => {
            *last = Some(e);
            None
        }
    };

    if let Some(clause) = wanted.release.as_deref().and_then(query::release) {
        if run.stopped() {
            return (stats, None);
        }
        let docs = note(
            searcher.items(&query::filtered(&clause)).await,
            &mut last_error,
        );
        found.extend(docs.into_iter().flatten().map(|doc| Found {
            doc,
            release_hit: true,
        }));
    }

    let target = wanted.target();
    let (clause, retry) = match text.and_then(query::text) {
        Some(clause) => (Some(clause), None),
        None if target.is_blank() => (None, None),
        None => (Some(target.phrase()), target.unquoted()),
    };
    if let Some(clause) = clause {
        stats.query_text = clause.clone();
        if run.stopped() {
            return (stats, None);
        }
        let mut docs = note(
            searcher.items(&query::filtered(&clause)).await,
            &mut last_error,
        );
        if let (Some(retry), Some([])) = (retry, docs.as_deref()) {
            if run.stopped() {
                return (stats, None);
            }
            info!("archive.org found nothing for {clause}; asking for {retry}");
            docs = note(
                searcher.items(&query::filtered(&retry)).await,
                &mut last_error,
            );
            stats.retried_texts.push(retry);
        }
        // An item the release query missed may name the release all the same.
        let release = wanted.release.as_deref();
        found.extend(docs.into_iter().flatten().map(|doc| Found {
            release_hit: release.is_some_and(|mbid| doc.names_release(mbid)),
            doc,
        }));
    }

    let shortlist = matching_ia::shortlist(found, wanted);
    stats.response_count = shortlist.len() as u32;
    for item in &shortlist {
        if run.stopped() {
            break;
        }
        let files = match searcher.files(&item.doc.identifier).await {
            Ok(files) => {
                answered = true;
                files
            }
            Err(e) => {
                warn!("archive.org files of {}: {e}", item.doc.identifier);
                last_error = Some(e);
                continue;
            }
        };
        let sorted = matching_ia::originals(&files);
        stats.file_count += sorted.audio;
        stats.not_audio += sorted.not_audio;
        let (group, below_floor) = matching_ia::group(item, &sorted, wanted, keep_below_floor);
        stats.below_floor += below_floor;
        if let Some(group) = group {
            run.add(group);
        }
    }
    let failed = (!answered).then_some(last_error).flatten();
    (stats, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::client::tests::ok;
    use crate::test_http::{serve, Reply};
    use shared::metadata::{Album, Track};

    fn track(title: &str, duration: &str) -> Track {
        Track {
            id: title.into(),
            title: title.into(),
            artist: "Duster".into(),
            album_id: None,
            album_title: None,
            release_date: None,
            duration: Some(duration.into()),
            mbid: None,
            release_mbid: None,
        }
    }

    fn album(title: &str, mbid: Option<&str>) -> Album {
        Album {
            id: "a".into(),
            title: title.into(),
            artist: "Duster".into(),
            release_date: None,
            mbid: mbid.map(str::to_string),
            cover_url: None,
            release_types: None,
            first_release_date: None,
        }
    }

    async fn ended(searches: &Searches, id: &str) -> SearchResult {
        for _ in 0..200 {
            let result = searches.poll(id).unwrap();
            if result.state != SearchState::InProgress {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the search never ended");
    }

    fn files_of(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/src/archive/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn query_of(seen: &crate::test_http::Seen) -> String {
        let query = seen
            .target
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or_default();
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn an_album_search_asks_by_release_then_by_name_and_reads_the_best_items_files() {
        let search = files_of("s1_duster_stratosphere.json");
        let (base, seen) = serve(vec![
            ok(r#"{"response":{"numFound":0,"start":0,"docs":[]}}"#),
            ok(&search),
            // Best named first: "Stratosphere" twice, "Duster - Stratosphere" twice, then
            // "Duster - Stratosphere (1998)".
            ok(&files_of("files_1998-duster-stratosphere.json")),
            ok(&files_of("missing_files.json")),
            ok(&files_of("files_duster-stratosphere_202403.json")),
            ok(&files_of("missing_files.json")),
            ok(&files_of("files_a0165062421_10.json")),
        ]);
        let api = Api::unpaced(&base);
        let searches = Searches::default();
        let tracks = vec![
            track("Moon Age", "1:06"),
            track("Heading for the Door", "3:08"),
            track("Gold Dust", "2:06"),
        ];
        let spec = SearchSpec::of(
            Some(album(
                "Stratosphere",
                Some("4f3d8a27-2d1e-4bd4-a6e7-4b9f1e7f4c10"),
            )),
            tracks,
        );
        let id = searches.start(&api, &spec).unwrap();
        let result = ended(&searches, &id).await;

        let seen = seen.lock().unwrap();
        assert_eq!(
            query_of(&seen[0]),
            "external-identifier:\"urn:mb_release_id:4f3d8a27-2d1e-4bd4-a6e7-4b9f1e7f4c10\" AND \
             mediatype:audio AND -collection:stream_only AND -access-restricted-item:true AND \
             -collection:etree"
        );
        assert!(query_of(&seen[1]).starts_with("creator:(\"Duster\") AND title:(\"Stratosphere\")"));
        assert_eq!(seen[2].target, "/metadata/1998-duster-stratosphere/files");
        assert_eq!(
            result.stats.query_text,
            "creator:(\"Duster\") AND title:(\"Stratosphere\")"
        );
        assert_eq!(result.stats.response_count, 5);
        assert!(result.stats.retried_texts.is_empty());
        assert!(!result.groups.is_empty());
        let sources: Vec<&str> = result.groups.iter().map(|g| g.source.as_str()).collect();
        assert_eq!(
            sources,
            [
                "1998-duster-stratosphere",
                "duster-stratosphere_202403",
                "a0165062421_10"
            ]
        );
        let best = &result.groups[0];
        assert_eq!(best.items.len(), 3, "{best:?}");
        assert_eq!(best.quality, "flac");
        assert!(best.score > 0.7, "{}", best.score);
        for item in &best.items {
            assert_eq!(item.backend, "archive");
            assert_eq!(item.source, best.source);
            assert!(item.id.starts_with(&format!("{}/", best.source)));
        }
    }

    #[tokio::test]
    async fn a_phrase_that_finds_nothing_is_asked_again_word_by_word() {
        let (base, seen) = serve(vec![
            ok(r#"{"response":{"numFound":0,"start":0,"docs":[]}}"#),
            ok(r#"{"response":{"numFound":0,"start":0,"docs":[]}}"#),
        ]);
        let api = Api::unpaced(&base);
        let searches = Searches::default();
        let spec = SearchSpec::of(
            Some(album("Back in Black", None)),
            vec![
                track("Hells Bells", "5:10"),
                track("Shoot to Thrill", "5:17"),
            ],
        );
        let id = searches.start(&api, &spec).unwrap();
        let result = ended(&searches, &id).await;
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(
            result.stats.retried_texts,
            ["creator:(\"Duster\") AND title:(Back AND in AND Black)"]
        );
        assert!(result.groups.is_empty());
    }

    #[tokio::test]
    async fn a_search_nothing_answers_fails_with_archive_orgs_error() {
        let down = || Reply {
            status: 503,
            body: "down".into(),
        };
        let (base, _) = serve((0..8).map(|_| down()).collect());
        let api = Api::unpaced(&base);
        let searches = Searches::default();
        let spec = SearchSpec::of(None, vec![track("Moon Age", "1:06")]);
        let id = searches.start(&api, &spec).unwrap();
        let mut polled = searches.poll(&id);
        for _ in 0..3000 {
            if !matches!(&polled, Ok(r) if r.state == SearchState::InProgress) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            polled = searches.poll(&id);
        }
        let Err(e) = polled else {
            panic!("a search nothing answered ended {polled:?}");
        };
        assert!(
            matches!(e, ArchiveError::Remote(RemoteError::Unavailable { .. })),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn a_stopped_search_sends_nothing_more_and_an_unknown_one_is_lost() {
        let (base, seen) = serve(vec![]);
        let api = Api::unpaced(&base);
        let searches = Searches::default();
        let spec = SearchSpec::of(None, vec![track("Moon Age", "1:06")]);
        let id = searches.start(&api, &spec).unwrap();
        searches.stop(&id);
        searches.stop("never-started");
        let result = ended(&searches, &id).await;
        assert_eq!(result.state, SearchState::Completed);
        assert!(seen.lock().unwrap().is_empty());
        assert!(matches!(
            searches.poll("never-started"),
            Err(ArchiveError::UnknownSearch { .. })
        ));
        assert!(matches!(
            searches.start(&api, &SearchSpec::of(None, vec![])),
            Err(ArchiveError::NoSearchTarget { tracks: 0 })
        ));
    }
}
