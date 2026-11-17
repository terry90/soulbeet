-- The change number the job tools answer and take (cursor, change_cursor, since), counted over
-- one user's jobs, so it tells nothing of how many changes other users' jobs made. changed_seq
-- stays the ledger's own, over every job. Each job's number starts at its changed_seq, so every
-- cursor already handed out stays below the user's next change.
ALTER TABLE jobs ADD COLUMN user_seq INTEGER NOT NULL DEFAULT 0;
UPDATE jobs SET user_seq = changed_seq;
CREATE INDEX jobs_user_seq ON jobs (user_id, user_seq);
