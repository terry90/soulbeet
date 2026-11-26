//! Deterministic 5-second Undo state machine for Pending Discovery drops.
//!
//! Dropping a staged discovery track hides it optimistically from triage lists immediately
//! without blocking playback, and only commits `api::curation_drop` once the 5-second Undo
//! snackbar expires (or when a subsequent drop displaces it).

use std::collections::HashSet;

/// A staged pending-discovery drop awaiting either Undo or timer expiration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDropItem {
    /// Unique toast key so repeated drops reset the 5-second countdown bar cleanly.
    pub key: String,
    pub song_id: String,
    pub discovery_track_id: Option<String>,
    pub title: String,
    pub artist: String,
}

impl PendingDropItem {
    /// Identifier set used to hide this track from triage lists while pending commit.
    pub fn matches_id(&self, id: &str) -> bool {
        self.song_id == id
            || self
                .discovery_track_id
                .as_deref()
                .is_some_and(|did| did == id)
    }

    pub fn snackbar_message(&self) -> String {
        format!("Dropped \"{}\" by {}", self.title, self.artist)
    }
}

/// Manages optimistic hiding, single active Undo toast, flushing displaced drops, and
/// restoring items if Undo is clicked or server commit fails.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingDropQueue {
    active: Option<PendingDropItem>,
    hidden_ids: HashSet<String>,
    next_seq: u64,
}

impl PendingDropQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently visible Undo toast item, if any.
    pub fn active(&self) -> Option<&PendingDropItem> {
        self.active.as_ref()
    }

    /// Returns `true` if `id` (either `song_id` or `discovery_track_id`) is optimistically
    /// hidden while awaiting Undo expiration or in-flight server commit.
    pub fn is_hidden(&self, id: &str) -> bool {
        self.hidden_ids.contains(id)
    }

    /// Stages `item` as the active pending drop and hides its IDs optimistically.
    ///
    /// If another drop was already waiting in the Undo bar, returns that previous item
    /// so the caller commits it immediately (zero dropped mutations!).
    pub fn stage_drop(
        &mut self,
        song_id: String,
        discovery_track_id: Option<String>,
        title: String,
        artist: String,
    ) -> Option<PendingDropItem> {
        let displaced = self.active.take();
        self.next_seq = self.next_seq.saturating_add(1);
        let key = format!("drop-{}-{}", self.next_seq, song_id);

        if !song_id.is_empty() {
            self.hidden_ids.insert(song_id.clone());
        }
        if let Some(ref did) = discovery_track_id {
            if !did.is_empty() {
                self.hidden_ids.insert(did.clone());
            }
        }

        self.active = Some(PendingDropItem {
            key,
            song_id,
            discovery_track_id,
            title,
            artist,
        });

        displaced
    }

    /// Cancels the active pending drop (Undo clicked): un-hides the track immediately
    /// with zero server mutation.
    pub fn undo_active(&mut self) -> Option<PendingDropItem> {
        let item = self.active.take()?;
        self.unhide(&item);
        Some(item)
    }

    /// Pops the active pending drop when its 5-second timer finishes (or user clicks Dismiss),
    /// keeping its IDs hidden while the server commit runs.
    pub fn commit_active(&mut self) -> Option<PendingDropItem> {
        self.active.take()
    }

    /// Called after a server drop commit succeeds to clear tracking for `item`.
    pub fn finish_committed(&mut self, item: &PendingDropItem) {
        self.unhide(item);
    }

    /// Called if a server drop commit fails so the track reappears in triage immediately.
    pub fn restore_failed(&mut self, item: &PendingDropItem) {
        self.unhide(item);
    }

    fn unhide(&mut self, item: &PendingDropItem) {
        self.hidden_ids.remove(&item.song_id);
        if let Some(ref did) = item.discovery_track_id {
            self.hidden_ids.remove(did);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimistic_hide_and_undo_restores_without_server_commit() {
        let mut q = PendingDropQueue::new();
        let displaced = q.stage_drop(
            "song-1".into(),
            Some("disc-1".into()),
            "Midnight City".into(),
            "M83".into(),
        );
        assert!(displaced.is_none());
        assert!(q.is_hidden("song-1"));
        assert!(q.is_hidden("disc-1"));
        assert_eq!(
            q.active().unwrap().snackbar_message(),
            "Dropped \"Midnight City\" by M83"
        );

        let undone = q.undo_active().unwrap();
        assert_eq!(undone.song_id, "song-1");
        assert!(q.active().is_none());
        assert!(!q.is_hidden("song-1"));
        assert!(!q.is_hidden("disc-1"));
    }

    #[test]
    fn timer_commit_keeps_hidden_until_server_finishes_or_restores_on_failure() {
        let mut q = PendingDropQueue::new();
        q.stage_drop(
            "song-2".into(),
            Some("disc-2".into()),
            "Veridis Quo".into(),
            "Daft Punk".into(),
        );

        let committed = q.commit_active().unwrap();
        assert!(q.active().is_none());
        // Still hidden while HTTP call is in flight
        assert!(q.is_hidden("song-2"));
        assert!(q.is_hidden("disc-2"));

        // If server fails, track reappears immediately
        q.restore_failed(&committed);
        assert!(!q.is_hidden("song-2"));
        assert!(!q.is_hidden("disc-2"));
    }

    #[test]
    fn consecutive_drop_flushes_previous_item_for_immediate_commit() {
        let mut q = PendingDropQueue::new();
        assert!(q
            .stage_drop("s1".into(), Some("d1".into()), "A".into(), "Art".into())
            .is_none());
        let displaced = q
            .stage_drop("s2".into(), Some("d2".into()), "B".into(), "Art".into())
            .expect("first item must be returned to commit");
        assert_eq!(displaced.song_id, "s1");
        assert_eq!(q.active().unwrap().song_id, "s2");
        assert!(q.is_hidden("s1"));
        assert!(q.is_hidden("s2"));

        // Undoing s2 restores s2 while s1 stays hidden until its commit settles
        q.undo_active();
        assert!(!q.is_hidden("s2"));
        assert!(q.is_hidden("s1"));
        q.finish_committed(&displaced);
        assert!(!q.is_hidden("s1"));
    }
}
