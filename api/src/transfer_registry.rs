//! Which batch owns each in-flight download, process-wide.
//!
//! A transfer is keyed by backend, peer and exact remote filename. A request
//! claims its keys before it enqueues anything, so a repeat click on the same
//! album joins the batch already downloading it instead of fetching it again,
//! and another user's request for the same file is refused.
//!
//! A batch that is still running accepts more files through its inbox: the
//! joining request reserves the inbox, enqueues into the batch's folder and
//! delivers the accepted transfers, which the batch's monitor then watches.
//! The monitor only finishes once no reservation is outstanding.

use crate::auth::UserId;
use crate::ledger::DuplicateRule;
use shared::download::{batch_folder, DownloadableItem, JobSlot};
use soulbeet::ImportMode;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

/// Who started a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferOwner {
    pub user_id: UserId,
    pub batch_id: String,
}

/// A running batch: who owns it, its name, the folder its transfers are
/// enqueued into, the library folder they are imported into, how its imports
/// treat what the library has and the inbox that takes more of them.
#[derive(Clone, Debug)]
pub struct BatchRef {
    pub owner: TransferOwner,
    pub label: String,
    pub destination: String,
    pub target: PathBuf,
    pub inbox: Arc<BatchInbox>,
    pub import_mode: ImportMode,
    pub duplicates: DuplicateRule,
}

impl BatchRef {
    /// A new batch downloading into `batch_folder(label, batch_id)` and
    /// importing into `target` in `import_mode`, its duplicates guarded.
    pub fn new(
        owner: TransferOwner,
        label: String,
        target: PathBuf,
        import_mode: ImportMode,
    ) -> Self {
        let destination = batch_folder(&label, &owner.batch_id);
        Self {
            owner,
            label,
            destination,
            target,
            inbox: BatchInbox::new(),
            import_mode,
            duplicates: DuplicateRule::Guarded,
        }
    }

    /// The batch with imports that skip what the library already has.
    pub fn skipping_duplicates(self) -> Self {
        Self {
            duplicates: DuplicateRule::Skip,
            ..self
        }
    }
}

/// A batch that holds transfers now, and how many.
#[derive(Clone, Debug, PartialEq)]
pub struct RunningBatch {
    pub batch_id: String,
    pub label: String,
    pub transfers: usize,
}

/// One transfer, by exact strings: slskd refuses a second unfinished
/// transfer of the same peer and filename, case included.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct TransferKey {
    pub backend: &'static str,
    pub peer: String,
    pub filename: String,
}

/// What a claim found for each key.
#[derive(Debug, Default)]
pub struct Claim {
    /// Free keys, now owned by the claiming batch.
    pub claimed: Vec<TransferKey>,
    /// Keys the same user is already downloading, with the batch that does.
    pub mine: Vec<(TransferKey, BatchRef)>,
    /// Keys another user is downloading.
    pub other_user: Vec<TransferKey>,
}

/// What `claim_for_request` found and did.
#[derive(Debug)]
pub struct RequestClaim {
    /// `claimed` lists the keys now owned by `batch`.
    pub claim: Claim,
    /// The running batch the request joined, or the request's own.
    pub batch: BatchRef,
    /// Holds the joined batch open. None for the request's own batch.
    pub reservation: Option<InboxReservation>,
}

/// The claims and cancel marks, behind one lock so a check of both is one
/// step.
#[derive(Default)]
struct Held {
    owners: HashMap<TransferKey, BatchRef>,
    /// Held keys their user asked to cancel. A mark lives only as long as
    /// its key's claim.
    cancelled: HashSet<TransferKey>,
    /// Jobs whose user asked to cancel them while they may have transfers
    /// (design D9). The ledger's writer sets and clears these marks.
    cancelled_jobs: HashSet<String>,
}

impl Held {
    /// Whether `owner` holds `key`.
    fn held_by(&self, key: &TransferKey, owner: &TransferOwner) -> bool {
        self.owners
            .get(key)
            .is_some_and(|held| held.owner == *owner)
    }

    /// Drops `key`'s claim and mark. Returns whether it was marked.
    fn remove(&mut self, key: &TransferKey) -> bool {
        self.owners.remove(key);
        self.cancelled.remove(key)
    }
}

#[derive(Default)]
pub struct TransferRegistry {
    held: Mutex<Held>,
}

static REGISTRY: LazyLock<TransferRegistry> = LazyLock::new(TransferRegistry::default);

impl TransferRegistry {
    pub fn global() -> &'static TransferRegistry {
        &REGISTRY
    }

    fn held(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sorts `keys` by who holds them, from `user`'s point of view, and
    /// lists the free ones in `claimed` without claiming them. A key repeated
    /// in `keys` is reported once.
    fn classify(
        owners: &HashMap<TransferKey, BatchRef>,
        keys: &[TransferKey],
        user: &UserId,
    ) -> Claim {
        let mut claim = Claim::default();
        let mut seen = HashSet::new();
        for key in keys.iter().filter(|key| seen.insert(*key)) {
            match owners.get(key) {
                None => claim.claimed.push(key.clone()),
                Some(running) if &running.owner.user_id == user => {
                    claim.mine.push((key.clone(), running.clone()));
                }
                Some(_) => claim.other_user.push(key.clone()),
            }
        }
        claim
    }

    /// Claims every free key for `batch` in one step.
    pub fn claim(&self, keys: &[TransferKey], batch: &BatchRef) -> Claim {
        let mut held = self.held();
        let claim = Self::classify(&held.owners, keys, &batch.owner.user_id);
        for key in &claim.claimed {
            held.owners.insert(key.clone(), batch.clone());
        }
        claim
    }

    /// Claims a request's free keys in one step, for the batch that will
    /// download them. When the user is already downloading some of the files
    /// into `fresh`'s library folder in its import mode and duplicates rule and that batch still
    /// takes files, its inbox is reserved and the free keys go straight to it. Otherwise they
    /// go to `fresh`. No other request ever sees them owned by a batch that
    /// gets no monitor.
    pub fn claim_for_request(&self, keys: &[TransferKey], fresh: &BatchRef) -> RequestClaim {
        let mut held = self.held();
        let claim = Self::classify(&held.owners, keys, &fresh.owner.user_id);
        let joined = match claim.claimed.is_empty() {
            true => None,
            false => claim
                .mine
                .iter()
                .filter(|(_, running)| {
                    running.target == fresh.target
                        && running.import_mode == fresh.import_mode
                        && running.duplicates == fresh.duplicates
                })
                .find_map(|(_, running)| Some((running.clone(), running.inbox.reserve()?))),
        };
        let (batch, reservation) = match joined {
            Some((running, reservation)) => (running, Some(reservation)),
            None => (fresh.clone(), None),
        };
        for key in &claim.claimed {
            held.owners.insert(key.clone(), batch.clone());
        }
        RequestClaim {
            claim,
            batch,
            reservation,
        }
    }

    /// Releases `key`, and its cancel mark, if `owner` still holds it. A key
    /// released and claimed again by someone else meanwhile stays theirs.
    /// Returns whether `owner` held it marked cancelled.
    pub fn release(&self, key: &TransferKey, owner: &TransferOwner) -> bool {
        let mut held = self.held();
        held.held_by(key, owner) && held.remove(key)
    }

    /// Releases every key the batch still holds, with their cancel marks.
    pub fn release_batch(&self, owner: &TransferOwner) {
        let Held {
            owners, cancelled, ..
        } = &mut *self.held();
        owners.retain(|_, batch| batch.owner != *owner);
        cancelled.retain(|key| owners.contains_key(key));
    }

    /// Moves `batch`'s claim, and its cancel mark, from `old` to `new`.
    /// Refused, with nothing changed, when another batch holds `new`, even
    /// the same user's: that batch has its own monitor. `old` is only
    /// released when `batch` holds it.
    pub fn rebind(&self, old: &TransferKey, new: TransferKey, batch: &BatchRef) -> bool {
        let mut held = self.held();
        if held
            .owners
            .get(&new)
            .is_some_and(|other| other.owner != batch.owner)
        {
            return false;
        }
        let cancelled = held.held_by(old, &batch.owner) && held.remove(old);
        if cancelled {
            held.cancelled.insert(new.clone());
        }
        held.owners.insert(new, batch.clone());
        true
    }

    /// Releases `old` if `owner` holds it and, when `old` was cancelled,
    /// marks `new` (held by `owner`) cancelled. Changes nothing when they
    /// are the same key: releasing it would free the claim the new transfer
    /// holds.
    pub fn hand_over(&self, old: &TransferKey, new: &TransferKey, owner: &TransferOwner) {
        if old == new {
            return;
        }
        let mut held = self.held();
        let cancelled = held.held_by(old, owner) && held.remove(old);
        if cancelled && held.held_by(new, owner) {
            held.cancelled.insert(new.clone());
        }
    }

    /// Marks `key` cancelled when one of `user`'s batches holds it. False
    /// otherwise, with nothing changed.
    pub fn request_cancel(&self, key: &TransferKey, user: &UserId) -> bool {
        let mut held = self.held();
        let mine = held
            .owners
            .get(key)
            .is_some_and(|batch| &batch.owner.user_id == user);
        if mine {
            held.cancelled.insert(key.clone());
        }
        mine
    }

    /// Whether `owner` holds `key` and it was marked cancelled.
    pub fn is_cancelled(&self, key: &TransferKey, owner: &TransferOwner) -> bool {
        let held = self.held();
        held.held_by(key, owner) && held.cancelled.contains(key)
    }

    /// Marks `job` cancelled: every monitor stops each of its slots.
    pub fn cancel_job(&self, job: &str) {
        self.held().cancelled_jobs.insert(job.to_string());
    }

    /// Whether `job` is marked cancelled.
    pub fn job_cancelled(&self, job: &str) -> bool {
        self.held().cancelled_jobs.contains(job)
    }

    /// Drops `job`'s cancel mark: it ended, or it downloads again.
    pub fn reopen_job(&self, job: &str) {
        self.held().cancelled_jobs.remove(job);
    }

    /// `user`'s batches holding a transfer now, each with how many it holds,
    /// sorted by label, then batch id.
    pub fn running_batches(&self, user: &UserId) -> Vec<RunningBatch> {
        let held = self.held();
        let mut counts: HashMap<&str, (&str, usize)> = HashMap::new();
        for batch in held.owners.values() {
            if &batch.owner.user_id == user {
                counts
                    .entry(&batch.owner.batch_id)
                    .or_insert((&batch.label, 0))
                    .1 += 1;
            }
        }
        let mut batches: Vec<RunningBatch> = counts
            .into_iter()
            .map(|(batch_id, (label, transfers))| RunningBatch {
                batch_id: batch_id.to_string(),
                label: label.to_string(),
                transfers,
            })
            .collect();
        batches.sort_by(|a, b| (&a.label, &a.batch_id).cmp(&(&b.label, &b.batch_id)));
        batches
    }

    /// Whether a batch may start watching a transfer it did not enqueue:
    /// only one nobody owns or one it owns already. Another batch's transfer,
    /// even the same user's, has its own monitor.
    pub fn may_adopt(&self, key: &TransferKey, owner: &TransferOwner) -> bool {
        self.held()
            .owners
            .get(key)
            .is_none_or(|held| held.owner == *owner)
    }
}

/// A transfer a joining request enqueued into a running batch.
#[derive(Clone, Debug, PartialEq)]
pub struct AddedSlot {
    pub source: String,
    pub filename: String,
    pub transfer_id: String,
    /// Other sources for the same track, best first; empty when the request
    /// had none.
    pub alternates: Vec<DownloadableItem>,
    /// The most sources the slot may use, the transfer being its first.
    pub max_sources: usize,
    /// The job slot the transfer downloads for.
    pub job: JobSlot,
}

#[derive(Debug)]
struct InboxState {
    open: bool,
    reserved: usize,
    slots: Vec<AddedSlot>,
}

/// Where a running batch takes transfers that joining requests enqueued.
#[derive(Debug)]
pub struct BatchInbox {
    state: Mutex<InboxState>,
}

impl BatchInbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(InboxState {
                open: true,
                reserved: 0,
                slots: Vec::new(),
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, InboxState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Holds the batch open while a request enqueues into it. `None` once
    /// the batch has finished.
    pub fn reserve(self: &Arc<Self>) -> Option<InboxReservation> {
        let mut state = self.state();
        if !state.open {
            return None;
        }
        state.reserved += 1;
        Some(InboxReservation {
            inbox: Arc::clone(self),
        })
    }

    /// The slots delivered since the last call.
    pub fn take(&self) -> Vec<AddedSlot> {
        std::mem::take(&mut self.state().slots)
    }

    /// Closes the inbox when no request is enqueueing into it and every
    /// delivered slot was taken. Returns whether it is closed.
    pub fn close_if_idle(&self) -> bool {
        let mut state = self.state();
        if state.reserved == 0 && state.slots.is_empty() {
            state.open = false;
        }
        !state.open
    }

    /// Closes the inbox whatever is outstanding, for a monitor that stops
    /// early, and returns the slots it never took. Later deliveries are
    /// handed back to their request.
    pub fn close(&self) -> Vec<AddedSlot> {
        let mut state = self.state();
        state.open = false;
        std::mem::take(&mut state.slots)
    }
}

/// A request's hold on a running batch. Dropping it without delivering
/// releases the hold.
#[derive(Debug)]
pub struct InboxReservation {
    inbox: Arc<BatchInbox>,
}

impl InboxReservation {
    /// Hands the transfers to the batch's monitor. Gives them back when the
    /// monitor stopped early and nobody will watch them.
    pub fn deliver(self, slots: Vec<AddedSlot>) -> Result<(), Vec<AddedSlot>> {
        let mut state = self.inbox.state();
        if !state.open {
            return Err(slots);
        }
        state.slots.extend(slots);
        Ok(())
    }
}

impl Drop for InboxReservation {
    fn drop(&mut self) {
        self.inbox.state().reserved -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_fns::download::failover::MAX_SOURCES_PER_TRACK;

    fn batch(user_id: &str, batch_id: &str, import_mode: ImportMode) -> BatchRef {
        BatchRef::new(
            TransferOwner {
                user_id: UserId::new(user_id),
                batch_id: batch_id.to_string(),
            },
            "Come Over".to_string(),
            PathBuf::from("/music"),
            import_mode,
        )
    }

    fn key(filename: &str) -> TransferKey {
        TransferKey {
            backend: "slskd",
            peer: "peer_a".to_string(),
            filename: filename.to_string(),
        }
    }

    #[test]
    fn a_request_never_joins_a_batch_with_another_duplicates_rule() {
        let registry = TransferRegistry::default();
        let running = batch("user-a", "b1", ImportMode::Singleton);
        registry.claim(&[key("01.flac")], &running);
        let fresh = BatchRef::new(
            TransferOwner {
                user_id: UserId::new("user-a"),
                batch_id: "b2".into(),
            },
            "Come Over".into(),
            running.target.clone(),
            ImportMode::Singleton,
        )
        .skipping_duplicates();
        let claim = registry.claim_for_request(&[key("01.flac"), key("02.flac")], &fresh);
        assert_eq!(
            (
                claim.batch.owner.batch_id.as_str(),
                claim.batch.duplicates,
                claim.reservation.is_none()
            ),
            ("b2", DuplicateRule::Skip, true)
        );
    }

    #[test]
    fn running_batches_lists_the_users_batches_once_each() {
        let registry = TransferRegistry::default();
        let advent = BatchRef::new(
            TransferOwner {
                user_id: UserId::new("user-a"),
                batch_id: "b0".into(),
            },
            "Advent".into(),
            PathBuf::from("/music"),
            ImportMode::Singleton,
        );
        registry.claim(
            &[key("01.flac"), key("02.flac")],
            &batch("user-a", "b1", ImportMode::Singleton),
        );
        registry.claim(&[key("03.flac")], &advent);
        registry.claim(
            &[key("04.flac")],
            &batch("user-b", "b2", ImportMode::Singleton),
        );
        assert_eq!(
            registry.running_batches(&UserId::new("user-a")),
            vec![
                RunningBatch {
                    batch_id: "b0".into(),
                    label: "Advent".into(),
                    transfers: 1
                },
                RunningBatch {
                    batch_id: "b1".into(),
                    label: "Come Over".into(),
                    transfers: 2
                },
            ]
        );
        assert!(registry.running_batches(&UserId::new("user-c")).is_empty());
    }

    fn slot(filename: &str) -> AddedSlot {
        AddedSlot {
            source: "peer_a".to_string(),
            filename: filename.to_string(),
            transfer_id: format!("t-{filename}"),
            alternates: vec![],
            max_sources: MAX_SOURCES_PER_TRACK,
            job: JobSlot {
                job: "j1".to_string(),
                slot: 0,
            },
        }
    }

    #[test]
    fn a_key_claimed_by_another_user_is_reported_as_other_user() {
        let registry = TransferRegistry::default();
        let alice = batch("alice", "a1", ImportMode::Singleton);
        registry.claim(&[key("01.flac")], &alice);

        let claim = registry.claim(
            &[key("01.flac"), key("02.flac")],
            &batch("bob", "b1", ImportMode::Singleton),
        );

        assert_eq!(claim.other_user, vec![key("01.flac")]);
        assert_eq!(claim.claimed, vec![key("02.flac")]);
        assert!(claim.mine.is_empty());
    }

    #[test]
    fn a_key_claimed_by_the_same_user_is_reported_as_mine_with_its_batch() {
        let registry = TransferRegistry::default();
        let running = batch("alice", "a1", ImportMode::Singleton);
        registry.claim(&[key("01.flac")], &running);

        let claim = registry.claim(
            &[key("01.flac"), key("02.flac")],
            &batch("alice", "a2", ImportMode::Singleton),
        );

        assert_eq!(claim.mine.len(), 1);
        assert_eq!(claim.mine[0].0, key("01.flac"));
        assert_eq!(claim.mine[0].1.owner, running.owner);
        assert!(Arc::ptr_eq(&claim.mine[0].1.inbox, &running.inbox));
        assert_eq!(claim.claimed, vec![key("02.flac")]);
        assert!(claim.other_user.is_empty());
    }

    #[test]
    fn a_key_repeated_in_one_request_is_reported_once() {
        let registry = TransferRegistry::default();
        registry.claim(
            &[key("02.flac")],
            &batch("alice", "a1", ImportMode::Singleton),
        );

        let claim = registry.claim(
            &[
                key("01.flac"),
                key("01.flac"),
                key("02.flac"),
                key("02.flac"),
            ],
            &batch("alice", "a2", ImportMode::Singleton),
        );

        assert_eq!(claim.claimed, vec![key("01.flac")]);
        assert_eq!(claim.mine.len(), 1);
        assert!(claim.other_user.is_empty());
    }

    #[test]
    fn release_lets_the_key_be_claimed_again() {
        let registry = TransferRegistry::default();
        let alice = batch("alice", "a1", ImportMode::Singleton);
        registry.claim(&[key("01.flac")], &alice);

        registry.release(&key("01.flac"), &alice.owner);

        let claim = registry.claim(
            &[key("01.flac")],
            &batch("bob", "b1", ImportMode::Singleton),
        );
        assert_eq!(claim.claimed, vec![key("01.flac")]);
    }

    #[test]
    fn a_late_release_leaves_a_key_someone_else_claimed_since() {
        let registry = TransferRegistry::default();
        let alice = batch("alice", "a1", ImportMode::Singleton);
        registry.claim(&[key("01.flac")], &alice);
        registry.release(&key("01.flac"), &alice.owner);
        let bob = batch("bob", "b1", ImportMode::Singleton);
        registry.claim(&[key("01.flac")], &bob);

        registry.release(&key("01.flac"), &alice.owner);

        assert!(registry.may_adopt(&key("01.flac"), &bob.owner));
        assert!(!registry.may_adopt(&key("01.flac"), &alice.owner));
    }

    #[test]
    fn releasing_a_batch_frees_only_its_own_keys() {
        let registry = TransferRegistry::default();
        let first = batch("alice", "a1", ImportMode::Singleton);
        registry.claim(&[key("01.flac"), key("02.flac")], &first);
        registry.claim(
            &[key("03.flac")],
            &batch("alice", "a2", ImportMode::Singleton),
        );

        registry.release_batch(&first.owner);

        let claim = registry.claim(
            &[key("01.flac"), key("02.flac"), key("03.flac")],
            &batch("bob", "b1", ImportMode::Singleton),
        );
        assert_eq!(claim.claimed, vec![key("01.flac"), key("02.flac")]);
        assert_eq!(claim.other_user, vec![key("03.flac")]);
    }

    #[test]
    fn rebind_refuses_another_users_key() {
        let registry = TransferRegistry::default();
        let alice = batch("alice", "a1", ImportMode::Singleton);
        registry.claim(&[key("01.flac")], &alice);
        registry.claim(
            &[key("02.flac")],
            &batch("bob", "b1", ImportMode::Singleton),
        );

        assert!(!registry.rebind(&key("01.flac"), key("02.flac"), &alice));
        // Nothing moved: alice still holds her key, bob his.
        assert!(registry.may_adopt(&key("01.flac"), &alice.owner));
        assert!(!registry.may_adopt(&key("02.flac"), &alice.owner));

        assert!(registry.rebind(&key("01.flac"), key("03.flac"), &alice));
        assert!(registry.may_adopt(&key("03.flac"), &alice.owner));
        let claim = registry.claim(
            &[key("01.flac")],
            &batch("bob", "b2", ImportMode::Singleton),
        );
        assert_eq!(claim.claimed, vec![key("01.flac")], "the old key was freed");
    }

    #[test]
    fn rebind_refuses_a_key_of_another_batch_of_the_same_user() {
        let registry = TransferRegistry::default();
        let mine = batch("alice", "a1", ImportMode::Singleton);
        let other = batch("alice", "a2", ImportMode::Singleton);
        registry.claim(&[key("01.flac")], &mine);
        registry.claim(&[key("02.flac")], &other);

        assert!(!registry.rebind(&key("01.flac"), key("02.flac"), &mine));
        assert!(registry.may_adopt(&key("01.flac"), &mine.owner));
        assert!(registry.may_adopt(&key("02.flac"), &other.owner));

        // Nor does it free an old key that another batch holds.
        assert!(registry.rebind(&key("02.flac"), key("03.flac"), &mine));
        assert!(registry.may_adopt(&key("02.flac"), &other.owner));
    }

    fn keys(range: std::ops::RangeInclusive<u32>) -> Vec<TransferKey> {
        range.map(|i| key(&format!("{i:02}.flac"))).collect()
    }

    #[test]
    fn a_request_joining_a_running_batch_claims_its_free_keys_for_that_batch() {
        let registry = TransferRegistry::default();
        let running = batch("alice", "r", ImportMode::Singleton);
        registry.claim(&keys(1..=5), &running);
        let fresh = batch("alice", "a", ImportMode::Singleton);

        let joined = registry.claim_for_request(&keys(1..=10), &fresh);

        assert_eq!(joined.batch.owner, running.owner);
        assert!(joined.reservation.is_some());
        assert_eq!(joined.claim.claimed, keys(6..=10));
        assert_eq!(joined.claim.mine.len(), 5);
        for k in keys(6..=10) {
            assert!(registry.may_adopt(&k, &running.owner));
            assert!(!registry.may_adopt(&k, &fresh.owner));
        }
    }

    #[test]
    fn a_second_request_never_sees_the_first_ones_unmonitored_batch() {
        // Request A joins the running batch R. Request B, for some of A's
        // files and more, comes right after A's claim and before A enqueues:
        // it must find R, not A's own batch, which never gets a monitor.
        let registry = TransferRegistry::default();
        let running = batch("alice", "r", ImportMode::Singleton);
        registry.claim(&keys(1..=5), &running);
        let fresh_a = batch("alice", "a", ImportMode::Singleton);
        let fresh_b = batch("alice", "b", ImportMode::Singleton);

        let a = registry.claim_for_request(&keys(1..=10), &fresh_a);
        let b = registry.claim_for_request(&keys(6..=12), &fresh_b);

        assert_eq!(a.batch.owner, running.owner);
        assert_eq!(b.batch.owner, running.owner);
        assert!(b
            .claim
            .mine
            .iter()
            .all(|(_, held)| held.owner == running.owner));
        assert_eq!(b.claim.claimed, keys(11..=12));
        for k in keys(1..=12) {
            assert!(!registry.may_adopt(&k, &fresh_a.owner), "{k:?}");
        }
    }

    #[test]
    fn a_request_whose_running_batch_finished_claims_for_itself() {
        let registry = TransferRegistry::default();
        let running = batch("alice", "r", ImportMode::Singleton);
        registry.claim(&keys(1..=1), &running);
        assert!(running.inbox.close_if_idle());
        let fresh = batch("alice", "a", ImportMode::Singleton);

        let own = registry.claim_for_request(&keys(1..=2), &fresh);

        assert_eq!(own.batch.owner, fresh.owner);
        assert!(own.reservation.is_none());
        assert!(registry.may_adopt(&key("02.flac"), &fresh.owner));
    }

    #[test]
    fn a_request_with_nothing_free_reserves_nothing() {
        let registry = TransferRegistry::default();
        let running = batch("alice", "r", ImportMode::Singleton);
        registry.claim(&keys(1..=2), &running);

        let none_free =
            registry.claim_for_request(&keys(1..=2), &batch("alice", "a", ImportMode::Singleton));

        assert!(none_free.reservation.is_none());
        assert!(running.inbox.close_if_idle(), "no reservation left behind");
    }

    #[test]
    fn may_adopt_only_unowned_or_own() {
        let registry = TransferRegistry::default();
        let mine = batch("alice", "a1", ImportMode::Singleton);
        registry.claim(&[key("01.flac")], &mine);
        registry.claim(
            &[key("02.flac")],
            &batch("alice", "a2", ImportMode::Singleton),
        );
        registry.claim(
            &[key("03.flac")],
            &batch("bob", "b1", ImportMode::Singleton),
        );

        assert!(registry.may_adopt(&key("00.flac"), &mine.owner), "unowned");
        assert!(
            registry.may_adopt(&key("01.flac"), &mine.owner),
            "own batch"
        );
        assert!(
            !registry.may_adopt(&key("02.flac"), &mine.owner),
            "another batch of the same user has its own monitor"
        );
        assert!(
            !registry.may_adopt(&key("03.flac"), &mine.owner),
            "another user"
        );
    }

    #[test]
    fn only_the_owning_user_can_cancel_a_transfer() {
        let reg = TransferRegistry::default();
        let mine = batch("user-a", "b1", ImportMode::Singleton);
        reg.claim(&[key("1.flac")], &mine);
        assert!(!reg.request_cancel(&key("1.flac"), &UserId::new("user-b")));
        assert!(!reg.is_cancelled(&key("1.flac"), &mine.owner));
        assert!(
            !reg.request_cancel(&key("2.flac"), &UserId::new("user-a")),
            "not held"
        );
        assert!(reg.request_cancel(&key("1.flac"), &UserId::new("user-a")));
        assert!(reg.is_cancelled(&key("1.flac"), &mine.owner));
    }

    #[test]
    fn a_cancel_moves_with_its_slot_and_ends_with_the_key() {
        let reg = TransferRegistry::default();
        let mine = batch("user-a", "b1", ImportMode::Singleton);
        reg.claim(&[key("1.flac")], &mine);
        reg.request_cancel(&key("1.flac"), &UserId::new("user-a"));
        reg.claim(&[key("2.flac")], &mine);
        reg.hand_over(&key("1.flac"), &key("2.flac"), &mine.owner);
        assert!(reg.is_cancelled(&key("2.flac"), &mine.owner));
        assert!(reg.may_adopt(
            &key("1.flac"),
            &batch("user-b", "b2", ImportMode::Singleton).owner
        ));
        let moved = TransferKey {
            peer: "peer_b".into(),
            ..key("2.flac")
        };
        assert!(reg.rebind(&key("2.flac"), moved.clone(), &mine));
        assert!(reg.is_cancelled(&moved, &mine.owner));
        reg.release(&moved, &mine.owner);
        reg.claim(std::slice::from_ref(&moved), &mine);
        assert!(
            !reg.is_cancelled(&moved, &mine.owner),
            "a released key starts clean"
        );
    }

    #[test]
    fn handing_a_key_over_to_itself_keeps_its_claim_and_mark() {
        let reg = TransferRegistry::default();
        let mine = batch("user-a", "b1", ImportMode::Singleton);
        reg.claim(&[key("1.flac")], &mine);
        reg.request_cancel(&key("1.flac"), &UserId::new("user-a"));

        reg.hand_over(&key("1.flac"), &key("1.flac"), &mine.owner);

        assert!(reg.is_cancelled(&key("1.flac"), &mine.owner));
    }

    #[test]
    fn a_transfer_rebound_by_failover_is_cancelled_under_its_new_peer() {
        let reg = TransferRegistry::default();
        let mine = batch("user-a", "b1", ImportMode::Singleton);
        reg.claim(&[key("1.flac")], &mine);
        let moved = TransferKey {
            peer: "peer_b".into(),
            ..key("1.flac")
        };

        assert!(reg.rebind(&key("1.flac"), moved.clone(), &mine));
        assert!(reg.request_cancel(&moved, &UserId::new("user-a")));
        assert!(!reg.request_cancel(&key("1.flac"), &UserId::new("user-a")));
    }

    #[test]
    fn a_batch_downloads_into_its_own_folder() {
        let b = batch("alice", "12345678-aaaa", ImportMode::Singleton);
        assert_eq!(b.destination, "Come Over [12345678]");
        assert_eq!(b.label, "Come Over");
    }

    #[test]
    fn a_closed_inbox_refuses_reservations() {
        let inbox = BatchInbox::new();
        assert!(inbox.close_if_idle());

        assert!(inbox.reserve().is_none());
    }

    #[test]
    fn close_if_idle_waits_for_outstanding_reservations() {
        let inbox = BatchInbox::new();
        let reservation = inbox.reserve().expect("open inbox");

        assert!(!inbox.close_if_idle());
        drop(reservation);
        assert!(inbox.close_if_idle());
    }

    #[test]
    fn close_if_idle_waits_for_delivered_slots_to_be_taken() {
        let inbox = BatchInbox::new();
        inbox
            .reserve()
            .expect("open inbox")
            .deliver(vec![slot("01.flac")])
            .expect("delivered");

        assert!(
            !inbox.close_if_idle(),
            "a delivered slot is not watched yet"
        );
        inbox.take();
        assert!(inbox.close_if_idle());
    }

    #[test]
    fn delivered_slots_are_taken_once() {
        let inbox = BatchInbox::new();
        let first = inbox.reserve().expect("open inbox");
        let second = inbox.reserve().expect("open inbox");
        first.deliver(vec![slot("01.flac")]).expect("delivered");
        second
            .deliver(vec![slot("02.flac"), slot("03.flac")])
            .expect("delivered");

        assert_eq!(
            inbox.take(),
            vec![slot("01.flac"), slot("02.flac"), slot("03.flac")]
        );
        assert!(inbox.take().is_empty());
    }

    #[test]
    fn a_forced_close_hands_back_undrained_slots_and_refuses_late_deliveries() {
        let inbox = BatchInbox::new();
        let late = inbox.reserve().expect("open inbox");
        inbox
            .reserve()
            .expect("open inbox")
            .deliver(vec![slot("01.flac")])
            .expect("delivered");

        assert_eq!(inbox.close(), vec![slot("01.flac")]);
        assert_eq!(
            late.deliver(vec![slot("02.flac")]),
            Err(vec![slot("02.flac")])
        );
        assert!(inbox.reserve().is_none());
    }

    #[test]
    fn a_request_does_not_join_a_batch_importing_in_another_mode() {
        let registry = TransferRegistry::default();
        let running = batch("alice", "r", ImportMode::Album);
        registry.claim(&keys(1..=5), &running);
        let fresh = batch("alice", "a", ImportMode::Singleton);

        let own = registry.claim_for_request(&keys(1..=10), &fresh);

        assert_eq!(own.batch.owner, fresh.owner);
        assert!(own.reservation.is_none());
        assert_eq!(own.claim.claimed, keys(6..=10));
        assert_eq!(own.claim.mine.len(), 5);
        assert!(
            running.inbox.close_if_idle(),
            "the album batch was not joined"
        );
    }
}
