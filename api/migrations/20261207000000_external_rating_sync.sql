-- External Subsonic client rating compatibility: when enabled (default), ratings set in
-- external clients (e.g. Symfonium, Feishin, Navidrome Web) can auto-promote or auto-drop
-- pending discovery tracks during rating sync. Pure star ratings written inside Soulbeet are
-- tracked in discovery_tracks.rating and skipped by the sync.
ALTER TABLE user_settings ADD COLUMN external_rating_sync_enabled INTEGER NOT NULL DEFAULT 1;
