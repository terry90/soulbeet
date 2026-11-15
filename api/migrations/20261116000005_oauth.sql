-- The OAuth authorization server Claude connects through. Times are unix
-- seconds; JSON arrays are TEXT. Tokens, codes and request ids are stored
-- only as SHA-256 hashes.
CREATE TABLE oauth_clients (
    client_id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('dcr', 'cimd')),
    client_name TEXT,
    redirect_uris TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
);
-- An authorize request waiting for the user's consent, with the client as
-- authorize validated it: a CIMD client gets its oauth_clients row only at
-- consent. seq orders a user's requests from oldest to newest.
CREATE TABLE oauth_requests (
    seq INTEGER PRIMARY KEY,
    id_hash TEXT NOT NULL UNIQUE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL,
    client_kind TEXT NOT NULL CHECK (client_kind IN ('dcr', 'cimd')),
    client_name TEXT,
    client_redirect_uris TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    state TEXT,
    code_challenge TEXT NOT NULL,
    scope TEXT NOT NULL,
    resource TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE oauth_grants (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL REFERENCES oauth_clients(client_id) ON DELETE CASCADE,
    -- The redirect URI the user allowed: a registered app is named by its host.
    redirect_uri TEXT NOT NULL,
    resource TEXT NOT NULL,
    scope TEXT NOT NULL,
    allow_delete INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
);
CREATE INDEX idx_oauth_grants_user ON oauth_grants(user_id);
CREATE TABLE oauth_codes (
    code_hash TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL REFERENCES oauth_clients(client_id) ON DELETE CASCADE,
    redirect_uri TEXT NOT NULL,
    code_challenge TEXT NOT NULL,
    scope TEXT NOT NULL,
    resource TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    grant_id TEXT REFERENCES oauth_grants(id) ON DELETE SET NULL
);
CREATE TABLE oauth_tokens (
    token_hash TEXT PRIMARY KEY NOT NULL,
    grant_id TEXT NOT NULL REFERENCES oauth_grants(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('access', 'refresh')),
    expires_at INTEGER NOT NULL,
    rotated_at INTEGER
);
CREATE INDEX idx_oauth_tokens_grant ON oauth_tokens(grant_id);
-- Every credentials_version bump (password change, admin reset, Navidrome
-- adoption) ends the user's connected apps in the same statement, with the
-- codes not yet redeemed and the consent requests not yet answered, so
-- neither can start a new grant afterwards.
CREATE TRIGGER oauth_grants_end_on_credential_change
AFTER UPDATE OF credentials_version ON users
WHEN NEW.credentials_version <> OLD.credentials_version
BEGIN
    DELETE FROM oauth_grants WHERE user_id = NEW.id;
    DELETE FROM oauth_codes WHERE user_id = NEW.id;
    DELETE FROM oauth_requests WHERE user_id = NEW.id;
END;
