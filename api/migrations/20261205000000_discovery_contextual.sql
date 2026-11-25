-- Discovery 2.0: cultural listening profiles, seasonal packs, negative taste rejections,
-- and explainable/anti-pollution metadata on staged discovery tracks.

CREATE TABLE IF NOT EXISTS user_culture_profiles (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    home_country TEXT,
    preferred_languages TEXT NOT NULL DEFAULT '[]',
    local_culture_ratio REAL NOT NULL DEFAULT 0.45,
    inferred_area_weights TEXT NOT NULL DEFAULT '[]',
    inferred_local_tags TEXT NOT NULL DEFAULT '[]',
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS seasonal_packs (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    slug TEXT NOT NULL,
    title TEXT NOT NULL,
    subtitle TEXT NOT NULL,
    steering TEXT NOT NULL DEFAULT 'balanced',
    local_ratio REAL NOT NULL DEFAULT 0.45,
    active_from TEXT NOT NULL,
    active_until TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    navidrome_playlist_id TEXT,
    last_generated_at TEXT,
    UNIQUE(user_id, slug)
);

CREATE INDEX IF NOT EXISTS idx_seasonal_packs_user ON seasonal_packs(user_id);

CREATE TABLE IF NOT EXISTS discovery_rejections (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    artist_key TEXT NOT NULL,
    track_key TEXT NOT NULL,
    artist TEXT NOT NULL,
    track TEXT NOT NULL,
    recording_mbid TEXT,
    reason TEXT NOT NULL DEFAULT 'dropped',
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(user_id, artist_key, track_key)
);

CREATE INDEX IF NOT EXISTS idx_discovery_rejections_user ON discovery_rejections(user_id, created_at);

ALTER TABLE discovery_tracks ADD COLUMN exclude_from_core_taste INTEGER NOT NULL DEFAULT 0;
ALTER TABLE discovery_tracks ADD COLUMN reason_label TEXT;
ALTER TABLE discovery_tracks ADD COLUMN culture_bucket TEXT;
ALTER TABLE discovery_tracks ADD COLUMN pack_slug TEXT;
