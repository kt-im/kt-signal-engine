-- Rollback for migrations/20261110120000_drop_kt_outbox.sql (KT receive
-- duplicate phase2, decision B).
--
-- When to run this: only to downgrade the engine binary to a phase1 build that
-- still expects the kt_outbox table. sqlx refuses to open a database whose
-- _sqlx_migrations row set is newer than the binary's embedded migrator, so a
-- pure binary rollback is not enough — the migration bookkeeping has to point
-- back at the pre-drop schema too.
--
-- Run once per account store (each account owns its own store.db):
--   sqlite3 accounts/<account>/store.db < 20261110120000_drop_kt_outbox_rollback.sql
--
-- Idempotent: IF NOT EXISTS / DELETE ... WHERE keep a second run a no-op. The
-- current (phase2) binary re-applies the drop on its next open, so this script
-- is a rollback-window measure only — anything written to kt_outbox while the
-- phase1 binary runs is discarded on the forward move. Phase1 code stopped
-- writing the table, so that window is expected to stay empty.
--
-- No transaction wrapper: sqlx executes each migration inside its own
-- transaction, and this file is also applied through sqlx::raw_sql in tests.
CREATE TABLE IF NOT EXISTS kt_outbox (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  dedupe_key TEXT NOT NULL UNIQUE,
  sender TEXT NOT NULL,
  client_timestamp INTEGER NOT NULL,
  content BLOB NOT NULL,
  metadata_json TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

DELETE FROM _sqlx_migrations WHERE version = 20261110120000;
