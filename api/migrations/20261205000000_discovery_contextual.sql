-- Discovery 2.0: Cultural profile, extensible contextual mix registry, per-user mix state,
-- rejection tracking, and core-taste anti-pollution metadata.

CREATE TABLE IF NOT EXISTS user_culture_profiles (
    user_id TEXT PRIMARY KEY NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    home_country TEXT,
    preferred_languages TEXT NOT NULL DEFAULT '[]',
    local_culture_ratio REAL NOT NULL DEFAULT 0.45,
    inferred_area_weights TEXT NOT NULL DEFAULT '[]',
    inferred_local_tags TEXT NOT NULL DEFAULT '[]',
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Data-driven registry of seasonal and moment contextual mixes.
-- Built-in presets are seeded with `built_in = 1`, `owner_user_id = NULL`, and `custom_name = NULL`
-- so clients localize them via `i18n_key`. Admins can add global presets (`owner_user_id = NULL`),
-- and users can create private custom mixes (`owner_user_id = user_id`).
CREATE TABLE IF NOT EXISTS contextual_mix_definitions (
    slug TEXT PRIMARY KEY NOT NULL,
    owner_user_id TEXT REFERENCES users(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,                  -- 'seasonal' | 'moment'
    i18n_key TEXT NOT NULL,
    custom_name TEXT,
    custom_description TEXT,
    built_in INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 1,
    theme_tags TEXT NOT NULL DEFAULT '[]',
    international_queries TEXT NOT NULL DEFAULT '[]',
    calendar_window TEXT,                -- JSON CalendarWindow for seasonal packs
    hour_window TEXT,                    -- JSON HourWindow for time-of-day mixes
    always_surfaced INTEGER NOT NULL DEFAULT 0,
    countries TEXT NOT NULL DEFAULT '[]',
    owned_ratio REAL NOT NULL DEFAULT 0.0,
    default_local_ratio REAL,
    exclude_from_core_taste INTEGER NOT NULL DEFAULT 0,
    offline_priority INTEGER NOT NULL DEFAULT 0,
    target_tracks INTEGER NOT NULL DEFAULT 25,
    sync_to_navidrome INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_contextual_mix_defs_owner
    ON contextual_mix_definitions(owner_user_id, kind);

-- Per-user runtime state and preferences for any contextual mix slug:
-- live steering, personal enable/disable overrides, Navidrome playlist id, and expiry.
CREATE TABLE IF NOT EXISTS contextual_mix_states (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    slug TEXT NOT NULL,
    steering TEXT NOT NULL DEFAULT 'balanced',
    local_ratio REAL,
    enabled_override INTEGER,            -- NULL = inherit definition.enabled, 0 = off, 1 = on
    navidrome_playlist_id TEXT,
    expires_at TEXT,                     -- ISO-8601 date YYYY-MM-DD when seasonal playlist expires
    last_generated_at TEXT,
    UNIQUE(user_id, slug)
);

CREATE INDEX IF NOT EXISTS idx_contextual_mix_states_user
    ON contextual_mix_states(user_id);

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

CREATE INDEX IF NOT EXISTS idx_discovery_rejections_user ON discovery_rejections(user_id);

-- Core-taste anti-pollution flag and structured client-localized provenance on staged tracks.
ALTER TABLE discovery_tracks ADD COLUMN exclude_from_core_taste INTEGER NOT NULL DEFAULT 0;
ALTER TABLE discovery_tracks ADD COLUMN reason_json TEXT;
ALTER TABLE discovery_tracks ADD COLUMN culture_bucket TEXT;
ALTER TABLE discovery_tracks ADD COLUMN pack_slug TEXT;

CREATE INDEX IF NOT EXISTS idx_discovery_tracks_pack_slug ON discovery_tracks(pack_slug);
