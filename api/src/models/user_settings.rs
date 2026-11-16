#[cfg(feature = "server")]
use crate::db::DB;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "server", derive(sqlx::FromRow))]
pub struct UserSettings {
    pub user_id: String,
    pub default_metadata_provider: Option<String>,
    pub last_search_type: Option<String>,
    pub auto_delete_enabled: bool,
    pub lastfm_api_key: Option<String>,
    pub lastfm_username: Option<String>,
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
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct UpdateUserSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_metadata_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_search_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_delete_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lastfm_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lastfm_username: Option<String>,
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

    /// What a user without a row has.
    pub(crate) fn defaults(user_id: &str) -> UserSettings {
        UserSettings {
            user_id: user_id.to_string(),
            default_metadata_provider: Some("musicbrainz".to_string()),
            last_search_type: Some("album".to_string()),
            auto_delete_enabled: false,
            lastfm_api_key: None,
            lastfm_username: None,
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
        }
    }

    pub async fn upsert(user_id: &str, update: UpdateUserSettings) -> Result<UserSettings, String> {
        Self::upsert_in(&DB, user_id, update).await
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
            default_metadata_provider,
            last_search_type,
            auto_delete_enabled,
            lastfm_api_key,
            lastfm_username,
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
            default_download_folder_id
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

    /// Update the stored Navidrome playlist ID for a specific discovery profile.
    pub async fn update_discovery_playlist_id(
        user_id: &str,
        profile: &str,
        playlist_id: &str,
    ) -> Result<(), String> {
        let current = Self::get(user_id).await?;
        let mut ids: std::collections::HashMap<String, String> = current
            .discovery_navidrome_playlist_id
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        ids.insert(profile.to_string(), playlist_id.to_string());
        let json = serde_json::to_string(&ids).map_err(|e| e.to_string())?;
        sqlx::query(
            "UPDATE user_settings SET discovery_navidrome_playlist_id = ? WHERE user_id = ?",
        )
        .bind(&json)
        .bind(user_id)
        .execute(&*DB)
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
}
