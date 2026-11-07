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
    }
}
