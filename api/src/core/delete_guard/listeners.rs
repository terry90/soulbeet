//! Rules 3 and 4 of design section 13: no other listener rated or starred the track, and
//! Navidrome's average shows no rater Soulful cannot read.

use super::{blocked_by_other_user, song_not_found, veto_unknown, Guard};
use crate::auth::UserId;
use crate::core::database_failure;
use crate::models::navidrome_read_failure::NavidromeReadFailureRow;
use crate::models::user::User;
use crate::services::{navidrome_client_for_user, NavidromeLookupError};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use soulbeet::navidrome::models::{keeps_song_averages, NativeSong, SubsonicAlbum, SubsonicSong};
use soulbeet::navidrome::NavidromeClient;
use soulbeet::ImportedItem;
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

/// Navidrome as each Soulful user sees it.
#[async_trait::async_trait]
pub trait Listeners: Send + Sync {
    /// `user`'s native record of song `id`; None when they cannot see it.
    async fn native_song(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Option<NativeSong>, NavidromeLookupError>;
    /// getSong as `user`; None when they cannot see it.
    async fn song(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Option<SubsonicSong>, NavidromeLookupError>;
    /// getAlbum as `user`; None when they cannot see it.
    async fn album(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Option<SubsonicAlbum>, NavidromeLookupError>;
    async fn set_rating(
        &self,
        user: &UserId,
        id: &str,
        rating: u8,
    ) -> Result<(), NavidromeLookupError>;
    /// The present songs `user` sees that may be `item` (core::index::candidates); None when
    /// Navidrome cannot be searched for its tags.
    async fn songs_for_item(
        &self,
        user: &UserId,
        item: &ImportedItem,
    ) -> Result<Option<Vec<NativeSong>>, NavidromeLookupError>;
    /// Navidrome's version as `user`'s ping reports it; None when it reports none.
    async fn server_version(&self, user: &UserId) -> Result<Option<String>, NavidromeLookupError>;
}

/// Each user's client from navidrome_client_for_user.
pub struct LiveListeners;

async fn client(user: &UserId) -> Result<Arc<NavidromeClient>, NavidromeLookupError> {
    navidrome_client_for_user(&user.to_string()).await
}

#[async_trait::async_trait]
impl Listeners for LiveListeners {
    async fn native_song(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Option<NativeSong>, NavidromeLookupError> {
        let navi = client(user).await?;
        navi.get_native_song(id)
            .await
            .map_err(NavidromeLookupError::Client)
    }

    async fn song(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Option<SubsonicSong>, NavidromeLookupError> {
        let navi = client(user).await?;
        navi.get_song(id)
            .await
            .map_err(NavidromeLookupError::Client)
    }

    async fn album(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Option<SubsonicAlbum>, NavidromeLookupError> {
        let navi = client(user).await?;
        navi.get_album(id)
            .await
            .map_err(NavidromeLookupError::Client)
    }

    async fn set_rating(
        &self,
        user: &UserId,
        id: &str,
        rating: u8,
    ) -> Result<(), NavidromeLookupError> {
        let navi = client(user).await?;
        navi.set_rating(id, rating)
            .await
            .map_err(NavidromeLookupError::Client)
    }

    async fn songs_for_item(
        &self,
        user: &UserId,
        item: &ImportedItem,
    ) -> Result<Option<Vec<NativeSong>>, NavidromeLookupError> {
        let navi = client(user).await?;
        crate::core::index::candidates(&navi, item)
            .await
            .map_err(NavidromeLookupError::Client)
    }

    async fn server_version(&self, user: &UserId) -> Result<Option<String>, NavidromeLookupError> {
        let navi = client(user).await?;
        navi.server_version()
            .await
            .map_err(NavidromeLookupError::Client)
    }
}

/// Why one other listener keeps the file.
enum Keeps {
    /// Their ratings could not be read, for this reason; admins see it.
    Unreadable(String),
    /// Their reads succeeded and give this refusal.
    Refused(Failure),
}

/// The search Navidrome answers for the tags of `song`.
pub(super) fn item_of(song: &NativeSong) -> ImportedItem {
    ImportedItem {
        path: song.path.clone().into(),
        recording_mbid: song.mbz_recording_id.clone(),
        artist: song.artist.clone(),
        title: song.title.clone(),
        album: song.album.clone(),
    }
}

impl Guard<'_> {
    /// Rule 3 over every user of User::linked_to_navidrome_in but `actor`, for `song`, the
    /// actor's record of the canonical file `path`: their positive track ratings, or
    /// blocked_by_other_user for a track rating above 1, a track star, an album rating above 1
    /// or an album star; veto_unknown when their ratings cannot be read (a read error, a song
    /// answer without albumId, an album answer without userRating, D4, or no album for a song
    /// they see) or when another song they see is or may be the same file. Every listener is
    /// read, so each one's record for admins is current: a failed read is recorded, any other
    /// answer clears it. The refusal is that of the first listener who keeps the file.
    pub(super) async fn others_permit(
        &self,
        actor: &UserId,
        song: &NativeSong,
        path: &Path,
    ) -> Result<Vec<u8>, Failure> {
        let users = User::linked_to_navidrome_in(self.db)
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?;
        let (actor, mut ratings, mut refusal) = (actor.to_string(), Vec::new(), None);
        for user in users.iter().filter(|u| u.id != actor) {
            let listener = UserId::new(&user.id);
            let marks = self.marks_of(&listener, song, path).await;
            let recorded = match &marks {
                Err(Keeps::Unreadable(error)) => {
                    NavidromeReadFailureRow::record_in(self.db, &user.id, error).await
                }
                _ => NavidromeReadFailureRow::clear_in(self.db, &user.id).await,
            };
            if let Err(e) = recorded {
                warn!(
                    "Could not store whether Navidrome account {} can be read: {e}",
                    user.username
                );
            }
            match marks {
                Ok(0) => {}
                Ok(rating) => ratings.push(rating),
                Err(keeps) => {
                    refusal.get_or_insert(match keeps {
                        Keeps::Unreadable(_) => veto_unknown(),
                        Keeps::Refused(f) => f,
                    });
                }
            }
        }
        match refusal {
            Some(f) => Err(f),
            None => Ok(ratings),
        }
    }

    /// `listener`'s rating of `song` (0 when unrated or unseen), or why they keep it.
    async fn marks_of(
        &self,
        listener: &UserId,
        song: &NativeSong,
        path: &Path,
    ) -> Result<u8, Keeps> {
        let unreadable = |e: NavidromeLookupError| Keeps::Unreadable(e.to_string());
        let blocked = || Keeps::Refused(blocked_by_other_user());
        let mut rating = 0;
        if let Some(seen) = self
            .listeners
            .song(listener, &song.id)
            .await
            .map_err(unreadable)?
        {
            let Some(album_id) = seen.album_id.filter(|id| !id.is_empty()) else {
                return Err(Keeps::Unreadable(
                    "Navidrome answers this account without a song's album, as it answers a minimal client"
                        .into(),
                ));
            };
            rating = seen.user_rating.unwrap_or(0);
            if rating > 1 || seen.starred.is_some() {
                return Err(blocked());
            }
            let Some(album) = self
                .listeners
                .album(listener, &album_id)
                .await
                .map_err(unreadable)?
            else {
                return Err(Keeps::Unreadable(
                    "Navidrome does not show this account the album of a song it shows".into(),
                ));
            };
            let Some(album_rating) = album.user_rating else {
                return Err(Keeps::Unreadable(
                    "Navidrome answers this account without an album's rating, as it answers a legacy client"
                        .into(),
                ));
            };
            if album_rating > 1 || album.starred.is_some() {
                return Err(blocked());
            }
        }
        // The same file may be another song this listener sees (another library, a link), or
        // one whose file Soulful cannot find.
        let Some(candidates) = self
            .listeners
            .songs_for_item(listener, &item_of(song))
            .await
            .map_err(unreadable)?
        else {
            return Err(Keeps::Refused(veto_unknown()));
        };
        if self
            .another_record(song, path, &candidates)
            .await
            .map_err(Keeps::Refused)?
            != super::paths::Other::Different
        {
            return Err(Keeps::Refused(veto_unknown()));
        }
        Ok(rating)
    }

    /// Rule 4: reads `actor`'s record of `song` again (N1), re-sends N1's rating so Navidrome
    /// recomputes the average, reads it again (N2), and compares N2's average with the mean of
    /// N1's rating (when above 0) and `others`: a difference beyond Navidrome's rounding is
    /// blocked_by_other_user. Navidrome before 0.60.0 or an unknown version keeps no average to
    /// compare, so every file is veto_unknown; a version the actor's own Navidrome does not
    /// answer is that read's own failure, with its error as the cause. An absent average means
    /// nobody rated the song (`omitempty`), which cannot be so while the actor rated it: that is
    /// veto_unknown too. N2 comes back for the caller to act on.
    pub(super) async fn no_unknown_rater(
        &self,
        actor: &UserId,
        song: &NativeSong,
        others: &[u8],
    ) -> Result<NativeSong, Failure> {
        let own = |e: NavidromeLookupError| e.failure(Stage::Delete, e.to_string());
        let version = self.listeners.server_version(actor).await.map_err(|e| {
            e.failure(
                Stage::Delete,
                "Soulful could not read your Navidrome server's version, which this deletion check needs",
            )
        })?;
        if !version.as_deref().is_some_and(keeps_song_averages) {
            return Err(navidrome_too_old(version.as_deref()));
        }
        let before = self
            .listeners
            .native_song(actor, &song.id)
            .await
            .map_err(own)?
            .ok_or_else(song_not_found)?;
        self.listeners
            .set_rating(actor, &song.id, before.rating)
            .await
            .map_err(own)?;
        let after = self
            .listeners
            .native_song(actor, &song.id)
            .await
            .map_err(own)?
            .ok_or_else(song_not_found)?;
        let average = match after.average_rating {
            Some(average) => average,
            None if before.rating > 0 => return Err(veto_unknown()),
            None => 0.0,
        };
        let raters: Vec<u8> = std::iter::once(before.rating)
            .chain(others.iter().copied())
            .collect();
        if !average_matches(average, positive_mean(&raters)) {
            return Err(blocked_by_other_user());
        }
        Ok(after)
    }
}

/// Navidrome cannot show other listeners' ratings: it reports `version`.
fn navidrome_too_old(version: Option<&str>) -> Failure {
    Failure::new(
        FailureCode::DeleteVetoUnknown,
        Stage::Delete,
        RetryAdvice::SameSourceLater,
        format!(
            "Navidrome 0.60 or newer is needed to see other listeners' ratings (this server reports {})",
            version.unwrap_or("no version")
        ),
    )
}

/// The mean of the ratings above 0, 0 for none, unrounded.
fn positive_mean(ratings: &[u8]) -> f64 {
    let positive: Vec<f64> = ratings
        .iter()
        .filter(|r| **r > 0)
        .map(|r| f64::from(*r))
        .collect();
    if positive.is_empty() {
        return 0.0;
    }
    positive.iter().sum::<f64>() / positive.len() as f64
}

/// Whether Navidrome's `average`, which it rounds to 2 decimals in SQLite (`round(avg(rating),
/// 2)`, persistence/sql_annotations.go:150-158), is `mean`: they differ by no more than that
/// rounding.
fn average_matches(average: f64, mean: f64) -> bool {
    (average - mean).abs() <= 0.005 + 1e-9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_average_matches_a_mean_navidrome_rounded_either_way() {
        assert_eq!(positive_mean(&[]), 0.0);
        assert_eq!(positive_mean(&[0]), 0.0);
        assert_eq!(positive_mean(&[1, 0, 2, 2]), 5.0 / 3.0);
        // 9/8 = 1.125: SQLite may round the half-cent either way.
        let mean = positive_mean(&[2, 1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(mean, 1.125);
        assert!(average_matches(1.12, mean));
        assert!(average_matches(1.13, mean));
        assert!(!average_matches(1.14, mean));
        assert!(average_matches(1.67, positive_mean(&[1, 2, 2])));
    }
}
