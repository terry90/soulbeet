-- The playlist an ensure_in_library batch fills as its jobs finish (design 8.3, 11.5).
ALTER TABLE batches ADD COLUMN playlist_id TEXT;
ALTER TABLE batches ADD COLUMN playlist_order TEXT;     -- JSON list of OrderEntry, one per item in wanted order
ALTER TABLE batches ADD COLUMN playlist_failure TEXT;   -- Failure JSON of the last fill that failed
