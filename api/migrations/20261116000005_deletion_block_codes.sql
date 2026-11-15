-- The tracks auto-delete kept at a user's last sync, with the refusal's code beside its message,
-- so a reader can tell why without parsing text. The rows are rebuilt by each sync, so the old
-- ones go.
DROP TABLE deletion_blocks;
CREATE TABLE deletion_blocks (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    song_id TEXT NOT NULL,
    title TEXT NOT NULL,
    artist TEXT NOT NULL,
    code TEXT NOT NULL,
    message TEXT NOT NULL,
    PRIMARY KEY (user_id, song_id)
);
