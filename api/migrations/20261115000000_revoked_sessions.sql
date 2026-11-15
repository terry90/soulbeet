-- Session tokens ended by signing out, kept until they would have expired.
CREATE TABLE IF NOT EXISTS revoked_sessions (
    jti TEXT PRIMARY KEY NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_revoked_sessions_expires_at ON revoked_sessions(expires_at);
