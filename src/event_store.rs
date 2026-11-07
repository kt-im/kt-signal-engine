//! E: kt-events.db (outbox/dispatch) storage foundation.

use sqlx::SqlitePool;

pub(crate) async fn open(url: &str) -> Result<SqlitePool, sqlx::Error> {
    let pool = SqlitePool::connect(url).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS kt_outbox (
           seq INTEGER PRIMARY KEY,
           epoch INTEGER NOT NULL,
           dedupe_key TEXT NOT NULL UNIQUE,
           payload TEXT NOT NULL,
           confirmed INTEGER NOT NULL DEFAULT 0
         )",
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

pub(crate) async fn enqueue(
    pool: &SqlitePool,
    epoch: i64,
    dedupe_key: &str,
    payload: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO kt_outbox(epoch, dedupe_key, payload) VALUES(?1, ?2, ?3)")
        .bind(epoch)
        .bind(dedupe_key)
        .bind(payload)
        .execute(pool)
        .await?;
    sqlx::query_scalar("SELECT seq FROM kt_outbox WHERE dedupe_key=?1")
        .bind(dedupe_key)
        .fetch_one(pool)
        .await
}

pub(crate) async fn confirm(pool: &SqlitePool, seq: i64) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("UPDATE kt_outbox SET confirmed=1 WHERE seq=?1 AND confirmed=0")
        .bind(seq)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

pub(crate) async fn pull(
    pool: &SqlitePool,
    epoch: i64,
    after: i64,
    limit: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT seq, payload FROM kt_outbox
         WHERE epoch=?1 AND seq>?2 AND confirmed=0 ORDER BY seq LIMIT ?3",
    )
    .bind(epoch)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub(crate) async fn open_account(
    data_dir: &std::path::Path,
    account: &str,
) -> Result<SqlitePool, sqlx::Error> {
    let dir = data_dir.join("kt-events");
    std::fs::create_dir_all(&dir).map_err(sqlx::Error::Io)?;
    let path = dir.join(format!("{account}.db"));
    if !path.exists() {
        std::fs::File::create(&path).map_err(sqlx::Error::Io)?;
    }
    let url = format!("sqlite://{}", path.display());
    open(&url).await
}

static REGISTRY: std::sync::OnceLock<EventStoreRegistry> = std::sync::OnceLock::new();

pub(crate) fn global() -> &'static EventStoreRegistry {
    REGISTRY.get_or_init(EventStoreRegistry::default)
}

#[derive(Default)]
pub(crate) struct EventStoreRegistry {
    pools: tokio::sync::Mutex<std::collections::HashMap<String, SqlitePool>>,
}

impl EventStoreRegistry {
    pub(crate) async fn get(
        &self,
        data_dir: &std::path::Path,
        account: &str,
    ) -> Result<SqlitePool, sqlx::Error> {
        let mut pools = self.pools.lock().await;
        if let Some(pool) = pools.get(account) {
            return Ok(pool.clone());
        }
        let pool = open_account(data_dir, account).await?;
        pools.insert(account.to_string(), pool.clone());
        Ok(pool)
    }
}

pub(crate) async fn enqueue_duplicate(
    pool: &SqlitePool,
    epoch: i64,
    sender: &str,
    timestamp: u64,
    payload: &str,
) -> Result<i64, sqlx::Error> {
    let key = crate::outbox_model::duplicate_dedupe_key(sender, timestamp);
    enqueue(pool, epoch, &key, payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kt_events_schema_is_usable() {
        let pool = open("sqlite::memory:").await.unwrap();
        sqlx::query(
            "INSERT INTO kt_outbox(seq, epoch, dedupe_key, payload) VALUES(1, 1, 'a', '{}')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kt_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        let seq = enqueue(&pool, 1, "a", "{}").await.unwrap();
        assert_eq!(enqueue(&pool, 1, "a", "{}").await.unwrap(), seq);
        assert_eq!(pull(&pool, 1, 0, 10).await.unwrap().len(), 1);
        assert!(confirm(&pool, seq).await.unwrap());
        assert!(!confirm(&pool, seq).await.unwrap());
        assert!(pull(&pool, 1, 0, 10).await.unwrap().is_empty());
        assert!(pull(&pool, 2, 0, 10).await.unwrap().is_empty());
        let d = enqueue_duplicate(&pool, 1, "aci", 42, "[]").await.unwrap();
        assert_eq!(
            enqueue_duplicate(&pool, 1, "aci", 42, "[]").await.unwrap(),
            d
        );
        assert_eq!(pull(&pool, 1, 0, 10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn account_db_path_is_isolated() {
        let dir = std::env::temp_dir().join(format!("kt-events-dir-{}", std::process::id()));
        let pool = open_account(&dir, "acct-1").await.unwrap();
        enqueue(&pool, 1, "a", "{}").await.unwrap();
        drop(pool);
        let pool = open_account(&dir, "acct-1").await.unwrap();
        assert_eq!(pull(&pool, 1, 0, 10).await.unwrap().len(), 1);
        drop(pool);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn registry_reuses_same_pool() {
        let dir = std::env::temp_dir().join(format!("kt-events-reg-{}", std::process::id()));
        let registry = EventStoreRegistry::default();
        let a = registry.get(&dir, "a").await.unwrap();
        let b = registry.get(&dir, "a").await.unwrap();
        assert_eq!(a.size(), b.size());
        drop(a);
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn account_outbox_is_isolated() {
        let dir = std::env::temp_dir().join(format!("kt-events-iso-{}", std::process::id()));
        let a = open_account(&dir, "acct-a").await.unwrap();
        let b = open_account(&dir, "acct-b").await.unwrap();
        enqueue(&a, 1, "a", "{}").await.unwrap();
        assert_eq!(pull(&a, 1, 0, 10).await.unwrap().len(), 1);
        assert!(pull(&b, 1, 0, 10).await.unwrap().is_empty());
        drop(a);
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn outbox_survives_reopen() {
        let path = std::env::temp_dir().join(format!("kt-events-{}.db", std::process::id()));
        std::fs::File::create(&path).unwrap();
        let url = format!("sqlite://{}", path.display());
        let seq = {
            let pool = open(&url).await.unwrap();
            enqueue(&pool, 1, "persist", "{}").await.unwrap()
        };
        let pool = open(&url).await.unwrap();
        assert_eq!(pull(&pool, 1, 0, 10).await.unwrap().len(), 1);
        assert!(confirm(&pool, seq).await.unwrap());
        drop(pool);
        let _ = std::fs::remove_file(&path);
    }
}
