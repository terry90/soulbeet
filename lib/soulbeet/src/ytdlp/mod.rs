//! The YouTube Music download backend: YouTube Music's own catalogue searched through
//! ytmusicapi, and Opus audio downloaded through yt-dlp, both run by a Python helper as a child
//! process of its own for each call.
//!
//! Its files are lossy only, about 130 kbps, so the app searches it last and never lets its
//! files replace a library copy. It runs anonymously: no account, no cookies. The engine is one
//! per process and outlives the handles the registry builds around it, so a handle rebuilt
//! while transfers run lists them still.

mod error;
mod matching;
mod protocol;
mod queue;
mod search;
mod standing;

pub use error::{Fault, Kind, YtDlpError};
pub use matching::item_name;
pub use protocol::Tools;
pub use queue::Pace;

use crate::backend_error::BackendResult;
use crate::partial::PartialDir;
use protocol::{Answer, Helper, Limits, Request, Versions, HELPER};
use queue::Queue;
use search::{Asker, Searches};
use shared::download::{
    DownloadProgress, DownloadableItem, QueuedDownload, SearchResult, SearchSpec, SourcePresence,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use shared::ytdlp::BACKEND_ID;
use standing::Standing;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tracing::{info, warn};

/// How long the helper's versions are taken as known.
const VERSIONS_KEPT: Duration = Duration::from_secs(10 * 60);
/// A probe runs only when YouTube was not heard from for this long.
const PROBE_EVERY: Duration = Duration::from_secs(30 * 60);
/// yt-dlp older than this is logged as likely to fail: YouTube changes often.
const YT_DLP_STALE_DAYS: i64 = 60;
/// The Creative Commons track a probe fetches the first bytes of: Kevin MacLeod, "Monkeys
/// Spinning Monkeys", CC BY 4.0.
const PROBE_VIDEO: &str = "MdzF0rFOC9Y";

/// The folders of the backend's partial folder that are not a transfer's.
const HOME: &str = "home";
const CACHE: &str = "cache";
const PROBE: &str = "probe";

/// What the engine needs of the server it runs in.
#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub download_path: PathBuf,
    /// This install's id: its partial files are its own.
    pub instance: String,
    pub tools: Tools,
}

/// What the backend runs in this process: its queue, its searches, and how YouTube treats this
/// server.
pub struct Engine {
    helper: Helper,
    dir: PartialDir,
    standing: Arc<Standing>,
    searches: Searches,
    asker: Asker,
    queue: Arc<Queue>,
    /// The helper's versions, and when they were read.
    versions: Mutex<Option<(Instant, Result<Versions, Fault>)>>,
    /// Held while the versions are read, so the checks at once share one read.
    reading: tokio::sync::Mutex<()>,
    /// Held while a health check runs, so the checks at once share one probe.
    probe: tokio::sync::Mutex<()>,
}

impl Engine {
    /// The engine of `settings`: its partial folder opened first, then every staging folder a
    /// previous run left deleted. When the folder cannot be opened (another server holds it, or
    /// it cannot be written) nothing is touched.
    pub async fn start(settings: EngineSettings, pace: Pace) -> Result<Arc<Engine>, YtDlpError> {
        Self::start_with(settings, pace, HELPER.into()).await
    }

    async fn start_with(
        settings: EngineSettings,
        pace: Pace,
        script: Arc<str>,
    ) -> Result<Arc<Engine>, YtDlpError> {
        let storage = |error| YtDlpError::Storage {
            download_path: settings.download_path.clone(),
            error,
        };
        // The helper runs in a folder of its own, so every path it is given must be absolute.
        let download_path = std::path::absolute(&settings.download_path).map_err(storage)?;
        let tools = settings.tools.absolute().map_err(storage)?;
        let dir =
            PartialDir::open(&download_path, &settings.instance, BACKEND_ID).map_err(storage)?;
        let clearing = dir.clone();
        let cleared = tokio::task::spawn_blocking(move || {
            clearing.clear_except(&HashSet::from([CACHE.to_string()]))?;
            std::fs::create_dir_all(clearing.part(HOME))?;
            std::fs::create_dir_all(clearing.part(CACHE))
        })
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r);
        cleared.map_err(storage)?;

        let helper = Helper::new(tools, script, dir.part(HOME), dir.part(CACHE));
        let standing = Arc::new(Standing::default());
        let queue = Queue::start(
            dir.clone(),
            download_path,
            helper.clone(),
            standing.clone(),
            pace,
        );
        let asker = Asker {
            helper: helper.clone(),
            standing: standing.clone(),
            turn: Arc::default(),
            gap: pace.search_gap,
        };
        let engine = Arc::new(Engine {
            helper,
            dir,
            standing,
            searches: Searches::default(),
            asker,
            queue,
            versions: Mutex::new(None),
            reading: tokio::sync::Mutex::new(()),
            probe: tokio::sync::Mutex::new(()),
        });
        let checking = engine.clone();
        tokio::spawn(async move { checking.log_versions().await });
        Ok(engine)
    }

    /// Stops every search and unfinished transfer, killing their helpers, and deletes their
    /// staging folders. Every handle built before is refused `download` and `start_search` from
    /// now on.
    pub async fn stop(&self) {
        self.searches.stop_all();
        self.queue.stop().await;
    }

    /// The helper's versions, read again when the last read is 10 minutes old.
    async fn versions(&self) -> Result<Versions, Fault> {
        let _reading = self.reading.lock().await;
        if let Some((at, versions)) = &*self.versions.lock().unwrap() {
            if at.elapsed() < VERSIONS_KEPT {
                return versions.clone();
            }
        }
        let read = match self
            .helper
            .call(&Request::Version, Limits::VERSION, &mut |_| {})
            .await
        {
            Ok(Answer::Version(versions)) => Ok(versions),
            Ok(other) => Err(Fault::new(
                Kind::Unreadable,
                format!("The helper answered its version with {other:?}"),
            )),
            Err(fault) => Err(fault),
        };
        *self.versions.lock().unwrap() = Some((Instant::now(), read.clone()));
        read
    }

    /// Logs what the helper has once the engine starts, and warns of a missing piece or a
    /// yt-dlp old enough that YouTube has likely changed since.
    async fn log_versions(&self) {
        let versions = match self.versions().await {
            Ok(versions) => versions,
            Err(fault) => {
                warn!("The YouTube Music helper cannot run: {}", fault.message);
                return;
            }
        };
        let missing = versions.missing();
        if !missing.is_empty() {
            warn!(
                "YouTube Music cannot download in this image: {} missing",
                missing.join(", ")
            );
            return;
        }
        info!(
            "YouTube Music: yt-dlp {}, ytmusicapi {}, deno {}, ffmpeg {}",
            versions.yt_dlp.as_deref().unwrap_or_default(),
            versions.ytmusicapi.as_deref().unwrap_or_default(),
            versions.deno.as_deref().unwrap_or_default(),
            versions.ffmpeg.as_deref().unwrap_or_default()
        );
        if let Some(age) = versions.yt_dlp.as_deref().and_then(release_age_days) {
            if age > YT_DLP_STALE_DAYS {
                warn!(
                    "yt-dlp {} is {age} days old; YouTube changes often, update the image",
                    versions.yt_dlp.as_deref().unwrap_or_default()
                );
            }
        }
    }

    /// Why the backend cannot work now, or Ok: the image's tools, then a pause, then what
    /// YouTube last answered, else a probe of its own.
    async fn health(&self) -> Result<(), Failure> {
        let _probe = self.probe.lock().await;
        let versions = self.versions().await.map_err(|fault| {
            let message = health_words(fault.kind);
            YtDlpError::Helper(fault).failure(Stage::SourceSearch, message)
        })?;
        let missing = versions.missing();
        if !missing.is_empty() {
            return Err(not_in_image(&missing));
        }
        let pause = self.standing.paused();
        if let Some(pause) = pause.filter(|p| p.why == error::Hold::RateLimited) {
            return Err(YtDlpError::Paused(pause).failure(Stage::SourceSearch, pause.to_string()));
        }
        let heard = match self.standing.heard_within(PROBE_EVERY) {
            Some(heard) => heard,
            None => {
                let probed = self.run_probe().await;
                if probed.is_ok() {
                    self.standing.passed_probe();
                }
                probed
            }
        };
        if let Some(pause) = self.standing.paused() {
            return Err(YtDlpError::Paused(pause).failure(Stage::SourceSearch, pause.to_string()));
        }
        heard.map_err(|fault| {
            let message = health_words(fault.kind);
            YtDlpError::Helper(fault).failure(Stage::SourceSearch, message)
        })
    }

    /// Fetches the first bytes of a Creative Commons track: whether YouTube serves media now.
    async fn run_probe(&self) -> Result<(), Fault> {
        let staging = self.dir.part(PROBE);
        let made = std::fs::create_dir_all(&staging);
        let probed = match made {
            Err(e) => Err(Fault::new(
                Kind::Internal,
                format!("Cannot make the probe folder: {e}"),
            )),
            Ok(()) => {
                let request = Request::Probe {
                    video_id: PROBE_VIDEO.to_string(),
                    staging_dir: staging.clone(),
                };
                match self.helper.call(&request, Limits::PROBE, &mut |_| {}).await {
                    Ok(Answer::Probe(_)) => Ok(()),
                    Ok(other) => Err(Fault::new(
                        Kind::Unreadable,
                        format!("The helper answered a probe with {other:?}"),
                    )),
                    Err(fault) => Err(fault),
                }
            }
        };
        if let Err(e) = std::fs::remove_dir_all(&staging) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Could not delete the YouTube Music probe folder: {e}");
            }
        }
        if let Err(fault) = &probed {
            warn!("YouTube Music health probe failed: {}", fault.message);
        }
        self.standing.heard(probed.as_ref().map(|_| ()));
        probed
    }
}

/// What a health failure of `kind` reads.
fn health_words(kind: Kind) -> &'static str {
    match kind {
        Kind::BotCheck | Kind::Forbidden => "YouTube asks this server to sign in",
        Kind::Network | Kind::TimedOut => "YouTube did not answer",
        Kind::Tooling => NOT_IN_IMAGE,
        _ => "yt-dlp could not read YouTube's answer; update the image",
    }
}

const NOT_IN_IMAGE: &str = "Not in this image: use the :full image";

/// The tools of the image are not all there.
fn not_in_image(missing: &[&str]) -> Failure {
    Failure::new(
        FailureCode::BackendNotConfigured,
        Stage::SourceSearch,
        RetryAdvice::NeedsUser,
        format!("{NOT_IN_IMAGE} ({} missing)", missing.join(", ")),
    )
    .with_cause(
        Service::Youtube,
        None,
        format!("missing: {}", missing.join(", ")),
    )
    .with_user_action(error::use_the_full_image())
}

/// The days since the release a yt-dlp version names (`2026.11.23`, maybe with a `.1`).
fn release_age_days(version: &str) -> Option<i64> {
    let mut parts = version.split('.');
    let date = chrono::NaiveDate::from_ymd_opt(
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    )?;
    Some((chrono::Utc::now().date_naive() - date).num_days())
}

/// The one engine of a process, made on first use.
pub struct EngineCell(tokio::sync::Mutex<Option<Arc<Engine>>>);

/// The engine the registry's handles share.
pub static ENGINE: EngineCell = EngineCell::new();

impl Default for EngineCell {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineCell {
    pub const fn new() -> Self {
        EngineCell(tokio::sync::Mutex::const_new(None))
    }

    /// The engine, started from `settings` when there is none yet.
    pub async fn get_or_start(&self, settings: EngineSettings) -> Result<Arc<Engine>, YtDlpError> {
        let mut engine = self.0.lock().await;
        if let Some(engine) = &*engine {
            return Ok(engine.clone());
        }
        let started = Engine::start(settings, Pace::YOUTUBE).await?;
        *engine = Some(started.clone());
        Ok(started)
    }

    /// Stops the engine (`Engine::stop`): whether one was started in this process.
    pub async fn stop(&self) -> bool {
        match &*self.0.lock().await {
            Some(engine) => {
                engine.stop().await;
                true
            }
            None => false,
        }
    }

    /// With no engine started in this process, deletes the staging folders a previous run
    /// left. An engine started meanwhile is stopped instead.
    pub async fn forget_unfinished(&self, settings: EngineSettings) -> Result<(), YtDlpError> {
        let engine = self.0.lock().await;
        if let Some(engine) = &*engine {
            engine.stop().await;
            return Ok(());
        }
        let storage = |error| YtDlpError::Storage {
            download_path: settings.download_path.clone(),
            error,
        };
        let dir = PartialDir::open(&settings.download_path, &settings.instance, BACKEND_ID)
            .map_err(storage)?;
        tokio::task::spawn_blocking(move || dir.clear_except(&HashSet::from([CACHE.to_string()])))
            .await
            .map_err(std::io::Error::other)
            .and_then(|r| r)
            .map_err(storage)
    }
}

/// The handle the registry keeps: the process's engine, and the generation it was built in.
pub struct YtDlpBackend {
    engine: Arc<Engine>,
    generation: u64,
}

impl YtDlpBackend {
    pub fn new(engine: Arc<Engine>) -> Self {
        let generation = engine.queue.generation();
        YtDlpBackend { engine, generation }
    }
}

#[async_trait::async_trait]
impl crate::DownloadBackend for YtDlpBackend {
    fn id(&self) -> &'static str {
        shared::backends::YTDLP.id
    }

    fn name(&self) -> &'static str {
        shared::backends::YTDLP.name
    }

    async fn start_search(&self, spec: &SearchSpec) -> BackendResult<String> {
        let engine = &self.engine;
        let queued = engine.queue.waiting();
        Ok(engine.queue.while_current(self.generation, || {
            engine.searches.start(&engine.asker, spec, queued)
        })?)
    }

    async fn poll_search(&self, search_id: &str) -> BackendResult<SearchResult> {
        Ok(self.engine.searches.poll(search_id)?)
    }

    fn new_request(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    async fn download(
        &self,
        items: Vec<DownloadableItem>,
        destination: &str,
        request: &str,
        owner: &str,
    ) -> BackendResult<Vec<QueuedDownload>> {
        Ok(self
            .engine
            .queue
            .download(self.generation, items, destination, request, owner)?)
    }

    async fn get_downloads_for(&self, sources: &[String]) -> BackendResult<Vec<DownloadProgress>> {
        Ok(self.engine.queue.listed(sources))
    }

    async fn get_download(
        &self,
        source: &str,
        id: &str,
    ) -> BackendResult<Option<DownloadProgress>> {
        Ok(self.engine.queue.one(source, id))
    }

    async fn cancel_download(
        &self,
        source: &str,
        download_id: &str,
        remove: bool,
    ) -> BackendResult<()> {
        self.engine.queue.cancel(source, download_id, remove).await;
        Ok(())
    }

    async fn queue_position(&self, source: &str, id: &str) -> BackendResult<Option<u32>> {
        Ok(self.engine.queue.queue_position(source, id))
    }

    /// A YouTube source has no presence of its own: whether the backend can fetch now. Away
    /// while a rate limit holds, which ends by itself; offline while YouTube asks for a sign-in
    /// or the image lacks a tool.
    async fn source_status(&self, _source: &str) -> BackendResult<SourcePresence> {
        if let Some(pause) = self.engine.standing.paused() {
            return Ok(match pause.why {
                error::Hold::RateLimited => SourcePresence::Away,
                error::Hold::BotCheck => SourcePresence::Offline,
            });
        }
        let known = self.engine.versions.lock().unwrap();
        let broken = known
            .as_ref()
            .is_some_and(|(_, v)| v.as_ref().map_or(true, |v| !v.missing().is_empty()));
        Ok(match broken {
            true => SourcePresence::Offline,
            false => SourcePresence::Online,
        })
    }

    async fn stop_search(&self, search_id: &str) -> BackendResult<()> {
        self.engine.searches.stop(search_id);
        Ok(())
    }

    /// The image's tools, a pause, and what YouTube answered in the last 30 minutes; only when
    /// it answered nothing in that time, a probe of its own. No request to YouTube otherwise.
    async fn health_check(&self) -> Result<(), Failure> {
        self.engine.health().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DownloadBackend;
    use queue::tests::{ended, item, Rig, QUICK};
    use shared::download::{DownloadState, EnqueueStatus, SearchState};

    fn probes(rig: &Rig) -> usize {
        rig.log().iter().filter(|(c, _)| c == "probe").count()
    }

    fn youtube_requests(rig: &Rig) -> usize {
        rig.log().iter().filter(|(c, _)| c != "version").count()
    }

    #[tokio::test]
    async fn the_backend_goes_by_its_meta_and_a_rebuilt_handle_shares_the_engine() {
        let rig = Rig::new();
        let engine = rig.engine(QUICK).await;
        let backend = YtDlpBackend::new(engine.clone());
        let meta = shared::backends::meta("ytdlp").unwrap();
        assert_eq!((backend.id(), backend.name()), (meta.id, meta.name));
        assert_eq!(meta.delivery, shared::backends::Delivery::LossyOnly);

        let sent = backend
            .download(vec![item(0, "MdzF0rFOC9Y")], "S [1]", "r", "u1")
            .await
            .unwrap();
        let rebuilt = YtDlpBackend::new(engine.clone());
        let listed = rebuilt
            .get_downloads_for(&[item(0, "MdzF0rFOC9Y").source])
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, sent[0].id);
    }

    #[tokio::test]
    async fn an_image_without_the_tools_says_so_and_asks_youtube_nothing() {
        let rig = Rig::new();
        rig.set_versions(serde_json::json!({
            "python": "3.11.2", "yt_dlp": null, "ejs": null, "ytmusicapi": null,
            "deno": null, "ffmpeg": null
        }));
        let engine = rig.engine(QUICK).await;
        let backend = YtDlpBackend::new(engine);
        let failure = backend.health_check().await.unwrap_err();
        assert_eq!(
            failure.code,
            shared::failure::FailureCode::BackendNotConfigured
        );
        assert_eq!(
            failure.message,
            "Not in this image: use the :full image (yt-dlp, yt-dlp-ejs, ytmusicapi, deno, ffmpeg missing)"
        );
        assert_eq!(
            failure.user_action.unwrap().code,
            shared::failure::UserActionCode::ConfigureBackend
        );
        assert_eq!(
            backend.source_status("ytm:x").await.unwrap(),
            SourcePresence::Offline
        );
        assert_eq!(youtube_requests(&rig), 0);
    }

    #[tokio::test]
    async fn a_health_check_probes_youtube_at_most_once_in_half_an_hour() {
        let rig = Rig::new();
        let engine = rig.engine(QUICK).await;
        let backend = YtDlpBackend::new(engine.clone());
        backend.health_check().await.unwrap();
        backend.health_check().await.unwrap();
        YtDlpBackend::new(engine).health_check().await.unwrap();
        assert_eq!(probes(&rig), 1);
    }

    #[tokio::test]
    async fn a_health_check_after_a_search_asks_youtube_nothing() {
        let rig = Rig::new();
        let engine = rig.engine(QUICK).await;
        let backend = YtDlpBackend::new(engine);
        let wanted = matching::tests::spiderland();
        let id = backend
            .start_search(&SearchSpec::of(wanted.album, wanted.tracks))
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while backend.poll_search(&id).await.unwrap().state == SearchState::InProgress {
            assert!(std::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let asked = youtube_requests(&rig);
        backend.health_check().await.unwrap();
        assert_eq!(youtube_requests(&rig), asked);
        assert_eq!(probes(&rig), 0);
    }

    #[tokio::test]
    async fn a_failed_probe_marks_youtube_unreachable() {
        let rig = Rig::new();
        std::fs::write(rig.path("probe-fails"), b"").unwrap();
        let engine = rig.engine(QUICK).await;
        let failure = YtDlpBackend::new(engine).health_check().await.unwrap_err();
        assert_eq!(
            failure.code,
            shared::failure::FailureCode::BackendUnreachable
        );
        assert_eq!(failure.message, "YouTube did not answer");
    }

    #[tokio::test]
    async fn a_rate_limit_shows_in_health_with_its_end_and_holds_the_source_away() {
        let rig = Rig::new();
        let engine = rig.engine(QUICK).await;
        let backend = YtDlpBackend::new(engine.clone());
        let limited = item(0, "RATELIMIT01");
        let sent = backend
            .download(vec![limited.clone()], "S [1]", "r", "u1")
            .await
            .unwrap();
        ended(&engine, &limited.source, &sent[0].id).await;
        let failure = backend.health_check().await.unwrap_err();
        assert_eq!(
            failure.code,
            shared::failure::FailureCode::SearchRateLimited
        );
        assert!(failure
            .message
            .starts_with("YouTube rate-limited this server until "));
        assert!(failure.retry_after_seconds.is_some());
        assert_eq!(
            backend.source_status(&limited.source).await.unwrap(),
            SourcePresence::Away
        );
        assert_eq!(probes(&rig), 0);
    }

    #[tokio::test]
    async fn a_bot_check_holds_the_source_offline() {
        let rig = Rig::new();
        let engine = rig.engine(QUICK).await;
        let backend = YtDlpBackend::new(engine.clone());
        let checked = item(0, "BOTCHECK001");
        let sent = backend
            .download(vec![checked.clone()], "S [1]", "r", "u1")
            .await
            .unwrap();
        let row = ended(&engine, &checked.source, &sent[0].id).await;
        assert_eq!(
            row.failure.unwrap().code,
            shared::failure::FailureCode::TransferRejectedOther
        );
        assert_eq!(
            backend.source_status(&checked.source).await.unwrap(),
            SourcePresence::Offline
        );
        let failure = backend.health_check().await.unwrap_err();
        assert!(failure
            .message
            .starts_with("YouTube asks this server to sign in"));
    }

    #[tokio::test]
    async fn turning_off_kills_the_fetch_and_leaves_no_staging_folder_or_unfinished_transfer() {
        let rig = Rig::new();
        let engine = rig.engine(QUICK).await;
        let before = YtDlpBackend::new(engine.clone());
        let done = item(1, "MdzF0rFOC9Y");
        let finished = before
            .download(vec![done.clone()], "S [1]", "r", "u1")
            .await
            .unwrap();
        assert_eq!(
            ended(&engine, &done.source, &finished[0].id).await.state,
            DownloadState::Completed
        );
        let slow = item(0, "SLOWSLOWSLO");
        let running = before
            .download(
                vec![slow.clone(), item(2, "alice000002")],
                "S [1]",
                "r",
                "u1",
            )
            .await
            .unwrap();
        let pid_file = rig.partial().join(&running[0].id).join("pid");
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

        engine.stop().await;

        assert!(protocol::tests::gone(pid).await);
        assert!(rig.staging().is_empty(), "{:?}", rig.staging());
        let listed = before
            .get_downloads_for(std::slice::from_ref(&slow.source))
            .await
            .unwrap();
        assert_eq!(
            listed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            [finished[0].id.as_str()],
            "only the finished transfer stays"
        );
        // A handle from before is refused.
        let refused = before
            .download(vec![item(3, "MdzF0rFOC9Y")], "S [1]", "r", "u1")
            .await
            .unwrap_err();
        assert_eq!(
            refused.failure(Stage::Enqueue, "m").code,
            shared::failure::FailureCode::BackendNotConfigured
        );
        let wanted = matching::tests::spiderland();
        assert!(before
            .start_search(&SearchSpec::of(wanted.album.clone(), wanted.tracks.clone()))
            .await
            .is_err());
        // A handle built again works.
        let after = YtDlpBackend::new(engine);
        let sent = after
            .download(vec![item(3, "MdzF0rFOC9Y")], "S [1]", "r", "u1")
            .await
            .unwrap();
        assert_eq!(sent[0].status, EnqueueStatus::Enqueued);
    }

    #[tokio::test]
    async fn turning_off_with_no_engine_deletes_what_a_previous_run_left() {
        let rig = Rig::new();
        let partial = rig.partial();
        std::fs::create_dir_all(partial.join("old-transfer")).unwrap();
        std::fs::write(partial.join("old-transfer").join("a.webm.part"), b"x").unwrap();
        std::fs::create_dir_all(partial.join("cache")).unwrap();
        let cell = EngineCell::new();
        assert!(!cell.stop().await, "no engine was started");
        cell.forget_unfinished(rig.settings()).await.unwrap();
        assert!(!partial.join("old-transfer").exists());
        assert!(partial.join("cache").exists());
    }

    #[test]
    fn a_yt_dlp_release_is_dated_by_its_version() {
        let today = chrono::Utc::now().date_naive();
        let ten_days_ago = today - chrono::Duration::days(10);
        let version = ten_days_ago.format("%Y.%m.%d").to_string();
        assert_eq!(release_age_days(&version), Some(10));
        assert_eq!(release_age_days(&format!("{version}.1")), Some(10));
        assert_eq!(release_age_days("nightly"), None);
    }
}
