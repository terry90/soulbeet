-- slskd's backend id is "slskd" everywhere. Rows stamped "soulseek", and batch rows written
-- without a backend, come from slskd, the only download backend before this.
UPDATE job_tracks SET last_row = json_set(last_row, '$.backend', 'slskd')
 WHERE last_row IS NOT NULL
   AND (json_extract(last_row, '$.backend') = 'soulseek'
        OR (json_extract(last_row, '$.backend') IS NULL AND json_extract(last_row, '$.batch_id') IS NOT NULL));

UPDATE job_attempts SET last_row = json_set(last_row, '$.backend', 'slskd')
 WHERE last_row IS NOT NULL
   AND (json_extract(last_row, '$.backend') = 'soulseek'
        OR (json_extract(last_row, '$.backend') IS NULL AND json_extract(last_row, '$.batch_id') IS NOT NULL));

-- StoredImport.rows is a list of [slot, row] pairs.
UPDATE job_tracks SET import_job = json_set(import_job, '$.rows', json((
    SELECT json_group_array(
             CASE WHEN json_extract(r.value, '$[1].backend') = 'soulseek'
                    OR (json_extract(r.value, '$[1].backend') IS NULL
                        AND json_extract(r.value, '$[1].batch_id') IS NOT NULL)
                  THEN json_set(r.value, '$[1].backend', 'slskd')
                  ELSE json(r.value) END
             ORDER BY r.key)
      FROM json_each(job_tracks.import_job, '$.rows') AS r)))
 WHERE import_job IS NOT NULL;
