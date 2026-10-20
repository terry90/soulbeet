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

use shared::download::batch_folder;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

/// Who started a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferOwner {
    pub user_id: String,
    pub batch_id: String,
}

/// A running batch: who owns it, its name, the folder its transfers are
/// enqueued into, the library folder they are imported into and the inbox
/// that takes more of them.
#[derive(Clone, Debug)]
pub struct BatchRef {
    pub owner: TransferOwner,
    pub label: String,
    pub destination: String,
    pub target: PathBuf,
    pub inbox: Arc<BatchInbox>,
}

impl BatchRef {
    /// A new batch downloading into `batch_folder(label, batch_id)` and
    /// importing into `target`.
    pub fn new(owner: TransferOwner, label: String, target: PathBuf) -> Self {
        let destination = batch_folder(&label, &owner.batch_id);
        Self {
            owner,
            label,
            destination,
            target,
            inbox: BatchInbox::new(),
        }
    }
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

#[derive(Default)]
pub struct TransferRegistry {
    owners: Mutex<HashMap<TransferKey, BatchRef>>,
}

static REGISTRY: LazyLock<TransferRegistry> = LazyLock::new(TransferRegistry::default);

impl TransferRegistry {
    pub fn global() -> &'static TransferRegistry {
        &REGISTRY
    }

    fn owners(&self) -> MutexGuard<'_, HashMap<TransferKey, BatchRef>> {
        self.owners.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Claims every free key for `batch` in one step. A key repeated in
    /// `keys` is reported once.
    pub fn claim(&self, keys: &[TransferKey], batch: &BatchRef) -> Claim {
        let mut owners = self.owners();
        let mut claim = Claim::default();
        let mut seen = HashSet::new();
        for key in keys.iter().filter(|key| seen.insert(*key)) {
            match owners.get(key) {
                None => {
                    owners.insert(key.clone(), batch.clone());
                    claim.claimed.push(key.clone());
                }
                Some(running) if running.owner.user_id == batch.owner.user_id => {
                    claim.mine.push((key.clone(), running.clone()));
                }
                Some(_) => claim.other_user.push(key.clone()),
            }
        }
        claim
    }

    /// Releases `key` if `owner` still holds it. A key released and claimed
    /// again by someone else meanwhile stays theirs.
    pub fn release(&self, key: &TransferKey, owner: &TransferOwner) {
        let mut owners = self.owners();
        if owners.get(key).is_some_and(|held| held.owner == *owner) {
            owners.remove(key);
        }
    }

    /// Releases every key the batch still holds.
    pub fn release_batch(&self, owner: &TransferOwner) {
        self.owners().retain(|_, batch| batch.owner != *owner);
    }

    /// Moves a claim from `old` to `new` and gives it to `batch`. Refused,
    /// with nothing changed, when another user holds `new`. `old` is only
    /// released when `batch`'s user holds it.
    pub fn rebind(&self, old: &TransferKey, new: TransferKey, batch: &BatchRef) -> bool {
        let mut owners = self.owners();
        let same_user = |held: &BatchRef| held.owner.user_id == batch.owner.user_id;
        if owners.get(&new).is_some_and(|held| !same_user(held)) {
            return false;
        }
        if owners.get(old).is_some_and(same_user) {
            owners.remove(old);
        }
        owners.insert(new, batch.clone());
        true
    }

    /// Whether a batch may start watching a transfer it did not enqueue:
    /// only one nobody owns or one it owns already. Another batch's transfer,
    /// even the same user's, has its own monitor.
    pub fn may_adopt(&self, key: &TransferKey, owner: &TransferOwner) -> bool {
        self.owners()
            .get(key)
            .is_none_or(|held| held.owner == *owner)
    }
}

/// A transfer a joining request enqueued into a running batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddedSlot {
    pub source: String,
    pub filename: String,
    pub transfer_id: String,
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

    fn batch(user_id: &str, batch_id: &str) -> BatchRef {
        BatchRef::new(
            TransferOwner {
                user_id: user_id.to_string(),
                batch_id: batch_id.to_string(),
            },
            "Come Over".to_string(),
            PathBuf::from("/music"),
        )
    }

    fn key(filename: &str) -> TransferKey {
        TransferKey {
            backend: "slskd",
            peer: "peer_a".to_string(),
            filename: filename.to_string(),
        }
    }

    fn slot(filename: &str) -> AddedSlot {
        AddedSlot {
            source: "peer_a".to_string(),
            filename: filename.to_string(),
            transfer_id: format!("t-{filename}"),
        }
    }

    #[test]
    fn a_key_claimed_by_another_user_is_reported_as_other_user() {
        let registry = TransferRegistry::default();
        let alice = batch("alice", "a1");
        registry.claim(&[key("01.flac")], &alice);

        let claim = registry.claim(&[key("01.flac"), key("02.flac")], &batch("bob", "b1"));

        assert_eq!(claim.other_user, vec![key("01.flac")]);
        assert_eq!(claim.claimed, vec![key("02.flac")]);
        assert!(claim.mine.is_empty());
    }

    #[test]
    fn a_key_claimed_by_the_same_user_is_reported_as_mine_with_its_batch() {
        let registry = TransferRegistry::default();
        let running = batch("alice", "a1");
        registry.claim(&[key("01.flac")], &running);

        let claim = registry.claim(&[key("01.flac"), key("02.flac")], &batch("alice", "a2"));

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
        registry.claim(&[key("02.flac")], &batch("alice", "a1"));

        let claim = registry.claim(
            &[
                key("01.flac"),
                key("01.flac"),
                key("02.flac"),
                key("02.flac"),
            ],
            &batch("alice", "a2"),
        );

        assert_eq!(claim.claimed, vec![key("01.flac")]);
        assert_eq!(claim.mine.len(), 1);
        assert!(claim.other_user.is_empty());
    }

    #[test]
    fn release_lets_the_key_be_claimed_again() {
        let registry = TransferRegistry::default();
        let alice = batch("alice", "a1");
        registry.claim(&[key("01.flac")], &alice);

        registry.release(&key("01.flac"), &alice.owner);

        let claim = registry.claim(&[key("01.flac")], &batch("bob", "b1"));
        assert_eq!(claim.claimed, vec![key("01.flac")]);
    }

    #[test]
    fn a_late_release_leaves_a_key_someone_else_claimed_since() {
        let registry = TransferRegistry::default();
        let alice = batch("alice", "a1");
        registry.claim(&[key("01.flac")], &alice);
        registry.release(&key("01.flac"), &alice.owner);
        let bob = batch("bob", "b1");
        registry.claim(&[key("01.flac")], &bob);

        registry.release(&key("01.flac"), &alice.owner);

        assert!(registry.may_adopt(&key("01.flac"), &bob.owner));
        assert!(!registry.may_adopt(&key("01.flac"), &alice.owner));
    }

    #[test]
    fn releasing_a_batch_frees_only_its_own_keys() {
        let registry = TransferRegistry::default();
        let first = batch("alice", "a1");
        registry.claim(&[key("01.flac"), key("02.flac")], &first);
        registry.claim(&[key("03.flac")], &batch("alice", "a2"));

        registry.release_batch(&first.owner);

        let claim = registry.claim(
            &[key("01.flac"), key("02.flac"), key("03.flac")],
            &batch("bob", "b1"),
        );
        assert_eq!(claim.claimed, vec![key("01.flac"), key("02.flac")]);
        assert_eq!(claim.other_user, vec![key("03.flac")]);
    }

    #[test]
    fn rebind_refuses_another_users_key() {
        let registry = TransferRegistry::default();
        let alice = batch("alice", "a1");
        registry.claim(&[key("01.flac")], &alice);
        registry.claim(&[key("02.flac")], &batch("bob", "b1"));

        assert!(!registry.rebind(&key("01.flac"), key("02.flac"), &alice));
        // Nothing moved: alice still holds her key, bob his.
        assert!(registry.may_adopt(&key("01.flac"), &alice.owner));
        assert!(!registry.may_adopt(&key("02.flac"), &alice.owner));

        assert!(registry.rebind(&key("01.flac"), key("03.flac"), &alice));
        assert!(registry.may_adopt(&key("03.flac"), &alice.owner));
        let claim = registry.claim(&[key("01.flac")], &batch("bob", "b2"));
        assert_eq!(claim.claimed, vec![key("01.flac")], "the old key was freed");
    }

    #[test]
    fn rebind_moves_a_key_to_another_batch_of_the_same_user() {
        let registry = TransferRegistry::default();
        let fresh = batch("alice", "a2");
        let running = batch("alice", "a1");
        registry.claim(&[key("01.flac")], &fresh);

        assert!(registry.rebind(&key("01.flac"), key("01.flac"), &running));

        assert!(registry.may_adopt(&key("01.flac"), &running.owner));
        assert!(!registry.may_adopt(&key("01.flac"), &fresh.owner));
    }

    #[test]
    fn may_adopt_only_unowned_or_own() {
        let registry = TransferRegistry::default();
        let mine = batch("alice", "a1");
        registry.claim(&[key("01.flac")], &mine);
        registry.claim(&[key("02.flac")], &batch("alice", "a2"));
        registry.claim(&[key("03.flac")], &batch("bob", "b1"));

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
    fn a_batch_downloads_into_its_own_folder() {
        let b = batch("alice", "12345678-aaaa");
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
}
