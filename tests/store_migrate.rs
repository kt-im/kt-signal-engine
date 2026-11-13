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

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
const DECRYPTING: &str = "store.db.decrypting";
const ENCRYPTED_BACKUP: &str = "store.db.encrypted-backup";
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

/// 官方 store 落库形态的注册数据（`kv` 表的 `registration` 行 JSON）。
///
/// 与 `src/store_migrate.rs` 单测里那份同源：只有真 JSON 才能让
/// `load_registration_data` 真的反序列化（凭空拼一个 `{uuid,device_name}`
/// 会让语义 probe 报「registration data unreadable」，那是夹具的错，不是迁移的错）。
fn registration_json(aci: &str, device_name: &str) -> serde_json::Value {
    use base64::Engine as _;
    use presage::libsignal_service::utils::{phonenumber_from_signal, TryIntoE164};
    let e164 = "+15551234567".try_into_e164().expect("e164");
    serde_json::json!({
        "signal_servers": "Production",
        "device_name": device_name,
        "phone_number": serde_json::to_value(phonenumber_from_signal(&e164)).expect("phone"),
        "uuid": aci,
        "pni": "7b3dc19e-11f2-4c5a-9d0e-2f6a8b4c1d3e",
        "password": "abcdefghijklmnopqrstuvwx",
        "device_id": Some(2_u32),
        "registration_id": 1234_u32,
        "pni_registration_id": Some(5678_u32),
        "profile_key": base64::engine::general_purpose::STANDARD.encode([7_u8; 32]),
    })
}

/// 往明文库的 `kv` 写注册行（源侧真值）。
async fn seed_registration_row(path: &Path, aci: &str, device_name: &str) {
    let mut connection = plain_connect(path).await;
    let value = serde_json::to_vec(&registration_json(aci, device_name)).expect("json");
    sqlx::query("INSERT OR REPLACE INTO kv (key, value) VALUES ('registration', ?)")
        .bind(value)
        .execute(&mut connection)
        .await
        .expect("registration row");
    connection.close().await.expect("close registration seed");
}

/// 读回注册行（往返之后必须能原样读出同一份 ACI / 设备名）。
async fn read_registration_row(path: &Path) -> serde_json::Value {
    let mut connection = plain_connect(path).await;
    let value: Vec<u8> = sqlx::query_scalar("SELECT value FROM kv WHERE key = 'registration'")
        .fetch_one(&mut connection)
        .await
        .expect("registration row");
    connection.close().await.expect("close registration read");
    serde_json::from_slice(&value).expect("registration json")
}

/// §5.5 谓词（测试侧实现）：恰好一种权威形态，且该形态自洽。
async fn assert_authoritative_state(store: &Path) {
    if is_plaintext_header(store) {
        let mut plain = plain_connect(store).await;
        let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&mut plain)
            .await
            .expect("plaintext integrity_check");
        plain.close().await.expect("close plaintext");
        assert!(
            !integrity.is_empty() && integrity.iter().all(|row| row == "ok"),
            "明文库必须 integrity_check=ok：{integrity:?}"
        );
    } else {
        let mut keyed = keyed_connect(store, KEY_A).await;
        let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&mut keyed)
            .await
            .expect("keyed integrity_check");
        let cipher: Vec<String> = sqlx::query_scalar("PRAGMA cipher_integrity_check")
            .fetch_all(&mut keyed)
            .await
            .expect("keyed cipher_integrity_check");
        keyed.close().await.expect("close keyed");
        assert!(
            !integrity.is_empty() && integrity.iter().all(|row| row == "ok"),
            "密文库必须 integrity_check=ok：{integrity:?}"
        );
        assert!(
            cipher.is_empty(),
            "密文库必须 cipher_integrity_check 空：{cipher:?}"
        );
    }
}

/// 关键表行数（按当前形态选连接）。
async fn message_count(store: &Path) -> i64 {
    if is_plaintext_header(store) {
        let mut plain = plain_connect(store).await;
        let count = sqlx::query_scalar("SELECT COUNT(*) FROM thread_messages")
            .fetch_one(&mut plain)
            .await
            .expect("plaintext count");
        plain.close().await.expect("close");
        count
    } else {
        let mut keyed = keyed_connect(store, KEY_A).await;
        let count = sqlx::query_scalar("SELECT COUNT(*) FROM thread_messages")
            .fetch_one(&mut keyed)
            .await
            .expect("keyed count");
        keyed.close().await.expect("close");
        count
    }
}

/// 账号目录的逐文件快照（文件名 → 字节）：11 路径「现场不变」的判据。
///
/// 只比 store.db 是不够的：旧实现在验 key 之前就把 `.encrypted-backup` 删了，
/// 而 staging / marker tmp / 探针新建的 sidecar 也只有目录快照看得见。
fn dir_snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir).expect("account dir").flatten() {
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        files.insert(name, std::fs::read(entry.path()).unwrap_or_default());
    }
    files
}

/// 逐文件断言（整份 map 的 assert_eq 失败时会把整个库的字节打进日志）。
fn assert_snapshot_unchanged(dir: &Path, before: &BTreeMap<String, Vec<u8>>, label: &str) {
    let after = dir_snapshot(dir);
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>(),
        "{label}: 改了文件集合（备份 / staging / marker tmp / sidecar 都不许动）"
    );
    for (file, bytes) in before {
        assert_eq!(after.get(file), Some(bytes), "{label}: 改动了 {file}");
    }
}

/// 无孤儿：两个方向的 staging 都不许留下，marker 的 `.tmp` 也不许留下。
fn assert_no_orphan_staging(dir: &Path) {
    for name in [STAGING, DECRYPTING] {
        assert!(
            !dir.join(name).exists(),
            "{name} 残留（第三态/孤儿 staging）"
        );
    }
    let leftovers: Vec<String> = std::fs::read_dir(dir)
        .expect("account dir")
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name.starts_with("store.format.") && name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "marker tmp 残留：{leftovers:?}");
}

/// 线性同余/异或位移随机源（种子固定 → 失败可复现）。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// 起一个真进程（随机 kill 用：stdout/stderr 都不要，避免管道阻塞）。
fn spawn_migrate(data: &Path, extra: &[&str]) -> std::process::Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kt-signal-engine"));
    command.arg("--data-dir").arg(data).arg("storeMigrate");
    for arg in extra {
        command.arg(arg);
    }
    command
        .env("KT_SIGNAL_STORE_KEY", KEY_A)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn migrator")
}

/// `Child::kill` 在 Unix 上就是 SIGKILL：用退出状态确认它**真的**落地了。
/// 子进程若在 kill 之前已经跑完（延迟比迁移还长），这一轮等于没测到崩溃点。
#[cfg(unix)]
fn killed_by_sigkill(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal() == Some(9)
}

#[cfg(not(unix))]
fn killed_by_sigkill(_status: &std::process::ExitStatus) -> bool {
    true
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

/// 用法错误（严格口径）：未知参数 / 两模式并存 / 非法方向 / 缺 data-dir
/// 一律 exit 2。反向（`--direction to-plaintext`）是**合法取值**，不再属于这一类。
#[tokio::test]
async fn cli_usage_errors_exit_2() {
    let data = sandbox("usage");
    let cases: &[&[&str]] = &[
        &["--bogus"],
        &["jsonRpc"],
        &["--account"],
        &["--account", "nope"],
        &["--direction", "sideways"],
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

/// M1（反向）：终态 11 必须**零改动**。旧实现 step0 在验 key / marker 之前
/// 就删 `store.db.encrypted-backup`（连带 staging 与 marker tmp）——一次错
/// key 就把唯一的回滚物吃掉。判据是**目录快照**，不是只比 store.db。
#[tokio::test]
async fn cli_reverse_terminal_failures_leave_the_account_dir_byte_identical() {
    for (name, tamper_marker) in [("reverse-dir-wrong-key", false), ("reverse-dir-fp", true)] {
        let data = sandbox(name);
        build_account(&data).await;
        assert_eq!(
            run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
            Some(0),
            "{name}: 正向必须先成功"
        );
        let dir = data.join("accounts").join(LK);
        // 上一次留下的现场：回滚物 + 半成品 staging + marker tmp。
        std::fs::write(dir.join(ENCRYPTED_BACKUP), b"previous rollback")
            .expect("stale encrypted backup");
        std::fs::write(dir.join(DECRYPTING), b"half written staging")
            .expect("stale decrypting staging");
        std::fs::write(dir.join(format!("{MARKER}.999.tmp")), b"tmp").expect("marker tmp");
        if tamper_marker {
            std::fs::write(
                dir.join(MARKER),
                b"magic=KTSF ver=1 state=sqlcipher-v1 keyid=deadbeef fp=deadbeef at=0 tool=test\n",
            )
            .expect("tampered marker");
        }
        let before = dir_snapshot(&dir);
        let key = if tamper_marker { KEY_A } else { KEY_B };
        let output = run_migrate(&data, Some(key), &["--direction", "to-plaintext", "--json"]);
        assert_eq!(
            output.status.code(),
            Some(11),
            "{name}: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = json_line(&output);
        let expected = if tamper_marker {
            "store_key_mismatch"
        } else {
            "store_key_rejected"
        };
        assert_eq!(report["accounts"][0]["code"], expected, "{name}: {report}");
        assert_snapshot_unchanged(&dir, &before, name);
        let _ = std::fs::remove_dir_all(&data);
    }
}

/// M3 + M1（正向）：明文**头正常但内容坏**是 §5.5 的第三态 → `store_state_invalid`
/// （旧实现套 `classify_connect_error` 报成 `store_key_rejected`——明文库没有
/// key 可拒，这个码会把运维引到换 key / 重试的死路）；同一条路径还必须
/// **不动现场**：旧实现先删 `.plaintext-backup`。
#[tokio::test]
async fn cli_corrupt_plaintext_is_state_invalid_and_touches_nothing() {
    let data = sandbox("plaintext-state-invalid");
    let store = build_account(&data).await;
    let dir = data.join("accounts").join(LK);
    // 保留 16B 明文头（分类只认头），把第 1 页的布局字段打成垃圾 → sqlite
    // 一打开就报 `not a database`。
    let mut bytes = std::fs::read(&store).expect("store bytes");
    for byte in &mut bytes[16..64] {
        *byte = 0xff;
    }
    std::fs::write(&store, &bytes).expect("corrupt plaintext store");
    std::fs::write(dir.join(BACKUP), b"previous rollback").expect("stale plaintext backup");
    std::fs::write(dir.join(STAGING), b"half written staging").expect("stale staging");
    std::fs::write(dir.join(format!("{MARKER}.999.tmp")), b"tmp").expect("marker tmp");
    let before = dir_snapshot(&dir);
    for (label, extra) in [
        ("forward", vec!["--json"]),
        ("reverse", vec!["--direction", "to-plaintext", "--json"]),
    ] {
        let output = run_migrate(&data, Some(KEY_A), &extra);
        assert_eq!(
            output.status.code(),
            Some(11),
            "{label}: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = json_line(&output);
        assert_eq!(
            report["accounts"][0]["code"], "store_state_invalid",
            "{label}: {report}"
        );
        assert_snapshot_unchanged(&dir, &before, label);
    }
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

// ——————————————————————— D2：反向迁移（加密 → 明文） ———————————————————————

/// 往返一致性要逐项比对的事实（两个方向都必须保持同一组值）。
#[derive(Debug, PartialEq, Eq)]
struct StoreFacts {
    threads: i64,
    messages: i64,
    message_bytes: i64,
    message_rowid_span: (i64, i64),
    ledger: Vec<String>,
    sequence: Vec<String>,
    user_version: i64,
    application_id: i64,
}

/// 按当前形态（头分类）取事实：明文无 key、密文带 key。
async fn store_facts(store: &Path) -> StoreFacts {
    if is_plaintext_header(store) {
        facts_of(plain_connect(store).await).await
    } else {
        facts_of(keyed_connect(store, KEY_A).await).await
    }
}

async fn facts_of(mut connection: SqliteConnection) -> StoreFacts {
    let threads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM threads")
        .fetch_one(&mut connection)
        .await
        .expect("threads count");
    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thread_messages")
        .fetch_one(&mut connection)
        .await
        .expect("messages count");
    let message_bytes: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(LENGTH(content_body)), 0) FROM thread_messages")
            .fetch_one(&mut connection)
            .await
            .expect("messages bytes");
    let message_rowid_span: (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(MIN(rowid), 0), COALESCE(MAX(rowid), 0) FROM thread_messages",
    )
    .fetch_one(&mut connection)
    .await
    .expect("messages rowid span");
    let ledger: Vec<String> = sqlx::query_scalar(
        "SELECT version || ':' || hex(checksum) || ':' || success FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&mut connection)
    .await
    .expect("migration ledger");
    let sequence: Vec<String> =
        sqlx::query_scalar("SELECT name || ':' || seq FROM sqlite_sequence ORDER BY name")
            .fetch_all(&mut connection)
            .await
            .expect("sqlite_sequence");
    let user_version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .expect("user_version");
    let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(&mut connection)
        .await
        .expect("application_id");
    connection.close().await.expect("close facts");
    StoreFacts {
        threads,
        messages,
        message_bytes,
        message_rowid_span,
        ledger,
        sequence,
        user_version,
        application_id,
    }
}

/// 反向主链路（真进程）：加密 → 明文，`--json` 契约、回滚物与可读性。
#[tokio::test]
async fn cli_unmigrates_encrypted_store_to_plaintext() {
    let data = sandbox("reverse-ok");
    let store = build_account(&data).await;
    let dir = data.join("accounts").join(LK);
    assert_eq!(
        run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
        Some(0),
        "正向必须先成功（反向的输入是加密库）"
    );
    assert!(!is_plaintext_header(&store));
    let output = run_migrate(
        &data,
        Some(KEY_A),
        &["--direction", "to-plaintext", "--json"],
    );
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
    assert_eq!(account["code"], "store_plaintext_restored");
    assert_eq!(account["backup"], ENCRYPTED_BACKUP);
    assert!(account["bytes"].as_u64().unwrap_or_default() > 0);
    // plan/did 与正向同一套步骤名（连接器按字面量对账）。
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
    let did: Vec<&str> = report["did"]
        .as_array()
        .expect("did steps")
        .iter()
        .map(|step| step.as_str().expect("step name"))
        .collect();
    assert_eq!(did, steps, "实跑必须五个步骤全落地：{report}");
    // 反向的校验项**逐项**断言（§4 矩阵的镜像）：顺序即实现顺序，少一项
    // 就是少一层闸；只断言「有 checks 字段」等于什么都没验。
    let checks: Vec<&str> = account["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .map(|check| check.as_str().expect("check name"))
        .collect();
    assert_eq!(
        checks,
        vec![
            "source_integrity_check",
            "source_cipher_integrity_check",
            "staging_header_plaintext",
            "staging_opens_without_key",
            "staging_integrity_check",
            "staging_foreign_key_check",
            "schema_set_equal",
            "row_counts_equal",
            "thread_messages_sample_equal",
            "migration_ledger_equal",
            "sqlite_sequence_equal",
            "user_version_equal",
            "application_id_equal",
            "semantic_registration_readable",
            "staging_rejects_key_open",
        ],
        "反向校验矩阵必须逐项落地：{report}"
    );
    // 头：主库回到明文；加密备份仍在（7 天回滚窗口，且还要 key 才能用）。
    assert!(is_plaintext_header(&store), "反向之后主库必须是明文");
    let backup = dir.join(ENCRYPTED_BACKUP);
    assert!(!is_plaintext_header(&backup), "加密备份必须仍是密文");
    let mut keyed_backup = keyed_connect(&backup, KEY_A).await;
    let backup_messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thread_messages")
        .fetch_one(&mut keyed_backup)
        .await
        .expect("encrypted backup readable with the key");
    keyed_backup.close().await.expect("close backup");
    assert_eq!(backup_messages, 2, "回滚物必须是完整的加密库");
    // 纯 sqlite 也能读（旧二进制/`sqlite3` 的直接可读性，设计 §10.5）。
    assert_eq!(message_count(&store).await, 2);
    // 无 key 必须打得开（这就是「旧二进制回退」的硬证据）。
    let mut plain = plain_connect(&store).await;
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut plain)
        .await
        .expect("plaintext integrity");
    plain.close().await.expect("close plaintext");
    assert!(integrity.iter().all(|row| row == "ok"), "{integrity:?}");
    assert_no_orphan_staging(&dir);
    let marker = std::fs::read_to_string(dir.join(MARKER)).expect("marker");
    assert!(marker.contains("state=plaintext"), "{marker}");
    assert!(!marker.contains(KEY_A), "marker 不得含 key：{marker}");
    let _ = std::fs::remove_dir_all(&data);
}

/// 往返一致性（设计 §6.4）：明文 → 加密 → 明文 之后，行数/账本/序列/头字段/
/// 注册事实逐项相等——「头变回去了」不是正确性证明。
#[tokio::test]
async fn cli_round_trip_preserves_rows_ledger_and_registration() {
    let data = sandbox("round-trip");
    let store = build_account(&data).await;
    let aci = "6a2cb17f-ef7c-4a5d-9c69-6f8b2b1f88ae";
    seed_registration_row(&store, aci, "KT Signal Engine").await;
    let before = store_facts(&store).await;
    let registration_before = read_registration_row(&store).await;
    assert_eq!(before.threads, 2);
    assert_eq!(before.messages, 2);
    assert_eq!(before.user_version, 7);
    assert_eq!(before.application_id, 8397123);
    assert!(
        !before.ledger.is_empty(),
        "夹具必须有 _sqlx_migrations 账本"
    );

    let forward_started = Instant::now();
    let forward = run_migrate(&data, Some(KEY_A), &["--json"]);
    let forward_wall = forward_started.elapsed().as_millis() as u64;
    assert_eq!(
        forward.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&forward.stderr)
    );
    let forward_report = json_line(&forward);
    assert_eq!(forward_report["accounts"][0]["code"], "store_migrated");
    // M2：`elapsedMs` 必须是**迁移器自己的非零计时**，且与墙钟同量级。
    // 旧实现在构造 `AccountOutcome` 那一刻取 `Instant` → 恒 0；原来的
    // `< 60_000` 是空转断言（0 当然小于 60000），抓不到这件事。
    let forward_elapsed = forward_report["accounts"][0]["elapsedMs"]
        .as_u64()
        .expect("elapsedMs 必须是数字");
    assert!(
        forward_elapsed > 0,
        "正向 elapsedMs 不得恒 0：{forward_report}"
    );
    assert!(
        forward_elapsed <= forward_wall,
        "正向 elapsedMs({forward_elapsed}) 不能超过墙钟({forward_wall}ms)：{forward_report}"
    );
    assert_eq!(
        store_facts(&store).await,
        before,
        "加密这一跳不许改任何事实"
    );

    let reverse_started = Instant::now();
    let reverse = run_migrate(
        &data,
        Some(KEY_A),
        &["--direction", "to-plaintext", "--json"],
    );
    let reverse_wall = reverse_started.elapsed().as_millis() as u64;
    assert_eq!(
        reverse.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&reverse.stderr)
    );
    let reverse_report = json_line(&reverse);
    assert_eq!(
        reverse_report["accounts"][0]["code"],
        "store_plaintext_restored"
    );
    let reverse_elapsed = reverse_report["accounts"][0]["elapsedMs"]
        .as_u64()
        .expect("elapsedMs 必须是数字");
    assert!(
        reverse_elapsed > 0,
        "反向 elapsedMs 不得恒 0：{reverse_report}"
    );
    assert!(
        reverse_elapsed <= reverse_wall,
        "反向 elapsedMs({reverse_elapsed}) 不能超过墙钟({reverse_wall}ms)：{reverse_report}"
    );
    assert!(is_plaintext_header(&store));
    let after = store_facts(&store).await;
    assert_eq!(after, before, "往返后事实必须逐项相等");
    let registration_after = read_registration_row(&store).await;
    assert_eq!(
        registration_after, registration_before,
        "注册事实必须原样回来"
    );
    assert_eq!(registration_after["uuid"], aci);
    assert_eq!(registration_after["device_name"], "KT Signal Engine");
    let _ = std::fs::remove_dir_all(&data);
}

/// 反向干跑：只出计划，不打开库、不写一个字节（`action`/`code`/`backup` 三项
/// 都不许报成「已完成」）。
#[tokio::test]
async fn cli_reverse_dry_run_plans_without_writing() {
    let data = sandbox("reverse-dry-run");
    let store = build_account(&data).await;
    assert_eq!(
        run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
        Some(0)
    );
    let before = std::fs::read(&store).expect("store bytes");
    let output = run_migrate(
        &data,
        Some(KEY_A),
        &["--direction", "to-plaintext", "--dry-run", "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["dryRun"], true);
    let account = &report["accounts"][0];
    assert_eq!(account["action"], "dry-run", "{report}");
    assert_eq!(account["code"], "store_migration_planned");
    assert!(account["backup"].is_null(), "干跑没有备份: {report}");
    let steps: Vec<&str> = report["plan"]
        .as_array()
        .expect("plan steps")
        .iter()
        .map(|step| step.as_str().expect("step name"))
        .collect();
    assert_eq!(
        steps,
        vec!["checkpoint", "backup", "export", "verify", "swap"]
    );
    assert!(report["did"].as_array().expect("did").is_empty());
    assert_eq!(std::fs::read(&store).expect("store bytes"), before);
    assert!(!is_plaintext_header(&store), "干跑不许替换主库");
    let dir = data.join("accounts").join(LK);
    assert!(!dir.join(DECRYPTING).exists(), "干跑不许建 staging");
    assert!(!dir.join(ENCRYPTED_BACKUP).exists(), "干跑不许建备份");
    let _ = std::fs::remove_dir_all(&data);
}

/// 反向对「已经是明文」的库是幂等 no-op：exit 0 + 零字节改动。
#[tokio::test]
async fn cli_reverse_on_plaintext_store_is_a_noop() {
    let data = sandbox("reverse-noop");
    let store = build_account(&data).await;
    let before = std::fs::read(&store).expect("store bytes");
    let output = run_migrate(
        &data,
        Some(KEY_A),
        &["--direction", "to-plaintext", "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["accounts"][0]["action"], "noop", "{report}");
    assert_eq!(report["accounts"][0]["code"], "store_migration_noop");
    assert_eq!(std::fs::read(&store).expect("store bytes"), before);
    assert!(is_plaintext_header(&store));
    assert_no_orphan_staging(&data.join("accounts").join(LK));
    let _ = std::fs::remove_dir_all(&data);
}

/// 反向失败路径（真进程 + 注入）：校验不过时必须**逐字节保留原加密库**、
/// 删掉明文 staging、整体 exit 11，且下一次能收敛。
#[tokio::test]
async fn cli_reverse_verify_failure_leaves_encrypted_source_untouched() {
    let data = sandbox("reverse-verify-fail");
    let store = build_account(&data).await;
    assert_eq!(
        run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
        Some(0)
    );
    let before = std::fs::read(&store).expect("store bytes");
    let output = run_migrate_with_env(
        &data,
        Some(KEY_A),
        &["--direction", "to-plaintext", "--json"],
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
    assert!(
        !dir.join(DECRYPTING).exists(),
        "校验不过必须删明文 staging（不留第三态）"
    );
    assert_eq!(std::fs::read(&store).expect("store bytes"), before);
    assert!(!is_plaintext_header(&store), "原库必须还是密文");
    assert_authoritative_state(&store).await;
    // 现场还能被同一个数据-dir 的后续运行收敛（可重试语义）。
    let recovered = run_migrate(
        &data,
        Some(KEY_A),
        &["--direction", "to-plaintext", "--json"],
    );
    assert_eq!(
        recovered.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(is_plaintext_header(&store));
    assert_eq!(message_count(&store).await, 2);
    assert_no_orphan_staging(&dir);
    let _ = std::fs::remove_dir_all(&data);
}

/// 反向的五个确定性断点（kill -9 等价）：下一次运行必须收敛到明文且无孤儿 staging。
#[tokio::test]
async fn cli_reverse_fault_points_converge() {
    for fault in [
        "after_backup",
        "after_export",
        "after_sidecar",
        "after_rename",
        "before_marker",
    ] {
        let data = sandbox(&format!("reverse-{fault}"));
        let store = build_account(&data).await;
        assert_eq!(
            run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
            Some(0)
        );
        let crashed = run_migrate_with_env(
            &data,
            Some(KEY_A),
            &["--direction", "to-plaintext", "--json"],
            &[("KT_ENGINE_STORE_MIGRATE_FAULT", fault)],
        );
        assert_eq!(
            crashed.status.code(),
            Some(90),
            "断点 {fault} 应中断进程；stderr={}",
            String::from_utf8_lossy(&crashed.stderr)
        );
        let recovered = run_migrate(
            &data,
            Some(KEY_A),
            &["--direction", "to-plaintext", "--json"],
        );
        assert_eq!(
            recovered.status.code(),
            Some(0),
            "断点 {fault} 未收敛；stderr={}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert!(
            is_plaintext_header(&store),
            "断点 {fault} 后主库必须收敛到明文"
        );
        assert_eq!(message_count(&store).await, 2, "断点 {fault} 后数据不完整");
        assert_no_orphan_staging(&data.join("accounts").join(LK));
        assert_authoritative_state(&store).await;
        let _ = std::fs::remove_dir_all(&data);
    }
}

/// 反向 + 错 key：终态 11 `store_key_rejected`，库字节不变（没有 key 就没有
/// 反向——设计 §6.2，文案不许暗示还有办法）。
#[tokio::test]
async fn cli_reverse_wrong_key_leaves_the_store() {
    let data = sandbox("reverse-wrong-key");
    let store = build_account(&data).await;
    assert_eq!(
        run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
        Some(0)
    );
    let before = std::fs::read(&store).expect("store bytes");
    let output = run_migrate(
        &data,
        Some(KEY_B),
        &["--direction", "to-plaintext", "--json"],
    );
    assert_eq!(
        output.status.code(),
        Some(11),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = json_line(&output);
    assert_eq!(report["accounts"][0]["code"], "store_key_rejected");
    assert_eq!(std::fs::read(&store).expect("store bytes"), before);
    assert!(!is_plaintext_header(&store));
    let _ = std::fs::remove_dir_all(&data);
}

/// 属性测试（设计 §10.4）：50 轮「随机时点 SIGKILL → 重启收敛」，每轮断言
/// 恰好一个权威状态（§5.5）、无孤儿 staging、数据完整。
///
/// 种子固定，失败时打印轮次与 kill 延迟即可复现同一条时间线。
#[tokio::test]
async fn cli_reverse_random_kill_converges_to_one_state() {
    let data = sandbox("reverse-random-kill");
    let store = build_account(&data).await;
    let dir = data.join("accounts").join(LK);
    assert_eq!(
        run_migrate(&data, Some(KEY_A), &["--json"]).status.code(),
        Some(0),
        "起点必须是加密库"
    );
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for round in 0..50_u32 {
        let mut delay_ms = rng.next() % 200;
        let mut attempts = 0_u32;
        loop {
            let mut child = spawn_migrate(&data, &["--direction", "to-plaintext", "--json"]);
            std::thread::sleep(Duration::from_millis(delay_ms));
            // `Child::kill` 在 Unix 上就是 SIGKILL（设计 §10.4 的崩溃注入）。
            let _ = child.kill();
            let status = child.wait().expect("child waits");
            if killed_by_sigkill(&status) {
                break;
            }
            // kill 落空 = 子进程先跑完了，这一轮没有测到崩溃点（旧用例直接
            // `let _ = child.wait()` 把这种情况当成了「随机杀成功」）。恢复
            // 成加密起点，把延迟压到 0ms 重来；0ms 必定落在飞行中。
            attempts += 1;
            assert!(
                attempts <= 3,
                "第 {round} 轮连续 {attempts} 次 kill 落空（子进程都先跑完了），这一轮的崩溃点不可信"
            );
            let reset = run_migrate(&data, Some(KEY_A), &["--json"]);
            assert_eq!(
                reset.status.code(),
                Some(0),
                "第 {round} 轮 kill 落空后重置成加密失败；stderr={}",
                String::from_utf8_lossy(&reset.stderr)
            );
            delay_ms = 0;
        }
        // 恢复之前先跑一次 §5.5 谓词：kill 现场必须已经是「恰好一种权威形态」。
        // 旧用例把这一步漏了——先跑恢复再断言，等于让恢复动作把违规现场盖掉。
        assert_authoritative_state(&store).await;
        let recovered = run_migrate(
            &data,
            Some(KEY_A),
            &["--direction", "to-plaintext", "--json"],
        );
        assert_eq!(
            recovered.status.code(),
            Some(0),
            "第 {round} 轮（kill@{delay_ms}ms）未收敛；stderr={}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert_authoritative_state(&store).await;
        assert_no_orphan_staging(&dir);
        assert_eq!(
            message_count(&store).await,
            2,
            "第 {round} 轮（kill@{delay_ms}ms）后数据不完整"
        );
        // 下一轮从同一个起点（加密）开始：这次不注入 kill。
        let reset = run_migrate(&data, Some(KEY_A), &["--json"]);
        assert_eq!(
            reset.status.code(),
            Some(0),
            "第 {round} 轮重置回加密失败；stderr={}",
            String::from_utf8_lossy(&reset.stderr)
        );
        assert!(!is_plaintext_header(&store));
        assert_no_orphan_staging(&dir);
    }
    let _ = std::fs::remove_dir_all(&data);
}
