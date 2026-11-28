-- Add missing hot query and sweep indexes across discovery, ledger, OAuth, delete tokens, and users.
CREATE INDEX IF NOT EXISTS idx_discovery_tracks_folder_status
    ON discovery_tracks(folder_id, status);
CREATE INDEX IF NOT EXISTS idx_discovery_tracks_song_id
    ON discovery_tracks(song_id);

CREATE INDEX IF NOT EXISTS idx_searches_user_id
    ON searches(user_id);
CREATE INDEX IF NOT EXISTS idx_searches_expires_at
    ON searches(expires_at);

CREATE INDEX IF NOT EXISTS idx_jobs_folder_id
    ON jobs(folder_id);

CREATE INDEX IF NOT EXISTS idx_oauth_tokens_expires_at
    ON oauth_tokens(expires_at);
CREATE INDEX IF NOT EXISTS idx_oauth_codes_user_id
    ON oauth_codes(user_id);
CREATE INDEX IF NOT EXISTS idx_oauth_codes_client_id
    ON oauth_codes(client_id);
CREATE INDEX IF NOT EXISTS idx_oauth_codes_expires_at
    ON oauth_codes(expires_at);
CREATE INDEX IF NOT EXISTS idx_oauth_requests_user_id
    ON oauth_requests(user_id);
CREATE INDEX IF NOT EXISTS idx_oauth_requests_expires_at
    ON oauth_requests(expires_at);

CREATE INDEX IF NOT EXISTS idx_request_keys_grant_id
    ON request_keys(grant_id);

CREATE INDEX IF NOT EXISTS idx_delete_tokens_user_id
    ON delete_tokens(user_id);
CREATE INDEX IF NOT EXISTS idx_delete_tokens_grant_id
    ON delete_tokens(grant_id);

CREATE INDEX IF NOT EXISTS idx_engine_reports_user_created
    ON engine_reports(user_id, created_at);

CREATE INDEX IF NOT EXISTS idx_users_username_nocase
    ON users(username COLLATE NOCASE);
