-- The Soulbeet version whose release notes each user last closed in "What's new".
-- Accounts from before this column ran 0.8.0, so they are shown the releases after
-- it. Every insert names the version (User::create_in, db::migrate); the empty
-- default only marks a row that did not, and reading it fails.
ALTER TABLE users ADD COLUMN changelog_seen TEXT NOT NULL DEFAULT '';
UPDATE users SET changelog_seen = '0.8.0';
