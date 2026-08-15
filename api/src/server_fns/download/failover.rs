//! Peer failover for downloads.
//!
//! A Soulseek peer can accept an enqueue and then never deliver a byte. The
//! search that picked it already returned other peers offering the same
//! track, so a failed transfer should move to the next-best source rather
//! than fail the track. This module owns those alternates and the per-track
//! attempt budget; the waiters (`DownloadMonitor`, discovery's poll loop)
//! decide *when* a transfer has definitively failed.

use shared::download::{DownloadableGroup, DownloadableItem, QueuedDownload};
use soulbeet::DownloadBackend;
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::{info, warn};

/// Sources tried per track before it is declared failed: the peer picked at
/// search time plus three alternates. slskd retries each peer 3 times on its
/// own (~40s) before reporting failure, so this caps a dead track at roughly
/// 2.5 minutes.
pub const MAX_SOURCES_PER_TRACK: usize = 4;

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
    /// Album or multi-track batch: map each track of the picked group to the
    /// same-titled items in the remaining groups. `rest` must already be in
    /// descending score order, which is how `auto_download` sorts it.
    ///
    /// Matching is by normalized title, never by filename: the same track has
    /// a completely different remote path on each peer.
    pub fn from_groups(picked: &DownloadableGroup, rest: &[DownloadableGroup]) -> Self {
        let slots = picked
            .items
            .iter()
            .map(|track| {
                let wanted = shared::recommendation::normalize_for_matching(&track.title);
                let alternates = rest
                    .iter()
                    .flat_map(|group| group.items.iter())
                    .filter(|candidate| {
                        shared::recommendation::normalize_for_matching(&candidate.title) == wanted
                    })
                    .cloned()
                    .collect();
                Slot {
                    alternates,
                    used: 1,
                }
            })
            .collect();

        Self { slots }
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

    /// Enqueue this slot's next alternate. Walks past alternates the backend
    /// rejects (peer offline, file already in slskd's list) so one bad
    /// candidate does not cost a whole poll cycle. `None` means the slot is
    /// out of sources or budget.
    pub async fn enqueue_next(
        &mut self,
        slot: usize,
        backend: &Arc<dyn DownloadBackend>,
    ) -> Option<QueuedDownload> {
        while let Some(item) = self.take_next(slot) {
            let source = item.source.clone();
            match backend.download(vec![item]).await {
                Ok(queued) => {
                    if let Some(ok) = queued.into_iter().find(|d| d.error.is_none()) {
                        info!("Failover enqueued {} from {}", ok.item, ok.source);
                        return Some(ok);
                    }
                    warn!("Failover source {} rejected the enqueue", source);
                }
                Err(e) => warn!("Failover enqueue to {} failed: {}", source, e),
            }
        }
        None
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use shared::download::{DownloadProgress, QueuedDownload, SearchResult};
    use shared::download::{DownloadableGroup, DownloadableItem};
    use shared::metadata::{Album, Track};
    use soulbeet::error::Result as SoulResult;
    use soulbeet::DownloadBackend;
    use std::sync::{Arc, Mutex};

    fn item(source: &str, title: &str, id: &str) -> DownloadableItem {
        DownloadableItem {
            id: id.to_string(),
            source: source.to_string(),
            title: title.to_string(),
            artist: "Kowloon".to_string(),
            album: "Come Over".to_string(),
            size: Some(27_213_250),
            duration: Some(200),
            quality: "FLAC".to_string(),
            quality_score: 1.0,
            backend_data: None,
        }
    }

    fn group(source: &str, items: Vec<DownloadableItem>, score: f64) -> DownloadableGroup {
        DownloadableGroup {
            source: source.to_string(),
            group_id: format!("{source}/group"),
            title: "Come Over".to_string(),
            artist: Some("Kowloon".to_string()),
            item_count: items.len(),
            total_size: 0,
            items,
            quality: "FLAC".to_string(),
            score,
        }
    }

    #[test]
    fn matches_alternates_by_title_across_differing_paths() {
        let picked = group(
            "ElevatorsForPeople",
            vec![item(
                "ElevatorsForPeople",
                "Wake Up",
                "music\\Kowloon\\Come Over (2021)\\Kowloon - Come Over - 07 - Wake Up - [FLAC 44.1kHz].flac",
            )],
            1.0,
        );
        let rest = vec![group(
            "Cornflake9026",
            vec![item(
                "Cornflake9026",
                "Wake Up ",
                "music\\Kowloon\\Kowloon - Come Over\\07 - Wake Up.flac",
            )],
            0.9,
        )];

        let mut pool = SourcePool::from_groups(&picked, &rest);
        let next = pool.take_next(0).expect("alternate for slot 0");
        assert_eq!(next.source, "Cornflake9026");
    }

    #[test]
    fn alternates_follow_the_order_of_the_remaining_groups() {
        let picked = group("peer_a", vec![item("peer_a", "Wake Up", "a.flac")], 1.0);
        let rest = vec![
            group("peer_b", vec![item("peer_b", "Wake Up", "b.flac")], 0.9),
            group("peer_c", vec![item("peer_c", "Wake Up", "c.flac")], 0.8),
            group("peer_d", vec![item("peer_d", "Wake Up", "d.flac")], 0.7),
        ];

        let mut pool = SourcePool::from_groups(&picked, &rest);
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_b".into()));
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_c".into()));
        assert_eq!(pool.take_next(0).map(|i| i.source), Some("peer_d".into()));
        assert!(pool.take_next(0).is_none());
    }

    #[test]
    fn ignores_alternates_for_a_different_title() {
        let picked = group("peer_a", vec![item("peer_a", "Wake Up", "a.flac")], 1.0);
        let rest = vec![group(
            "peer_b",
            vec![item("peer_b", "Come Over", "b.flac")],
            0.9,
        )];

        let mut pool = SourcePool::from_groups(&picked, &rest);
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

    /// Records every enqueue and answers each one from a scripted list.
    struct StubBackend {
        /// One entry per expected call: Some(source) enqueues, None errors.
        script: Mutex<Vec<Option<String>>>,
        calls: Mutex<Vec<String>>,
    }

    impl StubBackend {
        fn new(script: Vec<Option<String>>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl DownloadBackend for StubBackend {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn name(&self) -> &'static str {
            "Stub"
        }
        async fn start_search(&self, _album: Option<&Album>, _tracks: &[Track]) -> SoulResult<String> {
            Ok("search".to_string())
        }
        async fn poll_search(&self, _search_id: &str) -> SoulResult<SearchResult> {
            unreachable!("failover never searches")
        }
        async fn download(&self, items: Vec<DownloadableItem>) -> SoulResult<Vec<QueuedDownload>> {
            let item = items.into_iter().next().expect("one item per enqueue");
            self.calls.lock().unwrap().push(item.source.clone());
            let outcome = {
                let mut script = self.script.lock().unwrap();
                if script.is_empty() {
                    None
                } else {
                    script.remove(0)
                }
            };
            Ok(vec![QueuedDownload {
                id: item.id.clone(),
                source: item.source.clone(),
                item: item.id.clone(),
                size: item.size.unwrap_or(0),
                error: outcome.is_none().then(|| "peer offline".to_string()),
            }])
        }
        async fn get_downloads(&self) -> SoulResult<Vec<DownloadProgress>> {
            Ok(Vec::new())
        }
        async fn cancel_download(&self, _u: &str, _id: &str, _remove: bool) -> SoulResult<()> {
            Ok(())
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn enqueues_the_next_alternate() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![Some("peer_b".into())]);

        let queued = pool.enqueue_next(0, &backend).await.expect("enqueued");
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
        // peer_b's enqueue fails, peer_c's succeeds
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![None, Some("peer_c".into())]);

        let queued = pool.enqueue_next(0, &backend).await.expect("enqueued");
        assert_eq!(queued.source, "peer_c");
    }

    #[tokio::test]
    async fn returns_none_when_every_alternate_fails() {
        let items = vec![
            item("peer_a", "Wake Up", "a.flac"),
            item("peer_b", "Wake Up", "b.flac"),
        ];
        let mut pool = SourcePool::single(items);
        let backend: Arc<dyn DownloadBackend> = StubBackend::new(vec![None]);

        assert!(pool.enqueue_next(0, &backend).await.is_none());
    }
}
