-- Deletions by a connected app: the audit names the app (client, its host, NULL for the web UI,
-- auto-delete and rows from before), the day's count reads a user's rows by origin, and a
-- preview's confirmation token is stored as its SHA-256 hash with what it allows deleting.
ALTER TABLE deletion_reviews ADD COLUMN client TEXT;
CREATE INDEX deletion_reviews_by_origin ON deletion_reviews (user_id, origin, created_at);
CREATE TABLE delete_tokens (
    token_hash TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    grant_id TEXT NOT NULL REFERENCES oauth_grants(id) ON DELETE CASCADE,
    reason TEXT NOT NULL,
    files TEXT NOT NULL,              -- JSON Vec<PreviewedFile>
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER
);
CREATE INDEX delete_tokens_expiry ON delete_tokens (expires_at);
