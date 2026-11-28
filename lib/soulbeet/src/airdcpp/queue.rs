//! Queueing files in the sidecar and taking them out: one file bundle per item, named the way
//! `resolve_in_destination` reads it, each in the transfer book before AirDC++ is asked.

use super::error::{enqueue_refused, AirdcppError};
use super::models::Queued;
use super::progress::{bundle_by_id, finished, Data, Ended, PENDING_KEPT_SECS};
use super::{Engine, Link};
use crate::audio::is_audio_path;
use crate::BookEntry;
use reqwest::Method;
use serde_json::json;
use shared::airdcpp::{is_tiger_hash, ItemData, Source, BACKEND_ID};
use shared::download::{
    dotnet_ticks, sanitize_filename, ticks_copy, DownloadableItem, EnqueueStatus, QueuedDownload,
};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use std::collections::HashSet;
use std::path::Path;
use tracing::warn;

/// AirDC++ reads `%[name]` in a target folder as a parameter and erases it, then runs the
/// folder through `strftime`. A folder holding `%[` and a later `]` cannot be sent; any other is
/// sent with each `%` written `%%`, which `strftime` turns back into one. A lone `%` would make
/// AirDC++ 2.14.0 spin forever in `strftime`.
pub(crate) fn sidecar_folder(folder: &str) -> Option<String> {
    let erased = folder
        .find("%[")
        .is_some_and(|at| folder[at + 2..].contains(']'));
    (!erased).then(|| folder.replace('%', "%%"))
}

/// A batch folder Soulbeet made: one plain name.
fn plain_folder(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn invalid(message: &str) -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Enqueue,
        RetryAdvice::NotRetryable,
        message,
    )
}

fn already_fetching() -> Failure {
    Failure::new(
        FailureCode::EnqueueFailed,
        Stage::Enqueue,
        RetryAdvice::SameSourceLater,
        "AirDC++ is already fetching this file into this folder",
    )
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The file an item names, as `download` checks it: the peer, the file and the folder it lands
/// in under the name the sidecar saves it as.
struct File {
    source: Source,
    data: ItemData,
    name: String,
}

/// The file an item from the browser names, when it is an audio file of this backend whose
/// data agrees with its id and source.
fn file_of(item: &DownloadableItem) -> Option<File> {
    if item.backend != BACKEND_ID {
        return None;
    }
    let data: ItemData = serde_json::from_str(item.backend_data.as_deref()?).ok()?;
    let source = Source::parse(&item.source)?;
    let name = sanitize_filename(basename(&item.id));
    let fits = source.cid == data.cid
        && source.hub_url == data.hub_url
        && is_tiger_hash(&data.tth)
        && data.size > 0
        && is_audio_path(Path::new(&name))
        && !name.bytes().any(|b| (1..=31).contains(&b));
    fits.then_some(File { source, data, name })
}

impl Engine {
    /// The name `name` lands under in `folder`: itself, or a ticks copy when a file, a partial
    /// file or a queued bundle has it, or an earlier file of the call took it.
    fn free_name(
        &self,
        folder: &Path,
        sidecar_folder: &str,
        queued: &HashSet<String>,
        taken: &HashSet<String>,
        name: &str,
    ) -> String {
        let used = |candidate: &str| {
            taken.contains(candidate)
                || folder.join(candidate).exists()
                || folder.join(format!("{candidate}.dctmp")).exists()
                || queued.contains(&format!("{sidecar_folder}{candidate}"))
        };
        if !used(name) {
            return name.to_string();
        }
        let mut ticks = dotnet_ticks(std::time::SystemTime::now());
        loop {
            let copy = ticks_copy(name, ticks);
            if !used(&copy) {
                return copy;
            }
            ticks += 1;
        }
    }

    /// Queues `items` into the batch folder `destination` for a handle of `generation`. Each
    /// file is in the book before AirDC++ is asked, so one whose answer is lost is found by its
    /// path at the next listing. A stop waits for the call; the files after it are refused.
    pub(crate) async fn download(
        &self,
        link: &Link,
        generation: u64,
        items: Vec<DownloadableItem>,
        destination: &str,
        request: &str,
    ) -> Result<Vec<QueuedDownload>, AirdcppError> {
        let _queueing = self.queueing.read().await;
        self.current(generation)?;
        if !plain_folder(destination) {
            return Ok(vec![]);
        }
        let files: Vec<(DownloadableItem, File)> = (items.into_iter())
            .filter_map(|item| file_of(&item).map(|file| (item, file)))
            .collect();
        if files.is_empty() {
            return Ok(vec![]);
        }
        let settings = link.settings().await?;
        let mut root = settings.download_directory.clone();
        if !root.ends_with('/') {
            root.push('/');
        }
        let folder = format!("{root}{destination}/");
        let answer = |item: &DownloadableItem, id: String, size, status| QueuedDownload {
            id,
            source: item.source.clone(),
            item: item.id.clone(),
            size,
            status,
        };
        let Some(sent_folder) = sidecar_folder(&folder) else {
            let failure =
                invalid("AirDC++ would drop the part of this folder's name between %[ and ]");
            return Ok(files
                .iter()
                .map(|(item, file)| {
                    let status = EnqueueStatus::Failed(failure.clone());
                    answer(item, item.id.clone(), file.data.size, status)
                })
                .collect());
        };
        let listing = self.listing(&link.sidecar).await?;
        let queued: HashSet<String> = (listing.bundles.iter())
            .filter(|b| !finished(b))
            .map(|b| b.target.clone())
            .collect();
        let held = self
            .book
            .by_sources(
                BACKEND_ID,
                &(files.iter())
                    .map(|(item, _)| item.source.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;
        let local = self.download_path.join(destination);
        let mut taken = HashSet::new();
        let mut answers = Vec::new();
        let mut unreachable: Option<AirdcppError> = None;
        // A file is being fetched while its bundle is listed unfinished, or while its queue
        // request may still be answered.
        let in_flight = |e: &BookEntry| match e.transfer_id.as_deref() {
            Some(id) => (id.parse().ok())
                .and_then(|id| listing.bundle(id))
                .is_some_and(|b| !finished(b)),
            None => now_secs() - e.created_at <= PENDING_KEPT_SECS,
        };
        let mut spaced = false;
        for (item, file) in files {
            let size = file.data.size;
            let fetching = held.iter().any(|e| {
                e.item == item.id
                    && e.source == item.source
                    && e.target == destination
                    && Data::of(e).is_some_and(|d| d.ended.is_none() && !d.abandoned)
                    && in_flight(e)
            });
            if fetching {
                let status = EnqueueStatus::Failed(already_fetching());
                answers.push(answer(&item, item.id.clone(), size, status));
                continue;
            }
            if let Some(e) = &unreachable {
                let status = EnqueueStatus::Failed(
                    e.failure(Stage::Enqueue, "AirDC++ did not answer the queue request"),
                );
                answers.push(answer(&item, item.id.clone(), size, status));
                continue;
            }
            if spaced && self.current(generation).is_ok() {
                tokio::time::sleep(self.timing.enqueue_spacing).await;
            }
            spaced = true;
            // A stop came meanwhile: it waits for this call, so nothing more is queued.
            if let Err(e) = self.current(generation) {
                let status = EnqueueStatus::Failed(e.failure(Stage::Enqueue, e.to_string()));
                answers.push(answer(&item, item.id.clone(), size, status));
                continue;
            }
            let name = self.free_name(&local, &folder, &queued, &taken, &file.name);
            taken.insert(name.clone());
            let key = uuid::Uuid::new_v4().to_string();
            let data = Data {
                name: name.clone(),
                sidecar_target: format!("{folder}{name}"),
                size,
                tth: file.data.tth.clone(),
                ended: None,
                abandoned: false,
            };
            let entry = BookEntry {
                backend: BACKEND_ID.into(),
                key: key.clone(),
                transfer_id: None,
                file_id: None,
                source: item.source.clone(),
                item: item.id.clone(),
                request: request.to_string(),
                target: destination.to_string(),
                data: data.value(),
                created_at: now_secs(),
            };
            if let Err(e) = self.book.put(&entry).await {
                let failure = AirdcppError::Book(e).failure(
                    Stage::Enqueue,
                    "The Direct Connect transfer could not be recorded",
                );
                answers.push(answer(
                    &item,
                    item.id.clone(),
                    size,
                    EnqueueStatus::Failed(failure),
                ));
                continue;
            }
            let body = json!({
                "target_directory": sent_folder,
                "target_name": name,
                "size": size,
                "tth": file.data.tth,
                "user": {"cid": file.source.cid, "hub_url": file.source.hub_url},
            });
            let sent = link
                .sidecar
                .send(Method::POST, "queue/bundles/file", Some(&body))
                .await;
            // Whether AirDC++ may hold a bundle for the file: then the entry stays, marked so
            // the next listing removes a bundle it finds at its path.
            let (status, maybe_queued) = match sent {
                Ok(sent) if sent.ok() => match sent.json::<Queued>() {
                    Ok(queued) if !queued.merged => {
                        let id = queued.id.to_string();
                        if let Err(e) = self.book.bind(BACKEND_ID, &key, &id, None).await {
                            // The next listing finds it by its path.
                            warn!("Could not bind Direct Connect transfer {key}: {e}");
                        }
                        answers.push(answer(&item, id, size, EnqueueStatus::Enqueued));
                        continue;
                    }
                    // Another bundle took the file: it is not Soulbeet's to touch.
                    Ok(_) => (
                        EnqueueStatus::Failed(enqueue_refused(
                            200,
                            "AirDC++ merged the file into a bundle it already had",
                        )),
                        false,
                    ),
                    Err(e) => (
                        EnqueueStatus::Failed(e.failure(
                            Stage::Enqueue,
                            "AirDC++ queued the file with an answer Soulbeet cannot read",
                        )),
                        true,
                    ),
                },
                // AirDC++ refuses before it makes a bundle.
                Ok(refused) if refused.refused_by_airdcpp() => (
                    EnqueueStatus::Failed(enqueue_refused(refused.status, &refused.message())),
                    false,
                ),
                // A proxy in front of AirDC++ may answer after AirDC++ made the bundle.
                Ok(other) => (
                    EnqueueStatus::Failed(other.refused().failure(
                        Stage::Enqueue,
                        "AirDC++ answered the queue request with an error of its proxy",
                    )),
                    true,
                ),
                Err(e) => {
                    let status = EnqueueStatus::Failed(
                        e.failure(Stage::Enqueue, "AirDC++ did not answer the queue request"),
                    );
                    unreachable = Some(e);
                    (status, true)
                }
            };
            match maybe_queued {
                true => {
                    let abandoned = Data {
                        abandoned: true,
                        ..data
                    };
                    if let Err(e) = self
                        .book
                        .set_data(BACKEND_ID, &key, &abandoned.value())
                        .await
                    {
                        warn!("Could not record Direct Connect transfer {key}: {e}");
                    }
                }
                false => self.forget(&key).await,
            }
            answers.push(answer(&item, item.id.clone(), size, status));
        }
        Ok(answers)
    }

    /// The pending entry of `source` that AirDC++'s bundle `id` was queued for, bound to it now:
    /// for a cancel of a bundle whose queue answer came but could not be recorded.
    async fn adopt(
        &self,
        link: &Link,
        source: &str,
        id: u64,
    ) -> Result<Option<BookEntry>, AirdcppError> {
        let listing = self.listing(&link.sidecar).await?;
        let Some(bundle) = bundle_by_id(&link.sidecar, &listing, id).await? else {
            return Ok(None);
        };
        let pending = self
            .book
            .by_sources(BACKEND_ID, &[source.to_string()])
            .await?;
        let Some(mut entry) = pending.into_iter().find(|e| {
            e.transfer_id.is_none()
                && Data::of(e).is_some_and(|d| d.sidecar_target == bundle.target)
        }) else {
            return Ok(None);
        };
        let id = id.to_string();
        self.book.bind(BACKEND_ID, &entry.key, &id, None).await?;
        entry.transfer_id = Some(id);
        Ok(Some(entry))
    }

    /// Stops the bundle `id` of `source`, deleting its partial file and keeping it as cancelled,
    /// or with `remove` forgets it. A finished file is never touched: AirDC++ is only ever asked
    /// to remove a bundle with `remove_finished` false.
    pub(crate) async fn cancel(
        &self,
        link: &Link,
        source: &str,
        id: &str,
        remove: bool,
    ) -> Result<(), AirdcppError> {
        let Ok(bundle_id) = id.parse::<u64>() else {
            return Ok(());
        };
        let entry = match self.book.by_transfer(BACKEND_ID, id).await? {
            Some(entry) => entry,
            None => match self.adopt(link, source, bundle_id).await? {
                Some(entry) => entry,
                None => return Ok(()),
            },
        };
        if entry.source != source {
            return Ok(());
        }
        let data = Data::of(&entry);
        let ended = data.as_ref().is_some_and(|d| d.ended.is_some());
        let listing = self.listing(&link.sidecar).await?;
        let bundle = bundle_by_id(&link.sidecar, &listing, bundle_id).await?;
        if remove {
            if bundle.is_some() {
                self.remove_bundle(&link.sidecar, bundle_id).await?;
            }
            self.book.forget(BACKEND_ID, &entry.key).await?;
            self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).watches.remove(&bundle_id);
            return Ok(());
        }
        if ended || bundle.as_ref().is_some_and(finished) {
            return Ok(());
        }
        if bundle.is_some() {
            self.remove_bundle(&link.sidecar, bundle_id).await?;
        }
        if let Some(data) = data {
            let cancelled = Data {
                ended: Some(Ended::Cancelled),
                ..data
            };
            self.book
                .set_data(BACKEND_ID, &entry.key, &cancelled.value())
                .await?;
        }
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).watches.remove(&bundle_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percent_is_doubled_and_a_parameter_is_refused() {
        assert_eq!(
            sidecar_folder("/Downloads/100% Hits [ab12cd34]/").as_deref(),
            Some("/Downloads/100%% Hits [ab12cd34]/")
        );
        assert_eq!(sidecar_folder("/Downloads/%[x] [ab12cd34]/"), None);
        assert_eq!(
            sidecar_folder("/Downloads/a %[ b/"),
            Some("/Downloads/a %%[ b/".into())
        );
        assert_eq!(
            sidecar_folder("/Downloads/Plain [ab12cd34]/").as_deref(),
            Some("/Downloads/Plain [ab12cd34]/")
        );
    }
}
