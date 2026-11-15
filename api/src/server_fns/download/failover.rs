//! Peer failover for downloads.
//!
//! A Soulseek peer can accept an enqueue and then never deliver a byte. The
//! search that picked it already returned other peers offering the same
//! track, so a failed transfer should move to the next-best source rather
//! than fail the track. This module owns those alternates and the per-track
//! attempt budget; the waiters (`DownloadMonitor`, discovery's poll loop)
//! decide *when* a transfer has definitively failed.

use shared::download::{DownloadableGroup, DownloadableItem, EnqueueStatus, QueuedDownload};
use shared::failure::{Failure, Stage};
use soulbeet::DownloadBackend;
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::info;

use super::enqueue::{enqueue_claimed, held_failure, not_enqueued_failure, transfer_key};
use crate::transfer_registry::{BatchRef, TransferRegistry};

/// Sources tried per track before it is declared failed: the peer picked at
/// search time plus three alternates. slskd retries each peer 3 times on its
/// own (~40s) before reporting failure, so this caps a dead track at roughly
/// 2.5 minutes.
pub const MAX_SOURCES_PER_TRACK: usize = 4;

/// An alternate `enqueue_next` took from a slot and could not enqueue.
pub struct RefusedAlternate {
    pub source: String,
    pub item: String,
    pub failure: Failure,
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
}

/// Ranked alternate sources for every track in a batch, indexed by slot.
pub struct SourcePool {
    slots: Vec<Slot>,
}

impl SourcePool {
    /// Alternates for a batch, indexed by the slot the caller will monitor.
    ///
    /// Callers pass the tracks in the exact order they were enqueued, which is
    /// not the order they were searched: slskd groups an enqueue by peer, and
    /// items whose backend data will not parse are dropped before enqueue. A
    /// `None` entry keeps a slot aligned when its track cannot be recovered,
    /// and that slot simply gets no alternates.
    ///
    /// `rest` must already be in descending score order, which is how
    /// `auto_download` sorts it.
    pub fn from_tracks(tracks: &[Option<DownloadableItem>], rest: &[DownloadableGroup]) -> Self {
        let mut pool = Self { slots: Vec::new() };
        for track in tracks {
            let alternates = match track {
                Some(track) => alternates_for(track, rest),
                None => Vec::new(),
            };
            pool.push_slot(alternates);
        }
        pool
    }

    /// Adds a slot after the last; its transfer counts as its first source.
    pub fn push_slot(&mut self, alternates: Vec<DownloadableItem>) {
        self.slots.push(Slot {
            alternates: alternates.into(),
            used: 1,
        });
    }

    /// Single track, N ranked sources. `items[0]` is the source already
    /// enqueued; the rest are its alternates.
    pub fn single(items: Vec<DownloadableItem>) -> Self {
        let alternates = items.into_iter().skip(1).collect();
        Self {
            slots: vec![Slot {
                alternates,
                used: 1,
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

    /// Next alternate for this slot, or `None` once the budget is spent, the
    /// alternates run out, or the slot does not exist.
    pub fn take_next(&mut self, slot: usize) -> Option<DownloadableItem> {
        let slot = self.slots.get_mut(slot)?;
        if slot.used >= MAX_SOURCES_PER_TRACK {
            return None;
        }
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
        let mut refused = Vec::new();
        while let Some(item) = self.take_next(slot) {
            let (source, id) = (item.source.clone(), item.id.clone());
            let key = transfer_key(backend.id(), &source, &id);
            let claim = TransferRegistry::global().claim(&[key], batch);
            let failure = if claim.claimed.is_empty() {
                held_failure(&claim, batch)
            } else {
                match enqueue_claimed(backend, vec![item.clone()], batch).await {
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
            refused.push(RefusedAlternate {
                source,
                item: id,
                failure,
            });
        }
        NextSource {
            queued: None,
            refused,
        }
    }
}

/// The items of `rest` offering `track` (same normalized title), in `rest`'s
/// order. `rest` must be sorted by descending score, so the best group comes
/// first. Matching is by title, never by filename: the same track has a
/// completely different remote path on each peer.
pub fn alternates_for(
    track: &DownloadableItem,
    rest: &[DownloadableGroup],
) -> Vec<DownloadableItem> {
    let wanted = shared::recommendation::normalize_for_matching(&track.title);
    rest.iter()
        .flat_map(|group| group.items.iter())
        .filter(|candidate| {
            shared::recommendation::normalize_for_matching(&candidate.title) == wanted
        })
        .cloned()
        .collect()
}

/// Line the accepted downloads up with the tracks they came from, in the
/// order the caller will monitor them.
///
/// `queued` is neither the order nor the length of `items`: slskd groups an
/// enqueue by peer, items whose backend data will not parse are dropped
/// before the request is sent, and rejected enqueues are filtered out by the
/// caller.
/// Feeding `items` straight to `from_tracks` would hand slot N the alternates
/// of a different track.
///
/// A download that matches no item (`item_index`) stays `None` so the slots
/// after it keep their index.
pub fn align_pool_tracks(
    queued: &[QueuedDownload],
    items: &[DownloadableItem],
) -> Vec<Option<DownloadableItem>> {
    queued
        .iter()
        .map(|d| item_index(d, items).map(|at| items[at].clone()))
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
        batch, group, item, key, same_user_into, ScriptedOutcome, StubBackend,
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

    /// The ids `align_pool_tracks` lands in each slot, `None` for a gap.
    fn aligned_ids(queued: &[QueuedDownload], items: &[DownloadableItem]) -> Vec<Option<String>> {
        align_pool_tracks(queued, items)
            .into_iter()
            .map(|track| track.map(|i| i.id))
            .collect()
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

        let mut pool = SourcePool::from_tracks(&tracks, &rest);
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

        let mut pool = SourcePool::from_tracks(&tracks, &rest);
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_b".into()));
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_c".into()));
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_d".into()));
        assert!(pool.take_next(0).is_none());
    }

    #[test]
    fn align_pool_tracks_follows_the_enqueue_order() {
        // The enqueue came back with the dirB files first. Returning `items` in search order here would give
        // slot 0 the alternates for "One" while the monitor watches "Two".
        let items = searched();
        let accepted = vec![
            queued("peer_a", "dirB/02.flac"),
            queued("peer_a", "dirB/03.flac"),
            queued("peer_a", "dirA/01.flac"),
        ];

        assert_eq!(
            aligned_ids(&accepted, &items),
            vec![
                Some("dirB/02.flac".to_string()),
                Some("dirB/03.flac".to_string()),
                Some("dirA/01.flac".to_string()),
            ]
        );
    }

    #[test]
    fn align_pool_tracks_skips_an_item_dropped_before_enqueue() {
        // "Two" had unparseable backend data, so to_slskd_track dropped it and
        // slskd was never asked about it. Three items, two slots.
        let items = searched();
        let accepted = vec![
            queued("peer_a", "dirA/01.flac"),
            queued("peer_a", "dirB/03.flac"),
        ];

        assert_eq!(
            aligned_ids(&accepted, &items),
            vec![
                Some("dirA/01.flac".to_string()),
                Some("dirB/03.flac".to_string()),
            ]
        );
    }

    #[test]
    fn align_pool_tracks_skips_a_rejected_enqueue() {
        // "Two" came back with an error and the caller partitioned it out, and
        // the survivors arrived in another order than the search's.
        let items = searched();
        let accepted = vec![
            queued("peer_a", "dirB/03.flac"),
            queued("peer_a", "dirA/01.flac"),
        ];

        assert_eq!(
            aligned_ids(&accepted, &items),
            vec![
                Some("dirB/03.flac".to_string()),
                Some("dirA/01.flac".to_string()),
            ]
        );
    }

    #[test]
    fn align_pool_tracks_leaves_an_unrecognised_download_as_a_gap() {
        // Same remote filename, different peer: the source half of the key
        // must stop it matching, and the slot must stay in place so the
        // download after it is not shifted down.
        let items = searched();
        let accepted = vec![
            queued("peer_a", "dirA/01.flac"),
            queued("peer_z", "dirB/02.flac"),
            queued("peer_a", "dirB/03.flac"),
        ];

        assert_eq!(
            aligned_ids(&accepted, &items),
            vec![
                Some("dirA/01.flac".to_string()),
                None,
                Some("dirB/03.flac".to_string()),
            ]
        );
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

        let mut pool = SourcePool::from_tracks(&tracks, &rest);
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

        let mut pool = SourcePool::from_tracks(&tracks, &rest);
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

        let mut pool = SourcePool::from_tracks(&tracks, &rest);
        assert!(pool.take_next(0).is_none());
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
    fn a_pushed_slot_offers_its_own_alternates() {
        let mut pool = SourcePool::from_tracks(
            &[Some(item("peer_a", "One", "a/01.flac"))],
            &[group(
                "peer_b",
                vec![item("peer_b", "One", "b/01.flac")],
                0.9,
            )],
        );
        pool.push_slot(vec![item("peer_c", "Two", "c/02.flac")]);
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
            .map(|r| (r.source.as_str(), r.failure.code))
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
                    r.source.as_str(),
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
            .map(|r| (r.source.as_str(), r.item.as_str(), r.failure.code))
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
