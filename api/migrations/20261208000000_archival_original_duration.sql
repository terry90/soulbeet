-- Store original track duration on archival upgrade jobs and fast acquisitions,
-- and track whether a fast-acquired item was imported as-is without a MusicBrainz match.
ALTER TABLE archival_upgrade_jobs ADD COLUMN original_duration_s INTEGER;
ALTER TABLE fast_acquisitions ADD COLUMN duration_s INTEGER;
ALTER TABLE fast_acquisitions ADD COLUMN imported_as_is INTEGER NOT NULL DEFAULT 0;
