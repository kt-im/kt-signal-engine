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
    }
}
