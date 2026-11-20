-- The OpenID Connect identity an account signs in with: the issuer and the
-- subject (`sub` claim) it issued. A subject is unique only within its issuer
-- (OpenID Connect Core 1.0, section 5.7), so the pair is the key. Both NULL for
-- accounts that never used OIDC; SQLite lets a UNIQUE index hold many NULLs.
ALTER TABLE users ADD COLUMN oidc_issuer TEXT;
ALTER TABLE users ADD COLUMN oidc_subject TEXT
    CHECK ((oidc_issuer IS NULL) = (oidc_subject IS NULL));
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_oidc_identity ON users(oidc_issuer, oidc_subject);
