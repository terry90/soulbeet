-- Two columns nothing reads: when a library's root was learned (the root is kept while it is a
-- directory, whatever its age), and when a delete token was made (its expiry is what counts).
ALTER TABLE navidrome_libraries DROP COLUMN learned_at;
ALTER TABLE delete_tokens DROP COLUMN created_at;
