-- What a download backend records of its own transfers, to find them again after a restart.
CREATE TABLE backend_transfers (
    backend TEXT NOT NULL,
    key TEXT NOT NULL,
    transfer_id TEXT,
    file_id TEXT,
    source TEXT NOT NULL,
    item TEXT NOT NULL,
    request TEXT NOT NULL,
    target TEXT NOT NULL,
    data TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    PRIMARY KEY (backend, key)
);
CREATE UNIQUE INDEX backend_transfers_id ON backend_transfers (backend, transfer_id)
    WHERE transfer_id IS NOT NULL;
CREATE INDEX backend_transfers_source ON backend_transfers (backend, source);
