-- Soulbeet Client Core local offline-first SQLite mirror schema.

CREATE TABLE IF NOT EXISTS artists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    album_count INTEGER NOT NULL DEFAULT 0,
    cover_art_id TEXT,
    starred INTEGER NOT NULL DEFAULT 0,
    musicbrainz_id TEXT,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_artists_name ON artists(name COLLATE NOCASE);

CREATE TABLE IF NOT EXISTS albums (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    artist_id TEXT,
    artist_name TEXT NOT NULL,
    year INTEGER,
    genre TEXT,
    song_count INTEGER NOT NULL DEFAULT 0,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    cover_art_id TEXT,
    starred INTEGER NOT NULL DEFAULT 0,
    created_at TEXT,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_albums_artist_id ON albums(artist_id);
CREATE INDEX IF NOT EXISTS idx_albums_created_at ON albums(created_at DESC);

CREATE TABLE IF NOT EXISTS tracks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    artist_id TEXT,
    artist_name TEXT NOT NULL,
    album_id TEXT,
    album_name TEXT NOT NULL,
    track_number INTEGER,
    disc_number INTEGER,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    year INTEGER,
    genre TEXT,
    bit_rate INTEGER,
    suffix TEXT,
    content_type TEXT,
    size_bytes INTEGER,
    cover_art_id TEXT,
    path TEXT,
    starred INTEGER NOT NULL DEFAULT 0,
    user_rating INTEGER NOT NULL DEFAULT 0,
    play_count INTEGER NOT NULL DEFAULT 0,
    last_played_at INTEGER,
    musicbrainz_id TEXT,
    is_discovery INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tracks_album_id ON tracks(album_id);
CREATE INDEX IF NOT EXISTS idx_tracks_artist_id ON tracks(artist_id);
CREATE INDEX IF NOT EXISTS idx_tracks_starred ON tracks(starred);
CREATE INDEX IF NOT EXISTS idx_tracks_updated_at ON tracks(updated_at DESC);

-- External-content FTS5 index for sub-millisecond local search with unicode diacritic folding.
CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
    title,
    artist_name,
    album_name,
    genre,
    content='tracks',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS tracks_ai AFTER INSERT ON tracks BEGIN
    INSERT INTO tracks_fts(rowid, title, artist_name, album_name, genre)
    VALUES (new.rowid, new.title, new.artist_name, new.album_name, COALESCE(new.genre, ''));
END;

CREATE TRIGGER IF NOT EXISTS tracks_ad AFTER DELETE ON tracks BEGIN
    INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_name, album_name, genre)
    VALUES ('delete', old.rowid, old.title, old.artist_name, old.album_name, COALESCE(old.genre, ''));
END;

CREATE TRIGGER IF NOT EXISTS tracks_au AFTER UPDATE OF title, artist_name, album_name, genre ON tracks BEGIN
    INSERT INTO tracks_fts(tracks_fts, rowid, title, artist_name, album_name, genre)
    VALUES ('delete', old.rowid, old.title, old.artist_name, old.album_name, COALESCE(old.genre, ''));
    INSERT INTO tracks_fts(rowid, title, artist_name, album_name, genre)
    VALUES (new.rowid, new.title, new.artist_name, new.album_name, COALESCE(new.genre, ''));
END;

CREATE TABLE IF NOT EXISTS playlists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    comment TEXT,
    owner TEXT,
    song_count INTEGER NOT NULL DEFAULT 0,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    cover_art_id TEXT,
    is_smart INTEGER NOT NULL DEFAULT 0,
    kind TEXT NOT NULL DEFAULT 'user',
    slug TEXT,
    changed_at TEXT,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_playlists_kind ON playlists(kind);
CREATE INDEX IF NOT EXISTS idx_playlists_slug ON playlists(slug);

CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    track_id TEXT NOT NULL,
    PRIMARY KEY (playlist_id, position)
);

CREATE INDEX IF NOT EXISTS idx_playlist_tracks_track_id ON playlist_tracks(track_id);

CREATE TABLE IF NOT EXISTS discovery_tracks (
    id TEXT PRIMARY KEY,
    song_id TEXT,
    artist TEXT NOT NULL,
    title TEXT NOT NULL,
    album TEXT,
    folder_path TEXT,
    profile_slug TEXT,
    mix_slug TEXT,
    status TEXT NOT NULL,
    created_at TEXT,
    expires_at TEXT,
    reason_json TEXT,
    exclude_from_core_taste INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_discovery_tracks_status ON discovery_tracks(status);
CREATE INDEX IF NOT EXISTS idx_discovery_tracks_song_id ON discovery_tracks(song_id);

CREATE TABLE IF NOT EXISTS offline_downloads (
    track_id TEXT PRIMARY KEY,
    local_path TEXT NOT NULL,
    format TEXT NOT NULL,
    bit_rate INTEGER,
    size_bytes INTEGER NOT NULL,
    pinned INTEGER NOT NULL DEFAULT 0,
    pin_source TEXT,
    downloaded_at INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_offline_downloads_lru ON offline_downloads(pinned, last_accessed_at ASC);

CREATE TABLE IF NOT EXISTS offline_pins (
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    pinned_at INTEGER NOT NULL,
    PRIMARY KEY (target_kind, target_id)
);

CREATE TABLE IF NOT EXISTS cover_art_cache (
    cover_art_id TEXT PRIMARY KEY,
    local_path TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    downloaded_at INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_cover_art_cache_lru ON cover_art_cache(last_accessed_at ASC);

CREATE TABLE IF NOT EXISTS mutation_outbox (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    song_id TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    next_retry_at INTEGER NOT NULL DEFAULT 0,
    dead_letter INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_mutation_outbox_retry ON mutation_outbox(dead_letter, next_retry_at, seq);

CREATE TABLE IF NOT EXISTS playback_session (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    queue_json TEXT NOT NULL,
    current_index INTEGER NOT NULL DEFAULT 0,
    position_ms INTEGER NOT NULL DEFAULT 0,
    is_playing INTEGER NOT NULL DEFAULT 0,
    source_label TEXT,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_cursor (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
