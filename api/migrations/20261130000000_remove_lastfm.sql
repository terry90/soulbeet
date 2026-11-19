-- Soulbeet no longer uses Last.fm: MusicBrainz is the only metadata provider and discovery
-- reads Navidrome and ListenBrainz. No index, view or trigger names these columns.
ALTER TABLE user_settings DROP COLUMN default_metadata_provider;
ALTER TABLE user_settings DROP COLUMN lastfm_api_key;
ALTER TABLE user_settings DROP COLUMN lastfm_username;
