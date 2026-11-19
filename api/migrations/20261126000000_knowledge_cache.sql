-- Facts about music from ListenBrainz, MusicBrainz and Deezer, shared by every user of the install.
CREATE TABLE knowledge_cache (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    -- Unix seconds.
    expires_at INTEGER NOT NULL
);
CREATE INDEX knowledge_cache_expires_at ON knowledge_cache (expires_at);
