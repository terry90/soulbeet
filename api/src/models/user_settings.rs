#[cfg(feature = "server")]
use crate::db::DB;
use serde::{Deserialize, Serialize};
use std::ops::RangeInclusive;

// The bounds the server holds the discovery numbers to, shared with the Settings inputs so both
// sides refuse the same values.
/// The track count each discovery profile may have.
pub const TRACK_COUNTS: RangeInclusive<i64> = 1..=50;
/// The days a discovery profile's tracks may wait for a rating.
pub const LIFETIME_DAYS: RangeInclusive<i64> = 1..=90;
/// The rating at which the rating sync promotes a pending discovery track.
pub const PROMOTE_THRESHOLDS: RangeInclusive<u8> = 1..=5;

/// What auto-download does with a backend whose files are lossy only, when no other backend has a
/// match it would take.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(sqlx::Type))]
#[cfg_attr(feature = "server", sqlx(rename_all = "snake_case"))]
#[serde(rename_all = "snake_case")]
pub enum LossyFallback {
    /// Not searched, and never offered.
    Never,
    /// Searched, and offered to pick from; never picked on its own.
    #[default]
    Ask,
    /// Searched, and picked on its own when it scores well enough.
    Auto,
}

impl LossyFallback {
    pub const ALL: [LossyFallback; 3] = [
        LossyFallback::Never,
        LossyFallback::Ask,
        LossyFallback::Auto,
    ];

    /// The stored text of this choice.
    pub fn as_str(self) -> &'static str {
        match self {
            LossyFallback::Never => "never",
            LossyFallback::Ask => "ask",
            LossyFallback::Auto => "auto",
        }
    }

    /// The choice stored as `text`.
    pub fn parse(text: &str) -> Option<LossyFallback> {
        Self::ALL.into_iter().find(|c| c.as_str() == text)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct UserSettings {
    pub user_id: String,
    pub last_search_type: Option<String>,
    pub auto_delete_enabled: bool,
    pub discovery_promote_threshold: u8,
    pub navidrome_banner_dismissed: bool,
    pub listenbrainz_username: Option<String>,
    pub listenbrainz_token: Option<String>,
    pub discovery_enabled: bool,
    pub discovery_folder_id: Option<String>,
    pub discovery_track_count: String,
    pub discovery_lifetime_days: String,
    pub discovery_profiles: String,
    pub discovery_playlist_name: String,
    pub discovery_navidrome_playlist_id: Option<String>,
    pub discovery_last_generated_at: Option<String>,
    pub default_download_folder_id: Option<String>,
    pub deezer_recommendations: bool,
    pub lossy_fallback: LossyFallback,
    pub external_rating_sync_enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct UpdateUserSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_search_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_delete_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_promote_threshold: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navidrome_banner_dismissed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listenbrainz_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listenbrainz_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_folder_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_track_count: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_lifetime_days: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_profiles: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_playlist_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_download_folder_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deezer_recommendations: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lossy_fallback: Option<LossyFallback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_rating_sync_enabled: Option<bool>,
}

#[cfg(feature = "server")]
impl UserSettings {
    pub async fn get(user_id: &str) -> Result<UserSettings, String> {
        Self::get_in(&DB, user_id).await
    }

    pub(crate) async fn get_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
    ) -> Result<UserSettings, String> {
        let settings =
            sqlx::query_as::<_, UserSettings>("SELECT * FROM user_settings WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(db)
                .await
                .map_err(|e| e.to_string())?;
        Ok(settings.unwrap_or_else(|| Self::defaults(user_id)))
    }

    /// The chosen discovery folder's id. Settings clears the choice by storing an empty id.
    pub(crate) fn discovery_folder(&self) -> Option<&str> {
        self.discovery_folder_id
            .as_deref()
            .filter(|id| !id.is_empty())
    }

    /// What a user without a row has.
    pub(crate) fn defaults(user_id: &str) -> UserSettings {
        UserSettings {
            user_id: user_id.to_string(),
            last_search_type: Some("album".to_string()),
            auto_delete_enabled: false,
            discovery_promote_threshold: 3,
            navidrome_banner_dismissed: false,
            listenbrainz_username: None,
            listenbrainz_token: None,
            discovery_enabled: false,
            discovery_folder_id: None,
            discovery_track_count: r#"{"Conservative":10,"Balanced":10,"Adventurous":10}"#.to_string(),
            discovery_lifetime_days: r#"{"Conservative":7,"Balanced":7,"Adventurous":7}"#.to_string(),
            discovery_profiles: "Conservative,Balanced,Adventurous".to_string(),
            discovery_playlist_name: r#"{"Conservative":"Comfort Zone","Balanced":"Fresh Picks","Adventurous":"Deep Cuts"}"#.to_string(),
            discovery_navidrome_playlist_id: None,
            discovery_last_generated_at: None,
            default_download_folder_id: None,
            deezer_recommendations: false,
            lossy_fallback: LossyFallback::Ask,
            external_rating_sync_enabled: true,
        }
    }

    /// Stores the fields of `update` that are set, in one statement that writes no other
    /// column, so two writes at once both stay; a new row takes `defaults` for the rest.
    /// Switching auto-delete off drops the tracks its last run kept, in the same transaction,
    /// so Settings does not show them as current.
    pub(crate) async fn upsert_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
        update: UpdateUserSettings,
    ) -> Result<UserSettings, String> {
        /// `given` when set, else `default`, as the column's type.
        fn over<T: Into<U>, U>(given: Option<T>, default: U) -> U {
            given.map_or(default, Into::into)
        }
        let clears_blocks = update.auto_delete_enabled == Some(false);
        let defaults = Self::defaults(user_id);
        let mut given: Vec<&str> = Vec::new();
        let mut insert = sqlx::QueryBuilder::<sqlx::Sqlite>::new("");
        // Every field of `update`, named once: a field added to it and not here fails to build.
        macro_rules! columns {
            ($($field:ident),*) => {
                let UpdateUserSettings { $($field),* } = update;
                $(if $field.is_some() {
                    given.push(stringify!($field));
                })*
                insert.push(concat!(
                    "INSERT INTO user_settings (user_id",
                    $(", ", stringify!($field),)*
                    ") VALUES ("
                ));
                let mut values = insert.separated(", ");
                values.push_bind(user_id);
                $(values.push_bind(over($field, defaults.$field));)*
                insert.push(")");
            };
        }
        columns!(
            last_search_type,
            auto_delete_enabled,
            discovery_promote_threshold,
            navidrome_banner_dismissed,
            listenbrainz_username,
            listenbrainz_token,
            discovery_enabled,
            discovery_folder_id,
            discovery_track_count,
            discovery_lifetime_days,
            discovery_profiles,
            discovery_playlist_name,
            default_download_folder_id,
            deezer_recommendations,
            lossy_fallback,
            external_rating_sync_enabled
        );
        if given.is_empty() {
            insert.push(" ON CONFLICT(user_id) DO NOTHING");
        } else {
            insert.push(" ON CONFLICT(user_id) DO UPDATE SET ");
            let mut set = insert.separated(", ");
            for column in &given {
                set.push(format!("{column} = excluded.{column}"));
            }
        }

        // The first statement writes, so the transaction waits for other writers instead of
        // failing as one that read first would.
        let mut tx = db.begin().await.map_err(|e| e.to_string())?;
        insert
            .build()
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        if clears_blocks {
            super::deletion_block::DeletionBlockRow::clear_in(&mut tx, user_id).await?;
        }
        tx.commit().await.map_err(|e| e.to_string())?;

        Self::get_in(db, user_id).await
    }

    /// Stores `playlist_id` as the Navidrome playlist of discovery `profile`. One statement sets
    /// the key, so a write of another key meanwhile is kept; a value that is not JSON counts as
    /// no ids, as get_playlist_id_for_profile reads it.
    pub(crate) async fn update_discovery_playlist_id_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
        profile: &str,
        playlist_id: &str,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE user_settings SET discovery_navidrome_playlist_id = json_set(
                 CASE WHEN json_valid(discovery_navidrome_playlist_id)
                      THEN discovery_navidrome_playlist_id ELSE '{}' END,
                 ?, ?)
             WHERE user_id = ?",
        )
        .bind(format!("$.{profile}"))
        .bind(playlist_id)
        .bind(user_id)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_playlist_id_for_profile(
        playlist_ids_json: &Option<String>,
        profile: &str,
    ) -> Option<String> {
        playlist_ids_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<std::collections::HashMap<String, String>>(s).ok())
            .and_then(|m| m.get(profile).cloned())
    }

    /// Get the playlist name for a specific profile
    pub fn get_playlist_name_for_profile(names_json: &str, profile: &str) -> String {
        serde_json::from_str::<std::collections::HashMap<String, String>>(names_json)
            .ok()
            .and_then(|m| m.get(profile).cloned())
            .unwrap_or_else(|| match profile {
                "Conservative" => "Comfort Zone".to_string(),
                "Balanced" => "Fresh Picks".to_string(),
                "Adventurous" => "Deep Cuts".to_string(),
                _ => "Discovery".to_string(),
            })
    }

    /// Update the last generated timestamp for discovery
    pub async fn update_discovery_last_generated(user_id: &str) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO user_settings (user_id, discovery_last_generated_at) VALUES (?, ?)
             ON CONFLICT(user_id) DO UPDATE SET discovery_last_generated_at = excluded.discovery_last_generated_at",
        )
        .bind(user_id)
        .bind(&now)
        .execute(&*DB)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn reset_navidrome_banner(user_id: &str) -> Result<(), String> {
        Self::reset_navidrome_banner_in(&DB, user_id).await
    }

    pub async fn reset_navidrome_banner_in(
        db: &sqlx::SqlitePool,
        user_id: &str,
    ) -> Result<(), String> {
        sqlx::query("UPDATE user_settings SET navidrome_banner_dismissed = 0 WHERE user_id = ?")
            .bind(user_id)
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Parse per-profile track counts from the JSON column, with fallback for legacy integer values.
    pub fn parse_track_counts(&self) -> std::collections::HashMap<String, u32> {
        if let Ok(map) = serde_json::from_str::<std::collections::HashMap<String, u32>>(
            &self.discovery_track_count,
        ) {
            return map;
        }
        // Legacy fallback: plain integer -> split evenly across profiles
        let total: u32 = self.discovery_track_count.parse().unwrap_or(30);
        let per = total / 3;
        [
            ("Conservative".to_string(), per),
            ("Balanced".to_string(), per),
            ("Adventurous".to_string(), total - per * 2),
        ]
        .into_iter()
        .collect()
    }

    /// Parse per-profile lifetime days from the JSON column, with fallback for legacy integer values.
    pub fn parse_lifetime_days(&self) -> std::collections::HashMap<String, u32> {
        if let Ok(map) = serde_json::from_str::<std::collections::HashMap<String, u32>>(
            &self.discovery_lifetime_days,
        ) {
            return map;
        }
        // Legacy fallback: plain integer -> same value for all profiles
        let days: u32 = self.discovery_lifetime_days.parse().unwrap_or(7);
        [
            ("Conservative".to_string(), days),
            ("Balanced".to_string(), days),
            ("Adventurous".to_string(), days),
        ]
        .into_iter()
        .collect()
    }

    /// Get track count for a specific profile.
    pub fn track_count_for_profile(&self, profile: &str) -> usize {
        self.parse_track_counts()
            .get(profile)
            .copied()
            .unwrap_or(10) as usize
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::db::test_db;

    #[tokio::test]
    async fn a_write_keeps_what_another_write_changed_meanwhile() {
        let db = test_db::migrated().await;
        // user_settings.user_id references users(id).
        crate::core::folder_of(&db, "alice").await;
        let (a, b) = tokio::join!(
            UserSettings::upsert_in(
                &db,
                "alice",
                UpdateUserSettings {
                    auto_delete_enabled: Some(true),
                    ..Default::default()
                }
            ),
            UserSettings::upsert_in(
                &db,
                "alice",
                UpdateUserSettings {
                    discovery_promote_threshold: Some(4),
                    ..Default::default()
                }
            )
        );
        a.unwrap();
        b.unwrap();
        let s = UserSettings::get_in(&db, "alice").await.unwrap();
        assert_eq!(
            (s.auto_delete_enabled, s.discovery_promote_threshold),
            (true, 4)
        );
        // A new row has the defaults, not the column defaults of
        // api/migrations/20260323000000_discovery_per_user.sql:5-8.
        assert_eq!(
            s.discovery_track_count,
            UserSettings::defaults("alice").discovery_track_count
        );
    }

    #[tokio::test]
    async fn a_playlist_id_is_stored_beside_the_others() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        UserSettings::upsert_in(&db, "alice", UpdateUserSettings::default())
            .await
            .unwrap();
        let stored = |s: UserSettings| -> std::collections::HashMap<String, String> {
            serde_json::from_str(&s.discovery_navidrome_playlist_id.unwrap()).unwrap()
        };
        // No ids yet, or a value that is not JSON: the id is the only one.
        for start in [None, Some("not json")] {
            sqlx::query("UPDATE user_settings SET discovery_navidrome_playlist_id = ?")
                .bind(start)
                .execute(&db)
                .await
                .unwrap();
            UserSettings::update_discovery_playlist_id_in(&db, "alice", "Balanced", "b1")
                .await
                .unwrap();
            let s = UserSettings::get_in(&db, "alice").await.unwrap();
            assert_eq!(
                stored(s),
                std::collections::HashMap::from([("Balanced".to_string(), "b1".to_string())])
            );
        }
        // Two writes at once keep each other's id.
        let (a, b) = tokio::join!(
            UserSettings::update_discovery_playlist_id_in(&db, "alice", "Conservative", "c1"),
            UserSettings::update_discovery_playlist_id_in(&db, "alice", "Adventurous", "a1")
        );
        a.unwrap();
        b.unwrap();
        UserSettings::update_discovery_playlist_id_in(&db, "alice", "Balanced", "b2")
            .await
            .unwrap();
        let s = UserSettings::get_in(&db, "alice").await.unwrap();
        assert_eq!(
            stored(s),
            std::collections::HashMap::from([
                ("Conservative".to_string(), "c1".to_string()),
                ("Adventurous".to_string(), "a1".to_string()),
                ("Balanced".to_string(), "b2".to_string()),
            ])
        );
    }

    #[tokio::test]
    async fn deezer_is_off_until_the_user_turns_it_on() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        assert!(
            !UserSettings::get_in(&db, "alice")
                .await
                .unwrap()
                .deezer_recommendations
        );
        let s = UserSettings::upsert_in(
            &db,
            "alice",
            UpdateUserSettings {
                deezer_recommendations: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(s.deezer_recommendations);
    }

    #[tokio::test]
    async fn a_user_is_asked_before_lossy_files_until_they_choose_otherwise() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        let s = UserSettings::get_in(&db, "alice").await.unwrap();
        assert_eq!(s.lossy_fallback, LossyFallback::Ask);
        for choice in LossyFallback::ALL {
            let s = UserSettings::upsert_in(
                &db,
                "alice",
                UpdateUserSettings {
                    lossy_fallback: Some(choice),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert_eq!(s.lossy_fallback, choice);
            assert_eq!(LossyFallback::parse(choice.as_str()), Some(choice));
        }
        // The database holds the choice as its text, and nothing else.
        let stored: String =
            sqlx::query_scalar("SELECT lossy_fallback FROM user_settings WHERE user_id = 'alice'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(stored, "auto");
        assert!(
            sqlx::query("UPDATE user_settings SET lossy_fallback = 'sometimes'")
                .execute(&db)
                .await
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(LossyFallback::Never).unwrap(),
            serde_json::json!("never")
        );
    }

    #[tokio::test]
    async fn external_rating_sync_defaults_to_on_and_can_be_toggled() {
        let db = test_db::migrated().await;
        crate::core::folder_of(&db, "alice").await;
        assert!(
            UserSettings::get_in(&db, "alice")
                .await
                .unwrap()
                .external_rating_sync_enabled
        );
        let s = UserSettings::upsert_in(
            &db,
            "alice",
            UpdateUserSettings {
                external_rating_sync_enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!s.external_rating_sync_enabled);
        let s = UserSettings::upsert_in(
            &db,
            "alice",
            UpdateUserSettings {
                external_rating_sync_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(s.external_rating_sync_enabled);
    }
}
