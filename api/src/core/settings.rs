//! A user's settings.

use crate::auth::UserCtx;
use crate::core::index::{NavidromeOf, ServerNavidrome};
use crate::core::{database_failure, folders::owned_in};
use crate::models::user_settings::{UpdateUserSettings, UserSettings};
use serde_json::Value;
use shared::failure::{Failure, FailureCode, RetryAdvice, Stage};
use shared::recommendation::DiscoveryProfile;
use sqlx::SqlitePool;
use std::ops::RangeInclusive;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    DefaultFolder,
    DiscoveryFolder,
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

/// The track count each discovery profile may have.
pub(crate) const TRACK_COUNTS: RangeInclusive<i64> = 1..=50;
/// The days a discovery profile's tracks may wait for a rating.
pub(crate) const LIFETIME_DAYS: RangeInclusive<i64> = 1..=90;
/// The rating at which the rating sync promotes a pending discovery track.
pub(crate) const PROMOTE_THRESHOLDS: RangeInclusive<u8> = 1..=5;

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

/// Checks `update`, then the folders it chooses, saves it, and deletes the Navidrome smart
/// playlists it makes stale. Nothing is saved when a check fails.
pub(crate) async fn update_in(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    update: UpdateUserSettings,
) -> Result<UserSettings, NotSaved> {
    let update = checked(update)?;
    check_folder_choices(db, ctx, &update).await?;
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

/// Refuses a discovery or default download folder the user does not own,
/// as "Folder not found", the same as for a folder that does not exist.
/// Discovery downloads into that folder and deletes files there. An empty
/// id clears the choice.
async fn check_folder_choices(
    db: &SqlitePool,
    ctx: &UserCtx,
    update: &UpdateUserSettings,
) -> Result<(), NotSaved> {
    let chosen = [
        (Setting::DefaultFolder, &update.default_download_folder_id),
        (Setting::DiscoveryFolder, &update.discovery_folder_id),
    ];
    for (setting, id) in chosen {
        if let Some(id) = id.as_deref().filter(|id| !id.is_empty()) {
            owned_in(db, ctx, id).await.map_err(|failure| {
                // A database failure is not about the folder.
                let setting = (failure.code == FailureCode::UnknownHandle).then_some(setting);
                NotSaved { failure, setting }
            })?;
        }
    }
    Ok(())
}

/// Delete Navidrome smart playlists that are no longer valid because
/// discovery was disabled, the folder changed, or profiles were removed.
async fn cleanup_stale_discovery_playlists(
    db: &SqlitePool,
    navidrome: &dyn NavidromeOf,
    ctx: &UserCtx,
    old: &UserSettings,
    update: &UpdateUserSettings,
) {
    use dioxus::logger::tracing::{info, warn};

    let discovery_disabled = update.discovery_enabled == Some(false) && old.discovery_enabled;
    let folder_changed = update.discovery_folder_id.is_some()
        && update.discovery_folder_id != old.discovery_folder_id;
    let profiles_changed = update.discovery_profiles.is_some()
        && update.discovery_profiles.as_deref() != Some(&old.discovery_profiles);

    if !discovery_disabled && !folder_changed && !profiles_changed {
        return;
    }

    let old_ids: std::collections::HashMap<String, String> = old
        .discovery_navidrome_playlist_id
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    if old_ids.is_empty() {
        return;
    }

    let user_id = ctx.user_id.as_str();
    let navi = match navidrome.navidrome_of(&ctx.id()).await {
        Ok(c) => c,
        Err(_) => return,
    };

    // Determine which profile playlists to delete
    let profiles_to_delete: Vec<String> = if discovery_disabled || folder_changed {
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

    for profile in &profiles_to_delete {
        if let Some(playlist_id) = old_ids.get(profile) {
            if let Err(e) = navi.delete_smart_playlist(playlist_id).await {
                warn!(
                    "Failed to delete playlist '{}' ({}): {}",
                    profile, playlist_id, e
                );
            } else {
                info!(
                    "Deleted stale discovery playlist '{}' ({})",
                    profile, playlist_id
                );
            }
        }
    }

    // Clear stale IDs from user_settings
    let remaining: std::collections::HashMap<String, String> = old_ids
        .into_iter()
        .filter(|(k, _)| !profiles_to_delete.contains(k))
        .collect();
    let new_json = if remaining.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&remaining).unwrap_or_default())
    };
    let _ = sqlx::query(
        "UPDATE user_settings SET discovery_navidrome_playlist_id = ? WHERE user_id = ?",
    )
    .bind(&new_json)
    .bind(user_id)
    .execute(db)
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::folders::folder_not_found;
    use crate::core::{folder_of, test_ctx};
    use crate::db::test_db;
    use crate::services::NavidromeLookupError;
    use crate::test_navidrome::{NavidromeFor, NavidromeStub};

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
        let folder = folder_of(&db, "alice").await;
        let update = UpdateUserSettings {
            default_download_folder_id: Some(folder),
            discovery_promote_threshold: Some(9),
            ..Default::default()
        };
        assert!(update_in(&db, &NOT_LINKED, &test_ctx("alice"), update)
            .await
            .is_err());
        assert_eq!(
            UserSettings::get_in(&db, "alice")
                .await
                .unwrap()
                .default_download_folder_id,
            None
        );
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
    async fn another_users_folder_cannot_be_chosen() {
        let db = test_db::migrated().await;
        let alice_folder = folder_of(&db, "alice").await;
        folder_of(&db, "bob").await;
        for update in [
            UpdateUserSettings {
                discovery_folder_id: Some(alice_folder.clone()),
                ..Default::default()
            },
            UpdateUserSettings {
                default_download_folder_id: Some(alice_folder.clone()),
                ..Default::default()
            },
        ] {
            let setting = match update.discovery_folder_id {
                Some(_) => Setting::DiscoveryFolder,
                None => Setting::DefaultFolder,
            };
            assert_eq!(
                check_folder_choices(&db, &test_ctx("bob"), &update).await,
                Err(NotSaved {
                    failure: folder_not_found(),
                    setting: Some(setting)
                })
            );
        }
    }

    #[tokio::test]
    async fn another_users_shared_folder_cannot_be_the_discovery_folder() {
        // Discovery removes files in its folder as the folder's owner, so a shared folder of
        // another user must not be one.
        let db = test_db::migrated().await;
        let alice_folder = folder_of(&db, "alice").await;
        folder_of(&db, "bob").await;
        sqlx::query("UPDATE folders SET shared = 1 WHERE id = ?")
            .bind(&alice_folder)
            .execute(&db)
            .await
            .unwrap();
        let update = UpdateUserSettings {
            discovery_folder_id: Some(alice_folder),
            ..Default::default()
        };
        assert_eq!(
            check_folder_choices(&db, &test_ctx("bob"), &update).await,
            Err(NotSaved {
                failure: folder_not_found(),
                setting: Some(Setting::DiscoveryFolder)
            })
        );
    }

    #[tokio::test]
    async fn own_folders_and_an_empty_choice_are_accepted() {
        let db = test_db::migrated().await;
        let bob_folder = folder_of(&db, "bob").await;
        let bob = test_ctx("bob");
        let update = UpdateUserSettings {
            discovery_folder_id: Some(bob_folder.clone()),
            default_download_folder_id: Some(bob_folder),
            ..Default::default()
        };
        assert_eq!(check_folder_choices(&db, &bob, &update).await, Ok(()));
        let cleared = UpdateUserSettings {
            discovery_folder_id: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(check_folder_choices(&db, &bob, &cleared).await, Ok(()));
        assert_eq!(
            check_folder_choices(&db, &bob, &UpdateUserSettings::default()).await,
            Ok(())
        );
    }
}
