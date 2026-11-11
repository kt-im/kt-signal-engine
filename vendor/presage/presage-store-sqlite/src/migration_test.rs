// SPDX-License-Identifier: AGPL-3.0-only

//! KT (receive duplicate phase2): migration-level evidence for dropping
//! `kt_outbox` — fresh chain, phase1-era upgrade, rollback script, re-forward,
//! and idempotent re-runs. Kept out of `content.rs` because these tests assert
//! schema state, not message round-trips.

use std::path::{Path, PathBuf};

use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

use crate::{OnNewIdentity, SqliteStore};

/// Version of `20261107000000_add_kt_outbox.sql` — the last schema that still
/// carried the table.
const PRE_DROP_VERSION: i64 = 20_261_107_000_000;
const ROLLBACK_SQL: &str = include_str!("../rollback/20261110120000_drop_kt_outbox_rollback.sql");

fn temp_db(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "kt-store-migrate-{tag}-{}-{stamp}.db",
        std::process::id()
    ))
}

fn store_options(path: &Path) -> SqliteConnectOptions {
    path.to_str()
        .expect("temp path is utf-8")
        .parse::<SqliteConnectOptions>()
        .expect("sqlite options")
        .create_if_missing(true)
        .foreign_keys(true)
}

async fn raw_pool(path: &Path) -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(store_options(path))
        .await
        .expect("raw sqlite pool")
}

async fn table_present(pool: &SqlitePool, name: &str) -> bool {
    let present: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1")
            .bind(name)
            .fetch_one(pool)
            .await
            .expect("sqlite_master query");
    present == 1
}

async fn store_has_kt_outbox(store: &SqliteStore) -> bool {
    table_present(&store.db, "kt_outbox").await
}

/// Fresh data dir: the chain creates the table (phase0/phase1) and drops it
/// (phase2) in one open, so a new store never exposes kt_outbox.
#[tokio::test]
async fn fresh_store_never_exposes_kt_outbox() -> Result<(), Box<dyn std::error::Error>> {
    let store = SqliteStore::open(":memory:", OnNewIdentity::Trust).await?;
    assert!(
        !store_has_kt_outbox(&store).await,
        "phase2: fresh stores must not carry kt_outbox"
    );
    Ok(())
}

/// Old library upgrade: a store stopped at the phase1 schema (table present,
/// with a row) converges to "no table" on the first open with the phase2
/// migrator, and unrelated data survives untouched.
#[tokio::test]
async fn upgrade_from_phase1_schema_drops_kt_outbox_and_keeps_data()
-> Result<(), Box<dyn std::error::Error>> {
    let path = temp_db("upgrade");
    let pool = raw_pool(&path).await;

    sqlx::migrate!("./migrations")
        .run_to(PRE_DROP_VERSION, &pool)
        .await?;
    assert!(
        table_present(&pool, "kt_outbox").await,
        "precondition: phase1 schema carries kt_outbox"
    );
    sqlx::query(
        "INSERT INTO kt_outbox(dedupe_key, sender, client_timestamp, content, metadata_json, created_at)
         VALUES('dup:sender:1', 'sender', 1, ?1, '{}', 1)",
    )
    .bind(vec![1u8, 2, 3])
    .execute(&pool)
    .await?;
    sqlx::query("INSERT OR REPLACE INTO kv(key, value) VALUES('kt_upgrade_marker', ?1)")
        .bind(b"kept".to_vec())
        .execute(&pool)
        .await?;
    drop(pool);

    let store = SqliteStore::open_with_options(store_options(&path), OnNewIdentity::Trust).await?;
    assert!(
        !store_has_kt_outbox(&store).await,
        "upgrade must drop kt_outbox"
    );
    assert_eq!(
        store.kt_kv_get("kt_upgrade_marker").await?.as_deref(),
        Some("kept"),
        "upgrade must not touch unrelated rows"
    );

    drop(store);
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Rollback path (binary downgrade): the script recreates the exact pre-drop
/// schema, including the UNIQUE index on dedupe_key, and clears the migration
/// bookkeeping so an older migrator resolves the database again.
#[tokio::test]
async fn rollback_script_restores_the_pre_drop_schema() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_db("rollback");
    {
        let store =
            SqliteStore::open_with_options(store_options(&path), OnNewIdentity::Trust).await?;
        assert!(!store_has_kt_outbox(&store).await);
    }

    let pool = raw_pool(&path).await;
    sqlx::raw_sql(ROLLBACK_SQL).execute(&pool).await?;
    assert!(
        table_present(&pool, "kt_outbox").await,
        "rollback must recreate kt_outbox"
    );
    let columns: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('kt_outbox') ORDER BY cid")
            .fetch_all(&pool)
            .await?;
    assert_eq!(
        columns,
        vec![
            "seq",
            "dedupe_key",
            "sender",
            "client_timestamp",
            "content",
            "metadata_json",
            "created_at"
        ],
        "rollback table must match the phase1 column order"
    );
    let insert = "INSERT INTO kt_outbox(dedupe_key, sender, client_timestamp, content, metadata_json, created_at)
                  VALUES('dup:a:1', 'a', 1, x'00', '{}', 1)";
    sqlx::query(insert).execute(&pool).await?;
    assert!(
        sqlx::query(insert).execute(&pool).await.is_err(),
        "rollback must restore the dedupe_key UNIQUE index"
    );
    // Running the rollback twice stays a no-op (idempotent script).
    sqlx::raw_sql(ROLLBACK_SQL).execute(&pool).await?;
    let versions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 20261110120000")
            .fetch_one(&pool)
            .await?;
    assert_eq!(versions, 0, "rollback clears the drop-migration row");
    drop(pool);

    // Forward again: the embedded migrator re-applies the drop, so the current
    // binary can open a rolled-back store.
    let store = SqliteStore::open_with_options(store_options(&path), OnNewIdentity::Trust).await?;
    assert!(
        !store_has_kt_outbox(&store).await,
        "reopening after rollback must re-apply the drop"
    );

    drop(store);
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Idempotency: reopening an upgraded store re-runs the whole migration chain
/// as a no-op, and re-executing the DROP statement itself stays harmless.
#[tokio::test]
async fn drop_migration_is_idempotent_on_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_db("idempotent");
    {
        let store =
            SqliteStore::open_with_options(store_options(&path), OnNewIdentity::Trust).await?;
        store.kt_kv_set("kt_reopen_marker", "kept").await?;
        assert!(!store_has_kt_outbox(&store).await);
    }
    {
        let store =
            SqliteStore::open_with_options(store_options(&path), OnNewIdentity::Trust).await?;
        assert!(!store_has_kt_outbox(&store).await);
        assert_eq!(
            store.kt_kv_get("kt_reopen_marker").await?.as_deref(),
            Some("kept")
        );
    }
    let pool = raw_pool(&path).await;
    sqlx::raw_sql("DROP TABLE IF EXISTS kt_outbox; DROP TABLE IF EXISTS kt_outbox;")
        .execute(&pool)
        .await?;
    assert!(!table_present(&pool, "kt_outbox").await);
    drop(pool);

    let _ = std::fs::remove_file(&path);
    Ok(())
}
