-- Usernames are unique without regard to ASCII letter case. A unique index
-- cannot be built while older rows collide, so triggers refuse new
-- collisions and the rows that already collide stay until an admin deletes
-- the extra ones (sign-in refuses them meanwhile).
CREATE TRIGGER IF NOT EXISTS users_username_unique_nocase_insert
BEFORE INSERT ON users
WHEN EXISTS (SELECT 1 FROM users WHERE username = NEW.username COLLATE NOCASE)
BEGIN
    SELECT RAISE(ABORT, 'username taken');
END;

CREATE TRIGGER IF NOT EXISTS users_username_unique_nocase_update
BEFORE UPDATE OF username ON users
WHEN EXISTS (
    SELECT 1 FROM users WHERE username = NEW.username COLLATE NOCASE AND id <> NEW.id
)
BEGIN
    SELECT RAISE(ABORT, 'username taken');
END;
