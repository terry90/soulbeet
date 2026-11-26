//! Persistence models for Discovery 2.0: `user_culture_profiles`, `contextual_mix_definitions`,
//! `contextual_mix_states`, and `discovery_rejections`.

#[cfg(feature = "server")]
use std::collections::HashSet;

#[cfg(feature = "server")]
use shared::recommendation::{
    builtin_contextual_mixes, CalendarWindow, CandidateSet, ContextualMixDefinition,
    ContextualMixKind, CultureSteering, HourWindow, UserCultureProfile,
};
#[cfg(feature = "server")]
use sqlx::SqlitePool;
#[cfg(feature = "server")]
use uuid::Uuid;

#[cfg(feature = "server")]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserCultureProfileRow {
    pub user_id: String,
    pub home_country: Option<String>,
    pub preferred_languages: String,
    pub local_culture_ratio: f64,
    pub inferred_area_weights: String,
    pub inferred_local_tags: String,
    pub updated_at: String,
}

#[cfg(feature = "server")]
impl From<UserCultureProfileRow> for UserCultureProfile {
    fn from(row: UserCultureProfileRow) -> Self {
        Self {
            home_country: row.home_country,
            preferred_languages: serde_json::from_str(&row.preferred_languages).unwrap_or_default(),
            local_culture_ratio: (row.local_culture_ratio as f32).clamp(0.0, 1.0),
            inferred_area_weights: serde_json::from_str(&row.inferred_area_weights)
                .unwrap_or_default(),
            inferred_local_tags: serde_json::from_str(&row.inferred_local_tags).unwrap_or_default(),
        }
    }
}

#[cfg(feature = "server")]
impl UserCultureProfileRow {
    pub async fn get_in(
        db: &SqlitePool,
        user_id: &str,
    ) -> Result<Option<UserCultureProfile>, String> {
        let row = sqlx::query_as::<_, UserCultureProfileRow>(
            "SELECT * FROM user_culture_profiles WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(row.map(Into::into))
    }

    pub async fn upsert_in(
        db: &SqlitePool,
        user_id: &str,
        profile: &UserCultureProfile,
    ) -> Result<UserCultureProfile, String> {
        let langs = serde_json::to_string(&profile.preferred_languages)
            .map_err(|e| e.to_string())?;
        let areas = serde_json::to_string(&profile.inferred_area_weights)
            .map_err(|e| e.to_string())?;
        let local_tags =
            serde_json::to_string(&profile.inferred_local_tags).map_err(|e| e.to_string())?;
        let ratio = f64::from(profile.local_culture_ratio.clamp(0.0, 1.0));

        let row = sqlx::query_as::<_, UserCultureProfileRow>(
            "INSERT INTO user_culture_profiles \
                (user_id, home_country, preferred_languages, local_culture_ratio, \
                 inferred_area_weights, inferred_local_tags, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(user_id) DO UPDATE SET \
                home_country = excluded.home_country, \
                preferred_languages = excluded.preferred_languages, \
                local_culture_ratio = excluded.local_culture_ratio, \
                inferred_area_weights = excluded.inferred_area_weights, \
                inferred_local_tags = excluded.inferred_local_tags, \
                updated_at = CURRENT_TIMESTAMP \
             RETURNING *",
        )
        .bind(user_id)
        .bind(profile.home_country.as_deref())
        .bind(&langs)
        .bind(ratio)
        .bind(&areas)
        .bind(&local_tags)
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())?;

        Ok(row.into())
    }
}

#[cfg(feature = "server")]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContextualMixDefinitionRow {
    pub slug: String,
    pub owner_user_id: Option<String>,
    pub kind: String,
    pub i18n_key: String,
    pub custom_name: Option<String>,
    pub custom_description: Option<String>,
    pub built_in: i64,
    pub enabled: i64,
    pub theme_tags: String,
    pub international_queries: String,
    pub calendar_window: Option<String>,
    pub hour_window: Option<String>,
    pub always_surfaced: i64,
    pub countries: String,
    pub owned_ratio: f64,
    pub default_local_ratio: Option<f64>,
    pub exclude_from_core_taste: i64,
    pub offline_priority: i64,
    pub target_tracks: i64,
    pub sync_to_navidrome: i64,
}

#[cfg(feature = "server")]
impl From<ContextualMixDefinitionRow> for ContextualMixDefinition {
    fn from(row: ContextualMixDefinitionRow) -> Self {
        Self {
            slug: row.slug,
            owner_user_id: row.owner_user_id,
            kind: ContextualMixKind::parse(&row.kind).unwrap_or(ContextualMixKind::Moment),
            i18n_key: row.i18n_key,
            custom_name: row.custom_name,
            custom_description: row.custom_description,
            built_in: row.built_in != 0,
            enabled: row.enabled != 0,
            theme_tags: serde_json::from_str(&row.theme_tags).unwrap_or_default(),
            international_queries: serde_json::from_str(&row.international_queries)
                .unwrap_or_default(),
            calendar_window: row
                .calendar_window
                .as_deref()
                .and_then(|s| serde_json::from_str::<CalendarWindow>(s).ok()),
            hour_window: row
                .hour_window
                .as_deref()
                .and_then(|s| serde_json::from_str::<HourWindow>(s).ok()),
            always_surfaced: row.always_surfaced != 0,
            countries: serde_json::from_str(&row.countries).unwrap_or_default(),
            owned_ratio: (row.owned_ratio as f32).clamp(0.0, 1.0),
            default_local_ratio: row.default_local_ratio.map(|r| (r as f32).clamp(0.0, 1.0)),
            exclude_from_core_taste: row.exclude_from_core_taste != 0,
            offline_priority: row.offline_priority != 0,
            target_tracks: row.target_tracks.clamp(1, 200) as u32,
            sync_to_navidrome: row.sync_to_navidrome != 0,
        }
    }
}

#[cfg(feature = "server")]
impl ContextualMixDefinitionRow {
    /// Idempotently seeds the built-in global contextual mix presets (`INSERT OR IGNORE`)
    /// so existing admin edits remain preserved while newly added presets appear automatically.
    /// Fast-paths with a single `COUNT(1)` read so normal requests never trigger SQLite writes.
    pub async fn seed_builtins_in(db: &SqlitePool) -> Result<(), String> {
        let builtins = builtin_contextual_mixes();
        let existing_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(1) FROM contextual_mix_definitions WHERE built_in = 1",
        )
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())?;
        if existing_count >= builtins.len() as i64 {
            return Ok(());
        }

        for def in builtins {
            let theme_tags = serde_json::to_string(&def.theme_tags).map_err(|e| e.to_string())?;
            let intl =
                serde_json::to_string(&def.international_queries).map_err(|e| e.to_string())?;
            let cal = def
                .calendar_window
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| e.to_string())?;
            let hour = def
                .hour_window
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| e.to_string())?;
            let countries = serde_json::to_string(&def.countries).map_err(|e| e.to_string())?;

            sqlx::query(
                "INSERT OR IGNORE INTO contextual_mix_definitions \
                    (slug, owner_user_id, kind, i18n_key, custom_name, custom_description, \
                     built_in, enabled, theme_tags, international_queries, calendar_window, \
                     hour_window, always_surfaced, countries, owned_ratio, default_local_ratio, \
                     exclude_from_core_taste, offline_priority, target_tracks, sync_to_navidrome) \
                 VALUES (?, NULL, ?, ?, NULL, NULL, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&def.slug)
            .bind(def.kind.as_str())
            .bind(&def.i18n_key)
            .bind(i64::from(def.enabled))
            .bind(&theme_tags)
            .bind(&intl)
            .bind(cal.as_deref())
            .bind(hour.as_deref())
            .bind(i64::from(def.always_surfaced))
            .bind(&countries)
            .bind(f64::from(def.owned_ratio))
            .bind(def.default_local_ratio.map(f64::from))
            .bind(i64::from(def.exclude_from_core_taste))
            .bind(i64::from(def.offline_priority))
            .bind(i64::from(def.target_tracks))
            .bind(i64::from(def.sync_to_navidrome))
            .execute(db)
            .await
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub async fn list_visible_for_user_in(
        db: &SqlitePool,
        user_id: &str,
    ) -> Result<Vec<ContextualMixDefinition>, String> {
        Self::seed_builtins_in(db).await?;
        let rows = sqlx::query_as::<_, Self>(
            "SELECT * FROM contextual_mix_definitions \
             WHERE owner_user_id IS NULL OR owner_user_id = ? \
             ORDER BY built_in DESC, kind ASC, slug ASC",
        )
        .bind(user_id)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_visible_by_slug_in(
        db: &SqlitePool,
        user_id: &str,
        slug: &str,
    ) -> Result<Option<ContextualMixDefinition>, String> {
        Self::seed_builtins_in(db).await?;
        let row = sqlx::query_as::<_, Self>(
            "SELECT * FROM contextual_mix_definitions \
             WHERE slug = ? AND (owner_user_id IS NULL OR owner_user_id = ?)",
        )
        .bind(slug)
        .bind(user_id)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(row.map(Into::into))
    }

    pub async fn get_by_slug_in(
        db: &SqlitePool,
        slug: &str,
    ) -> Result<Option<ContextualMixDefinition>, String> {
        Self::seed_builtins_in(db).await?;
        let row = sqlx::query_as::<_, Self>(
            "SELECT * FROM contextual_mix_definitions WHERE slug = ?",
        )
        .bind(slug)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(row.map(Into::into))
    }

    pub async fn upsert_in(
        db: &SqlitePool,
        def: &ContextualMixDefinition,
    ) -> Result<ContextualMixDefinition, String> {
        let theme_tags = serde_json::to_string(&def.theme_tags).map_err(|e| e.to_string())?;
        let intl =
            serde_json::to_string(&def.international_queries).map_err(|e| e.to_string())?;
        let cal = def
            .calendar_window
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| e.to_string())?;
        let hour = def
            .hour_window
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| e.to_string())?;
        let countries = serde_json::to_string(&def.countries).map_err(|e| e.to_string())?;

        let row = sqlx::query_as::<_, Self>(
            "INSERT INTO contextual_mix_definitions \
                (slug, owner_user_id, kind, i18n_key, custom_name, custom_description, \
                 built_in, enabled, theme_tags, international_queries, calendar_window, \
                 hour_window, always_surfaced, countries, owned_ratio, default_local_ratio, \
                 exclude_from_core_taste, offline_priority, target_tracks, sync_to_navidrome, \
                 updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(slug) DO UPDATE SET \
                owner_user_id = excluded.owner_user_id, \
                kind = excluded.kind, \
                i18n_key = excluded.i18n_key, \
                custom_name = excluded.custom_name, \
                custom_description = excluded.custom_description, \
                enabled = excluded.enabled, \
                theme_tags = excluded.theme_tags, \
                international_queries = excluded.international_queries, \
                calendar_window = excluded.calendar_window, \
                hour_window = excluded.hour_window, \
                always_surfaced = excluded.always_surfaced, \
                countries = excluded.countries, \
                owned_ratio = excluded.owned_ratio, \
                default_local_ratio = excluded.default_local_ratio, \
                exclude_from_core_taste = excluded.exclude_from_core_taste, \
                offline_priority = excluded.offline_priority, \
                target_tracks = excluded.target_tracks, \
                sync_to_navidrome = excluded.sync_to_navidrome, \
                updated_at = CURRENT_TIMESTAMP \
             RETURNING *",
        )
        .bind(&def.slug)
        .bind(def.owner_user_id.as_deref())
        .bind(def.kind.as_str())
        .bind(&def.i18n_key)
        .bind(def.custom_name.as_deref())
        .bind(def.custom_description.as_deref())
        .bind(i64::from(def.built_in))
        .bind(i64::from(def.enabled))
        .bind(&theme_tags)
        .bind(&intl)
        .bind(cal.as_deref())
        .bind(hour.as_deref())
        .bind(i64::from(def.always_surfaced))
        .bind(&countries)
        .bind(f64::from(def.owned_ratio))
        .bind(def.default_local_ratio.map(f64::from))
        .bind(i64::from(def.exclude_from_core_taste))
        .bind(i64::from(def.offline_priority))
        .bind(i64::from(def.target_tracks))
        .bind(i64::from(def.sync_to_navidrome))
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(row.into())
    }

    pub async fn delete_by_slug_in(db: &SqlitePool, slug: &str) -> Result<bool, String> {
        let mut tx = db.begin().await.map_err(|e| e.to_string())?;
        let res = sqlx::query(
            "DELETE FROM contextual_mix_definitions WHERE slug = ? AND built_in = 0",
        )
        .bind(slug)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        if res.rows_affected() > 0 {
            sqlx::query("DELETE FROM contextual_mix_states WHERE slug = ?")
                .bind(slug)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(feature = "server")]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContextualMixStateRow {
    pub id: String,
    pub user_id: String,
    pub slug: String,
    pub steering: String,
    pub local_ratio: Option<f64>,
    pub enabled_override: Option<i64>,
    pub navidrome_playlist_id: Option<String>,
    pub expires_at: Option<String>,
    pub last_generated_at: Option<String>,
}

#[cfg(feature = "server")]
impl ContextualMixStateRow {
    pub async fn get_in(
        db: &SqlitePool,
        user_id: &str,
        slug: &str,
    ) -> Result<Option<Self>, String> {
        sqlx::query_as::<_, Self>(
            "SELECT * FROM contextual_mix_states WHERE user_id = ? AND slug = ?",
        )
        .bind(user_id)
        .bind(slug)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn list_for_user_in(db: &SqlitePool, user_id: &str) -> Result<Vec<Self>, String> {
        sqlx::query_as::<_, Self>("SELECT * FROM contextual_mix_states WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn upsert_steering_in(
        db: &SqlitePool,
        user_id: &str,
        slug: &str,
        steering: CultureSteering,
        local_ratio: f32,
    ) -> Result<Self, String> {
        let id = Uuid::new_v4().to_string();
        sqlx::query_as::<_, Self>(
            "INSERT INTO contextual_mix_states (id, user_id, slug, steering, local_ratio) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, slug) DO UPDATE SET \
                steering = excluded.steering, \
                local_ratio = excluded.local_ratio \
             RETURNING *",
        )
        .bind(&id)
        .bind(user_id)
        .bind(slug)
        .bind(steering.as_str())
        .bind(f64::from(local_ratio))
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn upsert_enabled_override_in(
        db: &SqlitePool,
        user_id: &str,
        slug: &str,
        enabled: bool,
    ) -> Result<Self, String> {
        let id = Uuid::new_v4().to_string();
        sqlx::query_as::<_, Self>(
            "INSERT INTO contextual_mix_states (id, user_id, slug, enabled_override) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(user_id, slug) DO UPDATE SET \
                enabled_override = excluded.enabled_override \
             RETURNING *",
        )
        .bind(&id)
        .bind(user_id)
        .bind(slug)
        .bind(i64::from(enabled))
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn upsert_playlist_id_in(
        db: &SqlitePool,
        user_id: &str,
        slug: &str,
        playlist_id: Option<&str>,
        expires_at: Option<&str>,
    ) -> Result<Self, String> {
        let id = Uuid::new_v4().to_string();
        sqlx::query_as::<_, Self>(
            "INSERT INTO contextual_mix_states \
                (id, user_id, slug, navidrome_playlist_id, expires_at, last_generated_at) \
             VALUES (?, ?, ?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(user_id, slug) DO UPDATE SET \
                navidrome_playlist_id = excluded.navidrome_playlist_id, \
                expires_at = COALESCE(excluded.expires_at, contextual_mix_states.expires_at), \
                last_generated_at = CURRENT_TIMESTAMP \
             RETURNING *",
        )
        .bind(&id)
        .bind(user_id)
        .bind(slug)
        .bind(playlist_id)
        .bind(expires_at)
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())
    }
}

#[cfg(feature = "server")]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DiscoveryRejectionRow {
    pub id: String,
    pub user_id: String,
    pub artist_key: String,
    pub track_key: String,
    pub artist: String,
    pub track: String,
    pub recording_mbid: Option<String>,
    pub reason: String,
    pub created_at: String,
}

#[cfg(feature = "server")]
impl DiscoveryRejectionRow {
    pub async fn record_in(
        db: &SqlitePool,
        user_id: &str,
        artist: &str,
        track: &str,
        recording_mbid: Option<&str>,
        reason: &str,
    ) -> Result<(), String> {
        let id = Uuid::new_v4().to_string();
        let artist_key = shared::recommendation::discovery_name(artist);
        let track_key = shared::recommendation::discovery_name(track);
        sqlx::query(
            "INSERT INTO discovery_rejections \
                (id, user_id, artist_key, track_key, artist, track, recording_mbid, reason) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, artist_key, track_key) DO UPDATE SET \
                reason = excluded.reason, \
                created_at = CURRENT_TIMESTAMP",
        )
        .bind(&id)
        .bind(user_id)
        .bind(&artist_key)
        .bind(&track_key)
        .bind(artist)
        .bind(track)
        .bind(recording_mbid)
        .bind(reason)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn keys_for_user_in(
        db: &SqlitePool,
        user_id: &str,
    ) -> Result<HashSet<String>, String> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT artist, track FROM discovery_rejections WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;

        Ok(rows
            .into_iter()
            .map(|(a, t)| CandidateSet::key(&a, &t))
            .collect())
    }
}
