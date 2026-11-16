//! The fact documents tool results carry: a candidate source (design 11.4), what a search read
//! (design 10.2) and a job (design 11.5). Text from peers and providers is cut to TEXT_MAX_CHARS.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use shared::download::{
    DownloadProgress, DownloadQuery, DownloadState, DownloadableItem, IndexState, SearchStats,
    SourcePresence,
};
use shared::failure::{cut_chars, Failure, FailureCode, RetryAdvice, Stage, TEXT_MAX_CHARS};
use shared::metadata::{Album, Track};
use shared::recommendation::normalize_for_matching;
use soulbeet::error::PeerAsk;
use soulbeet::{DownloadBackend, ImportedItem};

use super::contract::{error_object, NextCall, ToolError};
use super::next::next_calls;
use super::paging::{leave_out, length, Keep};
use crate::core::jobs::retry::enable_deletion;
use crate::core::jobs::{
    current_attempt, failed_slot, job_failure, job_filters, picked_file, shown_row, JobView,
    SlotView, Untried,
};
use crate::core::library::SongFacts;
use crate::core::sources::{self, Candidate, DroppedByFilter, SearchProgress};
use crate::ledger::store::AttemptRecord;
use crate::ledger::{JobKind, JobState, SlotResult, StoredSpec};
use crate::services::DEFAULT_DOWNLOAD_BACKEND;

/// Peer or provider text as a result shows it.
fn cut(text: &str) -> String {
    cut_chars(text.to_string(), TEXT_MAX_CHARS)
}

/// `facts` with each text cut as `cut` cuts one: what a backend reports of a file is the peer's.
fn cut_texts(facts: &Map<String, Value>) -> Map<String, Value> {
    (facts.iter())
        .map(|(name, value)| {
            let value = match value {
                Value::String(text) => json!(cut(text)),
                other => other.clone(),
            };
            (name.clone(), value)
        })
        .collect()
}

/// The wanted length of the file `item` stands for: the wanted track of its title, or the one
/// wanted track of a track search.
fn wanted_duration(item: &DownloadableItem, wanted: &DownloadQuery) -> Option<u32> {
    let title = normalize_for_matching(&item.title);
    match wanted.tracks.as_slice() {
        [only] if wanted.album.is_none() => only.duration_secs(),
        tracks => tracks
            .iter()
            .find(|t| normalize_for_matching(&t.title) == title)
            .and_then(|t| t.duration_secs()),
    }
}

/// One file's own facts, each null for a file not known.
fn file_fields(item: Option<&DownloadableItem>, wanted: &DownloadQuery) -> Map<String, Value> {
    let f = item.map(|item| &item.facts);
    let delta = item.and_then(|item| {
        (item.duration.zip(wanted_duration(item, wanted)))
            .map(|(file, wanted)| file.abs_diff(wanted))
    });
    let Value::Object(fields) = json!({
        "remote_file": item.map(|item| cut(&item.id)),
        "extension": f.and_then(|f| f.extension.as_deref()).map(cut),
        "size_bytes": item.map(|item| item.size),
        "bitrate_kbps": f.map(|f| f.bitrate_kbps),
        "variable_bitrate": f.map(|f| f.variable_bitrate),
        "sample_rate_hz": f.map(|f| f.sample_rate_hz),
        "bit_depth": f.map(|f| f.bit_depth),
        "duration_s": item.map(|item| item.duration),
        "duration_delta_s": delta,
        "match_score": item.map(|item| item.match_score),
        "quality_score": item.map(|item| item.quality_score),
        "below_floor": item.map(|item| item.below_floor),
        "backend_facts": f.map(|f| cut_texts(&f.backend_facts)),
    }) else {
        unreachable!("an object literal is an object");
    };
    fields
}

/// The folder holding every one of `ids`, each a remote path with `\` or `/` between its
/// parts: their longest common parent.
fn common_parent<'a>(mut ids: impl Iterator<Item = &'a str>) -> String {
    let separators = ['\\', '/'];
    let parent = |id: &'a str| &id[..id.rfind(separators).unwrap_or(0)];
    let Some(first) = ids.next() else {
        return String::new();
    };
    let mut common = parent(first);
    for id in ids {
        let folder = parent(id);
        while !(common.is_empty()
            || folder == common
            || folder
                .strip_prefix(common)
                .is_some_and(|rest| rest.starts_with(separators)))
        {
            common = &common[..common.rfind(separators).unwrap_or(0)];
        }
    }
    common.to_string()
}

/// A candidate as tools show it (design 11.4), its durations against the hydrated `wanted`
/// query. A track search's one-file group is offered as that file; any other group as its
/// folder with each file, and for an album the wanted tracks found in it and the titles of
/// those missing.
pub(crate) fn candidate_document(candidate: &Candidate, wanted: &DownloadQuery) -> Value {
    let group = &candidate.group;
    let mut doc = Map::new();
    doc.insert("candidate_id".into(), json!(candidate.id));
    doc.insert("backend".into(), json!(DEFAULT_DOWNLOAD_BACKEND));
    doc.insert("source".into(), json!(cut(&group.source)));
    let first = group.items.first();
    match group.items.as_slice() {
        [item] if wanted.album.is_none() => {
            doc.insert("download_scope".into(), json!("file"));
            doc.insert("artist".into(), json!(cut(&item.artist)));
            doc.insert("album".into(), json!(cut(&item.album)));
            doc.extend(file_fields(Some(item), wanted));
        }
        items => {
            doc.insert("download_scope".into(), json!("folder"));
            doc.insert(
                "remote_folder".into(),
                json!(cut(&common_parent(items.iter().map(|i| i.id.as_str())))),
            );
            doc.insert("artist".into(), json!(group.artist.as_deref().map(cut)));
            doc.insert("album".into(), json!(cut(&group.title)));
            doc.insert("size_bytes".into(), json!(group.total_size));
            doc.insert(
                "below_floor".into(),
                json!(items.iter().any(|i| i.below_floor)),
            );
            let files: Vec<Value> = items
                .iter()
                .map(|item| Value::Object(file_fields(Some(item), wanted)))
                .collect();
            doc.insert("files".into(), Value::Array(files));
            if wanted.album.is_some() {
                let found: Vec<String> = items
                    .iter()
                    .map(|i| normalize_for_matching(&i.title))
                    .collect();
                let (present, missing): (Vec<_>, Vec<_>) = wanted
                    .tracks
                    .iter()
                    .partition(|t| found.contains(&normalize_for_matching(&t.title)));
                doc.insert("tracks_found".into(), json!(present.len()));
                doc.insert(
                    "missing_titles".into(),
                    json!(missing.iter().map(|t| cut(&t.title)).collect::<Vec<_>>()),
                );
            }
        }
    }
    let facts = first.map(|item| &item.facts);
    doc.insert("score".into(), json!(group.score));
    doc.insert(
        "folder_track_count".into(),
        json!(facts.and_then(|f| f.folder_audio_files)),
    );
    doc.insert(
        "peer_has_free_slot".into(),
        json!(facts.and_then(|f| f.source_has_free_slot)),
    );
    doc.insert(
        "peer_upload_speed".into(),
        json!(facts.and_then(|f| f.source_upload_speed)),
    );
    doc.insert(
        "peer_queue_length".into(),
        json!(facts.and_then(|f| f.source_queue_length)),
    );
    doc.insert(
        "peer_recent_failures".into(),
        json!(candidate.recent_failures),
    );
    doc.insert("peer_flagged".into(), json!(candidate.flagged()));
    Value::Object(doc)
}

/// What a search read and what was dropped (design 10.2), each null when the backend did not
/// finish the search (`stats` None): files that are not audio (covers, playlists), files scored
/// below the floor unless the search kept them, flagged, as candidates, and what the filters
/// dropped.
pub(crate) fn search_facts(
    stats: Option<&SearchStats>,
    spec: &StoredSpec,
    dropped: &DroppedByFilter,
) -> Map<String, Value> {
    let Value::Object(facts) = (match stats {
        Some(s) => json!({
            "query_text_sent": cut(&s.query_text),
            "response_count": s.response_count,
            "file_count": s.file_count,
            "locked_file_count": s.locked_file_count,
            "dropped_not_audio": s.not_audio,
            "dropped_below_floor": if spec.keep_below_floor { 0 } else { s.below_floor },
            "dropped_by_filter": dropped,
        }),
        None => json!({
            "query_text_sent": null,
            "response_count": null,
            "file_count": null,
            "locked_file_count": null,
            "dropped_not_audio": null,
            "dropped_below_floor": null,
            "dropped_by_filter": null,
        }),
    }) else {
        unreachable!("an object literal is an object");
    };
    facts
}

/// How much of a job a document shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Detail {
    Summary,
    Full,
}

/// The backend's answers about a running transfer, read at most once per call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LiveFacts {
    /// Asked only for a transfer queued at its source.
    pub queue_position: Option<Result<Option<u32>, Failure>>,
    pub presence: Result<SourcePresence, Failure>,
}

/// What a full document reads now, before it is built.
#[derive(Debug, Default)]
pub(crate) struct ReadNow {
    /// The backend's answers, by slot.
    pub live: HashMap<u32, LiveFacts>,
    /// The songs Navidrome lists for an import beets found in the library, by the import's first
    /// slot.
    pub library: HashMap<u32, Result<Vec<SongFacts>, Failure>>,
}

/// How long a call waits for one answer of the backend about a transfer: the place in the queue
/// is asked of the peer over the network.
const LIVE_READ_WAIT: Duration = Duration::from_secs(10);

/// Whether a full document asks the backend about `row`: Some(true) for a transfer queued at its
/// source (its place in the queue and whether the source is online), Some(false) for one in
/// progress (whether the source is online), None for any other.
pub(crate) fn asks_backend(row: &DownloadProgress) -> Option<bool> {
    match row.state {
        DownloadState::Queued if row.queued_at_source => Some(true),
        DownloadState::InProgress => Some(false),
        _ => None,
    }
}

/// The backend's answers about `row`, as `asks_backend` says, each within LIVE_READ_WAIT.
pub(crate) async fn live_facts(
    backend: &Arc<dyn DownloadBackend>,
    row: &DownloadProgress,
) -> Option<LiveFacts> {
    let queued = asks_backend(row)?;
    let position = async {
        match queued {
            true => {
                let asked = backend.queue_position(&row.source, &row.id);
                Some(answered(PeerAsk::QueuePosition, asked).await)
            }
            false => None,
        }
    };
    let presence = answered(PeerAsk::Presence, backend.source_status(&row.source));
    let (queue_position, presence) = tokio::join!(position, presence);
    Some(LiveFacts {
        queue_position,
        presence,
    })
}

/// The answers about `row` when there is no backend to ask: `failure` for each.
pub(crate) fn unanswered(row: &DownloadProgress, failure: &Failure) -> Option<LiveFacts> {
    let queued = asks_backend(row)?;
    Some(LiveFacts {
        queue_position: queued.then(|| Err(failure.clone())),
        presence: Err(failure.clone()),
    })
}

/// The backend's answer to `ask`, or its failure in Soulful's own words: the backend's text,
/// which names peers and remote files, stays in the cause. No answer within LIVE_READ_WAIT is
/// the failure of the one asked (`PeerAsk::unanswered`): the peer for its queue, the Soulseek
/// server for a presence.
async fn answered<T>(
    ask: PeerAsk,
    read: impl Future<Output = soulbeet::error::Result<T>>,
) -> Result<T, Failure> {
    match tokio::time::timeout(LIVE_READ_WAIT, read).await {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(e)) => Err(e.ask_failure(ask, Stage::Transfer)),
        Err(_) => Err(Failure::new(
            ask.unanswered(),
            Stage::Transfer,
            RetryAdvice::SameSourceLater,
            "no answer within 10 seconds",
        )),
    }
}

/// The newest timeline entries a job document shows.
const TIMELINE_SHOWN: usize = 50;
/// The newest earlier attempts of each slot a job document shows.
const TRIED_PER_SLOT: usize = 10;

/// The job as get_jobs shows it (design 11.5). Summary: what it is, its state and its failure;
/// full adds each slot with its current transfer (`now.live` answering for the running ones),
/// the earlier attempts (`tried`), the timeline, what each import gave and `next`, computed from
/// the job and its `untried` candidates. `deletion_enabled`: whether the calling app may delete
/// music, which a rejected result's failure tells how to allow when it may not.
pub(crate) fn job_document(
    view: &JobView,
    untried: &Untried,
    detail: Detail,
    now: &ReadNow,
    deletion_enabled: bool,
) -> Value {
    let job = &view.job;
    // The search's query holds an album's tracklist.
    let wanted = view.search.as_ref().map_or(&job.wanted, |s| &s.query);
    let next = next_calls(view, untried);
    let mut doc = Map::new();
    doc.insert("job_id".into(), json!(job.id));
    doc.insert("batch_id".into(), json!(job.batch_id));
    doc.insert("kind".into(), json!(job.kind));
    doc.insert("wanted".into(), reference_of(job.kind, &job.wanted));
    doc.insert("state".into(), json!(job.state.as_str()));
    doc.insert("folder_id".into(), json!(job.folder.id));
    doc.insert("created_at".into(), json!(job.created_at));
    doc.insert("updated_at".into(), json!(job.updated_at));
    doc.insert(
        "failure".into(),
        failure_document(view, wanted, next.clone(), deletion_enabled),
    );
    if detail == Detail::Summary {
        return Value::Object(doc);
    }
    let slots: Vec<Value> = (view.slots.iter())
        .map(|slot| slot_document(view, slot, wanted, now))
        .collect();
    doc.insert("slots".into(), Value::Array(slots));
    let mut tried = Vec::new();
    let mut tried_omitted = 0;
    for slot in &view.slots {
        let (earlier, omitted) = earlier_attempts(slot);
        tried.extend(earlier.into_iter().map(|a| tried_entry(slot.slot, a)));
        tried_omitted += omitted;
    }
    doc.insert("tried".into(), Value::Array(tried));
    doc.insert("tried_omitted".into(), json!(tried_omitted));
    let omitted = view.timeline.len().saturating_sub(TIMELINE_SHOWN);
    let timeline: Vec<Value> = view.timeline[omitted..]
        .iter()
        .map(|entry| {
            let mut fields = Map::new();
            fields.insert("seq".into(), json!(entry.seq));
            fields.insert("at".into(), json!(entry.at));
            fields.insert("stage".into(), json!(entry.stage));
            fields.insert("event".into(), json!(entry.event));
            for (name, value) in &entry.facts {
                let value = match value {
                    Value::String(text) => json!(cut(text)),
                    other => other.clone(),
                };
                fields.insert(name.clone(), value);
            }
            Value::Object(fields)
        })
        .collect();
    doc.insert("timeline".into(), Value::Array(timeline));
    doc.insert("timeline_omitted".into(), json!(omitted));
    let missing = missing_titles(view);
    let results: Vec<Value> = results_of(view)
        .into_iter()
        .map(|(result, slots)| result_document(view, result, &slots, &missing, now))
        .collect();
    doc.insert("results".into(), Value::Array(results));
    if job.kind == JobKind::Album {
        doc.insert("missing_titles".into(), json!(missing));
    }
    doc.insert("next".into(), json!(next));
    bounded(&mut doc);
    Value::Object(doc)
}

/// The most characters of a full job document, so that one job always fits an answer with what
/// goes around it (RESULT_MAX_CHARS, less get_jobs' page cursor and its since call naming up to
/// MAX_JOB_IDS job ids, or get_batch's batch fields).
pub(crate) const JOB_DOCUMENT_MAX_CHARS: usize = 45_000;

/// `doc` within JOB_DOCUMENT_MAX_CHARS. While it is longer, items are left out whole, so no id is
/// cut, in this order: the oldest timeline entries, the oldest earlier attempts, the last items of
/// each import's lists (the last import's first), the last missing titles, then the last slots.
/// Each list's `{list}_omitted` counts what it left out.
fn bounded(doc: &mut Map<String, Value>) {
    let over = |doc: &Map<String, Value>| length(doc).saturating_sub(JOB_DOCUMENT_MAX_CHARS);
    for list in ["timeline", "tried"] {
        let over = over(doc);
        leave_out(doc, list, Keep::Newest, over);
    }
    let imports = doc
        .get("results")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    for import in (0..imports).rev() {
        for list in ["applied", "existing", "listed", "library_songs"] {
            let over = over(doc);
            let result = (doc.get_mut("results"))
                .and_then(|results| results.get_mut(import))
                .and_then(Value::as_object_mut);
            if let Some(result) = result {
                leave_out(result, list, Keep::First, over);
            }
        }
    }
    for list in ["missing_titles", "slots"] {
        let over = over(doc);
        leave_out(doc, list, Keep::First, over);
    }
}

/// What the job asks for, as a reference names it (D1): a track's artist, title and the album,
/// recording MBID and length it was given; an album's artist, title and release MBID.
fn reference_of(kind: JobKind, wanted: &DownloadQuery) -> Value {
    let mut fields = Map::new();
    match (kind, &wanted.album, wanted.tracks.first()) {
        (JobKind::Album, Some(album), _) => {
            fields.insert("kind".into(), json!("album"));
            fields.insert("artist".into(), json!(cut(&album.artist)));
            fields.insert("title".into(), json!(cut(&album.title)));
            fields.insert("release_mbid".into(), json!(album.mbid));
        }
        (JobKind::Track, _, Some(track)) => {
            fields.insert("kind".into(), json!("track"));
            fields.insert("artist".into(), json!(cut(&track.artist)));
            fields.insert("title".into(), json!(cut(&track.title)));
            let optional = [
                ("album", track.album_title.as_deref().map(|a| json!(cut(a)))),
                ("recording_mbid", track.mbid.as_ref().map(|id| json!(id))),
                ("duration_s", track.duration_secs().map(|s| json!(s))),
            ];
            for (name, value) in optional {
                if let Some(value) = value {
                    fields.insert(name.into(), value);
                }
            }
        }
        // A job stores what it asks for with it.
        _ => return Value::Null,
    }
    Value::Object(fields)
}

/// The slot's attempts that ended, but the one its row is of: the newest TRIED_PER_SLOT, oldest
/// first, and how many older ones are left out.
fn earlier_attempts(slot: &SlotView) -> (Vec<&AttemptRecord>, usize) {
    let current = current_attempt(slot).map(|a| a.n);
    let ended: Vec<&AttemptRecord> = (slot.attempts.iter())
        .filter(|a| a.ended_at.is_some() && Some(a.n) != current)
        .collect();
    let omitted = ended.len().saturating_sub(TRIED_PER_SLOT);
    (ended[omitted..].to_vec(), omitted)
}

/// An earlier attempt as `tried` lists it: the source and file it tried and how it ended.
fn tried_entry(slot: u32, attempt: &AttemptRecord) -> Value {
    let file = attempt.candidate.as_ref();
    let failure = attempt.failure.as_ref();
    json!({
        "slot": slot,
        "attempt": attempt.n,
        "source": attempt.source.as_deref().map(cut),
        "remote_file": file.map(|f| cut(&f.id)),
        "extension": file.and_then(|f| f.facts.extension.as_deref()).map(cut),
        "bitrate_kbps": file.and_then(|f| f.facts.bitrate_kbps),
        "size_bytes": file.and_then(|f| f.size),
        "started_at": attempt.started_at,
        "ended_at": attempt.ended_at,
        "code": failure.map(|f| f.code),
        "message": failure.map(|f| cut(&f.message)),
        "retry": failure.map(|f| f.retry),
    })
}

/// `failure` as the error object of a result, its message cut, with no facts or calls.
fn error_value(failure: &Failure) -> Value {
    error_object(&ToolError {
        failure: Failure {
            message: cut(&failure.message),
            ..failure.clone()
        },
        facts: Map::new(),
        tried: vec![],
        next: vec![],
    })
}

/// `job_failure` as an error object with its facts (D16): a failure of the job's search with
/// what the search read; any other with the attempt it is of, the slot's earlier attempts as
/// `tried`, and `next`. A rejected result stays in the library until deleted (design 11.5): when
/// deletion is not enabled for the app, its user_action says how a person enables it. Null for a
/// job that did not fail.
fn failure_document(
    view: &JobView,
    wanted: &DownloadQuery,
    next: Vec<NextCall>,
    deletion_enabled: bool,
) -> Value {
    let Some(mut failure) = job_failure(view) else {
        return Value::Null;
    };
    failure.message = cut(&failure.message);
    if failure.code == FailureCode::ResultRejected && !deletion_enabled {
        failure.user_action = Some(enable_deletion());
    }
    let mut facts = Map::new();
    facts.insert("job_id".into(), json!(view.job.id));
    let mut tried = vec![];
    if failure.stage == Stage::SourceSearch {
        facts.insert("search_id".into(), json!(view.job.search_id));
        facts.extend(job_search_facts(view));
    } else {
        let slot = failed_slot(view);
        facts.extend(attempt_facts(slot, wanted));
        let mut omitted = None;
        if let Some(slot) = slot {
            let (earlier, left_out) = earlier_attempts(slot);
            tried = earlier
                .into_iter()
                .map(|a| tried_entry(slot.slot, a))
                .collect();
            omitted = Some(left_out);
        }
        facts.insert("tried_omitted".into(), json!(omitted));
    }
    error_object(&ToolError {
        failure,
        facts,
        tried,
        next,
    })
}

/// What the job's search read and what the job's filters (as its runner reads the search with
/// them) and excluded sources dropped.
fn job_search_facts(view: &JobView) -> Map<String, Value> {
    let Some(search) = &view.search else {
        return search_facts(None, &StoredSpec::default(), &DroppedByFilter::default());
    };
    let spec = search.spec.clone().unwrap_or_default();
    let (stats, dropped) = match SearchProgress::of(search) {
        SearchProgress::Done(groups, stats) => {
            let kept = sources::view(
                &search.id,
                &groups,
                &job_filters(view),
                &HashMap::new(),
                &view.job.excluded_sources,
            );
            (stats, kept.dropped_by_filter)
        }
        SearchProgress::Failed(_, stats) => (stats, DroppedByFilter::default()),
        SearchProgress::Running => (None, DroppedByFilter::default()),
    };
    search_facts(stats.as_ref(), &spec, &dropped)
}

/// The facts of `slot`'s failed transfer (design 10.2): the attempt its row is of, the file and
/// how far the transfer came; each null when not known.
fn attempt_facts(slot: Option<&SlotView>, wanted: &DownloadQuery) -> Map<String, Value> {
    let row = slot.and_then(shown_row);
    let attempt = slot.and_then(current_attempt);
    let mut facts = file_fields(attempt.and_then(|a| a.candidate.as_ref()), wanted);
    facts.insert("slot".into(), json!(slot.map(|s| s.slot)));
    facts.insert("attempt".into(), json!(attempt.map(|a| a.n)));
    facts.insert("backend".into(), json!(DEFAULT_DOWNLOAD_BACKEND));
    facts.insert("source".into(), json!(row.map(|r| cut(&r.source))));
    facts.insert("remote_file".into(), json!(row.map(|r| cut(&r.item))));
    facts.insert("bytes_done".into(), json!(row.map(|r| r.transferred)));
    facts.insert("transfer_attempts".into(), json!(row.map(|r| r.attempts)));
    facts.insert(
        "queue_position".into(),
        json!(row.and_then(|r| r.queue_position)),
    );
    facts
}

/// A slot in full detail: its state, title, current transfer, its wait for Navidrome and, for a
/// slot that did not fail, a failure its row carries as `warning`.
fn slot_document(view: &JobView, slot: &SlotView, wanted: &DownloadQuery, now: &ReadNow) -> Value {
    let row = shown_row(slot);
    let title = match view.job.kind {
        JobKind::Track => view.job.wanted.tracks.first().map(|t| cut(&t.title)),
        JobKind::Album => picked_title(slot).map(cut),
    };
    let warning = match row.and_then(|r| r.failure.as_ref()) {
        Some(f) if slot.state != JobState::Failed => error_value(f),
        _ => Value::Null,
    };
    json!({
        "slot": slot.slot,
        "state": slot.state.as_str(),
        "title": title,
        "current_attempt": transfer_document(slot, wanted, now.live.get(&slot.slot)),
        "index": index_document(slot),
        "warning": warning,
    })
}

/// The title of the file the slot's pick took (`picked_file`).
fn picked_title(slot: &SlotView) -> Option<&str> {
    picked_file(slot).map(|c| c.title.as_str())
}

/// The slot's current transfer: its source and file, how far it came, the backend's answers
/// read now, and the file's facts. Null for a slot with neither a row nor an open attempt.
fn transfer_document(slot: &SlotView, wanted: &DownloadQuery, live: Option<&LiveFacts>) -> Value {
    let row = shown_row(slot);
    let attempt = current_attempt(slot);
    if row.is_none() && attempt.is_none() {
        return Value::Null;
    }
    let mut doc = file_fields(attempt.and_then(|a| a.candidate.as_ref()), wanted);
    doc.insert("attempt".into(), json!(attempt.map(|a| a.n)));
    let source = match row {
        Some(row) => Some(cut(&row.source)),
        None => attempt.and_then(|a| a.source.as_deref()).map(cut),
    };
    doc.insert("source".into(), json!(source));
    if let Some(row) = row {
        doc.insert("remote_file".into(), json!(cut(&row.item)));
        if row.size > 0 {
            doc.insert("size_bytes".into(), json!(row.size));
        }
    }
    doc.insert("bytes_done".into(), json!(row.map(|r| r.transferred)));
    doc.insert("speed_bytes_s".into(), json!(row.map(|r| r.speed)));
    let (position, position_failure) = match live.and_then(|l| l.queue_position.as_ref()) {
        Some(Ok(position)) => (json!(position), Value::Null),
        Some(Err(f)) => (Value::Null, error_value(f)),
        None => (json!(row.and_then(|r| r.queue_position)), Value::Null),
    };
    doc.insert("queue_position".into(), position);
    doc.insert("queue_position_failure".into(), position_failure);
    let (presence, presence_failure) = match live.map(|l| &l.presence) {
        Some(Ok(presence)) => (json!(presence), Value::Null),
        Some(Err(f)) => (Value::Null, error_value(f)),
        None => (Value::Null, Value::Null),
    };
    doc.insert("source_presence".into(), presence);
    doc.insert("presence_failure".into(), presence_failure);
    let elapsed = attempt.and_then(|a| {
        let end = a.ended_at.unwrap_or_else(|| chrono::Utc::now().timestamp());
        a.started_at.map(|start| end - start)
    });
    doc.insert("elapsed_s".into(), json!(elapsed));
    doc.insert("transfer_attempts".into(), json!(row.map(|r| r.attempts)));
    Value::Object(doc)
}

/// For a slot waiting for Navidrome, or failed waiting, what its wait knows (design 11.7): when
/// the import ended, the library it is in, whether a scan was asked for and Navidrome's last
/// scan when the files were last looked up. Null otherwise.
fn index_document(slot: &SlotView) -> Value {
    let failed = matches!(
        slot.row.as_ref().and_then(|r| r.index.as_ref()),
        Some(IndexState::Failed(_))
    );
    match &slot.index_wait {
        Some(wait) if slot.state == JobState::Indexing || failed => json!({
            "import_finished_at": wait.since,
            "library_path": cut(&wait.library.display().to_string()),
            "scan_requested": wait.baseline.is_some(),
            "last_scan_seen": wait.checked,
        }),
        _ => Value::Null,
    }
}

/// The job's import results, each once with the slots it is of, in slot order.
pub(crate) fn results_of(view: &JobView) -> Vec<(&SlotResult, Vec<u32>)> {
    let mut results: Vec<(&SlotResult, Vec<u32>)> = Vec::new();
    for slot in &view.slots {
        let Some(result) = &slot.result else { continue };
        match results.iter_mut().find(|(of, _)| *of == result) {
            Some((_, slots)) => slots.push(slot.slot),
            None => results.push((result, vec![slot.slot])),
        }
    }
    results
}

/// An album job's wanted titles that no slot took, by `normalize_for_matching`; None until its
/// tracklist was read.
fn missing_titles(view: &JobView) -> Option<Vec<String>> {
    let tracks = &view.search.as_ref()?.query.tracks;
    if view.job.kind != JobKind::Album || tracks.is_empty() {
        return None;
    }
    let taken: Vec<String> = (view.slots.iter())
        .filter_map(picked_title)
        .map(normalize_for_matching)
        .collect();
    Some(
        (tracks.iter())
            .filter(|t| !taken.contains(&normalize_for_matching(&t.title)))
            .map(|t| cut(&t.title))
            .collect(),
    )
}

/// One import's result: what beets applied, and what Navidrome lists for it; or the library's
/// own copies beets found, with the songs Navidrome lists for them as read now.
fn result_document(
    view: &JobView,
    result: &SlotResult,
    slots: &[u32],
    missing: &Option<Vec<String>>,
    now: &ReadNow,
) -> Value {
    match result {
        SlotResult::Imported {
            applied,
            untagged,
            listed,
        } => {
            let job = &view.job;
            let matches_wanted = match (job.kind, &job.wanted.album, job.wanted.tracks.first()) {
                (JobKind::Album, Some(album), _) => {
                    let tracks = (view.search.as_ref())
                        .map(|s| s.query.tracks.as_slice())
                        .filter(|tracks| !tracks.is_empty());
                    album_match(album, tracks, applied, missing)
                }
                (JobKind::Track, _, Some(track)) => match applied.first() {
                    Some(item) => track_match(track, item, listed.as_ref().and_then(|l| l.first())),
                    None => Value::Null,
                },
                _ => Value::Null,
            };
            json!({
                "kind": "imported",
                "slots": slots,
                "applied": applied.iter().map(item_document).collect::<Vec<_>>(),
                "untagged": untagged,
                "listed": listed,
                "matches_wanted": matches_wanted,
            })
        }
        SlotResult::AlreadyInLibrary { existing } => {
            let (songs, failure) = match slots.first().and_then(|s| now.library.get(s)) {
                Some(Ok(songs)) => (json!(songs), Value::Null),
                Some(Err(f)) => (Value::Null, error_value(f)),
                None => (Value::Null, Value::Null),
            };
            json!({
                "kind": "already_in_library",
                "slots": slots,
                "existing": existing.iter().map(item_document).collect::<Vec<_>>(),
                "library_songs": songs,
                "library_songs_failure": failure,
            })
        }
    }
}

/// An item beets imported or found: its ids, tags, how far the applied match was from the files
/// (0 an exact match), and its path in the library.
fn item_document(item: &ImportedItem) -> Value {
    json!({
        "beets_id": item.beets_id,
        "recording_mbid": item.recording_mbid,
        "release_mbid": item.release_mbid,
        "artist": cut(&item.artist),
        "title": cut(&item.title),
        "album": cut(&item.album),
        "distance": item.match_per_mille.map(|pm| f64::from(1000 - pm.min(1000)) / 1000.0),
        "path": cut(&item.path.display().to_string()),
    })
}

/// Two MBIDs: equal (ignoring case), differs, or unknown when either is missing.
fn compared(wanted: Option<&str>, got: Option<&str>) -> &'static str {
    match (wanted, got) {
        (Some(wanted), Some(got)) if wanted.eq_ignore_ascii_case(got) => "equal",
        (Some(_), Some(_)) => "differs",
        _ => "unknown",
    }
}

/// Two names by `normalize_for_matching`.
fn same_text(wanted: &str, got: &str) -> &'static str {
    match normalize_for_matching(wanted) == normalize_for_matching(got) {
        true => "equal",
        false => "differs",
    }
}

/// How the item beets applied for a track job matches the wanted `track`, its length taken from
/// the song Navidrome lists for it.
fn track_match(track: &Track, applied: &ImportedItem, listed: Option<&SongFacts>) -> Value {
    let delta = (listed.and_then(|song| song.duration_s))
        .zip(track.duration_secs())
        .map(|(listed, wanted)| listed.abs_diff(wanted));
    json!({
        "recording_mbid": compared(track.mbid.as_deref(), applied.recording_mbid.as_deref()),
        "artist": same_text(&track.artist, &applied.artist),
        "title": same_text(&track.title, &applied.title),
        "duration_delta_s": delta,
    })
}

/// How an album job's import matches the wanted `album`: its release against every applied
/// item's, the wanted tracks (`tracks`, the tracklist read) and those imported, and the titles
/// no slot took.
fn album_match(
    album: &Album,
    tracks: Option<&[Track]>,
    applied: &[ImportedItem],
    missing: &Option<Vec<String>>,
) -> Value {
    let releases: Vec<&str> = applied
        .iter()
        .filter_map(|item| item.release_mbid.as_deref())
        .collect();
    let release = match (album.mbid.as_deref(), releases.is_empty()) {
        (Some(wanted), false) if releases.iter().all(|r| r.eq_ignore_ascii_case(wanted)) => "equal",
        (Some(_), false) => "differs",
        _ => "unknown",
    };
    json!({
        "release_mbid": release,
        "tracks_wanted": tracks.map(<[Track]>::len),
        "tracks_imported": applied.len(),
        "missing_titles": missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::jobs::retry::{retry, RetryAnswer};
    use crate::core::jobs::runner::Acquire;
    use crate::core::jobs::test_support::{
        auto_options, done_track_job, failed_track_job, mcp_album_job, mcp_track_job, search_of,
        test_db_with_alice, view_of,
    };
    use crate::core::jobs::{untried, RetryMode};
    use crate::core::test_ctx;
    use crate::server_fns::download::test_support::{group, item, track};
    use shared::download::FileFacts;

    #[tokio::test]
    async fn a_failed_job_carries_its_failure_its_facts_and_what_was_tried() {
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
        let job = failed_track_job(&acq).await;
        let view = view_of(&acq, &job).await;
        let now = chrono::Utc::now().timestamp();
        let untried = untried(&acq.db, &view, now).await.unwrap();
        let doc = job_document(&view, &untried, Detail::Summary, &ReadNow::default(), false);
        assert_eq!(doc["state"], "failed");
        let failure = &doc["failure"];
        assert_eq!(
            (
                failure["code"].as_str(),
                failure["message"].as_str(),
                failure["cause"]["raw"].as_str()
            ),
            (
                Some("peer_offline"),
                Some("User carol_shares appears to be offline"),
                Some("User carol_shares appears to be offline")
            ),
            "{doc}"
        );
        let facts = &failure["facts"];
        assert_eq!(
            (
                facts["job_id"].as_str(),
                facts["attempt"].as_u64(),
                facts["backend"].as_str(),
                facts["source"].as_str(),
                facts["remote_file"].as_str(),
                facts["size_bytes"].as_u64()
            ),
            (
                Some(job.as_str()),
                Some(2),
                Some("slskd"),
                Some("carol_shares"),
                Some("c/03 - O Holy Night.flac"),
                Some(27_213_250)
            )
        );
        let tried = failure["tried"].as_array().unwrap();
        assert_eq!(tried.len(), 1, "{failure}");
        assert_eq!(
            (
                tried[0]["attempt"].as_u64(),
                tried[0]["source"].as_str(),
                tried[0]["code"].as_str(),
                tried[0]["message"].as_str(),
                tried[0]["remote_file"].as_str()
            ),
            (
                Some(1),
                Some("peer_a"),
                Some("transfer_rejected_not_shared"),
                Some("Transfer rejected: File not shared."),
                Some("a/03 - O Holy Night.flac")
            )
        );
        // The source its search found and it did not try, another search, or let it go; summary
        // detail stops at the failure.
        let offered: Vec<(&str, &str)> = (failure["next"].as_array().unwrap().iter())
            .map(|n| {
                let mode = n["args"]["mode"].as_str().unwrap_or("");
                (n["tool"].as_str().unwrap(), mode)
            })
            .collect();
        assert_eq!(
            offered,
            vec![
                ("retry_job", "next_source"),
                ("retry_job", "new_search"),
                ("dismiss_job", "")
            ]
        );
        assert_eq!(
            failure["next"][0]["best_candidate"]["source"], "peer_b",
            "{failure}"
        );
        assert!(doc.get("slots").is_none() && doc.get("next").is_none());

        let full = job_document(&view, &untried, Detail::Full, &ReadNow::default(), false);
        assert_eq!(full["tried"], failure["tried"]);
        let slot = &full["slots"][0];
        assert_eq!(
            (
                slot["state"].as_str(),
                slot["title"].as_str(),
                &slot["warning"]
            ),
            (Some("failed"), Some("O Holy Night"), &Value::Null)
        );
        let events: Vec<&str> = full["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["event"].as_str().unwrap())
            .collect();
        assert_eq!(events.last(), Some(&"failed"), "{full}");
        assert_eq!(full["timeline_omitted"], 0);
    }

    #[tokio::test]
    async fn a_rejected_result_says_how_to_allow_deleting_it() {
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
        let job = done_track_job(&acq).await;
        let alice = test_ctx("alice");
        let rejected = retry(&acq, &alice, &job, RetryMode::RejectResult, None);
        assert!(matches!(rejected.await.unwrap(), RetryAnswer::Applied(_)));
        let view = view_of(&acq, &job).await;
        let untried = untried(&acq.db, &view, chrono::Utc::now().timestamp())
            .await
            .unwrap();
        let doc = job_document(&view, &untried, Detail::Full, &ReadNow::default(), false);
        assert_eq!(doc["failure"]["code"], "result_rejected");
        // The file stays in the library until deleted, which this app may not do.
        assert_eq!(
            doc["failure"]["user_action"],
            json!({"code": "enable_deletion", "what": "Allow deleting music for this app",
                "where": "Settings > Users > Connected apps"})
        );
        let allowed = job_document(&view, &untried, Detail::Full, &ReadNow::default(), true);
        assert!(allowed["failure"]["user_action"].is_null());
    }

    #[tokio::test]
    async fn a_jobs_search_facts_count_what_the_jobs_own_filters_dropped() {
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
        let lossless = crate::ledger::JobOptions {
            filters: crate::ledger::SourceFilters {
                lossless_only: true,
                ..Default::default()
            },
            ..auto_options()
        };
        let job = mcp_track_job(&acq, "Marrow Choir", "O Holy Night", lossless).await;
        // Its search was stored without filters, as find_sources stores one with none given.
        let search_id = search_of(&acq, &job).await;
        let user = crate::core::test_ctx("alice").id();
        acq.ledger
            .record_search(crate::ledger::NewSearch {
                id: search_id.clone(),
                user,
                backend: Some("slskd".into()),
                query: DownloadQuery::new(vec![track("Marrow Choir", "O Holy Night")]),
                spec: Some(StoredSpec::default()),
            })
            .await
            .unwrap();
        let mp3 = DownloadableItem {
            facts: FileFacts {
                extension: Some("mp3".into()),
                ..FileFacts::default()
            },
            ..item("peer_b", "O Holy Night", "b/03.mp3")
        };
        let found = vec![group("peer_b", vec![mp3], 0.85)];
        acq.ledger
            .search_ended(&search_id, Ok((found, Some(SearchStats::default()))))
            .await
            .unwrap();
        crate::core::jobs::runner::drive(&acq, &crate::core::test_ctx("alice"), &job).await;
        acq.ledger.settled().await;
        let view = view_of(&acq, &job).await;
        let doc = job_document(
            &view,
            &Untried::default(),
            Detail::Summary,
            &ReadNow::default(),
            false,
        );
        assert_eq!(doc["failure"]["code"], "search_all_filtered", "{doc}");
        assert_eq!(
            doc["failure"]["facts"]["dropped_by_filter"]["not_lossless"],
            1
        );
    }

    #[tokio::test]
    async fn a_large_albums_full_document_keeps_within_its_bound_and_counts_what_it_left_out() {
        let (acq, _) = Acquire::for_test(test_db_with_alice().await);
        let mbid = "6a09041b-0f79-3278-88d9-0552dd28d37f";
        let job = mcp_album_job(&acq, "Kowloon", "Come Over", mbid).await;
        let search_id = search_of(&acq, &job).await;
        // A box set of 100 tracks whose names are as long as a peer may make them.
        let long = "x".repeat(480);
        let items = (0..100)
            .map(|i| {
                let file = format!("a\\{long}\\{i:03}.flac");
                (i, item("peer_a", &format!("{long} {i}"), &file))
            })
            .collect();
        let pick = crate::ledger::Transition::Pick { items, search_id };
        let user = crate::core::test_ctx("alice").id();
        acq.ledger.transition(&user, &job, pick).await.unwrap();
        let view = view_of(&acq, &job).await;
        let doc = job_document(
            &view,
            &Untried::default(),
            Detail::Full,
            &ReadNow::default(),
            false,
        );
        let Value::Object(fields) = &doc else {
            panic!("{doc}")
        };
        assert!(
            length(fields) <= JOB_DOCUMENT_MAX_CHARS,
            "{}",
            length(fields)
        );
        let shown = doc["slots"].as_array().unwrap();
        assert!(shown.len() < 100);
        assert_eq!(
            shown.len() as u64 + doc["slots_omitted"].as_u64().unwrap(),
            100
        );
        for (i, slot) in shown.iter().enumerate() {
            assert_eq!(slot["slot"], i);
        }
    }

    #[test]
    fn matches_wanted_compares_the_applied_recording() {
        let wanted = Track {
            mbid: Some("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into()),
            duration: Some("4:23".into()),
            ..track("Marrow Choir", "O Holy Night")
        };
        let applied = ImportedItem {
            path: "Marrow Choir/Winter Songs/03 - O Holy Night.flac".into(),
            recording_mbid: Some("6b5e1a0c-9f0e-4c57-8a51-0d7a2c3c1f42".into()),
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            beets_id: Some(412),
            release_mbid: None,
            album: "Winter Songs".into(),
            match_per_mille: Some(912),
        };
        let listed = SongFacts {
            song_id: "3fW9cK2LmQ8rT5vX1zB7nD".into(),
            title: "O Holy Night".into(),
            artist: Some("Marrow Choir".into()),
            album: Some("Winter Songs".into()),
            album_id: None,
            year: None,
            track: Some(3),
            duration_s: Some(265),
            format: Some("flac".into()),
            bitrate_kbps: Some(912),
            size_bytes: Some(28_734_112),
            path: None,
            recording_mbid: Some("6b5e1a0c-9f0e-4c57-8a51-0d7a2c3c1f42".into()),
            rating: 0,
            starred: false,
            starred_at: None,
            play_count: 0,
        };
        assert_eq!(
            track_match(&wanted, &applied, Some(&listed)),
            json!({"recording_mbid": "differs", "artist": "equal", "title": "equal",
                "duration_delta_s": 2})
        );
        assert_eq!(
            track_match(&track("Marrow Choir", "O Holy Night"), &applied, None),
            json!({"recording_mbid": "unknown", "artist": "equal", "title": "equal",
                "duration_delta_s": null})
        );
        assert_eq!(item_document(&applied)["distance"], json!(0.088));
    }

    #[test]
    fn a_folder_is_the_common_parent_of_its_files() {
        assert_eq!(
            common_parent(["a\\Come Over\\CD1\\07.flac", "a\\Come Over\\CD2\\01.flac"].into_iter()),
            "a\\Come Over"
        );
        assert_eq!(
            common_parent(["music/Kowloon/07.flac", "music/Kowloon/08.flac"].into_iter()),
            "music/Kowloon"
        );
        // A folder whose name starts like another's is not its parent.
        assert_eq!(
            common_parent(["a\\Come\\07.flac", "a\\Come Over\\08.flac"].into_iter()),
            "a"
        );
        assert_eq!(common_parent(["07.flac"].into_iter()), "");
        assert_eq!(common_parent(["07.flac", "x\\08.flac"].into_iter()), "");
    }

    #[test]
    fn a_candidate_of_a_track_search_is_its_file_with_its_peer_and_failures() {
        let mut wanted = DownloadQuery::new(vec![track("Kowloon", "Wake Up")]);
        wanted.tracks[0].duration = Some("3:30".into());
        let file = DownloadableItem {
            duration: Some(205),
            facts: FileFacts {
                extension: Some("flac".into()),
                folder_audio_files: Some(11),
                source_queue_length: Some(4),
                // What the peer's client wrote, as long as it likes.
                backend_facts: Map::from_iter([(
                    "declared_extension".to_string(),
                    json!("f".repeat(900)),
                )]),
                ..FileFacts::default()
            },
            ..item("peer_a", "Wake Up", &format!("a\\{}.flac", "x".repeat(600)))
        };
        let candidate = Candidate {
            id: "s1:2".into(),
            group: group("peer_a", vec![file], 0.8),
            recent_failures: 3,
        };
        let doc = candidate_document(&candidate, &wanted);
        assert_eq!(
            (
                doc["download_scope"].as_str(),
                doc["backend"].as_str(),
                doc["duration_delta_s"].as_u64(),
                doc["folder_track_count"].as_u64(),
                doc["peer_queue_length"].as_u64()
            ),
            (Some("file"), Some("slskd"), Some(5), Some(11), Some(4))
        );
        assert_eq!(
            (
                doc["peer_recent_failures"].as_u64(),
                doc["peer_flagged"].as_bool()
            ),
            (Some(3), Some(true))
        );
        assert!(doc["remote_file"].as_str().unwrap().chars().count() <= TEXT_MAX_CHARS);
        let declared = doc["backend_facts"]["declared_extension"].as_str().unwrap();
        assert!(declared.chars().count() <= TEXT_MAX_CHARS);
        assert!(doc.get("files").is_none() && doc.get("tracks_found").is_none());
    }

    #[test]
    fn a_search_that_did_not_finish_reports_no_counts() {
        let facts = search_facts(None, &StoredSpec::default(), &DroppedByFilter::default());
        assert!(facts.values().all(Value::is_null), "{facts:?}");
        let stats = SearchStats {
            query_text: "Kowloon Wake Up".into(),
            not_audio: 3,
            below_floor: 7,
            ..SearchStats::default()
        };
        let kept = StoredSpec {
            keep_below_floor: true,
            ..StoredSpec::default()
        };
        let dropped = DroppedByFilter {
            format: 2,
            ..DroppedByFilter::default()
        };
        assert_eq!(
            Value::Object(search_facts(Some(&stats), &kept, &dropped)),
            json!({"query_text_sent": "Kowloon Wake Up", "response_count": 0, "file_count": 0,
                "locked_file_count": 0, "dropped_not_audio": 3, "dropped_below_floor": 0,
                "dropped_by_filter": {"format": 2, "bitrate": 0, "not_lossless": 0,
                    "queue_length": 0, "excluded": 0}})
        );
        assert_eq!(
            search_facts(Some(&stats), &StoredSpec::default(), &dropped)["dropped_below_floor"],
            7
        );
    }
}
