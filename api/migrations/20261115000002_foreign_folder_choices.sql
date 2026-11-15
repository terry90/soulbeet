-- Folder choices saved before settings checked ownership can name another
-- user's folder. Clear them so the discovery and expiry code never acts on a
-- folder the user does not own.
UPDATE user_settings SET discovery_folder_id = NULL
WHERE EXISTS (
    SELECT 1 FROM folders f
    WHERE f.id = user_settings.discovery_folder_id AND f.user_id <> user_settings.user_id
);

UPDATE user_settings SET default_download_folder_id = NULL
WHERE EXISTS (
    SELECT 1 FROM folders f
    WHERE f.id = user_settings.default_download_folder_id AND f.user_id <> user_settings.user_id
);
