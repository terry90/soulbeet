//! The sidecar's bundles as Soulful's transfer rows: which bundles are Soulful's (the transfer
//! book says), how far each is, and when one that gets no byte is given up. AirDC++ itself never
//! fails a file it cannot get, so the backend does after `Timing::give_up` of failed connections.

use super::error::{ended, AirdcppError};
use super::http::Sidecar;
use super::models::{whole, Bundle, PathQueued, Transfer};
use super::{Engine, Link};
use crate::BookEntry;
use regex::Regex;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;
use shared::airdcpp::{Source, BACKEND_ID};
use shared::download::{DownloadProgress, DownloadState};
use shared::failure::Failure;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::time::Instant;
use tracing::{info, warn};

/// A file the book holds whose bundle never appeared is forgotten this long after it was queued.
pub(crate) const PENDING_KEPT_SECS: i64 = 600;
/// An ended or lost transfer's entry is forgotten this long after it was queued.
const ENDED_KEPT_SECS: i64 = 7 * 24 * 3600;
/// The bundles read in one page of a listing.
const PAGE: usize = 1000;

/// AirDC++'s text on a transfer whose peer has no free slot, followed by ` - Queued: 7` when the
/// peer gave its place.
const NO_SLOTS: &str = "No slots available";
/// AirDC++'s texts on a transfer it holds back for its own download limits.
const OWN_LIMITS: [&str; 2] = [
    "All download slots taken",
    "Wanted total download speed reached",
];

/// The place a peer gives in its slot queue, in English: `No slots available - Queued: 7`.
static QUEUED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Queued: (\d+)").unwrap());

/// The place a transfer's text gives in its peer's queue.
pub(crate) fn place_in_text(text: &str) -> Option<u32> {
    QUEUED.captures(text)?.get(1)?.as_str().parse().ok()
}

/// The constant body of every bundle removal: finished files are never deleted.
pub(crate) fn remove_body() -> serde_json::Value {
    json!({"remove_finished": false})
}

/// How a transfer the backend ended ended, as the book keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Ended {
    Cancelled,
    Failed { failure: Failure },
}

/// What the book keeps of a file besides its entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Data {
    /// The name the file is queued under in its batch folder.
    pub name: String,
    /// The file's path as AirDC++ names it: how a bundle whose queue answer was lost is found.
    pub sidecar_target: String,
    pub size: u64,
    pub tth: String,
    #[serde(default)]
    pub ended: Option<Ended>,
    /// `download` answered this file failed though its queue request may have reached AirDC++:
    /// a bundle found for it is removed.
    #[serde(default)]
    pub abandoned: bool,
}

impl Data {
    pub(crate) fn of(entry: &BookEntry) -> Option<Data> {
        serde_json::from_value(entry.data.clone()).ok()
    }

    pub(crate) fn value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }
}

/// What the engine remembers of a waiting bundle between listings.
#[derive(Debug, Clone)]
pub(crate) struct Watch {
    /// The last byte, or when the bundle was first seen without one.
    since: Instant,
    bytes: u64,
    /// A source of the file was online at some listing.
    online_seen: bool,
    attempts: u32,
    /// The last listing showed a failed connection.
    failing: bool,
    /// The last place its peer gave in its queue.
    place: Option<u32>,
    alternates_asked: bool,
}

/// The bundles and transfers the sidecar listed, at one moment.
pub(crate) struct Listing {
    at: Instant,
    pub bundles: Vec<Bundle>,
    pub transfers: Vec<Transfer>,
}

impl Listing {
    pub(crate) fn bundle(&self, id: u64) -> Option<&Bundle> {
        self.bundles.iter().find(|b| b.id == id)
    }

    fn by_target(&self, target: &str) -> Option<&Bundle> {
        self.bundles.iter().find(|b| b.target == target)
    }

    /// The file downloads of `bundle`.
    fn transfers_of<'a>(&'a self, bundle: &'a Bundle) -> impl Iterator<Item = &'a Transfer> {
        (self.transfers.iter())
            .filter(|t| t.download && !t.is_filelist() && t.target == bundle.target)
    }

    /// A file download that runs; a file list load is none.
    fn running(t: &Transfer) -> bool {
        t.download && !t.is_filelist() && t.status.id == "running"
    }

    /// Whether a file download from the peer `cid` runs, for another bundle than `bundle`.
    fn sibling_runs(&self, cid: &str, bundle: &Bundle) -> bool {
        (self.transfers.iter())
            .any(|t| Self::running(t) && t.user.cid == cid && t.target != bundle.target)
    }
}

/// Every bundle of the sidecar, a page at a time. AirDC++ pages through an unordered map, so a
/// bundle added or removed meanwhile can move one across pages: before the backend acts on a
/// bundle missing here, it asks for that bundle by itself.
async fn all_bundles(sidecar: &Sidecar) -> Result<Vec<Bundle>, AirdcppError> {
    let mut bundles: Vec<Bundle> = Vec::new();
    loop {
        let path = format!("queue/bundles/{}/{PAGE}", bundles.len());
        let answer = sidecar.send(Method::GET, &path, None).await?;
        // A start at or past the end, as bundles went since the last page.
        if !bundles.is_empty() && answer.status == 400 && answer.message() == "Invalid range" {
            return Ok(bundles);
        }
        let page: Vec<Bundle> = answer.json()?;
        let full = page.len() == PAGE;
        bundles.extend(page);
        if !full {
            return Ok(bundles);
        }
    }
}

/// The bundle `id`: from `listing`, else asked for by itself. None only when AirDC++ answers
/// that it has none.
pub(crate) async fn bundle_by_id(
    sidecar: &Sidecar,
    listing: &Listing,
    id: u64,
) -> Result<Option<Bundle>, AirdcppError> {
    if let Some(bundle) = listing.bundle(id) {
        return Ok(Some(bundle.clone()));
    }
    let answer = sidecar
        .send(Method::GET, &format!("queue/bundles/{id}"), None)
        .await?;
    match answer.status {
        404 => Ok(None),
        _ => answer.json().map(Some),
    }
}

/// The bundle that holds the file AirDC++ names `target`: from `listing`, else asked for by the
/// path. None only when AirDC++ answers that no bundle holds it.
pub(crate) async fn bundle_at(
    sidecar: &Sidecar,
    listing: &Listing,
    target: &str,
) -> Result<Option<Bundle>, AirdcppError> {
    if let Some(bundle) = listing.by_target(target) {
        return Ok(Some(bundle.clone()));
    }
    let queued: PathQueued = sidecar
        .post("queue/check_path_queued", &json!({ "path": target }))
        .await?;
    match queued.bundle {
        Some(found) => bundle_by_id(sidecar, listing, found.id).await,
        None => Ok(None),
    }
}

/// The bundle of `entry`: its bound one, else the one at its path.
async fn bundle_of(
    sidecar: &Sidecar,
    listing: &Listing,
    entry: &BookEntry,
    data: &Data,
) -> Result<Option<Bundle>, AirdcppError> {
    match entry.transfer_id.as_deref().and_then(|id| id.parse().ok()) {
        Some(id) => bundle_by_id(sidecar, listing, id).await,
        None => bundle_at(sidecar, listing, &data.sidecar_target).await,
    }
}

/// Whether a bundle's file is on disk under its name: downloaded, being checked, or done.
pub(crate) fn finished(bundle: &Bundle) -> bool {
    matches!(
        bundle.status.id.as_str(),
        "downloaded" | "completion_validation_running" | "completed" | "shared"
    )
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// What a bundle reads as now: a row, or an ending the backend must record first.
enum Read {
    Row(Box<DownloadProgress>),
    Ends(Failure),
}

impl Engine {
    /// The bundles and transfers as listed under a second ago, else listed now. Callers at once
    /// share one listing.
    pub(crate) async fn listing(&self, sidecar: &Sidecar) -> Result<Arc<Listing>, AirdcppError> {
        let mut kept = self.listing.lock().await;
        if let Some(listing) = &*kept {
            if listing.at.elapsed() < self.timing.listing_kept {
                return Ok(listing.clone());
            }
        }
        let bundles = all_bundles(sidecar).await?;
        let transfers: Vec<Transfer> = sidecar.get("transfers").await?;
        let listing = Arc::new(Listing {
            at: Instant::now(),
            bundles,
            transfers,
        });
        *kept = Some(listing.clone());
        Ok(listing)
    }

    /// The rows of every file the book holds from `sources`. A file whose bundle is gone is not
    /// listed: the monitor decides what that means.
    pub(crate) async fn listed(
        &self,
        link: &Arc<Link>,
        sources: &[String],
    ) -> Result<Vec<DownloadProgress>, AirdcppError> {
        let listing = self.listing(&link.sidecar).await?;
        self.prune_once(&link.sidecar, &listing).await;
        let entries = self.book.by_sources(BACKEND_ID, sources).await?;
        let mut rows = Vec::new();
        for entry in entries {
            if let Some(row) = self.row(link, &listing, entry).await {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// The row of one book entry, binding a pending one whose bundle appeared, and ending one
    /// that failed in the sidecar or was given up.
    async fn row(
        &self,
        link: &Arc<Link>,
        listing: &Listing,
        mut entry: BookEntry,
    ) -> Option<DownloadProgress> {
        let Some(data) = Data::of(&entry) else {
            warn!(
                "Direct Connect transfer {} has a record Soulbeet cannot read",
                entry.key
            );
            return None;
        };
        if let Some(ended) = &data.ended {
            let id = entry.transfer_id.clone()?;
            let row = self.base_row(&entry, &data, id);
            return Some(match ended {
                Ended::Cancelled => row.with_state(DownloadState::Cancelled),
                Ended::Failed { failure } => row.failed_with(failure.clone()),
            });
        }
        let bundle = match entry.transfer_id.as_deref() {
            Some(id) => listing.bundle(id.parse().ok()?)?.clone(),
            None => {
                let found = match listing.by_target(&data.sidecar_target) {
                    Some(found) => found.clone(),
                    None if now_secs() - entry.created_at <= PENDING_KEPT_SECS => return None,
                    // Forgotten only once AirDC++ says no bundle holds its path.
                    None => match bundle_at(&link.sidecar, listing, &data.sidecar_target).await {
                        Ok(Some(found)) => found,
                        Ok(None) => {
                            self.forget(&entry.key).await;
                            return None;
                        }
                        Err(e) => {
                            warn!(
                                "Could not look for Direct Connect transfer {}: {e}",
                                entry.key
                            );
                            return None;
                        }
                    },
                };
                if data.abandoned {
                    self.remove_bundle(&link.sidecar, found.id).await.ok()?;
                    self.forget(&entry.key).await;
                    return None;
                }
                let id = found.id.to_string();
                if let Err(e) = self.book.bind(BACKEND_ID, &entry.key, &id, None).await {
                    warn!("Could not bind Direct Connect transfer {}: {e}", entry.key);
                }
                entry.transfer_id = Some(id);
                found
            }
        };
        match self.read(link, listing, &entry, &data, &bundle) {
            Read::Row(row) => Some(*row),
            Read::Ends(failure) => {
                // Removed first, so its partial file goes; the book then says why.
                self.remove_bundle(&link.sidecar, bundle.id).await.ok()?;
                let ended = Data {
                    ended: Some(Ended::Failed {
                        failure: failure.clone(),
                    }),
                    ..data.clone()
                };
                if let Err(e) = self
                    .book
                    .set_data(BACKEND_ID, &entry.key, &ended.value())
                    .await
                {
                    warn!(
                        "Could not record Direct Connect transfer {}: {e}",
                        entry.key
                    );
                }
                self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).watches.remove(&bundle.id);
                info!(
                    "Direct Connect transfer {} failed: {}",
                    bundle.id, failure.message
                );
                Some(
                    self.base_row(&entry, &data, bundle.id.to_string())
                        .failed_with(failure),
                )
            }
        }
    }

    fn base_row(&self, entry: &BookEntry, data: &Data, id: String) -> DownloadProgress {
        let mut row =
            DownloadProgress::queued(id, entry.source.clone(), entry.item.clone(), data.size)
                .with_backend(BACKEND_ID);
        row.request = Some(entry.request.clone());
        row
    }

    /// The row a bundle reads as, its watch updated.
    fn read(
        &self,
        link: &Arc<Link>,
        listing: &Listing,
        entry: &BookEntry,
        data: &Data,
        bundle: &Bundle,
    ) -> Read {
        let mut row = self.base_row(entry, data, bundle.id.to_string());
        let bytes = whole(bundle.downloaded_bytes).min(data.size);
        row.transferred = bytes;
        row.percent = match data.size {
            0 => 0.0,
            size => bytes as f64 * 100.0 / size as f64,
        };
        row.speed = bundle.speed.max(0.0);
        let done = |mut row: DownloadProgress, state| {
            row.transferred = data.size;
            row.percent = 100.0;
            row.with_state(state)
        };
        match bundle.status.id.as_str() {
            // AirDC++ checked the file against its TTH.
            "completed" | "shared" => {
                return Read::Row(Box::new(done(row, DownloadState::Completed)))
            }
            // In place; the check follows within a second.
            "downloaded" | "completion_validation_running" => {
                return Read::Row(Box::new(done(row, DownloadState::InProgress)))
            }
            "download_error" => return Read::Ends(ended::download_error(&bundle.status.str)),
            "completion_validation_error" => {
                return Read::Ends(ended::validation(&bundle.status.str))
            }
            _ => {}
        }
        let source = Source::parse(&entry.source);
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let watch = state.watches.entry(bundle.id).or_insert(Watch {
            since: now,
            bytes,
            online_seen: false,
            attempts: 0,
            failing: false,
            place: None,
            alternates_asked: false,
        });
        if bytes > watch.bytes {
            watch.bytes = bytes;
            watch.since = now;
        }
        watch.online_seen |= whole(bundle.sources.online) > 0;
        let own: Vec<&Transfer> = listing.transfers_of(bundle).collect();
        let running = bundle.speed > 0.0 || own.iter().any(|t| Listing::running(t));
        let failed: Vec<&str> = (own.iter())
            .filter(|t| t.status.id == "failed")
            .map(|t| t.status.str.as_str())
            .collect();
        let slot_wait = failed.iter().find(|text| text.starts_with(NO_SLOTS));
        let own_limit = |text: &&str| OWN_LIMITS.iter().any(|l| text.starts_with(l));
        let held_back = failed.iter().any(own_limit);
        let failing = (failed.iter()).any(|text| !text.starts_with(NO_SLOTS) && !own_limit(text));
        if failing && !watch.failing {
            watch.attempts += 1;
        }
        watch.failing = failing;
        row.attempts = watch.attempts;
        if running {
            watch.since = now;
            watch.place = Some(0);
            return Read::Row(Box::new(row.with_state(DownloadState::InProgress)));
        }
        if let Some(text) = slot_wait {
            // A wait for a slot is the monitor's to time. Some peers give no place.
            watch.since = now;
            watch.place = place_in_text(text);
            row.queued_at_source = true;
            row.queue_position = watch.place;
            return Read::Row(Box::new(row));
        }
        let sibling = source
            .as_ref()
            .is_some_and(|s| listing.sibling_runs(&s.cid, bundle));
        // The peer sends one file at a time, or AirDC++ holds the file back for its own limits.
        if sibling || held_back {
            watch.since = now;
            row.queued_at_source = sibling;
            row.queue_position = watch.place;
            return Read::Row(Box::new(row));
        }
        let waited = now - watch.since;
        if waited >= self.timing.give_up {
            let minutes = self.timing.give_up.as_secs() / 60;
            return Read::Ends(ended::gave_up(minutes, !watch.online_seen));
        }
        if waited >= self.timing.alternates_after && !watch.alternates_asked {
            watch.alternates_asked = true;
            self.ask_alternates(link, bundle.id);
        }
        row.queue_position = watch.place;
        Read::Row(Box::new(row))
    }

    /// Asks AirDC++ once, in the search turn after every search waiting for it, to look for other
    /// peers with the bundle's file. One removed meanwhile is answered 404, and no search goes.
    fn ask_alternates(&self, link: &Arc<Link>, bundle: u64) {
        let (link, shared) = (link.clone(), self.shared.clone());
        tokio::spawn(async move {
            let interval = match link.settings().await {
                Ok(settings) => Duration::from_secs(settings.min_search_interval),
                Err(e) => {
                    warn!("AirDC++ settings for an alternate search: {e}");
                    return;
                }
            };
            let path = format!("queue/bundles/{bundle}/search");
            let asked = shared
                .turn
                .take_after_searches(interval.max(shared.timing.min_hub_interval), async {
                    let answer = link
                        .sidecar
                        .send(Method::POST, &path, Some(&json!({})))
                        .await;
                    (matches!(&answer, Ok(a) if a.ok()), answer)
                })
                .await;
            if let Err(e) = asked.and_then(|a| a.done()) {
                warn!("AirDC++ alternate search for bundle {bundle}: {e}");
            }
        });
    }

    /// Removes a bundle from the sidecar's queue: its partial file goes, a finished file stays.
    /// One already gone is no error.
    pub(crate) async fn remove_bundle(
        &self,
        sidecar: &Sidecar,
        id: u64,
    ) -> Result<(), AirdcppError> {
        let path = format!("queue/bundles/{id}/remove");
        let answer = sidecar
            .send(Method::POST, &path, Some(&remove_body()))
            .await?;
        *self.listing.lock().await = None;
        match answer.status {
            404 => Ok(()),
            _ => answer.done(),
        }
    }

    pub(crate) async fn forget(&self, key: &str) {
        if let Err(e) = self.book.forget(BACKEND_ID, key).await {
            warn!("Could not forget Direct Connect transfer {key}: {e}");
        }
    }

    /// Once per process, at the first listing: forgets the entries of transfers that ended, or
    /// whose bundle is gone, queued more than 7 days ago.
    async fn prune_once(&self, sidecar: &Sidecar, listing: &Listing) {
        {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.pruned {
                return;
            }
            state.pruned = true;
        }
        let entries = match self.book.all(BACKEND_ID).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Could not read the Direct Connect transfers: {e}");
                return;
            }
        };
        for entry in entries {
            if now_secs() - entry.created_at <= ENDED_KEPT_SECS {
                continue;
            }
            let gone = match Data::of(&entry) {
                Some(data) if data.ended.is_none() => {
                    match bundle_of(sidecar, listing, &entry, &data).await {
                        Ok(bundle) => bundle.is_none(),
                        Err(e) => {
                            warn!(
                                "Could not look for Direct Connect transfer {}: {e}",
                                entry.key
                            );
                            false
                        }
                    }
                }
                _ => true,
            };
            if gone {
                self.forget(&entry.key).await;
            }
        }
    }

    /// The place the peer last gave the bundle `id` in its queue: 0 once it runs; None when the
    /// sidecar no longer has it, or the peer gave none.
    pub(crate) async fn place(
        &self,
        link: &Arc<Link>,
        source: &str,
        id: &str,
    ) -> Result<Option<u32>, AirdcppError> {
        let Some(entry) = self.book.by_transfer(BACKEND_ID, id).await? else {
            return Ok(None);
        };
        if entry.source != source {
            return Ok(None);
        }
        let listing = self.listing(&link.sidecar).await?;
        let Some(bundle) = id.parse().ok().and_then(|id| listing.bundle(id)) else {
            return Ok(None);
        };
        if finished(bundle) || bundle.speed > 0.0 {
            return Ok(Some(0));
        }
        Ok((self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).watches.get(&bundle.id)).and_then(|w| w.place))
    }

    /// Removes from the sidecar every unfinished bundle of the book and forgets its entry.
    pub(crate) async fn remove_unfinished(&self, sidecar: Option<&Sidecar>) {
        let entries = match self.book.all(BACKEND_ID).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Could not read the Direct Connect transfers to stop them: {e}");
                return;
            }
        };
        let unfinished: Vec<BookEntry> = (entries.into_iter())
            .filter(|e| Data::of(e).is_some_and(|d| d.ended.is_none()))
            .collect();
        if unfinished.is_empty() {
            return;
        }
        let Some(sidecar) = sidecar else {
            warn!(
                "{} Direct Connect transfers stay recorded: AirDC++'s settings are not usable",
                unfinished.len()
            );
            return;
        };
        *self.listing.lock().await = None;
        let listing = match self.listing(sidecar).await {
            Ok(listing) => listing,
            Err(e) => {
                warn!(
                    "{} Direct Connect transfers stay recorded: AirDC++ could not list them: {e}",
                    unfinished.len()
                );
                return;
            }
        };
        for entry in unfinished {
            let Some(data) = Data::of(&entry) else {
                continue;
            };
            let bundle = match bundle_of(sidecar, &listing, &entry, &data).await {
                Ok(bundle) => bundle,
                Err(e) => {
                    warn!(
                        "Direct Connect transfer {} stays recorded: AirDC++ could not say where it is: {e}",
                        entry.key
                    );
                    continue;
                }
            };
            match bundle {
                Some(bundle) if finished(&bundle) => continue,
                Some(bundle) => {
                    if let Err(e) = self.remove_bundle(sidecar, bundle.id).await {
                        warn!("Could not remove Direct Connect bundle {}: {e}", bundle.id);
                        continue;
                    }
                    self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).watches.remove(&bundle.id);
                }
                None => {}
            }
            self.forget(&entry.key).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_place_a_peer_gives_is_read_from_its_text() {
        assert_eq!(place_in_text("No slots available - Queued: 7"), Some(7));
        assert_eq!(place_in_text("No slots available - Queued: 14"), Some(14));
        assert_eq!(place_in_text("No slots available"), None);
        assert_eq!(place_in_text("Connection timeout"), None);
        assert_eq!(place_in_text("Connecting..."), None);
    }

    #[test]
    fn a_removal_never_asks_for_finished_files() {
        assert_eq!(remove_body(), json!({"remove_finished": false}));
    }
}
