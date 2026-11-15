-- The backend request (DownloadBackend::new_request) each send of a job's files went in, so a
-- restart tells the transfers that send made from any other of the same file.
ALTER TABLE jobs ADD COLUMN request TEXT;            -- the job's latest enqueue
ALTER TABLE job_attempts ADD COLUMN request TEXT;    -- a failover's send; NULL for a first source or an adopted transfer
