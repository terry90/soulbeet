-- What auto-download does for a user with a backend whose files are lossy only, when no other
-- backend has a match it would take: leave it out, offer it to pick from, or pick from it.
ALTER TABLE user_settings ADD COLUMN lossy_fallback TEXT NOT NULL DEFAULT 'ask'
    CHECK (lossy_fallback IN ('never', 'ask', 'auto'));
