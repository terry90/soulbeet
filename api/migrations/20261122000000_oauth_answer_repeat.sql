-- A consent request once answered stays REPEAT_WINDOW seconds with its answer and the session
-- that gave it, so the same answer sent again from that session (a double click on Allow) gets
-- the same redirect instead of a page saying the request is gone.
ALTER TABLE oauth_requests ADD COLUMN answer TEXT CHECK (answer IN ('allow', 'deny'));
ALTER TABLE oauth_requests ADD COLUMN answered_session TEXT;
