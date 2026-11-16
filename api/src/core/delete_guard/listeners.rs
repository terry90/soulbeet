//! Rules 3 and 4 of design section 13: no other listener rated, starred or kept the track in a
//! static playlist, and Navidrome's average shows no rater Soulful cannot read.

use super::{blocked_by_other_user, song_not_found, veto_unknown, Guard};
use crate::auth::UserId;
use crate::core::database_failure;
use crate::models::navidrome_read_failure::NavidromeReadFailureRow;
use crate::models::user::User;
use crate::services::{navidrome_client_for_user, NavidromeLookupError};
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use soulbeet::navidrome::models::{keeps_song_averages, NativeSong, SubsonicAlbum, SubsonicSong};
use soulbeet::navidrome::{NativePlaylist, NavidromeClient};
use soulbeet::ImportedItem;
use std::path::Path;
use std::sync::Arc;
use tracing::{info, warn};

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
    /// The playlists holding song `id` that `user` may see: public ones and their own, every one
    /// for a Navidrome admin.
    async fn song_playlists(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Vec<NativePlaylist>, NavidromeLookupError>;
    /// The present songs `user` sees that may be `item` (core::index::candidates); None when
    /// Navidrome cannot be searched for its tags.
    async fn songs_for_item(
        &self,
        user: &UserId,
        item: &ImportedItem,
    ) -> Result<Option<Vec<NativeSong>>, NavidromeLookupError>;
    /// Navidrome's version as `user`'s ping reports it; None when it reports none.
    async fn server_version(&self, user: &UserId) -> Result<Option<String>, NavidromeLookupError>;
    /// The name of every Navidrome user, listed as `user`; None when `user` is not a Navidrome
    /// admin and may not list them.
    async fn navidrome_users(
        &self,
        user: &UserId,
    ) -> Result<Option<Vec<String>>, NavidromeLookupError>;
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

    async fn song_playlists(
        &self,
        user: &UserId,
        id: &str,
    ) -> Result<Vec<NativePlaylist>, NavidromeLookupError> {
        let navi = client(user).await?;
        navi.get_song_playlists(id)
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

    async fn navidrome_users(
        &self,
        user: &UserId,
    ) -> Result<Option<Vec<String>>, NavidromeLookupError> {
        let navi = client(user).await?;
        navi.user_names()
            .await
            .map_err(NavidromeLookupError::Client)
    }
}

/// What rule 3 read: the ratings rule 4 compares Navidrome's average with, and whose.
pub(super) struct Read {
    /// The other listeners' ratings of the song above 0.
    ratings: Vec<u8>,
    /// The Soulful names of the linked accounts whose rating of the song this check reads: the
    /// actor's (rule 4 reads it) and each other listener who sees the song.
    names: Vec<String>,
    /// The Soulful names of the other linked accounts that cannot see the song, so their rating
    /// of it is not read.
    unseen: Vec<String>,
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
        beets_id: None,
        release_mbid: None,
        match_per_mille: None,
    }
}

impl Guard<'_> {
    /// Rule 3 over every user of User::linked_to_navidrome_in but `actor` (whose user name is
    /// `actor_name`), for `song`, the actor's record of the canonical file `path`: their positive
    /// track ratings, or blocked_by_other_user for a track rating above 1, a track star, an album
    /// rating above 1, an album star or a static playlist they may see that holds the song and
    /// is not the actor's (kept_in_playlist); veto_unknown when their marks cannot be read (a
    /// read error, a song answer without albumId, an album answer without userRating, D4, or no
    /// album for a song they see) or when another song they see is or may be the same file.
    /// Every listener is read, so each one's record for admins is current: a failed read is
    /// recorded, any other answer clears it. The refusal is that of the first listener who keeps
    /// the file.
    pub(super) async fn others_permit(
        &self,
        actor: &UserId,
        actor_name: &str,
        song: &NativeSong,
        path: &Path,
    ) -> Result<Read, Failure> {
        let users = User::linked_to_navidrome_in(self.db)
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?;
        let actor = actor.to_string();
        let mut read = Read {
            ratings: Vec::new(),
            names: users
                .iter()
                .filter(|u| u.id == actor)
                .map(|u| u.username.clone())
                .collect(),
            unseen: Vec::new(),
        };
        let mut refusal = None;
        for user in users.iter().filter(|u| u.id != actor) {
            let listener = UserId::new(&user.id);
            let marks = self.marks_of(&listener, actor_name, song, path).await;
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
                Ok(None) => read.unseen.push(user.username.clone()),
                Ok(Some(rating)) => {
                    read.names.push(user.username.clone());
                    if rating > 0 {
                        read.ratings.push(rating);
                    }
                }
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
            None => Ok(read),
        }
    }

    /// `listener`'s rating of `song` (0 when unrated), None when they cannot see it, or why they
    /// keep it. Their playlists are read whether or not they see the song: Navidrome lists a
    /// playlist's entries by the playlist's visibility, not by the library's
    /// (persistence/playlist_repository.go:238-256), so an entry added before they lost access
    /// still holds the file. The actor's own playlists (named `actor_name`) do not keep it.
    async fn marks_of(
        &self,
        listener: &UserId,
        actor_name: &str,
        song: &NativeSong,
        path: &Path,
    ) -> Result<Option<u8>, Keeps> {
        let unreadable = |e: NavidromeLookupError| Keeps::Unreadable(e.to_string());
        let blocked = || Keeps::Refused(blocked_by_other_user());
        let mut rating = None;
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
            let seen_rating = seen.user_rating.unwrap_or(0);
            rating = Some(seen_rating);
            if seen_rating > 1 || seen.starred.is_some() {
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
        let playlists = self
            .listeners
            .song_playlists(listener, &song.id)
            .await
            .map_err(unreadable)?;
        if kept_in_playlist(&playlists, actor_name) {
            return Err(blocked());
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

    /// Rule 3 in the actor's own read (D3), which also shows the public playlists of Navidrome
    /// users without a Soulful account: blocked_by_other_user when a static playlist holding
    /// `song_id` is not the actor's (`actor_name`, kept_in_playlist). A read error is the actor's
    /// own failure, with its error as the message.
    pub(super) async fn own_read_permits(
        &self,
        actor: &UserId,
        actor_name: &str,
        song_id: &str,
    ) -> Result<(), Failure> {
        let playlists = self
            .listeners
            .song_playlists(actor, song_id)
            .await
            .map_err(|e| e.failure(Stage::Delete, e.to_string()))?;
        match kept_in_playlist(&playlists, actor_name) {
            true => Err(blocked_by_other_user()),
            false => Ok(()),
        }
    }

    /// Rule 4: reads `actor`'s record of `song` again (N1), re-sends N1's rating so Navidrome
    /// recomputes the average, reads it again (N2), and compares N2's average with the mean of
    /// N1's rating (when above 0) and the other listeners' ratings in `others`: a difference
    /// beyond Navidrome's rounding is blocked_by_other_user. Navidrome before 0.60.0 or an
    /// unknown version keeps no average to compare, so every file is veto_unknown; a version
    /// the actor's own Navidrome does not answer is that read's own failure, with its error as
    /// the cause. An absent average means nobody rated the song (`omitempty`), which cannot be
    /// so while the actor rated it: that is veto_unknown too. When the ratings read are all 1
    /// (or none), an average that equals their mean has no other rating above 1 in it. When one
    /// is above 1, a Navidrome user Soulful cannot read who gave ratings that keep the mean
    /// hides in it, so the file passes only when no such user exists (every_rater_read). N2
    /// comes back for the caller to act on.
    pub(super) async fn no_unknown_rater(
        &self,
        actor: &UserId,
        song: &NativeSong,
        others: &Read,
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
            .chain(others.ratings.iter().copied())
            .collect();
        if !average_matches(average, positive_mean(&raters)) {
            return Err(blocked_by_other_user());
        }
        if raters.iter().any(|r| *r > 1) {
            self.every_rater_read(others).await?;
        }
        Ok(after)
    }

    /// Whether every Navidrome user is a linked account whose rating this check read (`read`):
    /// the first linked account Navidrome lets list its users (an admin's) lists them, and each
    /// name must be one of `read.names`, compared without regard to ASCII case as both Soulful
    /// (`COLLATE NOCASE`) and Navidrome's login (persistence/user_repository.go:203) compare
    /// names. The listing is read once per guard. Else unknown_listener: CannotSee when a name
    /// not read is a linked account that cannot see the song (signing in again cannot change
    /// that, so it is named before any user who never signed in), NotSignedIn otherwise.
    async fn every_rater_read(&self, read: &Read) -> Result<(), Failure> {
        let listed = self
            .navidrome_users
            .get_or_init(|| self.list_navidrome_users())
            .await
            .clone()?;
        let among =
            |names: &[String], name: &str| names.iter().any(|n| n.eq_ignore_ascii_case(name));
        let unread: Vec<&str> = listed
            .iter()
            .filter(|name| !among(&read.names, name))
            .map(String::as_str)
            .collect();
        if unread.is_empty() {
            return Ok(());
        }
        info!(
            "Navidrome users whose ratings Soulful did not read keep a file: {}",
            unread.join(", ")
        );
        Err(unknown_listener(
            if unread.iter().any(|name| among(&read.unseen, name)) {
                UnknownListener::CannotSee
            } else {
                UnknownListener::NotSignedIn
            },
        ))
    }

    /// Navidrome's users, as the first linked account that may list them lists them. None of
    /// them may (none is a Navidrome admin): unknown_listener NoAdmin; a read that failed
    /// before any list was read: unknown_listener Unreadable with its error.
    async fn list_navidrome_users(&self) -> Result<Vec<String>, Failure> {
        let users = User::linked_to_navidrome_in(self.db)
            .await
            .map_err(|e| database_failure(Stage::Delete, e))?;
        let mut failed = None;
        for user in &users {
            match self.listeners.navidrome_users(&UserId::new(&user.id)).await {
                Ok(Some(names)) => return Ok(names),
                Ok(None) => {}
                Err(e) => {
                    warn!("Could not list Navidrome's users as {}: {e}", user.username);
                    failed.get_or_insert(e);
                }
            }
        }
        Err(unknown_listener(match failed {
            Some(e) => UnknownListener::Unreadable(e),
            None => UnknownListener::NoAdmin,
        }))
    }
}

/// Whether one of `playlists` keeps their song for a listener other than `actor_name`: a static
/// playlist (a smart one's rules picked the song, not a person) whose owner is not the actor,
/// names compared without regard to ASCII case as Navidrome's login compares them
/// (persistence/user_repository.go:203).
fn kept_in_playlist(playlists: &[NativePlaylist], actor_name: &str) -> bool {
    playlists
        .iter()
        .any(|p| !p.is_smart() && !p.owner_name.eq_ignore_ascii_case(actor_name))
}

/// Why Soulful cannot show that no Navidrome user it cannot read rated a track.
enum UnknownListener {
    /// No linked account is a Navidrome admin, so Navidrome's users cannot be listed.
    NoAdmin,
    /// Listing them failed with this error.
    Unreadable(NavidromeLookupError),
    /// Navidrome lists a user who has no linked Soulful account.
    NotSignedIn,
    /// A linked account Navidrome lists cannot see the song (per-user libraries), so a rating
    /// it gave the song earlier stays in the average unread.
    CannotSee,
}

/// Rule 4 cannot rule out a rater Soulful cannot read; the next step says how it can.
fn unknown_listener(why: UnknownListener) -> Failure {
    let failure = |retry| {
        Failure::new(
            FailureCode::DeleteVetoUnknown,
            Stage::Delete,
            retry,
            "Another Navidrome listener Soulful cannot read may have rated this track",
        )
    };
    match why {
        UnknownListener::NoAdmin => failure(RetryAdvice::NeedsUser)
            .with_user_action(UserAction::new(
            UserActionCode::RelinkNavidrome,
            "Sign in to Soulful once as a Navidrome admin, so Soulful can list Navidrome's users",
            "Soulful sign-in",
        )),
        UnknownListener::NotSignedIn => failure(RetryAdvice::NeedsUser)
            .with_user_action(UserAction::new(
            UserActionCode::RelinkNavidrome,
            "Have every Navidrome user sign in to Soulful once, so Soulful can read their ratings",
            "Soulful sign-in",
        )),
        UnknownListener::CannotSee => Failure::new(
            FailureCode::DeleteVetoUnknown,
            Stage::Delete,
            RetryAdvice::NeedsUser,
            "A linked listener's Navidrome account cannot see this track, so Soulful cannot rule out a rating they gave it earlier, and the copy is kept",
        )
        .with_user_action(UserAction::new(
            UserActionCode::ConfigureNavidrome,
            "Give every Navidrome account linked to Soulful access to this track's library, so Soulful can read their ratings of it",
            "Navidrome's user settings",
        )),
        // The lookup error's own cause (Navidrome's answer), none for a stored password.
        UnknownListener::Unreadable(e) => Failure {
            cause: e.failure(Stage::Delete, String::new()).cause,
            ..failure(RetryAdvice::SameSourceLater)
        },
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
