-- Stored download items name their backend. Every item stored before this came from slskd.
-- The ordered aggregates keep the order of groups and items, which candidate ids depend on.
UPDATE job_attempts SET candidate = json_set(candidate, '$.backend', 'slskd') WHERE candidate IS NOT NULL;
UPDATE searches SET candidates = (
  SELECT json_group_array(json_set(g.value, '$.items', json((
           SELECT json_group_array(json_set(i.value, '$.backend', 'slskd') ORDER BY i.key)
           FROM json_each(g.value, '$.items') AS i))) ORDER BY g.key)
  FROM json_each(searches.candidates) AS g)
 WHERE candidates IS NOT NULL;
