# ADR 0004: Presage receive outbox boundary

Status: proposed
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
