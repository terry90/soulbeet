-- Persistent negative cache for cover art IDs that returned placeholders, HTTP 404/410,
-- or Subsonic error 70 so background cover-art warming does not starve valid artwork.

CREATE TABLE IF NOT EXISTS cover_art_missing (
    cover_art_id TEXT PRIMARY KEY,
    checked_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_cover_art_missing_checked_at
    ON cover_art_missing(checked_at);
