CREATE TABLE IF NOT EXISTS kt_outbox (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  dedupe_key TEXT NOT NULL UNIQUE,
  sender TEXT NOT NULL,
  client_timestamp INTEGER NOT NULL,
  content BLOB NOT NULL,
  metadata_json TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
