// SPDX-License-Identifier: AGPL-3.0-only

//! M6 P0 运行时能力门禁的断言（原 `src/store_cipher.rs` 内联 `mod tests`
//! 迁出）。
//!
//! 迁出理由：`src/store_cipher.rs` 现在被 `src/bin/backup5-spike.rs` 用
//! `#[path]` 同源引入（同一条门），而那个 bin crate 里没有 `crate::service`；
//! 依赖 `open_store` 的断言留在主 bin 的测试面，两边都编得干净。
//!
//! 口径：
//! - (a)(b)(c) 原有三条：能力可探、明文库行为不变、`PRAGMA key` 不是 no-op；
//! - (d) 评审第 1 条：门**前置**——缺能力时一个文件都不落（连目标目录都不
//!   建），更不会进 `open_with_options`；
//! - (e) 评审第 2 条：失败进负缓存——`:memory:` 探测每进程一次、失败证据行
//!   每进程一条，后续 open 只回错误；
//! - (f) 正缓存对照：能力可用时同样只探一次。

use std::path::{Path, PathBuf};

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};

use crate::store_cipher::{self, CipherGate, CipherProbeError};

/// 测试沙盒目录（进程 id + 测试名隔离；用后即删）。
fn sandbox(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kt-engine-storecipher-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmp dir");
    dir
}

/// 目录里的条目数（不存在按 0 计——「零文件」断言用它）。
fn entry_count(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.count(),
        Err(_) => 0,
    }
}

/// 裸连接（**不设 key**）：`create_if_missing` 由调用方给，用于证明
/// 「无 key 的世界」看到什么。
async fn connect_plain(path: &Path, create_if_missing: bool) -> SqliteConnection {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(create_if_missing);
    SqliteConnection::connect_with(&options)
        .await
        .expect("plain sqlite connection")
}

/// 明文 SQLite 文件头（前 16 字节）。
const PLAINTEXT_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// (a) 正常构建下 `cipher_version` 非空，且落盘证据行带 SQLCipher 标识。
#[tokio::test]
async fn cipher_version_is_present_and_identifies_sqlcipher() {
    let version = store_cipher::read_cipher_version()
        .await
        .expect("SQLCipher build answers PRAGMA cipher_version");
    assert!(
        !version.trim().is_empty(),
        "cipher_version must not be empty, got {version:?}"
    );
    // 形状：`<major>.<minor>.<patch>[ <build>]`（本图 `4.10.0 community`）。
    let number = version.split_whitespace().next().unwrap_or_default();
    let numeric = number.split('.').all(|part| part.parse::<u32>().is_ok());
    assert!(
        numeric && number.matches('.').count() >= 2,
        "unexpected cipher_version shape: {version:?}"
    );
    // 产品标识由证据行承担（SQLCipher 自身的版本串不含产品名）。
    let line = store_cipher::capability_line(&version);
    assert!(
        line.contains("SQLCipher") && line.contains(&version),
        "capability evidence must identify SQLCipher: {line:?}"
    );
}

/// (b) 未设 key 的库仍可读写：存量明文库行为不变（文件头仍明文、无 key
/// 的裸连接能读能写、重走引擎打开路径后数据还在）。
#[tokio::test]
async fn keyless_store_remains_plaintext_readable_and_writable() {
    let dir = sandbox("plaintext");
    let db = dir.join("store.db");
    // 引擎路径建库（跑迁移）。
    drop(crate::service::open_store(&dir).await.expect("store opens"));
    let head = std::fs::read(&db).expect("store.db readable");
    assert_eq!(
        &head[..16],
        PLAINTEXT_HEADER,
        "without a key the store must stay plaintext SQLite"
    );

    // 读：迁移账本在无 key 连接上可见。
    let mut plain = connect_plain(&db, false).await;
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&mut plain)
        .await
        .expect("plaintext read");
    assert!(applied > 0, "migrations must be visible without a key");

    // 写：无 key 连接也能写。
    sqlx::raw_sql("CREATE TABLE p0_keyless_probe (k TEXT PRIMARY KEY, v TEXT NOT NULL)")
        .execute(&mut plain)
        .await
        .expect("plaintext ddl");
    sqlx::raw_sql("INSERT INTO p0_keyless_probe (k, v) VALUES ('probe', 'value')")
        .execute(&mut plain)
        .await
        .expect("plaintext insert");
    drop(plain);

    // 重走引擎打开路径：能力门禁不误伤明文库，写入的数据仍在。
    drop(
        crate::service::open_store(&dir)
            .await
            .expect("plaintext store reopens"),
    );
    let mut plain = connect_plain(&db, false).await;
    let value: String = sqlx::query_scalar("SELECT v FROM p0_keyless_probe WHERE k = 'probe'")
        .fetch_one(&mut plain)
        .await
        .expect("plaintext read back");
    assert_eq!(value, "value");
    drop(plain);

    let _ = std::fs::remove_dir_all(&dir);
}

/// (c) 设 key 后文件不可被无 key 打开：`PRAGMA key` 不是 no-op。
#[tokio::test]
async fn keyed_store_is_unreadable_without_key() {
    let dir = sandbox("keyed");
    let db = dir.join("store.db");
    // raw key 形态与 connector 同源（`x'<64hex>'`，跳过 PBKDF2）。sqlx 的
    // `pragma()` 原样拼 `PRAGMA key = <值>`（不加引号，见
    // sqlx-sqlite 0.9 options/connect.rs pragma_string），SQLCipher 的 raw
    // key 规格要用双引号包住才被解析器接受。
    const RAW_KEY: &str = "\"x'000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f'\"";

    // 带 key 建库 + 写一行。
    let mut keyed = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&db)
            .create_if_missing(true)
            .pragma("key", RAW_KEY),
    )
    .await
    .expect("keyed connection");
    sqlx::raw_sql("CREATE TABLE secret (v TEXT NOT NULL)")
        .execute(&mut keyed)
        .await
        .expect("keyed ddl");
    sqlx::raw_sql("INSERT INTO secret (v) VALUES ('classified')")
        .execute(&mut keyed)
        .await
        .expect("keyed insert");
    drop(keyed);

    // 文件头是盐（16 字节），不是明文 SQLite 头。
    let head = std::fs::read(&db).expect("keyed db readable");
    assert_ne!(
        &head[..16],
        PLAINTEXT_HEADER,
        "a keyed store must not carry the plaintext header"
    );

    // 无 key 直接 open + 查询 → 必须报错（SQLCipher: `file is not a
    // database`，sqlx: `error returned from database: file is not a
    // database` / code 26）。
    let keyless: Result<i64, String> = async {
        let mut plain = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&db)
                .create_if_missing(false),
        )
        .await
        .map_err(|error| error.to_string())?;
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM secret")
            .fetch_one(&mut plain)
            .await
            .map_err(|error| error.to_string())
    }
    .await;
    let error = keyless.expect_err("keyless open of a keyed store must fail");
    let text = error.to_ascii_lowercase();
    assert!(
        text.contains("not a database") || text.contains("code 26") || text.contains("encrypted"),
        "unexpected keyless error: {error}"
    );

    // 同 key 重开能读回（证明写进去的是密文而不是脏文件）。
    let mut keyed = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&db)
            .create_if_missing(false)
            .pragma("key", RAW_KEY),
    )
    .await
    .expect("keyed reopen");
    let value: String = sqlx::query_scalar("SELECT v FROM secret")
        .fetch_one(&mut keyed)
        .await
        .expect("keyed read");
    assert_eq!(value, "classified");
    drop(keyed);

    let _ = std::fs::remove_dir_all(&dir);
}

/// (d) 评审第 1 条：能力门**前置**。
///
/// 走的是生产同一条 `open_store_gated`（只是把门换成自建的「缺能力」实例），
/// 因此断言的是真实打开顺序：门在 `open_with_options` 之前，缺能力时目标
/// 目录**零文件**——不建 `store.db`、不建目录、不起连接池、不跑 schema 迁移。
#[tokio::test]
async fn missing_capability_refuses_before_touching_disk() {
    let root = sandbox("gate-zero-files");
    let target = root.join("accounts/test-unregistered");
    let gate = CipherGate::incapable_for_tests(CipherProbeError::Unavailable);

    let refused = crate::service::open_store_gated(&target, &gate).await;
    assert!(
        refused.is_err(),
        "missing cipher capability must fail closed"
    );

    assert!(
        !target.exists() || entry_count(&target) == 0,
        "target dir must stay empty when the gate refuses: {}",
        target.display()
    );
    assert!(
        !target.join("store.db").exists(),
        "no store.db may be created before the capability gate passes"
    );
    assert_eq!(
        entry_count(&root),
        0,
        "gate refusal must not create anything under {:?} (not even the accounts dir)",
        root
    );

    // 负缓存命中路径同样零文件（重试不该「试出」一个库来）。
    assert!(crate::service::open_store_gated(&target, &gate)
        .await
        .is_err());
    assert_eq!(entry_count(&root), 0);
    assert_eq!(gate.probes(), 1, "retry must hit the negative cache");

    let _ = std::fs::remove_dir_all(&root);
}

/// (e) 评审第 2 条：失败也缓存 + 证据行每进程只打一次。
///
/// 一次判定失败后：`PRAGMA cipher_version` 探测计数停在 1（不重复建
/// `:memory:` 连接）、失败证据行条数停在 1（error/eprintln 不再刷）；
/// 无论再判定多少次、再 `open_store` 多少次，都只回同一个缓存错误。
#[tokio::test]
async fn missing_capability_is_probed_and_reported_once() {
    let root = sandbox("gate-negative-cache");
    let target = root.join("accounts/probe-once");
    let gate = CipherGate::incapable_for_tests(CipherProbeError::Unavailable);

    assert_eq!(gate.ensure().await, Err(CipherProbeError::Unavailable));
    assert_eq!(
        gate.probes(),
        1,
        "first denial probes :memory: exactly once"
    );
    assert_eq!(
        gate.denial_reports(),
        1,
        "first denial emits the evidence line exactly once"
    );

    // 继续判定 + 真正走打开路径：不重探、不再落证据行。
    for _ in 0..3 {
        assert_eq!(gate.ensure().await, Err(CipherProbeError::Unavailable));
    }
    for _ in 0..2 {
        assert!(crate::service::open_store_gated(&target, &gate)
            .await
            .is_err());
    }
    assert_eq!(
        gate.probes(),
        1,
        "negative cache must skip further :memory: probes"
    );
    assert_eq!(
        gate.denial_reports(),
        1,
        "evidence line must be emitted once per process"
    );
    assert_eq!(
        entry_count(&root),
        0,
        "no store may appear on the refusal path"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// (f) 正缓存对照：能力可用时也只探一次，且不落失败证据行。
#[tokio::test]
async fn capable_gate_probes_once_and_caches_version() {
    let gate = CipherGate::new();
    let first = gate.ensure().await.expect("SQLCipher build is capable");
    let second = gate.ensure().await.expect("cached conclusion");
    assert_eq!(first, second, "cached version must equal the probed one");
    assert_eq!(gate.probes(), 1, "success conclusion is cached as well");
    assert_eq!(
        gate.denial_reports(),
        0,
        "no denial evidence on the happy path"
    );
}
