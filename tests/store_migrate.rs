// SPDX-License-Identifier: AGPL-3.0-only

//! M6 D1 真进程验收：`storeMigrate` 子命令（设计：
//! docs/plan/engine-store-migration-design.md §2.2/§5/§9/§10）。
//!
//! 这些用例**起真进程**（`CARGO_BIN_EXE_kt-signal-engine`），因此覆盖了单测
//! 覆盖不到的三段：argv 严格解析与退出码（0/2/10/11）、`--json` 契约、
//! 以及「迁移中途被杀」的五个确定性断点（debug 构建的
//! `KT_ENGINE_STORE_MIGRATE_FAULT`）。
//!
//! 夹具用**引擎真实 schema**（presage 迁移建库）——只有真 schema 才能让
//! 迁移器的结构比对/语义 probe 有意义。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};

/// 两把形状合法的 32B key（第一字节就不同，错 key 一定解不开）。
const KEY_A: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const KEY_B: &str = "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f";
/// 账号目录名（ADR 0004：lk- + hex）。
const LK: &str = "lk-0123456789abcdef";
const PLAINTEXT_HEADER: &[u8; 16] = b"SQLite format 3\0";
const STORE: &str = "store.db";
const BACKUP: &str = "store.db.plaintext-backup";
const STAGING: &str = "store.db.encrypting";
const MARKER: &str = "store.format";

fn sandbox(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kt-engine-storemig-cli-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("sandbox");
    dir
}

async fn plain_connect(path: &Path) -> SqliteConnection {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true);
    SqliteConnection::connect_with(&options)
        .await
        .expect("plaintext connection")
}

async fn keyed_connect(path: &Path, key_hex: &str) -> SqliteConnection {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .pragma("key", format!("\"x'{key_hex}'\""))
        .foreign_keys(true);
    SqliteConnection::connect_with(&options)
        .await
        .expect("keyed connection")
}

fn is_plaintext_header(path: &Path) -> bool {
    let mut head = [0_u8; 16];
    match std::fs::File::open(path) {
        Ok(mut file) => file.read_exact(&mut head).is_ok() && &head == PLAINTEXT_HEADER,
        Err(_) => false,
    }
}

/// 建一个「引擎会看到的样子」的明文账号库：presage 真实 schema + 两行数据 +
/// 非零 user_version/application_id。
async fn build_account(data_dir: &Path) -> PathBuf {
    let dir = data_dir.join("accounts").join(LK);
    std::fs::create_dir_all(&dir).expect("account dir");
    let path = dir.join(STORE);
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .foreign_keys(true);
    let store = presage_store_sqlite::SqliteStore::open_with_options(
        options,
        presage_store_sqlite::OnNewIdentity::Trust,
    )
    .await
    .expect("presage store opens");
    drop(store);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut connection = plain_connect(&path).await;
    sqlx::raw_sql(
        "INSERT INTO threads (group_master_key, recipient_id) VALUES (NULL, 'aci-one');
         INSERT INTO threads (group_master_key, recipient_id) VALUES (NULL, 'aci-two');
         INSERT INTO thread_messages (ts, thread_id, sender_service_id, sender_device_id,
             destination_service_id, needs_receipt, unidentified_sender, content_body, was_plaintext)
         VALUES (1700000000000, 1, 'aci-sender', 1, 'aci-one', 1, 0, X'00ff', 0);
         INSERT INTO thread_messages (ts, thread_id, sender_service_id, sender_device_id,
             destination_service_id, needs_receipt, unidentified_sender, content_body, was_plaintext)
         VALUES (1700000000001, 2, 'aci-sender', 1, 'aci-two', 1, 0, X'00fe', 0);
         PRAGMA user_version = 7;
         PRAGMA application_id = 8397123;",
    )
    .execute(&mut connection)
    .await
    .expect("seed data");
    connection.close().await.expect("close fixture");
    path
}

fn run_migrate(data_dir: &Path, key: Option<&str>, extra: &[&str]) -> Output {
    run_migrate_with_env(data_dir, key, extra, &[])
}

fn run_migrate_with_env(
    data_dir: &Path,
    key: Option<&str>,
    extra: &[&str],
    envs: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kt-signal-engine"));
    command.arg("--data-dir").arg(data_dir).arg("storeMigrate");
    for arg in extra {
        command.arg(arg);
    }
    match key {
        Some(key) => {
            command.env("KT_SIGNAL_STORE_KEY", key);
        }
        None => {
            command.env_remove("KT_SIGNAL_STORE_KEY");
        }
    }
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("engine runs")
}

/// 取 stdout 的**最后一行** JSON（契约：--json 只输出一份结果）。
fn json_line(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().last().unwrap_or_else(|| {
        panic!(
            "no stdout; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    serde_json::from_str(line)
        .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout}"))
}

/// 正向主链路（真进程）：exit 0 + `--json` 契约 + 头/备份/数据三项证据。
#[tokio::test]
async fn cli_migrates_plaintext_store_and_reports_json() {
    let data = sandbox("ok");
    let store = build_account(&data).await;
    let output = run_migrate(&data, Some(KEY_A), &["--json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["ok"], true, "{report}");
    let account = &report["accounts"][0];
    assert_eq!(account["lk"], LK);
    assert_eq!(account["action"], "migrate");
    assert_eq!(account["code"], "store_migrated");
    assert_eq!(account["backup"], BACKUP);
    assert!(
        account["keyid"].as_str().unwrap_or_default().len() == 8,
        "{account}"
    );
    assert!(account["bytes"].as_u64().unwrap_or_default() > 0);
    // 头：主库是密文、备份仍是明文。
    assert!(!is_plaintext_header(&store));
    assert!(is_plaintext_header(
        &data.join("accounts").join(LK).join(BACKUP)
    ));
    // 数据：有 key 读得到、无 key 打不开。
    let mut keyed = keyed_connect(&store, KEY_A).await;
    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thread_messages")
        .fetch_one(&mut keyed)
        .await
        .expect("keyed read");
    let user_version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut keyed)
        .await
        .expect("user_version");
    keyed.close().await.expect("close keyed");
    assert_eq!(messages, 2);
    assert_eq!(user_version, 7, "user_version 必须显式补齐");
    let options = SqliteConnectOptions::new()
        .filename(&store)
        .create_if_missing(false);
    let mut plain = SqliteConnection::connect_with(&options)
        .await
        .expect("connect attempt");
    let denied = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master")
        .fetch_one(&mut plain)
        .await
        .is_err();
    let _ = plain.close().await;
    assert!(denied, "无 key 必须打不开加密库");
    // 现场：无 staging 残留、marker 落盘且不含密钥材料。
    let dir = data.join("accounts").join(LK);
    assert!(!dir.join(STAGING).exists());
    let marker = std::fs::read_to_string(dir.join(MARKER)).expect("marker");
    assert!(
        marker.contains("fp=") && marker.contains("keyid="),
        "{marker}"
    );
    assert!(!marker.contains(KEY_A), "marker 不得含 key：{marker}");
    let _ = std::fs::remove_dir_all(&data);
}

/// 幂等（真进程）：第二次 exit 0 + noop，库字节不变。
#[tokio::test]
async fn cli_second_run_is_a_noop() {
    let data = sandbox("noop");
    let store = build_account(&data).await;
    assert_eq!(
        run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
        Some(0)
    );
    let before = std::fs::read(&store).expect("store bytes");
    let output = run_migrate(&data, Some(KEY_A), &["--json"]);
    assert_eq!(output.status.code(), Some(0));
    let report = json_line(&output);
    assert_eq!(report["accounts"][0]["action"], "noop");
    assert_eq!(report["accounts"][0]["code"], "store_migration_noop");
    assert_eq!(std::fs::read(&store).expect("store bytes"), before);
    let _ = std::fs::remove_dir_all(&data);
}

/// 五个确定性断点（kill -9 等价）逐个注入：下一次运行必须收敛到「无第三态」。
#[tokio::test]
async fn cli_fault_points_converge() {
    for fault in [
        "after_backup",
        "after_export",
        "after_sidecar",
        "after_rename",
        "before_marker",
    ] {
        let data = sandbox(fault);
        let store = build_account(&data).await;
        let crashed = run_migrate_with_env(
            &data,
            Some(KEY_A),
            &["--json"],
            &[("KT_ENGINE_STORE_MIGRATE_FAULT", fault)],
        );
        assert_eq!(
            crashed.status.code(),
            Some(90),
            "断点 {fault} 应中断进程；stderr={}",
            String::from_utf8_lossy(&crashed.stderr)
        );
        let recovered = run_migrate(&data, Some(KEY_A), &["--json"]);
        assert_eq!(
            recovered.status.code(),
            Some(0),
            "断点 {fault} 未收敛；stderr={}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        // §5.5 谓词：要么明文且无 key 可读、要么密文且有 key 可读——这里迁移
        // 只可能收敛到密文（原库在迁移成功后才 rename）。
        assert!(!is_plaintext_header(&store), "断点 {fault} 后主库仍是明文");
        assert!(
            !data.join("accounts").join(LK).join(STAGING).exists(),
            "断点 {fault} 残留 staging"
        );
        let mut keyed = keyed_connect(&store, KEY_A).await;
        let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thread_messages")
            .fetch_one(&mut keyed)
            .await
            .expect("keyed read after recovery");
        keyed.close().await.expect("close");
        assert_eq!(messages, 2, "断点 {fault} 后数据不完整");
        let _ = std::fs::remove_dir_all(&data);
    }
}

/// 缺 key（D-08）：exit 11、零写入、明文库原样可读。
#[tokio::test]
async fn cli_missing_key_exits_11_and_writes_nothing() {
    let data = sandbox("no-key");
    let store = build_account(&data).await;
    let before = std::fs::read(&store).expect("store bytes");
    let output = run_migrate(&data, None, &["--json"]);
    assert_eq!(output.status.code(), Some(11));
    let report = json_line(&output);
    assert_eq!(report["ok"], false);
    assert_eq!(report["error"]["code"], "StoreKeyRequired");
    assert_eq!(report["error"]["retryable"], false);
    assert_eq!(std::fs::read(&store).expect("store bytes"), before);
    assert!(is_plaintext_header(&store));
    let dir = data.join("accounts").join(LK);
    assert!(!dir.join(BACKUP).exists(), "缺 key 不得留备份");
    assert!(!dir.join(STAGING).exists(), "缺 key 不得留 staging");
    let _ = std::fs::remove_dir_all(&data);
}

/// 错 key（D-10）：迁移完成后用另一把 key 跑 → exit 11 + store_key_rejected，
/// 库字节不变（绝不重建）。
#[tokio::test]
async fn cli_wrong_key_exits_11_and_leaves_the_store() {
    let data = sandbox("wrong-key");
    let store = build_account(&data).await;
    assert_eq!(
        run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
        Some(0)
    );
    let before = std::fs::read(&store).expect("store bytes");
    let output = run_migrate(&data, Some(KEY_B), &["--json"]);
    assert_eq!(output.status.code(), Some(11));
    let report = json_line(&output);
    assert_eq!(report["accounts"][0]["code"], "store_key_rejected");
    assert_eq!(std::fs::read(&store).expect("store bytes"), before);
    let _ = std::fs::remove_dir_all(&data);
}

/// 干跑（真进程）：只出 plan，不写库/备份/staging。
///
/// 同时锁死「不许谎报」：`action`/`code`/`backup` 三项都必须表明这是计划而
/// 不是一次已完成的迁移（旧实现报 migrate/store_migrated/备份文件名）。
#[tokio::test]
async fn cli_dry_run_plans_without_writing() {
    let data = sandbox("dry-run");
    let store = build_account(&data).await;
    let before = std::fs::read(&store).expect("store bytes");
    let output = run_migrate(&data, Some(KEY_A), &["--json", "--dry-run"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["dryRun"], true);
    assert_eq!(report["accounts"][0]["action"], "dry-run", "{report}");
    assert_eq!(report["accounts"][0]["code"], "store_migration_planned");
    assert!(
        report["accounts"][0]["backup"].is_null(),
        "干跑没有备份: {report}"
    );
    // 设计 §2.2/§10.2：`plan` 是步骤名数组。
    let steps: Vec<&str> = report["plan"]
        .as_array()
        .expect("plan steps")
        .iter()
        .map(|step| step.as_str().expect("step name"))
        .collect();
    assert_eq!(
        steps,
        vec!["checkpoint", "backup", "export", "verify", "swap"],
        "{report}"
    );
    assert!(report["did"].as_array().expect("did").is_empty());
    assert_eq!(std::fs::read(&store).expect("store bytes"), before);
    assert!(is_plaintext_header(&store));
    let dir = data.join("accounts").join(LK);
    assert!(!dir.join(BACKUP).exists());
    assert!(!dir.join(STAGING).exists());
    let _ = std::fs::remove_dir_all(&data);
}

/// 用法错误（D1 的严格口径）：未知参数 / 两模式并存 / 非法方向 / 缺 data-dir
/// 一律 exit 2；反向迁移（D2）也是 exit 2。
#[tokio::test]
async fn cli_usage_errors_exit_2() {
    let data = sandbox("usage");
    let cases: &[&[&str]] = &[
        &["--bogus"],
        &["jsonRpc"],
        &["--account"],
        &["--account", "nope"],
        &["--direction", "sideways"],
        &["--direction", "to-plaintext"],
    ];
    for extra in cases {
        let output = run_migrate(&data, Some(KEY_A), extra);
        assert_eq!(
            output.status.code(),
            Some(2),
            "argv {extra:?} 应 exit 2；stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // 缺 --data-dir：进程直接跑 storeMigrate。
    let output = Command::new(env!("CARGO_BIN_EXE_kt-signal-engine"))
        .arg("storeMigrate")
        .env("KT_SIGNAL_STORE_KEY", KEY_A)
        .output()
        .expect("engine runs");
    assert_eq!(output.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&data);
}

/// 无库 / 0 字节：no-op（新建加密库是 C 的事），exit 0。
#[tokio::test]
async fn cli_absent_store_is_a_noop() {
    let data = sandbox("absent");
    let dir = data.join("accounts").join(LK);
    std::fs::create_dir_all(&dir).expect("account dir");
    let output = run_migrate(&data, Some(KEY_A), &["--json"]);
    assert_eq!(output.status.code(), Some(0));
    let report = json_line(&output);
    assert_eq!(report["accounts"][0]["action"], "absent");
    assert_eq!(report["accounts"][0]["code"], "store_migration_absent");
    assert!(!dir.join(STORE).exists(), "不得凭空建库");
    let _ = std::fs::remove_dir_all(&data);
}

/// 只迁一个账号（--account）：其它账号一个字节都不动。
#[tokio::test]
async fn cli_account_filter_leaves_other_accounts_alone() {
    let data = sandbox("filter");
    build_account(&data).await;
    let other = "lk-ffffffffffffffff";
    let other_dir = data.join("accounts").join(other);
    std::fs::create_dir_all(&other_dir).expect("other dir");
    let other_store = other_dir.join(STORE);
    std::fs::write(&other_store, b"not a sqlite file").expect("other store");
    let output = run_migrate(&data, Some(KEY_A), &["--json", "--account", LK]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["accounts"].as_array().expect("accounts").len(), 1);
    assert_eq!(report["accounts"][0]["lk"], LK);
    assert_eq!(
        std::fs::read(&other_store).expect("other store"),
        b"not a sqlite file"
    );
    let _ = std::fs::remove_dir_all(&data);
}
/// 缺 data-dir（真进程）：终态 11 + 专用码，且**不代建目录**。
///
/// 旧行为是 ok:true/exit 0——路径打错、槽没建、挂载丢了，全都长得和
/// 「迁移成功」一样，连接器于是照常起引擎再炸在别处。
#[tokio::test]
async fn cli_missing_data_dir_exits_11() {
    let data = sandbox("missing-dir");
    let absent = data.join("no-such-slot");
    let output = run_migrate(&absent, Some(KEY_A), &["--json"]);
    assert_eq!(
        output.status.code(),
        Some(11),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["ok"], false, "{report}");
    assert_eq!(report["error"]["code"], "store_migrate_data_dir_missing");
    assert_eq!(report["error"]["retryable"], false);
    assert!(!absent.exists(), "缺 data-dir 不得代建目录");
    let _ = std::fs::remove_dir_all(&data);
}

/// argv 误吞（真进程）：`--data-dir storeMigrate jsonRpc` 里的
/// `storeMigrate` 是**目录名**，必须按 jsonRpc 启动（旧实现按字面位置找
/// 子命令 → 误判成迁移模式 → exit 2）。
#[tokio::test]
async fn cli_data_dir_named_store_migrate_still_starts_json_rpc() {
    let data = sandbox("argv-data-dir");
    let output = Command::new(env!("CARGO_BIN_EXE_kt-signal-engine"))
        .current_dir(&data)
        .arg("--data-dir")
        .arg("storeMigrate")
        .arg("jsonRpc")
        .stdin(Stdio::null())
        .env_remove("KT_SIGNAL_STORE_KEY")
        .output()
        .expect("engine runs");
    assert_eq!(
        output.status.code(),
        Some(0),
        "按 jsonRpc 启动（stdin EOF）应 exit 0；stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dir = data.join("storeMigrate");
    assert!(
        !dir.join("store-migrate.lock").exists(),
        "不得进入迁移模式（迁移器会落 data-dir 级锁）"
    );
    let _ = std::fs::remove_dir_all(&data);
}

/// 设计 §5.5 + `store_state_invalid`（真进程）：加密库**打得开但页已损坏**
/// 时不许报 noop/exit 0。
#[tokio::test]
async fn cli_corrupt_encrypted_store_is_state_invalid() {
    let data = sandbox("state-invalid");
    let store = build_account(&data).await;
    assert_eq!(
        run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
        Some(0)
    );
    // 破坏最后一页：首页（sqlite_master）保持可读 → key probe 仍成功，
    // 只有 integrity/cipher 层能发现。
    let mut bytes = std::fs::read(&store).expect("store bytes");
    assert!(bytes.len() > 3 * 4096, "至少 3 页: {}", bytes.len());
    let start = bytes.len() - 4096 + 64;
    for byte in &mut bytes[start..start + 64] {
        *byte ^= 0xff;
    }
    std::fs::write(&store, &bytes).expect("corrupt store");
    let before = std::fs::read(&store).expect("store bytes");
    let output = run_migrate(&data, Some(KEY_A), &["--json"]);
    assert_eq!(
        output.status.code(),
        Some(11),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["ok"], false, "{report}");
    assert_eq!(report["accounts"][0]["code"], "store_state_invalid");
    assert_eq!(std::fs::read(&store).expect("store bytes"), before);
    let _ = std::fs::remove_dir_all(&data);
}

/// verify 失败路径（真进程 + 注入）：导出之后人为改 staging 的行数 → 必须
/// 删 staging、保留原库与备份、整体 exit 11（设计 §4 / D-19）。
#[tokio::test]
async fn cli_verify_failure_discards_staging_and_keeps_source_and_backup() {
    let data = sandbox("verify-fail");
    let store = build_account(&data).await;
    let output = run_migrate_with_env(
        &data,
        Some(KEY_A),
        &["--json"],
        &[("KT_ENGINE_STORE_MIGRATE_FAULT", "corrupt_staging_rows")],
    );
    assert_eq!(
        output.status.code(),
        Some(11),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["ok"], false, "{report}");
    assert_eq!(report["accounts"][0]["action"], "failed");
    assert_eq!(
        report["accounts"][0]["code"],
        "store_migration_verify_failed"
    );
    let dir = data.join("accounts").join(LK);
    assert!(!dir.join(STAGING).exists(), "校验不过必须删 staging");
    assert!(is_plaintext_header(&store), "原库必须还是明文（不替换）");
    let backup = dir.join(BACKUP);
    assert!(is_plaintext_header(&backup), "备份必须保留且仍是明文回滚物");
    // 原库数据完整可读（失败路径不许把源库写坏）。
    let mut plain = plain_connect(&store).await;
    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thread_messages")
        .fetch_one(&mut plain)
        .await
        .expect("plaintext read after failed verify");
    plain.close().await.expect("close");
    assert_eq!(messages, 2);
    let _ = std::fs::remove_dir_all(&data);
}

/// 裁决 #10（真进程 + 注入）：删 sidecar 之前必须再 checkpoint 一次源库。
/// 有外部读者持快照 + 之后写入的帧时，TRUNCATE 折不进去 → exit 10，且
/// **绝不删除**还带活帧的 WAL（旧顺序直接 remove_sidecars = 静默丢已提交帧）。
#[tokio::test]
async fn cli_pre_swap_checkpoint_refuses_to_drop_live_wal_frames() {
    let data = sandbox("pre-swap-checkpoint");
    let store = build_account(&data).await;
    let output = run_migrate_with_env(
        &data,
        Some(KEY_A),
        &["--json"],
        &[("KT_ENGINE_STORE_MIGRATE_FAULT", "before_swap")],
    );
    assert_eq!(
        output.status.code(),
        Some(10),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["accounts"][0]["code"], "store_busy");
    assert!(is_plaintext_header(&store), "源库不得被替换");
    let wal = data.join("accounts").join(LK).join(format!("{STORE}-wal"));
    let wal_bytes = std::fs::metadata(&wal).map(|meta| meta.len()).unwrap_or(0);
    assert!(wal_bytes > 0, "还带活帧的 WAL 不得被删");
    // 占用方退出后同一现场照常收敛（可重试语义成立）。
    let retry = run_migrate(&data, Some(KEY_A), &["--json"]);
    assert_eq!(
        retry.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let _ = std::fs::remove_dir_all(&data);
}
