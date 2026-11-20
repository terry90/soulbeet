-- Off until the user turns it on: Deezer has not approved this use.
ALTER TABLE user_settings ADD COLUMN deezer_recommendations BOOLEAN NOT NULL DEFAULT 0;
