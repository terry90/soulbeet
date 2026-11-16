//! Ratings and stars of the caller's own songs and albums, each checked to exist for the
//! caller first, and what the rating sync does with a rated pending discovery track.

use std::path::Path;

use shared::failure::{Failure, FailureCode, RetryAdvice, Stage, UserAction, UserActionCode};
use shared::navidrome::DiscoveryTrack;
use soulbeet::navidrome::models::SubsonicSong;
use soulbeet::navidrome::{NavidromeClient, StarTarget};
use soulbeet::NavidromeError;
use sqlx::SqlitePool;

use crate::auth::UserId;
use crate::core::database_failure;
use crate::core::delete_guard::song_not_found;
use crate::core::library::{album_not_found, navidrome_failure};
use crate::models::discovery_playlist::DiscoveryTrackRow;
use crate::models::user_settings::UserSettings;

/// A song or album, by its Navidrome id.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    Song(String),
    Album(String),
}

impl Target {
    pub(crate) fn id(&self) -> &str {
        match self {
            Target::Song(id) | Target::Album(id) => id,
        }
    }

    /// `id` is unknown, or in a library this account cannot see.
    fn not_found(&self) -> Failure {
        match self {
            Target::Song(_) => song_not_found(),
            Target::Album(id) => album_not_found(id),
        }
    }
}

/// What the next rating sync does with a song at its new rating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SyncEffect {
    PromotesDiscoveryTrack,
    RemovesDiscoveryTrack,
    DeletesFile,
}

impl SyncEffect {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SyncEffect::PromotesDiscoveryTrack => "promotes_discovery_track",
            SyncEffect::RemovesDiscoveryTrack => "removes_discovery_track",
            SyncEffect::DeletesFile => "deletes_file",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            SyncEffect::PromotesDiscoveryTrack => {
                "This is a pending discovery track: the next rating sync moves it into your library"
            }
            SyncEffect::RemovesDiscoveryTrack => {
                "This is a pending discovery track: the next rating sync deletes it"
            }
            SyncEffect::DeletesFile => {
                "Auto-delete is on: the next rating sync deletes this song's file unless the delete checks keep it"
            }
        }
    }
}

/// A rated or starred song or album as Navidrome listed it for the caller before the change.
/// `title`, `artist` and `album` are its tags, Navidrome's text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Annotated {
    pub target: Target,
    pub title: String,
    pub artist: Option<String>,
    /// The song's album; None for an album.
    pub album: Option<String>,
    pub previous_rating: Option<u8>,
    pub previously_starred: bool,
    /// What the next rating sync does with a rated song; None for an album and a star.
    pub effect: Option<SyncEffect>,
}

/// Why a rating or star was not made.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NotSet {
    /// Refused or failed before the write request was sent: nothing changed.
    Refused(Failure),
    /// The write request was sent and failed, so the change may have been made.
    WriteFailed(Failure),
}

impl NotSet {
    pub(crate) fn failure(&self) -> &Failure {
        match self {
            NotSet::Refused(f) | NotSet::WriteFailed(f) => f,
        }
    }

    pub(crate) fn after_write(&self) -> bool {
        matches!(self, NotSet::WriteFailed(_))
    }
}

/// A 1-star song rating from a connected app while the user's auto-delete is on: the next rating
/// sync would delete the file, also of a pending discovery track that is promoted first.
fn one_star_refused() -> Failure {
    Failure::new(
        FailureCode::PermissionNotGranted,
        Stage::Input,
        RetryAdvice::NeedsUser,
        "A 1-star rating from a connected app would have Soulful delete this song's file at the next rating sync, because auto-delete is on. Rate it 1 star yourself in Navidrome, or turn auto-delete off in Soulful.",
    )
    .with_user_action(UserAction::new(
        UserActionCode::RateInNavidrome,
        "Rate the song 1 star yourself in Navidrome",
        "Navidrome",
    ))
}

/// Sets `user`'s rating of `target` to `rating` (0 clears it) for a connected app, after
/// reading the target as Navidrome lists it for `navi`'s account (a song or album it lists for
/// no one, or in a library this account cannot see, is not found). For a song, `effect` says
/// what the next rating sync does with it; a 1-star song rating is refused while the user's
/// auto-delete is on, or when that setting cannot be read, before anything is written.
pub(crate) async fn rate(
    db: &SqlitePool,
    navi: &NavidromeClient,
    user: &UserId,
    target: &Target,
    rating: u8,
) -> Result<Annotated, NotSet> {
    let (mut annotated, song) = read(navi, target).await.map_err(NotSet::Refused)?;
    if let Some(song) = song {
        annotated.effect = sync_effect(db, user, song, rating)
            .await
            .map_err(NotSet::Refused)?;
    }
    navi.set_rating(target.id(), rating)
        .await
        .map_err(|e| NotSet::WriteFailed(write_failure(target, &e)))?;
    Ok(annotated)
}

/// Stars or unstars `target` after reading it as `rate` does; `effect` is None.
pub(crate) async fn star(
    navi: &NavidromeClient,
    target: &Target,
    starred: bool,
) -> Result<Annotated, NotSet> {
    let (annotated, _) = read(navi, target).await.map_err(NotSet::Refused)?;
    let star_target = match target {
        Target::Song(id) => StarTarget::Song(id),
        Target::Album(id) => StarTarget::Album(id),
    };
    navi.set_star(star_target, starred)
        .await
        .map_err(|e| NotSet::WriteFailed(write_failure(target, &e)))?;
    Ok(annotated)
}

/// `target` as Navidrome lists it for `navi`'s account, and the song when it is one.
async fn read(
    navi: &NavidromeClient,
    target: &Target,
) -> Result<(Annotated, Option<SubsonicSong>), Failure> {
    let failed = |e: NavidromeError| navidrome_failure(Stage::LibraryCheck, &e);
    match target {
        Target::Song(id) => {
            let song = (navi.get_song(id).await.map_err(failed)?).ok_or_else(song_not_found)?;
            let annotated = Annotated {
                target: target.clone(),
                title: song.title.clone(),
                artist: song.artist.clone(),
                album: song.album.clone(),
                previous_rating: rated(song.user_rating),
                previously_starred: song.starred.is_some(),
                effect: None,
            };
            Ok((annotated, Some(song)))
        }
        Target::Album(id) => {
            let album =
                (navi.get_album(id).await.map_err(failed)?).ok_or_else(|| album_not_found(id))?;
            let annotated = Annotated {
                target: target.clone(),
                title: album.name.or(album.title).unwrap_or_default(),
                artist: album.artist,
                album: None,
                previous_rating: rated(album.user_rating),
                previously_starred: album.starred.is_some(),
                effect: None,
            };
            Ok((annotated, None))
        }
    }
}

/// A rating as Navidrome sends it, None when unrated: getAlbum sends 0 then.
fn rated(rating: Option<u8>) -> Option<u8> {
    rating.filter(|r| *r > 0)
}

/// What the next rating sync does with `song` rated `rating`, as the sync decides it: the
/// pending discovery track of the user's discovery folder it is promoted or removed, else
/// nothing. A 1-star rating while auto-delete is on is refused.
async fn sync_effect(
    db: &SqlitePool,
    user: &UserId,
    song: SubsonicSong,
    rating: u8,
) -> Result<Option<SyncEffect>, Failure> {
    let unreadable = |e: String| database_failure(Stage::Input, e);
    let settings = UserSettings::get_in(db, &user.to_string())
        .await
        .map_err(unreadable)?;
    if rating == 1 && settings.auto_delete_enabled {
        return Err(one_star_refused());
    }
    let pending = match &settings.discovery_folder_id {
        Some(folder) => DiscoveryTrackRow::get_pending_by_folder_in(db, folder)
            .await
            .map_err(unreadable)?,
        None => vec![],
    };
    // The song as the sync will list it: Navidrome lists a cleared rating as none.
    let rated = SubsonicSong {
        user_rating: (rating > 0).then_some(rating),
        ..song
    };
    Ok(
        match pending_action(&rated, &pending, settings.discovery_promote_threshold) {
            Some(PendingAction::Promote(_)) => Some(SyncEffect::PromotesDiscoveryTrack),
            Some(PendingAction::Remove(_)) => Some(SyncEffect::RemovesDiscoveryTrack),
            None => None,
        },
    )
}

/// A failed write of `target`: Navidrome's "data not found" (Subsonic 70) is the target gone.
fn write_failure(target: &Target, e: &NavidromeError) -> Failure {
    if e.is_not_found() {
        target.not_found()
    } else {
        navidrome_failure(Stage::LibraryCheck, e)
    }
}

/// The pending discovery track `song` is: the track's song id when it has one, else the file
/// name of the song's Subsonic path, ignoring ASCII case.
pub(crate) fn pending_track_for<'a>(
    song: &SubsonicSong,
    pending: &'a [DiscoveryTrack],
) -> Option<&'a DiscoveryTrack> {
    let file_name = |path: &str| Path::new(path).file_name().map(|f| f.to_ascii_lowercase());
    pending.iter().find(|track| match &track.song_id {
        Some(song_id) => *song_id == song.id,
        None => song
            .path
            .as_deref()
            .and_then(file_name)
            .is_some_and(|name| Some(name) == file_name(&track.path)),
    })
}

/// What the rating sync does with a pending discovery track the user rated.
pub(crate) enum PendingAction<'a> {
    Promote(&'a DiscoveryTrack),
    Remove(&'a DiscoveryTrack),
}

/// Promote the pending track of `song` at `promote_threshold` or above, else remove it at
/// rating 1.
pub(crate) fn pending_action<'a>(
    song: &SubsonicSong,
    pending: &'a [DiscoveryTrack],
    promote_threshold: u8,
) -> Option<PendingAction<'a>> {
    let rating = song.user_rating?;
    let track = pending_track_for(song, pending)?;
    if rating >= promote_threshold {
        Some(PendingAction::Promote(track))
    } else if rating == 1 {
        Some(PendingAction::Remove(track))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::delete_guard::test_support::Library;
    use crate::db::test_db;
    use crate::models::user_settings::UpdateUserSettings;
    use crate::test_navidrome::{
        album_body, song_body, subsonic_failed, subsonic_song, NavidromeStub, HOLY,
    };
    use shared::navidrome::DiscoveryStatus;

    const WINTER: &str = "7hQ2nLr5VxZ0cYbM8wTkAf";

    async fn set(db: &SqlitePool, update: UpdateUserSettings) {
        UserSettings::upsert_in(db, "alice", update).await.unwrap();
    }

    fn auto_delete(on: bool) -> UpdateUserSettings {
        UpdateUserSettings {
            auto_delete_enabled: Some(on),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_one_star_rating_is_refused_while_auto_delete_is_on() {
        let lib = Library::new().await;
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getSong",
            &[&song_body(HOLY.0, HOLY.1, Some(4), false)],
        );
        let (alice, holy) = (lib.alice(), Target::Song(HOLY.0.into()));
        let a = rate(&lib.db, &navi.client(), &alice, &holy, 1)
            .await
            .unwrap();
        assert_eq!(
            a,
            Annotated {
                target: holy.clone(),
                title: HOLY.1.into(),
                artist: Some("Marrow Choir".into()),
                album: Some("Winter Songs".into()),
                previous_rating: Some(4),
                previously_starred: false,
                effect: None,
            }
        );
        // A pending discovery track of the discovery folder with that song id: the sync removes
        // it as discovery's own cleanup.
        let staged = lib.root.join("music/alice/Discovery/Explore/03.flac");
        lib.pending_track("t1", "alice-music", "Explore", &staged, Some(HOLY.0))
            .await;
        set(
            &lib.db,
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                ..Default::default()
            },
        )
        .await;
        let a = rate(&lib.db, &navi.client(), &alice, &holy, 1).await;
        assert_eq!(a.unwrap().effect, Some(SyncEffect::RemovesDiscoveryTrack));
        let sent = format!("id={}&rating=1", HOLY.0);
        assert_eq!(navi.posted("/rest/setRating"), [sent.clone(), sent.clone()]);

        // On: refused, pending track or not (it could be promoted first, then deleted).
        set(&lib.db, auto_delete(true)).await;
        assert_eq!(
            rate(&lib.db, &navi.client(), &alice, &holy, 1).await,
            Err(NotSet::Refused(one_star_refused()))
        );
        // A setting that cannot be read refuses too.
        lib.db.close().await;
        let e = rate(&lib.db, &navi.client(), &alice, &holy, 1)
            .await
            .unwrap_err();
        assert_eq!(
            (e.after_write(), e.failure().code),
            (false, FailureCode::SettingsUnreadable)
        );
        assert_eq!(navi.posted("/rest/setRating"), [sent.clone(), sent]);
    }

    #[tokio::test]
    async fn a_pending_track_rated_at_the_threshold_is_promoted() {
        let lib = Library::new().await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getSong", &[&song_body(HOLY.0, HOLY.1, None, true)]);
        // No song id yet: the sync matches it by the file name of the song's path.
        let staged = lib
            .root
            .join("music/alice/Discovery/Balanced/01-03 - O Holy Night.flac");
        lib.pending_track("t1", "alice-music", "Balanced", &staged, None)
            .await;
        set(
            &lib.db,
            UpdateUserSettings {
                discovery_folder_id: Some("alice-music".into()),
                auto_delete_enabled: Some(true),
                ..Default::default()
            },
        )
        .await;
        let holy = Target::Song(HOLY.0.into());
        let a = rate(&lib.db, &navi.client(), &lib.alice(), &holy, 3)
            .await
            .unwrap();
        assert_eq!(
            (a.previous_rating, a.previously_starred, a.effect),
            (None, true, Some(SyncEffect::PromotesDiscoveryTrack))
        );
        let a = rate(&lib.db, &navi.client(), &lib.alice(), &holy, 2)
            .await
            .unwrap();
        assert_eq!(a.effect, None);
    }

    #[tokio::test]
    async fn an_album_rated_one_star_is_written_while_auto_delete_is_on() {
        // Auto-delete reads song ratings only.
        let lib = Library::new().await;
        set(&lib.db, auto_delete(true)).await;
        let navi = NavidromeStub::start();
        navi.answer("/rest/getAlbum", &[&album_body(WINTER, "Winter Songs", 0)]);
        let winter = Target::Album(WINTER.into());
        let a = rate(&lib.db, &navi.client(), &lib.alice(), &winter, 1)
            .await
            .unwrap();
        assert_eq!(
            a,
            Annotated {
                target: winter,
                title: "Winter Songs".into(),
                artist: Some("Marrow Choir".into()),
                album: None,
                previous_rating: None,
                previously_starred: true,
                effect: None,
            }
        );
        assert_eq!(
            navi.posted("/rest/setRating"),
            [format!("id={WINTER}&rating=1")]
        );
    }

    #[tokio::test]
    async fn a_star_reports_what_was_starred_before() {
        let navi = NavidromeStub::start();
        navi.answer(
            "/rest/getSong",
            &[&song_body(HOLY.0, HOLY.1, Some(2), true)],
        );
        navi.answer("/rest/getAlbum", &[&album_body(WINTER, "Winter Songs", 5)]);
        let a = star(&navi.client(), &Target::Song(HOLY.0.into()), true)
            .await
            .unwrap();
        assert_eq!(
            (a.previous_rating, a.previously_starred, a.effect),
            (Some(2), true, None)
        );
        let a = star(&navi.client(), &Target::Album(WINTER.into()), false)
            .await
            .unwrap();
        assert_eq!(
            (a.title.as_str(), a.previous_rating),
            ("Winter Songs", Some(5))
        );
        assert_eq!(navi.posted("/rest/star"), [format!("id={}", HOLY.0)]);
        assert_eq!(navi.posted("/rest/unstar"), [format!("albumId={WINTER}")]);
    }

    #[tokio::test]
    async fn an_unknown_song_or_album_is_never_rated_or_starred() {
        let navi = NavidromeStub::start();
        // server/subsonic/browsing.go:251-253 and :194-197 (0.64.2); also what a song in a library
        // this account cannot see answers.
        navi.answer("/rest/getSong", &[&subsonic_failed(70, "Song not found")]);
        navi.answer("/rest/getAlbum", &[&subsonic_failed(70, "Album not found")]);
        let db = test_db::migrated().await;
        let alice = UserId::new("alice");
        assert_eq!(
            rate(&db, &navi.client(), &alice, &Target::Song("gone".into()), 5).await,
            Err(NotSet::Refused(song_not_found()))
        );
        assert_eq!(
            star(&navi.client(), &Target::Album("gone".into()), true).await,
            Err(NotSet::Refused(album_not_found("gone")))
        );
        assert!(!navi
            .asked()
            .iter()
            .any(|p| p == "/rest/setRating" || p == "/rest/star"));
    }

    #[tokio::test]
    async fn a_song_that_goes_before_the_rating_is_not_found() {
        let navi = NavidromeStub::start();
        navi.answer("/rest/getSong", &[&song_body(HOLY.0, HOLY.1, None, false)]);
        // server/subsonic/media_annotation.go:42-46 (model.GetEntityByID), api.go:304-305
        navi.answer("/rest/setRating", &[&subsonic_failed(70, "data not found")]);
        let db = test_db::migrated().await;
        let holy = Target::Song(HOLY.0.into());
        assert_eq!(
            rate(&db, &navi.client(), &UserId::new("alice"), &holy, 2).await,
            Err(NotSet::WriteFailed(song_not_found()))
        );
    }

    #[test]
    fn a_track_with_a_song_id_matches_only_that_song() {
        let track = DiscoveryTrack {
            id: "t1".into(),
            song_id: Some("s5".into()),
            title: "O Holy Night".into(),
            artist: "Marrow Choir".into(),
            album: "Winter Songs".into(),
            path: "/music/alice/Discovery/Explore/03.flac".into(),
            folder_id: "alice-music".into(),
            profile: "Explore".into(),
            rating: None,
            status: DiscoveryStatus::Pending,
            created_at: "2026-11-15 20:00:00".into(),
        };
        let same_name = SubsonicSong {
            path: Some("Marrow Choir/Explore/03.flac".into()),
            ..subsonic_song("s6", "O Holy Night", Some(1))
        };
        assert!(pending_track_for(&same_name, std::slice::from_ref(&track)).is_none());
        assert!(pending_track_for(
            &subsonic_song("s5", "O Holy Night", Some(1)),
            std::slice::from_ref(&track)
        )
        .is_some());
    }
}
