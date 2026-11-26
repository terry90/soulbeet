//! Peer failover for downloads.
//!
//! A peer can refuse a file when it is asked for it, or accept it and then
//! never deliver a byte. The search that picked it already returned other
//! peers offering the same track, so the track should move to the next-best
//! source rather than fail. This module owns those alternates and the
//! per-track attempt budget; the request that enqueues a file and the waiters
//! (`DownloadMonitor`, discovery's poll loop) decide *when* a source has
//! definitively failed.

use shared::download::{
    DownloadProgress, DownloadableGroup, DownloadableItem, EnqueueStatus, JobSlot, QueuedDownload,
};
use shared::failure::{Failure, Stage};
use soulbeet::DownloadBackend;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use tracing::{info, warn};

use super::enqueue::{enqueue_claimed, held_failure, not_enqueued_failure, transfer_key};
use crate::core::database_failure;
use crate::ledger::{JobEvent, JobState, Ledger};
use crate::transfer_registry::{BatchRef, TransferRegistry};

/// Sources tried per track before it is declared failed: the peer picked at
/// search time plus three alternates. slskd retries each peer 3 times on its
/// own (~40s) before reporting failure, so this caps a dead track at roughly
/// 2.5 minutes.
pub const MAX_SOURCES_PER_TRACK: usize = 4;

/// An alternate `enqueue_next` took from a slot and could not enqueue.
pub struct RefusedAlternate {
    pub candidate: DownloadableItem,
    pub failure: Failure,
}

/// An alternate a slot's failover sent, and the backend request it went in.
/// An answer that refused it, or never came, does not prove the backend made
/// no transfer of it: one it made is listed with that request.
#[derive(Clone, Debug, PartialEq)]
pub struct Sent {
    pub candidate: DownloadableItem,
    pub request: String,
}

impl Sent {
    /// Whether `dl` is the transfer this send made: the same file from the
    /// same source, made in its request.
    pub fn made(&self, dl: &DownloadProgress) -> bool {
        dl.source == self.candidate.source
            && dl.item == self.candidate.id
            && dl.request.as_deref() == Some(self.request.as_str())
    }
}

/// What `enqueue_next` did with a slot.
pub struct NextSource {
    /// The accepted alternate and its download. `None` when the slot is out
    /// of sources or budget.
    pub queued: Option<(DownloadableItem, QueuedDownload)>,
    /// Every alternate refused before it, in the order tried.
    pub refused: Vec<RefusedAlternate>,
}

/// Alternates and attempt budget for one track.
struct Slot {
    alternates: VecDeque<DownloadableItem>,
    used: usize,
    max: usize,
}

/// Ranked alternate sources for every track in a batch, indexed by slot.
pub struct SourcePool {
    slots: Vec<Slot>,
}

impl SourcePool {
    /// Alternates for `tracks`, slot N being the alternates of `tracks[N]`,
    /// each slot having used its first of `max_sources` sources. A `None`
    /// entry keeps a slot aligned when its track cannot be recovered, and
    /// that slot simply gets no alternates.
    ///
    /// `rest` must already be in descending score order, which is how
    /// `auto_download` sorts it.
    pub fn from_tracks(
        tracks: &[Option<DownloadableItem>],
        rest: &[DownloadableGroup],
        max_sources: usize,
    ) -> Self {
        let mut pool = Self { slots: Vec::new() };
        for track in tracks {
            let alternates = match track {
                Some(track) => alternates_for(track, rest),
                None => Vec::new(),
            };
            pool.push_slot(alternates, 1, max_sources);
        }
        pool
    }

    /// Adds a slot after the last, which used `used` of its `max_sources`
    /// sources, its transfer being the last of them.
    pub fn push_slot(
        &mut self,
        alternates: Vec<DownloadableItem>,
        used: usize,
        max_sources: usize,
    ) {
        self.slots.push(Slot {
            alternates: alternates.into(),
            used,
            max: max_sources,
        });
    }

    /// This slot's untried alternates, the sources it used and its cap, as
    /// `resumed` and `push_slot` take them; nothing, of the default cap, for
    /// a slot that does not exist.
    pub fn sources_of(&self, slot: usize) -> (Vec<DownloadableItem>, usize, usize) {
        match self.slots.get(slot) {
            Some(s) => (s.alternates.iter().cloned().collect(), s.used, s.max),
            None => (Vec::new(), 0, MAX_SOURCES_PER_TRACK),
        }
    }

    /// A pool resumed after a restart: per slot its untried alternates, the
    /// sources it used and its own cap.
    pub fn resumed(slots: Vec<(Vec<DownloadableItem>, usize, usize)>) -> Self {
        Self {
            slots: slots
                .into_iter()
                .map(|(alternates, used, max)| Slot {
                    alternates: alternates.into(),
                    used,
                    max,
                })
                .collect(),
        }
    }

    /// Single track, N ranked sources. `items[0]` is the source already
    /// enqueued; the rest are its alternates.
    pub fn single(items: Vec<DownloadableItem>) -> Self {
        let alternates = items.into_iter().skip(1).collect();
        Self {
            slots: vec![Slot {
                alternates,
                used: 1,
                max: MAX_SOURCES_PER_TRACK,
            }],
        }
    }

    /// How many slots the pool has.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// How many sources this slot has consumed, including the original.
    pub fn attempts_used(&self, slot: usize) -> usize {
        self.slots.get(slot).map(|s| s.used).unwrap_or(0)
    }

    /// The most sources this slot may use, or the default cap for a slot that
    /// does not exist.
    pub fn max_sources(&self, slot: usize) -> usize {
        self.slots
            .get(slot)
            .map_or(MAX_SOURCES_PER_TRACK, |s| s.max)
    }

    /// Whether `take_next` would give this slot an alternate: the slot exists, has budget left
    /// and an alternate.
    pub fn has_next(&self, slot: usize) -> bool {
        self.slots
            .get(slot)
            .is_some_and(|s| s.used < s.max && !s.alternates.is_empty())
    }

    /// Next alternate for this slot, or `None` when `has_next` says there is none.
    pub fn take_next(&mut self, slot: usize) -> Option<DownloadableItem> {
        if !self.has_next(slot) {
            return None;
        }
        let slot = self.slots.get_mut(slot)?;
        let item = slot.alternates.pop_front()?;
        slot.used += 1;
        Some(item)
    }

    /// Enqueue this slot's next alternate into `batch`'s folder, walking past
    /// the ones it cannot have (another batch owns the transfer, the peer is
    /// offline, slskd is already fetching it) until one is accepted or the
    /// slot runs dry. The accepted alternate's key stays claimed by `batch`.
    /// A refused alternate still costs the slot a source, so three dead peers
    /// in a row spend the whole budget without a transfer ever being
    /// attempted. Every refused alternate comes back with its failure.
    pub async fn enqueue_next(
        &mut self,
        slot: usize,
        backend: &Arc<dyn DownloadBackend>,
        batch: &BatchRef,
    ) -> NextSource {
        self.enqueue_next_noting(
            slot,
            backend,
            batch,
            |_, _| std::future::ready(Ok(())),
            |_, _| {},
        )
        .await
    }

    /// `enqueue_next`, awaiting `sending` with each alternate and the backend
    /// request it goes in before it is sent to the backend, and calling
    /// `refusing` with each refused alternate and its failure before the next
    /// one is tried. When `sending` fails, that alternate is refused with its
    /// failure and not sent, and no other is tried.
    pub async fn enqueue_next_noting<F, Noted, R>(
        &mut self,
        slot: usize,
        backend: &Arc<dyn DownloadBackend>,
        batch: &BatchRef,
        mut sending: F,
        mut refusing: R,
    ) -> NextSource
    where
        F: FnMut(&DownloadableItem, &str) -> Noted,
        Noted: Future<Output = Result<(), Failure>>,
        R: FnMut(&DownloadableItem, &Failure),
    {
        let mut refused = Vec::new();
        let mut refuse = |candidate: DownloadableItem, failure: Failure| {
            refusing(&candidate, &failure);
            refused.push(RefusedAlternate { candidate, failure });
        };
        while let Some(item) = self.take_next(slot) {
            let key = transfer_key(backend.id(), &item.source, &item.id);
            let registry = TransferRegistry::global();
            let claim = registry.claim(std::slice::from_ref(&key), batch);
            let failure = if claim.claimed.is_empty() {
                held_failure(&claim, batch)
            } else {
                let request = backend.new_request();
                if let Err(failure) = sending(&item, &request).await {
                    registry.release(&key, &batch.owner);
                    refuse(item, failure);
                    break;
                }
                match enqueue_claimed(backend, vec![item.clone()], batch, &request).await {
                    Ok(queued) => {
                        if let Some(ok) =
                            queued.iter().find(|d| d.status == EnqueueStatus::Enqueued)
                        {
                            info!("Failover enqueued {} from {}", ok.item, ok.source);
                            return NextSource {
                                queued: Some((item, ok.clone())),
                                refused,
                            };
                        }
                        not_enqueued_failure(&queued)
                    }
                    Err(e) => e.failure(Stage::Enqueue, e.to_string()),
                }
            };
            refuse(item, failure);
        }
        NextSource {
            queued: None,
            refused,
        }
    }

    /// `enqueue_next` for this slot, which downloads for `job` and whose
    /// source ended with `left` (its row, marked with the batch and `job`,
    /// its failure set), with every step stored in `ledger`: `left` first,
    /// which ends that attempt while the slot is still downloading; then each
    /// alternate and the backend request it goes in, before it is sent, so a
    /// restart knows which peer may hold the slot's transfer and in which
    /// request; then each refused alternate, before the next is tried, so an
    /// accepted one is the newest attempt. An alternate the ledger cannot
    /// store is not sent. Returns what `enqueue_next` did, and every
    /// alternate it sent: the only transfers the slot may adopt besides its
    /// own (`Sent::made`).
    pub async fn fail_over(
        &mut self,
        slot: usize,
        backend: &Arc<dyn DownloadBackend>,
        batch: &BatchRef,
        ledger: &Ledger,
        job: &JobSlot,
        left: DownloadProgress,
    ) -> (NextSource, Vec<Sent>) {
        let filename = left.item.clone();
        ledger.record(JobEvent::Slots {
            state: JobState::Downloading,
            rows: vec![left],
        });
        let mut sends = Vec::new();
        let record = |item: &DownloadableItem, request: &str| {
            sends.push(Sent {
                candidate: item.clone(),
                request: request.to_string(),
            });
            let ledger = ledger.clone();
            let attempt = JobEvent::Attempt {
                slot: job.clone(),
                candidate: Some(Box::new(item.clone())),
                request: Some(request.to_string()),
            };
            async move {
                ledger
                    .record_stored(attempt)
                    .await
                    .map_err(|e| database_failure(Stage::Enqueue, e))
            }
        };
        let refusing = |item: &DownloadableItem, failure: &Failure| {
            warn!(
                "Failover source {} refused {} for {}: {}: {}",
                item.source,
                item.id,
                filename,
                failure.code.as_str(),
                failure.message
            );
            ledger.record(JobEvent::Refused {
                slot: job.clone(),
                candidate: Box::new(item.clone()),
                failure: failure.clone(),
            });
        };
        let next = self
            .enqueue_next_noting(slot, backend, batch, record, refusing)
            .await;
        (next, sends)
    }
}

/// The item of each group of `rest` offering `track` (same normalized title)
/// on `track`'s backend, in `rest`'s order: a slot's sources are all on the
/// backend its batch watches. `rest` must be sorted by descending score, so
/// the best group comes first. Matching is by title, never by filename: the same track
/// has a completely different remote path on each peer. A group offers a title
/// twice only when its album has it twice, one file per track, as two discs
/// can: then the file closest to the track's length is that track's own.
pub fn alternates_for(
    track: &DownloadableItem,
    rest: &[DownloadableGroup],
) -> Vec<DownloadableItem> {
    let wanted = shared::recommendation::normalize_for_matching(&track.title);
    // How far a file's length is from the track's, unknown last.
    let distance = |candidate: &DownloadableItem| {
        let diff = track
            .duration
            .zip(candidate.duration)
            .map(|(t, c)| t.abs_diff(c));
        (diff.is_none(), diff)
    };
    rest.iter()
        .filter_map(|group| {
            group
                .items
                .iter()
                .filter(|candidate| {
                    candidate.backend == track.backend
                        && shared::recommendation::normalize_for_matching(&candidate.title)
                            == wanted
                })
                .min_by_key(|&candidate| distance(candidate))
        })
        .cloned()
        .collect()
}

/// The index in `items` of the item `queued` was enqueued from (same id and
/// source). Matching keys on both: `QueuedDownload::item` is the item's `id`
/// verbatim, both being the remote filename, and the same filename can be
/// offered by more than one peer.
pub fn item_index<'a>(
    queued: &QueuedDownload,
    items: impl IntoIterator<Item = &'a DownloadableItem>,
) -> Option<usize> {
    items
        .into_iter()
        .position(|item| item.id == queued.item && item.source == queued.source)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::server_fns::download::enqueue::{ALREADY_MINE, ANOTHER_FOLDER, NOTHING_ENQUEUED};
    use crate::server_fns::download::test_support::{
        batch, group, item, item_on, key, same_user_into, ScriptedOutcome, StubBackend,
    };
    use crate::transfer_registry::TransferRegistry;
    use shared::download::DownloadableItem;
    use shared::failure::{FailureCode, RetryAdvice};
    use soulbeet::DownloadBackend;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn queued(source: &str, item: &str) -> QueuedDownload {
        QueuedDownload {
            id: item.to_string(),
            source: source.to_string(),
            item: item.to_string(),
            size: 0,
            status: EnqueueStatus::Enqueued,
        }
    }

    /// Three tracks of one album as the search returned them, `dirA` first.
    fn searched() -> Vec<DownloadableItem> {
        vec![
            item("peer_a", "One", "dirA/01.flac"),
            item("peer_a", "Two", "dirB/02.flac"),
            item("peer_a", "Three", "dirB/03.flac"),
        ]
    }

    #[test]
    fn matches_alternates_by_title_across_differing_paths() {
        let tracks = vec![Some(item(
            "ElevatorsForPeople",
            "Wake Up",
            "music\\Kowloon\\Come Over (2021)\\Kowloon - Come Over - 07 - Wake Up - [FLAC 44.1kHz].flac",
        ))];
        let rest = vec![group(
            "Cornflake9026",
            vec![item(
                "Cornflake9026",
                "Wake Up ",
                "music\\Kowloon\\Kowloon - Come Over\\07 - Wake Up.flac",
            )],
            0.9,
        )];

        let mut pool = SourcePool::from_tracks(&tracks, &rest, MAX_SOURCES_PER_TRACK);
        let next = pool.take_next(0).expect("alternate for slot 0");
        assert_eq!(next.source, "Cornflake9026");
    }

    #[test]
    fn alternates_follow_the_order_of_the_remaining_groups() {
        let tracks = vec![Some(item("peer_a", "Wake Up", "a.flac"))];
        let rest = vec![
            group("peer_b", vec![item("peer_b", "Wake Up", "b.flac")], 0.9),
            group("peer_c", vec![item("peer_c", "Wake Up", "c.flac")], 0.8),
            group("peer_d", vec![item("peer_d", "Wake Up", "d.flac")], 0.7),
        ];

        let mut pool = SourcePool::from_tracks(&tracks, &rest, MAX_SOURCES_PER_TRACK);
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_b".into()));
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_c".into()));
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_d".into()));
        assert!(pool.take_next(0).is_none());
    }

    #[test]
    fn a_resumed_pool_keeps_its_budget() {
        // Three sources used: one more is left of MAX_SOURCES_PER_TRACK, not two.
        let mut pool = SourcePool::resumed(vec![(
            vec![
                item("peer_c", "One", "c/01.flac"),
                item("peer_d", "One", "d/01.flac"),
            ],
            3,
            MAX_SOURCES_PER_TRACK,
        )]);
        assert_eq!(pool.take_next(0).map(|i| i.id), Some("c/01.flac".into()));
        assert!(pool.take_next(0).is_none());
    }

    #[test]
    fn item_index_finds_the_item_of_the_same_file_from_the_same_peer() {
        assert_eq!(
            item_index(&queued("peer_a", "dirB/02.flac"), &searched()),
            Some(1)
        );
        assert_eq!(
            item_index(&queued("peer_b", "dirB/02.flac"), &searched()),
            None
        );
    }

    #[test]
    fn from_tracks_slots_follow_the_given_order() {
        // The enqueue order is not the search order: slskd returns its enqueue
        // results grouped by peer, so slot 0 here is
        // the track the alternate group happens to list second. Slots must
        // follow the tracks handed in, never the order found in `rest`.
        let rest = vec![group(
            "peer_b",
            vec![
                item("peer_b", "Come Over", "b-come-over.flac"),
                item("peer_b", "Wake Up", "b-wake-up.flac"),
            ],
            0.9,
        )];
        let tracks = vec![
            Some(item("peer_a", "Wake Up", "a-wake-up.flac")),
            Some(item("peer_a", "Come Over", "a-come-over.flac")),
        ];

        let mut pool = SourcePool::from_tracks(&tracks, &rest, MAX_SOURCES_PER_TRACK);
        assert_eq!(
            pool.take_next(0).map(|i| i.id),
            Some("b-wake-up.flac".to_string())
        );
        assert_eq!(
            pool.take_next(1).map(|i| i.id),
            Some("b-come-over.flac".to_string())
        );
    }

    #[test]
    fn an_unrecoverable_track_keeps_the_slots_after_it_aligned() {
        let rest = vec![group(
            "peer_b",
            vec![
                item("peer_b", "Wake Up", "b-wake-up.flac"),
                item("peer_b", "Come Over", "b-come-over.flac"),
            ],
            0.9,
        )];
        // Slot 1's track could not be matched back to the picked group. It
        // must still take a slot: compacting it away would shift "Come Over"
        // down to slot 1 and hand slot 2's failover the wrong track.
        let tracks = vec![
            Some(item("peer_a", "Wake Up", "a-wake-up.flac")),
            None,
            Some(item("peer_a", "Come Over", "a-come-over.flac")),
        ];

        let mut pool = SourcePool::from_tracks(&tracks, &rest, MAX_SOURCES_PER_TRACK);
        assert_eq!(
            pool.take_next(0).map(|i| i.id),
            Some("b-wake-up.flac".to_string())
        );
        assert!(pool.take_next(1).is_none());
        assert_eq!(
            pool.take_next(2).map(|i| i.id),
            Some("b-come-over.flac".to_string())
        );
    }

    #[test]
    fn ignores_alternates_for_a_different_title() {
        let tracks = vec![Some(item("peer_a", "Wake Up", "a.flac"))];
        let rest = vec![group(
            "peer_b",
            vec![item("peer_b", "Come Over", "b.flac")],
            0.9,
        )];

        let mut pool = SourcePool::from_tracks(&tracks, &rest, MAX_SOURCES_PER_TRACK);
        assert!(pool.take_next(0).is_none());
    }

    #[test]
    fn only_groups_on_the_picks_backend_offer_alternates() {
        let track = item_on("a", "peer_a", "Wake Up", "a.flac");
        let rest = vec![
            group(
                "peer_b",
                vec![item_on("b", "peer_b", "Wake Up", "b.flac")],
                0.9,
            ),
            group(
                "peer_c",
                vec![item_on("a", "peer_c", "Wake Up", "c.flac")],
                0.8,
            ),
        ];
        let offered: Vec<(String, String)> = alternates_for(&track, &rest)
            .into_iter()
            .map(|i| (i.backend, i.source))
            .collect();
        assert_eq!(offered, vec![("a".to_string(), "peer_c".to_string())]);
    }

    #[test]
    fn a_title_a_group_offers_twice_is_the_file_of_the_tracks_length() {
        let night = |source: &str, id: &str, duration: u32| DownloadableItem {
            duration: Some(duration),
            ..item(source, "O Holy Night", id)
        };
        // Each album has a studio "O Holy Night" on disc 1 and a longer live one on disc 2.
        let rest = vec![
            group(
                "peer_b",
                vec![
                    night("peer_b", "b/CD1/03.flac", 263),
                    night("peer_b", "b/CD2/03.flac", 301),
                ],
                0.9,
            ),
            group(
                "peer_c",
                vec![
                    night("peer_c", "c/CD1/03.flac", 262),
                    night("peer_c", "c/CD2/03.flac", 300),
                ],
                0.8,
            ),
        ];
        let tracks = vec![
            Some(night("peer_a", "a/CD1/03.flac", 263)),
            Some(night("peer_a", "a/CD2/03.flac", 301)),
        ];

        let mut pool = SourcePool::from_tracks(&tracks, &rest, MAX_SOURCES_PER_TRACK);
        let ids = |pool: &mut SourcePool, slot: usize| -> Vec<String> {
            std::iter::from_fn(|| pool.take_next(slot))
                .map(|i| i.id)
                .collect()
        };
        assert_eq!(ids(&mut pool, 0), vec!["b/CD1/03.flac", "c/CD1/03.flac"]);
        assert_eq!(ids(&mut pool, 1), vec!["b/CD2/03.flac", "c/CD2/03.flac"]);
    }

    #[test]
    fn stops_after_the_source_budget_is_spent() {
        let items: Vec<DownloadableItem> = (0..8)
            .map(|i| item(&format!("peer_{i}"), "Wake Up", &format!("{i}.flac")))
            .collect();

        let mut pool = SourcePool::single(items);
        // slot starts having used its first source, so 3 alternates remain
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_1".into()));
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_2".into()));
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_3".into()));
        assert!(pool.take_next(0).is_none());
        assert_eq!(pool.attempts_used(0), MAX_SOURCES_PER_TRACK);
    }

    #[test]
    fn dry_pool_and_unknown_slot_return_none() {
        let mut pool = SourcePool::single(vec![item("peer_a", "Wake Up", "a.flac")]);
        assert!(pool.take_next(0).is_none());
        assert!(pool.take_next(9).is_none());
    }

    #[test]
    fn each_slot_keeps_its_own_cap() {
        let rest = vec![
            group("peer_b", vec![item("peer_b", "One", "b/01.flac")], 0.9),
            group("peer_c", vec![item("peer_c", "One", "c/01.flac")], 0.8),
        ];
        let mut pool =
            SourcePool::from_tracks(&[Some(item("peer_a", "One", "a/01.flac"))], &rest, 1);
        assert_eq!(
            pool.take_next(0),
            None,
            "a slot with one source has no failover"
        );
        pool.push_slot(
            vec![
                item("peer_b", "One", "b/01.flac"),
                item("peer_c", "One", "c/01.flac"),
            ],
            1,
            2,
        );
        assert_eq!(pool.max_sources(0), 1);
        assert_eq!(pool.max_sources(1), 2);
        assert_eq!(pool.take_next(1).map(|i| i.source), Some("peer_b".into()));
        assert_eq!(pool.take_next(1), None);
    }

    #[test]
    fn a_pushed_slot_offers_its_own_alternates() {
        let mut pool = SourcePool::from_tracks(
            &[Some(item("peer_a", "One", "a/01.flac"))],
            &[group(
                "peer_b",
                vec![item("peer_b", "One", "b/01.flac")],
                0.9,
            )],
            MAX_SOURCES_PER_TRACK,
        );
        pool.push_slot(
            vec![item("peer_c", "Two", "c/02.flac")],
            1,
            MAX_SOURCES_PER_TRACK,
        );
        assert_eq!(pool.attempts_used(1), 1);
        assert_eq!(
            pool.take_next(1).map(|i| i.id),
            Some("c/02.flac".to_string())
        );
        assert_eq!(
            pool.take_next(0).map(|i| i.id),
            Some("b/01.flac".to_string())
        );
    }

    #[test]
    fn a_slots_sources_carry_over_to_another_pool_with_its_budget() {
        let rest = vec![
            group("peer_b", vec![item("peer_b", "One", "b/01.flac")], 0.9),
            group("peer_c", vec![item("peer_c", "One", "c/01.flac")], 0.8),
            group("peer_d", vec![item("peer_d", "One", "d/01.flac")], 0.7),
        ];
        let mut pool = SourcePool::from_tracks(
            &[Some(item("peer_a", "One", "a/01.flac"))],
            &rest,
            MAX_SOURCES_PER_TRACK,
        );
        pool.take_next(0);
        let (alternates, used, max) = pool.sources_of(0);
        assert_eq!((alternates.len(), used, max), (2, 2, MAX_SOURCES_PER_TRACK));

        let mut moved = SourcePool { slots: vec![] };
        moved.push_slot(alternates, used, max);
        assert_eq!(moved.take_next(0).map(|i| i.id), Some("c/01.flac".into()));
        assert_eq!(moved.take_next(0).map(|i| i.id), Some("d/01.flac".into()));
        assert!(moved.take_next(0).is_none(), "its budget is spent");
        assert_eq!(pool.sources_of(9), (vec![], 0, MAX_SOURCES_PER_TRACK));
    }

    #[tokio::test]
    async fn enqueues_the_next_alternate() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);

        let (_, queued) = pool
            .enqueue_next(0, &backend, &batch("X"))
            .await
            .queued
            .expect("enqueued");
        assert_eq!(queued.source, "peer_b");
    }

    #[tokio::test]
    async fn skips_an_alternate_whose_enqueue_errors() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
            item("peer_c", "Wake Up", "c.flac"),
        ];
        let mut pool = SourcePool::single(items);
        // peer_b's enqueue is rejected (Ok response, item status Failed), peer_c's succeeds
        let backend: Arc<dyn DownloadBackend> =
            StubBackend::new(vec![ScriptedOutcome::Rejected, ScriptedOutcome::Enqueued]);

        let (_, queued) = pool
            .enqueue_next(0, &backend, &batch("X"))
            .await
            .queued
            .expect("enqueued");
        assert_eq!(queued.source, "peer_c");
    }

    #[tokio::test]
    async fn skips_an_alternate_whose_transport_call_errors() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
            item("peer_c", "Wake Up", "c.flac"),
        ];
        let mut pool = SourcePool::single(items);
        // peer_b's download() call itself fails, peer_c's succeeds
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![
            ScriptedOutcome::TransportError,
            ScriptedOutcome::Enqueued,
        ]);

        let (_, queued) = pool
            .enqueue_next(0, &backend, &batch("X"))
            .await
            .queued
            .expect("enqueued");
        assert_eq!(queued.source, "peer_c");
    }

    #[tokio::test]
    async fn already_in_flight_alternate_is_skipped() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
            item("peer_c", "Wake Up", "c.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![
            ScriptedOutcome::AlreadyInFlight,
            ScriptedOutcome::Enqueued,
        ]);

        let (candidate, queued) = pool
            .enqueue_next(0, &backend, &batch("X"))
            .await
            .queued
            .expect("enqueued");
        assert_eq!(
            (candidate.source.as_str(), candidate.id.as_str()),
            ("peer_c", "c.flac")
        );
        assert_eq!(queued.source, "peer_c");
        assert_eq!(queued.id, "t-c.flac");
        assert_eq!(pool.attempts_used(0), 3);
    }

    #[tokio::test]
    async fn failover_enqueues_into_the_batch_folder() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();

        pool.enqueue_next(0, &backend, &batch("X"))
            .await
            .queued
            .expect("enqueued");
        assert_eq!(stub.destinations(), vec!["X [12345678]".to_string()]);
    }

    #[tokio::test]
    async fn a_claimed_key_is_released_when_slskd_says_already_in_flight() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
            item("peer_c", "Wake Up", "c.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![
            ScriptedOutcome::AlreadyInFlight,
            ScriptedOutcome::Enqueued,
        ]);
        let mine = batch("X");
        let someone_else = batch("Y").owner;

        let (_, queued) = pool
            .enqueue_next(0, &backend, &mine)
            .await
            .queued
            .expect("enqueued");

        assert_eq!(queued.source, "peer_c");
        let registry = TransferRegistry::global();
        assert!(
            registry.may_adopt(&key(&backend, "peer_b", "b.flac"), &someone_else),
            "the refused alternate's key was released"
        );
        assert!(registry.may_adopt(&key(&backend, "peer_c", "c.flac"), &mine.owner));
        assert!(
            !registry.may_adopt(&key(&backend, "peer_c", "c.flac"), &someone_else),
            "the accepted alternate stays claimed by its batch"
        );
    }

    #[tokio::test]
    async fn an_alternate_another_user_is_downloading_is_skipped() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
            item("peer_c", "Wake Up", "c.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let stub = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let backend: Arc<dyn DownloadBackend> = stub.clone();
        let other = batch("Other");
        TransferRegistry::global().claim(&[key(&backend, "peer_b", "b.flac")], &other);

        let next = pool.enqueue_next(0, &backend, &batch("X")).await;

        assert_eq!(next.queued.expect("enqueued").1.source, "peer_c");
        assert_eq!(
            next.refused[0].failure.code,
            FailureCode::AlreadyInFlightOtherUser
        );
        assert_eq!(stub.destinations().len(), 1, "peer_b was never asked");
        assert!(
            TransferRegistry::global().may_adopt(&key(&backend, "peer_b", "b.flac"), &other.owner)
        );
    }

    #[tokio::test]
    async fn returns_none_when_every_alternate_fails() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Rejected]);

        assert!(pool
            .enqueue_next(0, &backend, &batch("X"))
            .await
            .queued
            .is_none());
    }

    #[tokio::test]
    async fn stops_enqueueing_once_the_budget_is_spent() {
        // 1 already-used source plus 6 alternates: more alternates than the
        // budget (MAX_SOURCES_PER_TRACK - 1 = 3) allows.
        let items: Vec<DownloadableItem> = (0..7)
            .map(|i| item(&format!("peer_{i}"), "Wake Up", &format!("{i}.flac")))
            .collect();
        let mut pool = SourcePool::single(items);
        // Every alternate the backend is asked about is rejected. A
        // regression that drained `alternates` directly instead of going
        // through `take_next` would call the backend a 4th time here.
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![
            ScriptedOutcome::Rejected,
            ScriptedOutcome::Rejected,
            ScriptedOutcome::Rejected,
        ]);

        assert!(pool
            .enqueue_next(0, &backend, &batch("X"))
            .await
            .queued
            .is_none());
        assert_eq!(pool.attempts_used(0), MAX_SOURCES_PER_TRACK);
    }

    #[tokio::test]
    async fn enqueue_next_reports_every_refused_alternate() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
            item("peer_c", "Wake Up", "c.flac"),
            item("peer_d", "Wake Up", "d.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![
            ScriptedOutcome::Rejected,
            ScriptedOutcome::TransportError,
            ScriptedOutcome::Enqueued,
        ]);
        let next = pool.enqueue_next(0, &backend, &batch("X")).await;
        assert_eq!(next.queued.unwrap().1.source, "peer_d");
        let refused: Vec<(&str, FailureCode)> = next
            .refused
            .iter()
            .map(|r| (r.candidate.source.as_str(), r.failure.code))
            .collect();
        assert_eq!(
            refused,
            vec![
                ("peer_b", FailureCode::PeerOffline),
                ("peer_c", FailureCode::BackendResponseUnreadable)
            ]
        );
    }

    #[tokio::test]
    async fn enqueue_next_codes_alternates_the_same_user_already_holds() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
            item("peer_c", "Wake Up", "c.flac"),
            item("peer_d", "Wake Up", "d.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Enqueued]);
        let mine = batch("X");
        TransferRegistry::global().claim(
            &[key(&backend, "peer_b", "b.flac")],
            &same_user_into(&mine, PathBuf::from("/other-library")),
        );
        TransferRegistry::global().claim(
            &[key(&backend, "peer_c", "c.flac")],
            &same_user_into(&mine, PathBuf::from("/music")),
        );
        let next = pool.enqueue_next(0, &backend, &mine).await;
        assert_eq!(next.queued.unwrap().1.source, "peer_d");
        let refused: Vec<(&str, FailureCode, &str)> = next
            .refused
            .iter()
            .map(|r| {
                (
                    r.candidate.source.as_str(),
                    r.failure.code,
                    r.failure.message.as_str(),
                )
            })
            .collect();
        assert_eq!(
            refused,
            vec![
                (
                    "peer_b",
                    FailureCode::AlreadyInFlightOtherFolder,
                    ANOTHER_FOLDER
                ),
                ("peer_c", FailureCode::AlreadyRunning, ALREADY_MINE)
            ]
        );
    }

    #[tokio::test]
    async fn an_alternate_the_backend_answers_nothing_for_is_refused_as_nothing_enqueued() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![ScriptedOutcome::Unanswered]);

        let next = pool.enqueue_next(0, &backend, &batch("X")).await;

        assert!(next.queued.is_none());
        let f = &next.refused[0].failure;
        assert_eq!(
            (f.code, f.stage, f.retry, f.message.as_str()),
            (
                FailureCode::EnqueueFailed,
                Stage::Enqueue,
                RetryAdvice::OtherSource,
                NOTHING_ENQUEUED
            )
        );
        assert!(TransferRegistry::global()
            .may_adopt(&key(&backend, "peer_b", "b.flac"), &batch("Y").owner));
    }

    #[tokio::test]
    async fn every_alternate_refused_is_reported_when_none_is_accepted() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
            item("peer_c", "Wake Up", "c.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![
            ScriptedOutcome::AlreadyInFlight,
            ScriptedOutcome::Rejected,
        ]);
        let next = pool.enqueue_next(0, &backend, &batch("X")).await;
        assert!(next.queued.is_none());
        let refused: Vec<(&str, &str, FailureCode)> = next
            .refused
            .iter()
            .map(|r| {
                (
                    r.candidate.source.as_str(),
                    r.candidate.id.as_str(),
                    r.failure.code,
                )
            })
            .collect();
        assert_eq!(
            refused,
            vec![
                ("peer_b", "b.flac", FailureCode::EnqueueFailed),
                ("peer_c", "c.flac", FailureCode::PeerOffline)
            ]
        );
    }
}
