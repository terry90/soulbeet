//! MCP acquisition: what a request asks for, and the seam the tools and the job runner act
//! through.

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::library::Reference;
use crate::ledger::store::{self, AttemptRecord, JobRecord, StoredSearch, TimelineEntry};
use crate::ledger::{IndexWait, JobKind, JobState, Ledger, SlotResult};
use shared::download::{DownloadProgress, DownloadQuery, IndexState, JobSlot};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::metadata::{Album, Track};
use sqlx::SqlitePool;
use std::collections::HashMap;

pub mod runner;
#[cfg(test)]
pub(crate) mod test_support;

/// An album asked for by its MusicBrainz release (D1).
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumRef {
    pub artist: String,
    pub title: String,
    pub release_mbid: String,
}

/// What a request asks for (D1): a track as `find_in_library` describes one, or an album.
#[derive(Debug, Clone, PartialEq)]
pub enum Wanted {
    Track(Reference),
    Album(AlbumRef),
}

impl Wanted {
    pub fn kind(&self) -> JobKind {
        match self {
            Wanted::Track(_) => JobKind::Track,
            Wanted::Album(_) => JobKind::Album,
        }
    }

    /// The query a job and its search store: one track (its duration as "m:ss"), or an album
    /// whose id and mbid are the release MBID, its tracks empty until hydrated.
    pub fn query(&self) -> DownloadQuery {
        match self {
            Wanted::Track(r) => DownloadQuery::new(vec![Track {
                id: String::new(),
                title: r.title.clone(),
                artist: r.artist.clone(),
                album_id: None,
                album_title: r.album.clone(),
                release_date: None,
                duration: r.duration_s.map(|s| format!("{}:{:02}", s / 60, s % 60)),
                mbid: r.recording_mbid.clone(),
                release_mbid: None,
            }]),
            Wanted::Album(a) => DownloadQuery::new(vec![]).album(Album {
                id: a.release_mbid.clone(),
                title: a.title.clone(),
                artist: a.artist.clone(),
                release_date: None,
                mbid: Some(a.release_mbid.clone()),
                cover_url: None,
                release_types: None,
            }),
        }
    }

    /// What it is called in labels: "{artist} - {title}".
    pub fn label(&self) -> String {
        let (artist, title) = match self {
            Wanted::Track(r) => (&r.artist, &r.title),
            Wanted::Album(a) => (&a.artist, &a.title),
        };
        format!("{artist} - {title}")
    }
}

/// One slot of a job as the job tools show it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SlotView {
    pub slot: u32,
    pub state: JobState,
    /// The last row the ledger stored, its `job` set.
    pub row: Option<DownloadProgress>,
    /// The last row sent while the slot is not terminal, kept in memory only.
    pub live: Option<DownloadProgress>,
    /// Oldest first.
    pub attempts: Vec<AttemptRecord>,
    pub result: Option<SlotResult>,
    /// Its import's wait for Navidrome, kept once the wait ended.
    pub index_wait: Option<IndexWait>,
}

/// A job with everything the ledger keeps of it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobView {
    pub job: JobRecord,
    pub slots: Vec<SlotView>,
    pub timeline: Vec<TimelineEntry>,
    /// The search the job reads its sources from, while it is kept.
    pub search: Option<StoredSearch>,
}

/// Which jobs a tool shows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JobSelect {
    Ids(Vec<String>),
    /// The user's jobs of `batch_id` when given, else all of them; with `states` not empty only
    /// those in one of them.
    Filter {
        batch_id: Option<String>,
        states: Vec<JobState>,
    },
}

/// `ctx`'s jobs as `select` says, in creation order; an id that is not one of `ctx`'s jobs is
/// left out (get_jobs reports it).
pub(crate) async fn jobs_view(
    db: &SqlitePool,
    ledger: &Ledger,
    ctx: &UserCtx,
    select: &JobSelect,
) -> Result<Vec<JobView>, Failure> {
    let failed = |e| database_failure(Stage::Input, e);
    let (ids, batch_id, states) = match select {
        JobSelect::Ids(ids) => (Some(ids.as_slice()), None, &[][..]),
        JobSelect::Filter { batch_id, states } => (None, batch_id.as_deref(), states.as_slice()),
    };
    let jobs = store::jobs_of(db, &ctx.id(), ids, batch_id, states)
        .await
        .map_err(failed)?;
    let mut views = Vec::with_capacity(jobs.len());
    for job in jobs {
        let mut attempts: HashMap<u32, Vec<AttemptRecord>> = store::attempts_of(db, &job.id)
            .await
            .map_err(failed)?
            .into_iter()
            .collect();
        let slots = store::tracks_of(db, &job.id)
            .await
            .map_err(failed)?
            .into_iter()
            .map(|track| SlotView {
                live: ledger.live_row(&JobSlot {
                    job: job.id.clone(),
                    slot: track.slot,
                }),
                attempts: attempts.remove(&track.slot).unwrap_or_default(),
                slot: track.slot,
                state: track.state,
                row: track.row,
                result: track.result,
                index_wait: track.index_wait,
            })
            .collect();
        let timeline = store::timeline_of(db, &job.id).await.map_err(failed)?;
        let search = match &job.search_id {
            Some(id) => store::search_of(db, id, &ctx.id()).await.map_err(failed)?,
            None => None,
        };
        views.push(JobView {
            job,
            slots,
            timeline,
            search,
        });
    }
    Ok(views)
}

/// What a failed slot failed with: its index failure, else its row's failure.
pub(crate) fn slot_failure(slot: &SlotView) -> Option<&Failure> {
    let row = slot.row.as_ref()?;
    match &row.index {
        Some(IndexState::Failed(f)) => Some(f),
        _ => row.failure.as_ref(),
    }
}

/// The failed slot whose failure is the job's: the first in slot order that carries one.
pub(crate) fn failed_slot(view: &JobView) -> Option<&SlotView> {
    (view.slots.iter()).find(|slot| slot.state == JobState::Failed && slot_failure(slot).is_some())
}

/// The job's failure: `failed_slot`'s, else the failure of a job that failed before it had a
/// slot.
pub(crate) fn job_failure(view: &JobView) -> Option<Failure> {
    match failed_slot(view) {
        Some(slot) => slot_failure(slot).cloned(),
        None => view.job.failure.clone(),
    }
}

/// The job does not exist or is another user's.
pub(crate) fn job_not_found() -> Failure {
    Failure::new(
        FailureCode::UnknownHandle,
        Stage::Input,
        RetryAdvice::NotRetryable,
        "Job not found",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_becomes_the_query_a_search_scores_against() {
        let track = Wanted::Track(Reference {
            artist: "Marrow Choir".into(),
            title: "O Holy Night".into(),
            album: Some("Winter Songs".into()),
            recording_mbid: Some("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8".into()),
            duration_s: Some(263),
        });
        let query = track.query();
        assert_eq!(query.album, None);
        let t = &query.tracks[0];
        assert_eq!(
            (
                t.duration.as_deref(),
                t.duration_secs(),
                t.album_title.as_deref(),
                t.mbid.as_deref()
            ),
            (
                Some("4:23"),
                Some(263),
                Some("Winter Songs"),
                Some("4d04e9b5-ecd1-4ef3-bae0-5c98492af1c8")
            )
        );
        assert_eq!(
            (track.kind(), track.label()),
            (JobKind::Track, "Marrow Choir - O Holy Night".to_string())
        );

        let album = Wanted::Album(AlbumRef {
            artist: "Kowloon".into(),
            title: "Come Over".into(),
            release_mbid: "6a09041b-0f79-3278-88d9-0552dd28d37f".into(),
        });
        let query = album.query();
        let a = query.album.as_ref().unwrap();
        assert_eq!(
            (a.id.as_str(), a.mbid.as_deref(), query.tracks.len()),
            (
                "6a09041b-0f79-3278-88d9-0552dd28d37f",
                Some("6a09041b-0f79-3278-88d9-0552dd28d37f"),
                0
            )
        );
        assert_eq!(
            (album.kind(), album.label()),
            (JobKind::Album, "Kowloon - Come Over".to_string())
        );
    }
}
