# ADR 0004: Presage receive outbox boundary

Status: accepted; amended 2026-11-10 (phase1 stops writing kt_outbox;
phase2 drops the table and the remaining compatibility surface)
Date: 2026-11-07

## Context

Received::Duplicate means presage already stored the envelope. The engine's separate kt-events.db outbox cannot be written atomically with that store transaction, so a crash between presage save and outbox write can ack a redelivery with no recoverable row.

## Decision

Presage store gains a kt_outbox table written inside the same transaction as save_messages:

- seq INTEGER PRIMARY KEY AUTOINCREMENT
- dedupe_key TEXT UNIQUE
- sender TEXT
- client_timestamp INTEGER
- content BLOB (Content protobuf)
- created_at INTEGER

The engine reads this table by dedupe_key on Received::Duplicate. Projection must use a dedicated read-only store connection, never the receive manager lock. After projection and enqueue into the product outbox, the engine confirms the row. Rows are pruned only after confirmation.

## Consequences

- The presage save transaction becomes the durable receive boundary.
- The engine needs a duplicate-specific projector boundary; calling the normal project_content under the receive manager lock is forbidden.
- The product outbox remains the connector-facing event log; the presage outbox is only the crash-recovery source.

## Amendment: phase1 (2026-11-10, decision B)

The recovery copy is retired in favour of the durable `thread_messages` row:

- `save_messages` no longer writes `kt_outbox`; the table and migration stay
  in place (DROP is phase2).
- Duplicate replay rebuilds `Content` from `thread_messages`. The receive arm
  resolves the envelope's thread and reads thread+ts, which also finds the
  nil-ACI `NullMessage` tombstone written by a remote delete; the legacy
  `dup:{sender}:{ts}` key is parsed into query parameters only, never used as
  a store row key.
- Replay preserves `pni_verified` and now also preserves `server_ts` (the
  receive insert previously left it unset).
- `receive.confirm` is an idempotent no-op (known account -> true).
- Only a store-confirmed empty result (`Ok(None)`) returns Unavailable, waits
  for the flush barrier, then acks. A store open/read/decode failure is not a
  miss: it returns Failed and withholds the ack (the row state is unknown).

Details and tests: `docs/receive-duplicate-phase1.md`.

## Amendment: phase2 (2026-11-10, decision B completed)

The recovery copy is gone; the durable `thread_messages` row is the only
source.

- New presage migration `20261110120000_drop_kt_outbox.sql`
  (`DROP TABLE IF EXISTS kt_outbox`). sqlx applies it to an existing store the
  next time it is opened; a fresh store runs create-then-drop in one chain, so
  both converge on "no table". The table's implicit UNIQUE index on
  `dedupe_key` disappears with the table; it never had an explicit index.
- Store read surface `kt_outbox_get` / `kt_outbox_confirm` /
  `kt_outbox_len` removed; `outbox_model` keeps only the
  `dup:{sender}:{ts}` wire-key helpers (the key is never a row identity).
- Engine `event_store` module, `Service::event_pool` and
  `reconstruct_outbox_content` removed. `kt-events.db` files written by older
  runs are inert leftovers — nothing reads or writes them any more.
- `receive.confirm` / `receive.replay` stay for one compatibility version
  (zero call sites in kt-signal-connector, verified). `confirm` is an
  idempotent no-op (known account -> `true`, unknown -> `false`, dedupe key
  ignored); `replay` still rebuilds from `thread_messages` by parsing the key
  into sender+ts. Wire shape unchanged.
- Duplicate replay's store read surface returns `Result<Option<_>>`: store
  open, row query and Content decode errors are `Failed` (ack withheld), never
  folded into the "no row" `Unavailable` path that flushes and acks. Only a
  successful query with an empty result is a confirmed miss.
- Rollback: sqlx refuses to open a database whose `_sqlx_migrations` row set is
  newer than the binary's migrator, so a binary downgrade also needs
  `vendor/presage/presage-store-sqlite/rollback/20261110120000_drop_kt_outbox_rollback.sql`
  applied per account store. It recreates the phase1 table (with its UNIQUE
  index) and clears the migration row; the phase2 binary re-applies the drop on
  its next open.
- Evidence: fresh chain has no `kt_outbox`; a phase1-schema store with a row
  upgrades to no table with unrelated rows intact; the rollback script restores
  the pre-drop schema including the UNIQUE constraint and re-forward works;
  reopening an upgraded store is a no-op; redelivery keeps one row; duplicate
  replay still rebuilds from `thread_messages` after the upgrade;
  `receive.confirm` is a no-op at both the service and wire level.

Details and test names: `docs/receive-duplicate-phase1.md` §7.
