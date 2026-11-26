-- Per-grant admin library access: off by default so an admin's connected app only sees
-- and moves across other users' folders once explicitly enabled in Connected apps.
ALTER TABLE oauth_grants ADD COLUMN allow_admin INTEGER NOT NULL DEFAULT 0;
