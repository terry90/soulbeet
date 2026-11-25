//! Persistence models for Discovery 2.0: `user_culture_profiles`, `seasonal_packs`, and
//! `discovery_rejections`.

#[cfg(feature = "server")]
use std::collections::HashSet;

#[cfg(feature = "server")]
use shared::recommendation::{
    CandidateSet, CultureSteering, SeasonalKind, SeasonalPackSpec, UserCultureProfile,
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
pub struct SeasonalPackRow {
    pub id: String,
    pub user_id: String,
    pub slug: String,
    pub title: String,
    pub subtitle: String,
    pub steering: String,
    pub local_ratio: f64,
    pub active_from: String,
    pub active_until: String,
    pub expires_at: String,
    pub navidrome_playlist_id: Option<String>,
    pub last_generated_at: Option<String>,
}

#[cfg(feature = "server")]
impl SeasonalPackRow {
    pub async fn get_by_slug_in(
        db: &SqlitePool,
        user_id: &str,
        slug: &str,
    ) -> Result<Option<SeasonalPackRow>, String> {
        sqlx::query_as::<_, SeasonalPackRow>(
            "SELECT * FROM seasonal_packs WHERE user_id = ? AND slug = ?",
        )
        .bind(user_id)
        .bind(slug)
        .fetch_optional(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub async fn upsert_steering_in(
        db: &SqlitePool,
        user_id: &str,
        spec: &SeasonalPackSpec,
    ) -> Result<SeasonalPackRow, String> {
        let id = Uuid::new_v4().to_string();
        sqlx::query_as::<_, SeasonalPackRow>(
            "INSERT INTO seasonal_packs \
                (id, user_id, slug, title, subtitle, steering, local_ratio, \
                 active_from, active_until, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, slug) DO UPDATE SET \
                title = excluded.title, \
                subtitle = excluded.subtitle, \
                steering = excluded.steering, \
                local_ratio = excluded.local_ratio \
             RETURNING *",
        )
        .bind(&id)
        .bind(user_id)
        .bind(&spec.slug)
        .bind(&spec.title)
        .bind(&spec.subtitle)
        .bind(spec.steering.as_str())
        .bind(f64::from(spec.local_ratio))
        .bind(&spec.active_window)
        .bind(&spec.active_window)
        .bind(&spec.expires_label)
        .fetch_one(db)
        .await
        .map_err(|e| e.to_string())
    }

    pub fn into_spec(self, kind: SeasonalKind) -> SeasonalPackSpec {
        SeasonalPackSpec {
            slug: self.slug,
            kind,
            title: self.title,
            subtitle: self.subtitle,
            steering: CultureSteering::parse(&self.steering),
            local_ratio: self.local_ratio as f32,
            exclude_from_core_taste: true,
            active_window: kind.active_window_label().to_string(),
            expires_label: kind.expires_label().to_string(),
        }
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

    /// Returns normalized `CandidateSet::key(artist, track)` keys rejected by `user_id`.
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
