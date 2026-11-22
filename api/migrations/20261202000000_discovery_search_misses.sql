-- Tracks whose source search found nothing in a discovery run, by user and seen_key, with the
-- number of runs that found nothing.
CREATE TABLE discovery_search_misses (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    track_key TEXT NOT NULL,
    misses INTEGER NOT NULL,
    PRIMARY KEY (user_id, track_key)
);
