-- Where each Navidrome library is on this machine, learned from a file Soulful imported.
CREATE TABLE navidrome_libraries (
    library_id INTEGER PRIMARY KEY NOT NULL,
    navidrome_path TEXT NOT NULL,   -- the library's path as Navidrome reports it (libraryPath)
    local_path TEXT NOT NULL,       -- where that path is on this machine
    learned_at TEXT NOT NULL DEFAULT (datetime('now'))
);
