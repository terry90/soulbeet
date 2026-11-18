-- The entries of other libraries a batch's playlist fill dropped by rewriting the list whole (design 11.5, 11.8).
ALTER TABLE batches ADD COLUMN playlist_hidden_dropped INTEGER NOT NULL DEFAULT 0;
