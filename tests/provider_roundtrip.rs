// SPDX-License-Identifier: AGPL-3.0-only

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;
use std::path::PathBuf;

fn sandbox(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kt-provider-roundtrip-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create sandbox");
    dir
}

fn options(path: &PathBuf, key: &str) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .pragma("key", format!("'{key}'"))
}

#[tokio::test]
async fn encrypted_roundtrip_under_current_provider() {
    let dir = sandbox("roundtrip");
    let db = dir.join("store.db");
    let key = "0123456789abcdef0123456789abcdef";

    // ── ① 建库 + 写入 ────────────────────────────────────────────
    {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options(&db, key))
            .await
            .expect("open encrypted store");

        let cipher_version: String = sqlx::query("PRAGMA cipher_version")
            .fetch_one(&pool)
            .await
            .expect("cipher_version pragma")
            .get(0);

        assert!(
            !cipher_version.trim().is_empty(),
            "cipher_version 为空 —— 说明链的是纯 SQLite，加密根本没生效"
        );
        // 只有名字能证明「测的是哪个 provider」。历史上这块吃过亏：
        // 改了 vendored C / feature 但 cargo 没重编，测试跑在旧二进制上，
        // 「通过」是假象。所以这里打出来并且按平台硬断言。
        let provider: String = sqlx::query("PRAGMA cipher_provider")
            .fetch_one(&pool)
            .await
            .map(|row| row.get(0))
            .unwrap_or_else(|_| "unknown".to_string());
        eprintln!("✅ cipher_version = {cipher_version} (provider = {provider})");
        // Windows 走自定义 Rust provider（SQLCIPHER_CRYPTO_CUSTOM），名字由
        // src/sqlcipher_provider.rs 里的 PROVIDER_NAME 决定。断言它，是为了防
        // 「门禁测错对象」：历史上改过 feature/构建配置但 cargo 没重编，
        // 测试跑在旧二进制上照样绿。
        #[cfg(target_os = "windows")]
        assert_eq!(
            provider, "rustcrypto-kt",
            "Windows 必须走自定义 Rust provider，实际是 {provider} —— 门禁测错了对象"
        );

        sqlx::query("CREATE TABLE probe (id INTEGER PRIMARY KEY, body TEXT NOT NULL)")
            .execute(&pool)
            .await
            .expect("create table");
        sqlx::query("INSERT INTO probe (body) VALUES (?)")
            .bind("the quick brown fox 跳过了懒狗")
            .execute(&pool)
            .await
            .expect("insert");

        pool.close().await;
    }

    // ── ② 落盘是密文 ────────────────────────────────────────────
    let head = std::fs::read(&db).expect("read db");
    assert!(
        !head.starts_with(b"SQLite format 3"),
        "文件头是明文 SQLite 魔数 —— 加密没有落盘"
    );
    eprintln!(
        "✅ 文件头非明文：{:02x}{:02x}{:02x}{:02x}",
        head[0], head[1], head[2], head[3]
    );

    // ── ③ 正确密钥能读回 ────────────────────────────────────────
    {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options(&db, key))
            .await
            .expect("reopen encrypted store");

        let body: String = sqlx::query("SELECT body FROM probe WHERE id = 1")
            .fetch_one(&pool)
            .await
            .expect("select")
            .get(0);
        assert_eq!(body, "the quick brown fox 跳过了懒狗", "读回内容不一致");
        eprintln!("✅ 换密钥重开读回一致");
        pool.close().await;
    }

    // ── ④ 错误密钥必须打不开 ────────────────────────────────────
    let wrong = options(&db, "ffffffffffffffffffffffffffffffff");
    let wrong_open = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(wrong)
        .await;
    let rejected = match wrong_open {
        // sqlx 可能在连接后才报，实际读一次确认
        Ok(pool) => {
            let r = sqlx::query("SELECT count(*) FROM probe")
                .fetch_one(&pool)
                .await
                .is_err();
            pool.close().await;
            r
        }
        Err(_) => true,
    };
    assert!(rejected, "错误密钥竟然能读 —— HMAC 校验没有生效");
    eprintln!("✅ 错误密钥被拒绝");

    let _ = std::fs::remove_dir_all(&dir);
}
