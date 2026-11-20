-- Nothing reads the profile or its hash: the profile is rebuilt every run.
ALTER TABLE user_profiles DROP COLUMN top_artists_hash;
ALTER TABLE user_profiles DROP COLUMN profile_json;
