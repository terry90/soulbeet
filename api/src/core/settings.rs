//! A user's settings.

use crate::auth::UserCtx;
use crate::core::database_failure;
use crate::core::index::{NavidromeOf, ServerNavidrome};
use crate::core::playlists::{discovery_key, lock};
use crate::models::user_settings::{
    UpdateUserSettings, UserSettings, LIFETIME_DAYS, PROMOTE_THRESHOLDS, TRACK_COUNTS,
};
use serde_json::Value;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::recommendation::DiscoveryProfile;
use sqlx::SqlitePool;

const PLAYLIST_NAME_MAX_CHARS: usize = 255;

pub async fn get(ctx: &UserCtx) -> Result<UserSettings, Failure> {
    get_in(&crate::db::DB, ctx).await
}

pub(crate) async fn get_in(db: &SqlitePool, ctx: &UserCtx) -> Result<UserSettings, Failure> {
    UserSettings::get_in(db, &ctx.user_id)
        .await
        .map_err(|e| database_failure(Stage::Input, e))
}

pub async fn update(ctx: &UserCtx, update: UpdateUserSettings) -> Result<UserSettings, Failure> {
    (update_in(&crate::db::DB, &ServerNavidrome, ctx, update).await).map_err(|n| n.failure)
}

/// A setting `update_in` checks, named in its refusal.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    DiscoveryProfiles,
    DiscoveryTrackCounts,
    DiscoveryLifetimes,
    DiscoveryPlaylistNames,
    DiscoveryPromoteThreshold,
}

/// A change `update_in` did not save: why, and the setting it refused, None when the failure is
/// not about one (the database).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NotSaved {
    pub failure: Failure,
    pub setting: Option<Setting>,
}

impl NotSaved {
    fn of(setting: Setting) -> impl FnOnce(Failure) -> NotSaved {
        move |failure| NotSaved {
            failure,
            setting: Some(setting),
        }
    }
}

fn refused(message: String) -> Failure {
    Failure::new(
        FailureCode::InvalidInput,
        Stage::Input,
        RetryAdvice::NotRetryable,
        message,
    )
}

fn unknown_profile(profile: &str) -> Failure {
    refused(format!(
        "Unknown discovery profile '{profile}': use Conservative, Balanced or Adventurous"
    ))
}

/// `update` with its discovery values checked against the bounds the Settings inputs carry, and
/// its playlist names trimmed (D14). Each map must name all three profiles.
pub(crate) fn checked(mut update: UpdateUserSettings) -> Result<UpdateUserSettings, NotSaved> {
    if let Some(profiles) = &update.discovery_profiles {
        let profiles_refused = NotSaved::of(Setting::DiscoveryProfiles);
        let names = || profiles.split(',').map(str::trim).filter(|p| !p.is_empty());
        if names().next().is_none() {
            return Err(profiles_refused(refused(
                "At least one discovery profile must be selected".into(),
            )));
        }
        let unknown = names().find(|p| !DiscoveryProfile::NAMES.contains(p));
        if let Some(profile) = unknown {
            return Err(profiles_refused(unknown_profile(profile)));
        }
    }
    if let Some(counts) = &update.discovery_track_count {
        let values = profile_values(counts, "track counts", Value::as_i64);
        for (profile, n) in values.map_err(NotSaved::of(Setting::DiscoveryTrackCounts))? {
            if !TRACK_COUNTS.contains(&n) {
                let (min, max) = (TRACK_COUNTS.start(), TRACK_COUNTS.end());
                return Err(NotSaved::of(Setting::DiscoveryTrackCounts)(refused(
                    format!("Discovery track count for {profile} must be {min} to {max}, not {n}"),
                )));
            }
        }
    }
    if let Some(lifetimes) = &update.discovery_lifetime_days {
        let values = profile_values(lifetimes, "lifetimes", Value::as_i64);
        for (profile, n) in values.map_err(NotSaved::of(Setting::DiscoveryLifetimes))? {
            if !LIFETIME_DAYS.contains(&n) {
                let (min, max) = (LIFETIME_DAYS.start(), LIFETIME_DAYS.end());
                return Err(NotSaved::of(Setting::DiscoveryLifetimes)(refused(format!(
                    "Discovery lifetime for {profile} must be {min} to {max} days, not {n}"
                ))));
            }
        }
    }
    if let Some(names) = &update.discovery_playlist_name {
        let names_refused = NotSaved::of(Setting::DiscoveryPlaylistNames);
        let trimmed = profile_values(names, "playlist names", |v| {
            v.as_str().map(|name| name.trim().to_string())
        });
        let trimmed = match trimmed {
            Ok(trimmed) => trimmed,
            Err(f) => return Err(names_refused(f)),
        };
        for (profile, name) in &trimmed {
            if name.is_empty() {
                return Err(names_refused(refused(format!(
                    "The discovery playlist name for {profile} is empty"
                ))));
            }
            if name.chars().count() > PLAYLIST_NAME_MAX_CHARS {
                return Err(names_refused(refused(format!(
                    "The discovery playlist name for {profile} is longer than {PLAYLIST_NAME_MAX_CHARS} characters"
                ))));
            }
        }
        let map: serde_json::Map<String, Value> = (trimmed.into_iter())
            .map(|(profile, name)| (profile.to_string(), Value::String(name)))
            .collect();
        update.discovery_playlist_name = Some(Value::Object(map).to_string());
    }
    if let Some(n) = update.discovery_promote_threshold {
        if !PROMOTE_THRESHOLDS.contains(&n) {
            let (min, max) = (PROMOTE_THRESHOLDS.start(), PROMOTE_THRESHOLDS.end());
            return Err(NotSaved::of(Setting::DiscoveryPromoteThreshold)(refused(
                format!("The discovery promote threshold must be {min} to {max}, not {n}"),
            )));
        }
    }
    Ok(update)
}

/// The value of each profile in the JSON object `json`, in profile order, read by `read`.
/// `what` names the map in a refusal.
fn profile_values<T>(
    json: &str,
    what: &str,
    read: impl Fn(&Value) -> Option<T>,
) -> Result<Vec<(&'static str, T)>, Failure> {
    let not_an_object = || {
        refused(format!(
            "Discovery {what} are not an object of profile to value"
        ))
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(json) else {
        return Err(not_an_object());
    };
    if let Some(profile) = map
        .keys()
        .find(|k| !DiscoveryProfile::NAMES.contains(&k.as_str()))
    {
        return Err(unknown_profile(profile));
    }
    if DiscoveryProfile::NAMES
        .iter()
        .any(|p| !map.contains_key(*p))
    {
        return Err(refused(format!(
            "Discovery {what} must name Conservative, Balanced and Adventurous"
        )));
    }
    DiscoveryProfile::NAMES
        .iter()
        .map(|p| {
            map.get(*p)
                .and_then(&read)
                .map(|v| (*p, v))
                .ok_or_else(not_an_object)
        })
        .collect()
}

/// Checks `update`, saves it, and deletes any Navidrome smart playlists it makes stale.
pub(crate) async fn update_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    update: UpdateUserSettings,
) -> Result<UserSettings, NotSaved> {
    let update = checked(update)?;
    let unsaved = |failure| NotSaved {
        failure,
        setting: None,
    };
    let old = get_in(db, ctx).await.map_err(unsaved)?;
    let result = UserSettings::upsert_in(db, &ctx.user_id, update.clone())
        .await
        .map_err(|e| unsaved(database_failure(Stage::Input, e)))?;
    cleanup_stale_discovery_playlists(db, navidrome, ctx, &old, &update).await;

    Ok(result)
}

/// Delete Navidrome smart playlists that are no longer valid because
/// discovery was disabled or profiles were removed.
async fn cleanup_stale_discovery_playlists(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    old: &UserSettings,
    update: &UpdateUserSettings,
) {
    use dioxus::logger::tracing::{info, warn};

    let discovery_disabled = update.discovery_enabled == Some(false) && old.discovery_enabled;
    let profiles_changed = update.discovery_profiles.is_some()
        && update.discovery_profiles.as_deref() != Some(&old.discovery_profiles);

    if !discovery_disabled && !profiles_changed {
        return;
    }

    let user_id = ctx.user_id.as_str();
    // A reconcile may have stored an id since `old` was read, so the ids are read under its lock.
    let _reconciling = lock(&discovery_key(user_id)).await;
    let stored = match UserSettings::get_in(db, user_id).await {
        Ok(settings) => settings.discovery_navidrome_playlist_id,
        Err(e) => {
            warn!("Could not read the discovery playlist ids of user {user_id}: {e}");
            return;
        }
    };
    let old_ids: std::collections::HashMap<String, String> = stored
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    if old_ids.is_empty() {
        return;
    }

    let navi = match navidrome.navidrome_of(&ctx.id()).await {
        Ok(c) => c,
        Err(_) => return,
    };

    // Determine which profile playlists to delete
    let profiles_to_delete: Vec<String> = if discovery_disabled {
        // All playlists are stale
        old_ids.keys().cloned().collect()
    } else {
        // Only profiles that were removed
        let new_profiles = update.discovery_profiles.as_deref().unwrap_or("");
        let new_set: std::collections::HashSet<&str> =
            new_profiles.split(',').map(|s| s.trim()).collect();
        old_ids
            .keys()
            .filter(|p| !new_set.contains(p.as_str()))
            .cloned()
            .collect()
    };

    // An id is forgotten only once its playlist is gone, so a failed delete is tried again on
    // the next change instead of leaving a playlist Soulful no longer knows.
    let mut deleted = Vec::new();
    for profile in &profiles_to_delete {
        if let Some(playlist_id) = old_ids.get(profile) {
            match navi.delete_smart_playlist(playlist_id).await {
                Ok(()) => {
                    info!(
                        "Deleted stale discovery playlist '{}' ({})",
                        profile, playlist_id
                    );
                    deleted.push(profile);
                }
                // A non-admin's delete of an id Navidrome no longer has answers 500 "data not
                // found" (persistence/playlist_repository.go Delete), so a read tells.
                Err(e) => match navi.get_native_playlist(playlist_id).await {
                    Ok(None) => deleted.push(profile),
                    _ => warn!(
                        "Failed to delete playlist '{}' ({}), its id is kept: {}",
                        profile, playlist_id, e
                    ),
                },
            }
        }
    }

    let remaining: std::collections::HashMap<String, String> = old_ids
        .into_iter()
        .filter(|(k, _)| !deleted.contains(&k))
        .collect();
    let new_json = if remaining.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&remaining).unwrap_or_default())
    };
    if let Err(e) = sqlx::query(
        "UPDATE user_settings SET discovery_navidrome_playlist_id = ? WHERE user_id = ?",
    )
    .bind(&new_json)
    .bind(user_id)
    .execute(db)
    .await
    {
        warn!("Could not clear the discovery playlist ids of user {user_id}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{folder_of, test_ctx};
    use crate::db::test_db;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{
        native_playlist, NavidromeFor, NavidromeStub, ADVENT, DISCOVERY_RULES,
    };

    const NOT_LINKED: NavidromeFor = NavidromeFor(Err(|| NavidromeLookupError::NotLinked));

    #[test]
    fn discovery_values_outside_the_settings_bounds_are_refused() {
        let counts = |json: &str| UpdateUserSettings {
            discovery_track_count: Some(json.into()),
            ..Default::default()
        };
        assert!(checked(counts(
            r#"{"Conservative":10,"Balanced":50,"Adventurous":1}"#
        ))
        .is_ok());
        let long = format!(
            r#"{{"Conservative":"Comfort Zone","Balanced":"{}","Adventurous":"Deep Cuts"}}"#,
            "x".repeat(256)
        );
        for (update, message) in [
            (counts(r#"{"Conservative":10,"Balanced":0,"Adventurous":10}"#), "Discovery track count for Balanced must be 1 to 50, not 0"),
            (counts(r#"{"Conservative":10,"Balanced":10}"#), "Discovery track counts must name Conservative, Balanced and Adventurous"),
            (counts(r#"{"Conservative":10,"Balanced":10,"Explore":10}"#), "Unknown discovery profile 'Explore': use Conservative, Balanced or Adventurous"),
            (counts("30"), "Discovery track counts are not an object of profile to value"),
            (
                UpdateUserSettings {
                    discovery_lifetime_days: Some(r#"{"Conservative":7,"Balanced":91,"Adventurous":7}"#.into()),
                    ..Default::default()
                },
                "Discovery lifetime for Balanced must be 1 to 90 days, not 91",
            ),
            (
                UpdateUserSettings { discovery_promote_threshold: Some(0), ..Default::default() },
                "The discovery promote threshold must be 1 to 5, not 0",
            ),
            (
                UpdateUserSettings { discovery_profiles: Some(String::new()), ..Default::default() },
                "At least one discovery profile must be selected",
            ),
            (
                UpdateUserSettings { discovery_profiles: Some("Conservative,Wild".into()), ..Default::default() },
                "Unknown discovery profile 'Wild': use Conservative, Balanced or Adventurous",
            ),
            (
                UpdateUserSettings {
                    discovery_playlist_name: Some(r#"{"Conservative":"Comfort Zone","Balanced":" ","Adventurous":"Deep Cuts"}"#.into()),
                    ..Default::default()
                },
                "The discovery playlist name for Balanced is empty",
            ),
            (
                UpdateUserSettings { discovery_playlist_name: Some(long), ..Default::default() },
                "The discovery playlist name for Balanced is longer than 255 characters",
            ),
        ] {
            let refused = checked(update.clone()).unwrap_err();
            let f = refused.failure;
            assert_eq!(
                (f.code, f.stage, f.message.as_str()),
                (FailureCode::InvalidInput, Stage::Input, message)
            );
            let setting = if update.discovery_track_count.is_some() {
                Setting::DiscoveryTrackCounts
            } else if update.discovery_lifetime_days.is_some() {
                Setting::DiscoveryLifetimes
            } else if update.discovery_promote_threshold.is_some() {
                Setting::DiscoveryPromoteThreshold
            } else if update.discovery_profiles.is_some() {
                Setting::DiscoveryProfiles
            } else {
                Setting::DiscoveryPlaylistNames
            };
            assert_eq!(refused.setting, Some(setting), "{message}");
        }
    }

    #[tokio::test]
    async fn a_refused_value_writes_nothing() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let update = UpdateUserSettings {
            accept_shares: Some(false),
            discovery_promote_threshold: Some(9),
            ..Default::default()
        };
        assert!(update_in(&db, &NOT_LINKED, &test_ctx("alice"), update)
            .await
            .is_err());
        assert!(UserSettings::get_in(&db, "alice")
            .await
            .unwrap()
            .accept_shares);
    }

    #[tokio::test]
    async fn a_dropped_profile_has_its_discovery_playlist_deleted_and_forgotten() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let ids = r#"{"Balanced":"9fR2kLm4NpQ7sT1vW3xY5z","Adventurous":"2kD8sP4vX7zQ1mN5bT3wYa"}"#;
        UserSettings::upsert_in(&db, "alice", UpdateUserSettings::default())
            .await
            .unwrap();
        sqlx::query("UPDATE user_settings SET discovery_navidrome_playlist_id = ?")
            .bind(ids)
            .execute(&db)
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        let of = NavidromeFor(Ok(navi.client()));
        let update = UpdateUserSettings {
            discovery_profiles: Some("Conservative,Balanced".into()),
            ..Default::default()
        };
        let saved = update_in(&db, &of, &test_ctx("alice"), update).await;
        assert_eq!(saved.unwrap().discovery_profiles, "Conservative,Balanced");
        assert_eq!(
            navi.asked(),
            ["/auth/login", "/api/playlist/2kD8sP4vX7zQ1mN5bT3wYa"]
        );
        let kept = UserSettings::get_in(&db, "alice").await.unwrap();
        assert_eq!(
            kept.discovery_navidrome_playlist_id.as_deref(),
            Some(r#"{"Balanced":"9fR2kLm4NpQ7sT1vW3xY5z"}"#)
        );
        // Discovery turned off: the rest goes too.
        let off = UpdateUserSettings {
            discovery_enabled: Some(false),
            ..Default::default()
        };
        sqlx::query("UPDATE user_settings SET discovery_enabled = 1")
            .execute(&db)
            .await
            .unwrap();
        update_in(&db, &of, &test_ctx("alice"), off).await.unwrap();
        assert_eq!(
            navi.asked().last().map(String::as_str),
            Some("/api/playlist/9fR2kLm4NpQ7sT1vW3xY5z")
        );
        let kept = UserSettings::get_in(&db, "alice").await.unwrap();
        assert_eq!(kept.discovery_navidrome_playlist_id, None);
    }

    #[tokio::test]
    async fn a_cleanup_deletes_the_playlist_stored_after_the_settings_were_read() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let on = UpdateUserSettings {
            discovery_enabled: Some(true),
            ..Default::default()
        };
        let old = UserSettings::upsert_in(&db, "alice", on).await.unwrap();
        // A reconcile stores this id between the read of `old` and the cleanup.
        sqlx::query("UPDATE user_settings SET discovery_navidrome_playlist_id = ?")
            .bind(r#"{"Balanced":"9fR2kLm4NpQ7sT1vW3xY5z"}"#)
            .execute(&db)
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        let off = UpdateUserSettings {
            discovery_enabled: Some(false),
            ..Default::default()
        };
        let of = NavidromeFor(Ok(navi.client()));
        cleanup_stale_discovery_playlists(&db, &of, &test_ctx("alice"), &old, &off).await;
        assert_eq!(
            navi.asked(),
            ["/auth/login", "/api/playlist/9fR2kLm4NpQ7sT1vW3xY5z"]
        );
        let kept = UserSettings::get_in(&db, "alice").await.unwrap();
        assert_eq!(kept.discovery_navidrome_playlist_id, None);
    }

    #[tokio::test]
    async fn a_playlist_whose_delete_failed_keeps_its_id_unless_navidrome_has_none() {
        let db = test_db::migrated().await;
        folder_of(&db, "alice").await;
        let on = UpdateUserSettings {
            discovery_enabled: Some(true),
            ..Default::default()
        };
        let old = UserSettings::upsert_in(&db, "alice", on).await.unwrap();
        sqlx::query("UPDATE user_settings SET discovery_navidrome_playlist_id = ?")
            .bind(format!(r#"{{"Balanced":"{ADVENT}","Adventurous":"gone"}}"#))
            .execute(&db)
            .await
            .unwrap();
        let navi = NavidromeStub::start();
        let kept_path = format!("/api/playlist/{ADVENT}");
        let playlist = native_playlist(ADVENT, "Fresh Picks", 2, false, Some(DISCOVERY_RULES));
        // Each delete fails, then the read finds the first playlist and not the second.
        navi.answer_with_status(
            &kept_path,
            &[
                (500, r#"{"error":"database is locked"}"#),
                (200, playlist.as_str()),
            ],
        );
        navi.answer_with_status(
            "/api/playlist/gone",
            &[
                (500, r#"{"error":"data not found"}"#),
                (404, r#"{"error":"playlist(id:gone) not found"}"#),
            ],
        );
        let off = UpdateUserSettings {
            discovery_enabled: Some(false),
            ..Default::default()
        };
        let of = NavidromeFor(Ok(navi.client()));
        cleanup_stale_discovery_playlists(&db, &of, &test_ctx("alice"), &old, &off).await;
        let mut asked = navi.asked();
        asked.sort();
        assert_eq!(
            asked,
            [
                kept_path.as_str(),
                kept_path.as_str(),
                "/api/playlist/gone",
                "/api/playlist/gone",
                "/auth/login"
            ]
        );
        let kept = UserSettings::get_in(&db, "alice").await.unwrap();
        assert_eq!(
            kept.discovery_navidrome_playlist_id,
            Some(format!(r#"{{"Balanced":"{ADVENT}"}}"#))
        );
    }

    #[tokio::test]
    async fn accept_shares_can_be_toggled() {
        let db = test_db::migrated().await;
        folder_of(&db, "bob").await;
        let bob = test_ctx("bob");
        assert!(get_in(&db, &bob).await.unwrap().accept_shares);
        let saved = update_in(
            &db,
            &NOT_LINKED,
            &bob,
            UpdateUserSettings {
                accept_shares: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!saved.accept_shares);
    }
}
