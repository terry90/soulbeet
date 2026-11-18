-- The OpenID Connect subject (`sub` claim) an account signs in with. NULL for
-- accounts that never used OIDC; SQLite lets a UNIQUE index hold many NULLs.
ALTER TABLE users ADD COLUMN oidc_subject TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_oidc_subject ON users(oidc_subject);
