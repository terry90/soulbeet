-- Nothing reads the profile hash: the profile is rebuilt every run.
ALTER TABLE user_profiles DROP COLUMN top_artists_hash;
