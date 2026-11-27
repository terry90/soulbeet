-- Expand contextual mixes with day-of-week schedules and library selection rules,
-- retire the built-in Diwali pack, open Lunar New Year and Carnival globally,
-- and extend Morning Lift to start at 05:00.

ALTER TABLE contextual_mix_definitions ADD COLUMN days_of_week TEXT NOT NULL DEFAULT '[]';
ALTER TABLE contextual_mix_definitions ADD COLUMN selection_rules TEXT;

DELETE FROM contextual_mix_definitions WHERE slug = 'diwali' AND built_in = 1;
DELETE FROM contextual_mix_states
WHERE slug = 'diwali'
  AND navidrome_playlist_id IS NULL
  AND NOT EXISTS (SELECT 1 FROM contextual_mix_definitions WHERE slug = 'diwali');

UPDATE contextual_mix_definitions
SET countries = '[]'
WHERE slug = 'lunar_new_year'
  AND built_in = 1
  AND countries = '["CN","TW","HK","SG","VN","KR","MY"]';

UPDATE contextual_mix_definitions
SET countries = '[]'
WHERE slug = 'carnival'
  AND built_in = 1
  AND countries = '["BR","CO","TT","DE","IT"]';

UPDATE contextual_mix_definitions
SET hour_window = '{"start_hour":5,"end_hour":10}'
WHERE slug = 'morning' AND built_in = 1 AND hour_window = '{"start_hour":6,"end_hour":10}';
