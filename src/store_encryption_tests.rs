// SPDX-License-Identifier: AGPL-3.0-only

//! M6 C 阶段（engine 半边）矩阵断言（设计：
//! docs/plan/engine-store-encryption.md §7「C 阶段」）。
//!
//! 覆盖契约里的五种磁盘情形 + 两条门禁 + 三处评审补口 + 收尾两条（可重试类
//! 失败路径的固定行、每码一个闸的可失败证据）：
//!
//! | 断言 | 矩阵 | 证明什么 |
//! |---|---|---|
//! | `store_key_accepts_hex_and_base64_...` | — | 64 hex（大小写）/ 32B base64（标准/url-safe）都收；其余形状全拒；key 不进 Debug/错误文本 |
//! | `invalid_key_shape_refuses_before_touching_disk` | 1 | 形状非法 = `StoreKeyInvalid` + 终态 + 磁盘零文件 |
//! | `keyed_fresh_store_is_created_encrypted` | b | 有 key + 无库 → 建加密库（头非明文、无 key 打不开、有 key 读得到迁移账本） |
//! | `wrong_key_is_rejected_and_never_rewrites_the_store` | d | 错 key = `StoreKeyRejected` + 终态 + 文件逐字节不变 + 不新建明文库；换回对 key 仍可开 |
//! | `plaintext_store_with_key_opens_legacy_and_is_left_intact` | c | 明文 + 有 key → 不设 key 打开、warn 恰一次、头仍是明文、存量数据可读 |
//! | `encrypted_store_without_key_reports_store_key_required` | — | 密文 + 无 key = `StoreKeyRequired` + 终态，绝不静默新建明文库 |
//! | `corrupt_store_is_classified_apart_from_key_failures` | e | 明文头垃圾 / 短于一页 → `StoreCorrupt`（与 `StoreKeyRejected` 分开），现场保留 |
//! | `capability_gate_still_precedes_key_and_disk_actions` | — | 缺能力时即使有 key / key 非法也是 `StoreCipherUnavailable` 且零文件（门在最前） |
//! | `failure_text_classification_maps_not_a_database_to_key_rejection` | — | sqlx 文本 → 码的归一（带/不带 key 的同一句 `not a database`） |
//! | `classify_keeps_io_failures_retryable` | — | **评审 1**：EACCES / ELOOP / ENOTDIR 等读盘失败 = `StoreOpenFailed`（可重试），不判 `Corrupt` 终态 |
//! | `corrupt_unreadable_layout_is_terminal_and_leaves_the_scene` | e | **评审 7b**：`store.db` 位置被目录占住 = `Corrupt(Unreadable)` 终态，且不动现场 |
//! | `probe_keyed_store_requires_write_access` | d | **评审 4**：验真是读写连接（要建 `-wal`），目录只读 → `StoreOpenFailed`（可重试），不是「库坏了」 |
//! | `every_store_open_code_emits_the_fixed_line_once` | — | **评审 3**：六个码共用 `event=store_open_failed code=… retryable=…` 形状（含缺能力/可重试类），逐字与门侧同形；**闸的可失败证据不在这里**（同进程边打边读是假绿，见收尾 2） |
//! | `each_store_open_code_emits_its_own_line` | — | **收尾 2**：新进程里两个不同码各触发一次 → stderr 两行都在、各恰一条、与顺序无关；把两码合并到同一闸会只剩一行（变异已验证会红） |
//! | `create_dir_all_failure_reports_store_open_failed_once` | — | **收尾 1**：只读父目录 → `create_dir_all` 失败 = `code=StoreOpenFailed retryable=true` 恰一条，且不建账号目录、不建库 |
//! | `non_utf8_store_path_reports_store_open_failed_once` | — | **收尾 1（第二条）**：账号目录名非 UTF-8（`path.to_str()` 为 `None`）同样落恰一条可重试固定行 |
//! | `legacy_warn_is_emitted_by_the_open_not_by_a_manual_gate` | c | **评审 7a**：legacy warn 由 `open_store_with` 触发（不是测试先自打闸），且只打一次 |
//! | `require_encrypted_switch_parses_only_explicit_words` | — | **C-1**：开关只认显式词（`on/1/true/yes` 与 `off/0/false/no`，去空白、大小写不敏感）；认不出 = off（默认 off 是硬约束） |
//! | 八格 `switch_off_*` / `switch_on_*` | a–d | **C-1 网格**：2（开关）× 2（明文/密文）× 2（有/无 key）。off 四格与 C 阶段逐字节同行为；on 的明文两格（有/无 key）= `StorePlaintextRequiresMigration` 终态，零写入/零删除/零新建；on 的密文两格不变 |
//! | 九格 ⑨ `switch_on_absent_*` / `switch_off_absent_*` | a–b | **C-1 补漏**：开关 on + 无 key + **库不存在** = `StoreKeyRequired` 终态（不建目录、不建库、固定行恰一条）——放它过去就等于开关 on 还在造新的明文库；on + 有 key 仍建密文库；off + 无 key 仍建明文库 |
//! | `switch_on_classifies_plaintext_by_header_before_opening` | e | **C-1 顺序**：开关 on 时「明文头 + 垃圾内容」先报「要迁移」（头是权威，与迁移器同口径），不是 `StoreCorrupt` |
//! | `switch_on_plaintext_is_terminal_and_reports_once_per_process` | — | **C-1 真进程**：`KT_SIGNAL_STORE_REQUIRE_ENCRYPTED=on` 的明文库在新进程里落**恰一条** `code=StorePlaintextRequiresMigration retryable=false` |

use std::path::{Path, PathBuf};

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};

use crate::service::{open_store_with, open_store_with_requirement};
use crate::store_cipher::{CipherGate, CipherProbeError};
use crate::store_key::{StoreKey, StoreKeyState};
use crate::store_open::{
    self, parse_require_encrypted, require_encrypted_from_env_value, CorruptDetail,
    RequireEncrypted, StoreOpenError, PLAINTEXT_HEADER, REQUIRE_ENCRYPTED_ENV,
};

/// 两把形状合法的测试 key（32B；A/B 的第一字节就不同，错 key 一定解不开）。
const KEY_A_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const KEY_B_HEX: &str = "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f";

/// 测试沙盒目录（进程 id + 测试名隔离；用后即删）。
fn sandbox(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("kt-engine-storeenc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmp dir");
    dir
}

/// legacy warn 的串行闸：那是**进程级一次性**闸，而 cargo 并行跑用例。
/// 会触发 legacy 路径的用例（矩阵 c 与矩阵 e 的「明文头 + 有 key」分支）
/// 都先拿这把锁再归零计数，断言才不会被并行用例提前推高的计数搅乱。
///
/// 用 async 版互斥锁：这些用例在多个 `.await` 之间一直持有它，std
/// `MutexGuard` 会踩 clippy 的 `await_holding_lock`（测试里也确实存在
/// 「持锁期间被调度让出」的真实风险）。
static LEGACY_WARN_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn legacy_warn_serial() -> tokio::sync::MutexGuard<'static, ()> {
    LEGACY_WARN_SERIAL.lock().await
}

fn entry_count(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.count(),
        Err(_) => 0,
    }
}

fn key(hex: &str) -> StoreKey {
    StoreKey::parse(hex).expect("valid 64-hex key")
}

fn present(hex: &str) -> StoreKeyState {
    StoreKeyState::Present(key(hex))
}

/// 裸 sqlx 连接（可选 key）：证明「无 key 的世界」看到什么。
async fn raw_connect(path: &Path, key: Option<&StoreKey>) -> Result<SqliteConnection, String> {
    let mut options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true);
    if let Some(key) = key {
        options = options.pragma("key", key.pragma_literal());
    }
    SqliteConnection::connect_with(&options)
        .await
        .map_err(|error| error.to_string())
}

async fn query_i64(path: &Path, key: Option<&StoreKey>, sql: &'static str) -> Result<i64, String> {
    let mut connection = raw_connect(path, key).await?;
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(&mut connection)
        .await
        .map_err(|error| error.to_string())
}

async fn query_text(
    path: &Path,
    key: Option<&StoreKey>,
    sql: &'static str,
) -> Result<String, String> {
    let mut connection = raw_connect(path, key).await?;
    sqlx::query_scalar::<_, String>(sql)
        .fetch_one(&mut connection)
        .await
        .map_err(|error| error.to_string())
}

async fn execute(path: &Path, sql: &'static str) {
    let mut connection = raw_connect(path, None).await.expect("plaintext connection");
    sqlx::raw_sql(sql)
        .execute(&mut connection)
        .await
        .expect("plaintext write");
}

async fn execute_keyed(path: &Path, key: &StoreKey, sql: &'static str) {
    let mut connection = raw_connect(path, Some(key))
        .await
        .expect("keyed connection");
    sqlx::raw_sql(sql)
        .execute(&mut connection)
        .await
        .expect("keyed write");
}

/// 等 sqlx 池把 SQLite worker 线程收口完再结束用例。
///
/// sqlx 的连接关闭是异步的（drop 只发关闭信号），带 SQLCipher codec 的
/// worker 线程在进程退出瞬间仍在释放 crypto 上下文时会偶发 exit-time
/// SIGSEGV（本机 1/5 复现，加 120ms 静默后 10/10 稳定）。生产进程同源
/// 风险记在 docs/plan/engine-store-encryption.md §7「C 阶段」的「已知风险」，
/// 修法（shutdown drain / 显式 close）归 D 阶段；测试面先确定性地收口。
async fn quiesce_store_teardown() {
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
}

/// 账号目录里的「非 store.db 家族」条目（拒绝路径不得凭空造文件）。
///
/// 白名单包含 `store.db` **主体 + SQLite sidecar**（`-wal`/`-shm`）：验真
/// 是读写连接，SQLite 在 WAL 模式下必须建 sidecar（实测：正确 key 的验真
/// 结束后立刻能看到 `store.db-wal`，连接收口后自动回收）。所以文档口径
/// 是「不创建 `store.db` 主体，可能产生 sidecar」，与本白名单一致——不再是
/// 早期那句「前四步零落盘」（评审第 4 条）。
fn unexpected_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| !matches!(name.as_str(), "store.db" | "store.db-wal" | "store.db-shm"))
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// key 形状面：64 hex（大小写都收）+ 32B base64（四种写法都收）；其余全拒。
/// key 材料不进 Debug、不进错误文本。
#[test]
fn store_key_accepts_hex_and_base64_and_rejects_every_other_shape() {
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use base64::Engine as _;

    let canonical = key(KEY_A_HEX);
    // 大写 hex 等价（同一把 key）。
    let upper = key(&KEY_A_HEX.to_ascii_uppercase());
    assert_eq!(upper.pragma_literal(), canonical.pragma_literal());

    // base64（标准 + url-safe 无补位）：与 hex 解出同一把 key。
    let bytes: [u8; 32] = std::array::from_fn(|index| u8::try_from(index).expect("0..32"));
    let from_bytes = StoreKey::from_bytes(bytes);
    assert_eq!(from_bytes.pragma_literal(), canonical.pragma_literal());
    for encoded in [STANDARD.encode(bytes), URL_SAFE_NO_PAD.encode(bytes)] {
        let parsed = StoreKey::parse(&encoded).expect("32-byte base64 is a valid key");
        assert_eq!(parsed.pragma_literal(), canonical.pragma_literal());
    }
    // 尾换行（`export KEY=$(cat …)`）先剥空白再判。
    assert_eq!(
        StoreKey::parse(&format!("{KEY_A_HEX}\n"))
            .expect("trailing newline tolerated")
            .pragma_literal(),
        canonical.pragma_literal()
    );

    // 一切非 32B/64hex 的形状都拒。
    let rejected = [
        String::new(),
        "   ".to_string(),
        "not-a-key".to_string(),
        KEY_A_HEX[..63].to_string(),
        format!("{KEY_A_HEX}00"),
        "z".repeat(64),
        STANDARD.encode([0_u8; 31]),
        STANDARD.encode([0_u8; 33]),
    ];
    for candidate in rejected {
        let error = StoreKey::parse(&candidate).expect_err("shape must be rejected");
        assert_eq!(error, crate::store_key::StoreKeyError::InvalidShape);
        assert!(
            !error.to_string().contains(&candidate) || candidate.is_empty(),
            "error text must not echo the candidate"
        );
        assert_eq!(
            StoreKeyState::from_env_value(Some(&candidate)).label(),
            "invalid"
        );
    }

    // 三态入口：缺席 = Absent（不是非法）。
    assert_eq!(StoreKeyState::from_env_value(None).label(), "absent");
    assert_eq!(
        StoreKeyState::from_env_value(Some(KEY_A_HEX)).label(),
        "present"
    );

    // 材料不进文本：Debug 打码，错误文本只有固定字面量。
    let debug = format!("{:?}", canonical);
    assert!(
        !debug.contains("000102030405"),
        "Debug leaked key bytes: {debug}"
    );
    assert_eq!(debug, "StoreKey(<redacted 32 bytes>)");

    // SQLCipher raw key 规格：外层双引号 + x'<64hex>'。
    let literal = canonical.pragma_literal();
    // 3 ("x') + 64 hex + 2 ('") = 69。
    assert_eq!(literal.len(), 69, "literal shape: {literal:?}");
    assert!(
        literal.starts_with("\"x'") && literal.ends_with("'\""),
        "{literal:?}"
    );
    assert!(
        literal[3..67]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "raw key body must be 64 lowercase hex chars"
    );
}

/// 矩阵 1：key 形状非法 → `StoreKeyInvalid`（终态），一个文件都不落。
#[tokio::test]
async fn invalid_key_shape_refuses_before_touching_disk() {
    let root = sandbox("c-key-invalid");
    let target = root.join("accounts/lk-invalid");
    let gate = CipherGate::new();

    let error = open_store_with(&target, &gate, &StoreKeyState::Invalid)
        .await
        .expect_err("invalid key shape must fail closed");
    assert_eq!(error, StoreOpenError::KeyInvalid);
    assert_eq!(error.code(), "StoreKeyInvalid");
    assert!(error.is_terminal(), "StoreKeyInvalid never retries");
    assert!(
        !target.exists(),
        "invalid key must refuse before creating the account dir"
    );
    assert_eq!(
        entry_count(&root),
        0,
        "no file may appear on the refuse path"
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// 矩阵 b：有 key + 没有库 → 建的是**加密**库（打开时 `PRAGMA key`）。
#[tokio::test]
async fn keyed_fresh_store_is_created_encrypted() {
    let root = sandbox("c-new-encrypted");
    let dir = root.join("accounts/lk-new");
    let db = dir.join("store.db");
    let gate = CipherGate::new();
    let key = key(KEY_A_HEX);

    drop(
        open_store_with(&dir, &gate, &StoreKeyState::Present(key.clone()))
            .await
            .expect("keyed create"),
    );

    let bytes = std::fs::read(&db).expect("store.db");
    assert!(bytes.len() >= 512, "a keyed store is at least one page");
    assert_ne!(
        &bytes[..16],
        PLAINTEXT_HEADER,
        "with a key the store must not carry the plaintext header"
    );

    // 无 key：打不开（不是「打开了但读不到数据」）。
    assert!(
        query_i64(&db, None, "SELECT count(*) FROM _sqlx_migrations")
            .await
            .is_err(),
        "keyless read of a keyed store must fail"
    );
    // 有 key：迁移账本在（建库时跑过迁移），重开同 key 仍可读。
    assert!(
        query_i64(&db, Some(&key), "SELECT count(*) FROM _sqlx_migrations")
            .await
            .expect("keyed read")
            > 0,
        "migrations must be visible with the key"
    );
    drop(
        open_store_with(&dir, &gate, &StoreKeyState::Present(key.clone()))
            .await
            .expect("keyed reopen"),
    );
    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// 矩阵 d：有 key + 已是加密库 → 正常打开；**错 key 必须给
/// `StoreKeyRejected`**（终态），且不重写文件、不新建明文库。
#[tokio::test]
async fn wrong_key_is_rejected_and_never_rewrites_the_store() {
    let root = sandbox("c-wrong-key");
    let dir = root.join("accounts/lk-keyed");
    let db = dir.join("store.db");
    let gate = CipherGate::new();

    drop(
        open_store_with(&dir, &gate, &present(KEY_A_HEX))
            .await
            .expect("keyed create"),
    );
    // 先在加密库里落一行探针：拒绝路径若重写/截断文件，这行必丢。
    execute_keyed(
        &db,
        &key(KEY_A_HEX),
        "CREATE TABLE c_reject_probe (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
    )
    .await;
    execute_keyed(
        &db,
        &key(KEY_A_HEX),
        "INSERT INTO c_reject_probe (k, v) VALUES ('probe', 'value')",
    )
    .await;
    let head = std::fs::read(&db).expect("store.db");
    assert_ne!(&head[..16], PLAINTEXT_HEADER);
    assert!(
        unexpected_entries(&dir).is_empty(),
        "the keyed account dir holds only store.db (+WAL sidecars): {:?}",
        unexpected_entries(&dir)
    );

    let error = open_store_with(&dir, &gate, &present(KEY_B_HEX))
        .await
        .expect_err("wrong key must be rejected");
    assert_eq!(error, StoreOpenError::KeyRejected);
    assert_eq!(error.code(), "StoreKeyRejected");
    assert!(
        error.is_terminal(),
        "a rejected key must not degrade into a retryable/network class"
    );

    // 现场未被改写：头仍是密文、探针行仍在（用对 key 读回）、没有多出文件。
    let after = std::fs::read(&db).expect("store.db still readable");
    assert_ne!(
        &after[..16],
        PLAINTEXT_HEADER,
        "a rejected open must not turn the store into plaintext"
    );
    assert!(
        unexpected_entries(&dir).is_empty(),
        "a rejected open must not create extra files: {:?}",
        unexpected_entries(&dir)
    );
    assert!(
        !dir.join("store.db-journal").exists(),
        "a rejected open must not leave a half-written journal"
    );
    assert_eq!(
        query_text(
            &db,
            Some(&key(KEY_A_HEX)),
            "SELECT v FROM c_reject_probe WHERE k = 'probe'"
        )
        .await
        .expect("right-key read after the rejection"),
        "value",
        "the rejected open must not lose or rewrite rows"
    );
    assert!(
        query_i64(&db, None, "SELECT count(*) FROM _sqlx_migrations")
            .await
            .is_err(),
        "no plaintext store exists after the rejection"
    );

    // 换回对 key：同一份文件照常打开（证明拒绝路径没动过格式）。
    drop(
        open_store_with(&dir, &gate, &present(KEY_A_HEX))
            .await
            .expect("right key still opens the untouched store"),
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// 矩阵 c：有 key + 存量明文库 → 按 legacy 打开（**不设 key**）、存量数据与
/// 明文头都不动（迁移在 D 阶段）。warn 的**触发面与次数**由下一条用例专测
/// （评审 7a：以前这条用例先自打闸，等于把要证明的东西当成前提）。
#[tokio::test]
async fn plaintext_store_with_key_opens_legacy_and_is_left_intact() {
    // legacy warn 是进程级一次性闸：本用例也会触发它，先拿串行锁，免得
    // 和专测那条用例互相推高计数。
    let _serial = legacy_warn_serial().await;
    let root = sandbox("c-legacy-plaintext");
    let dir = root.join("accounts/lk-legacy");
    let db = dir.join("store.db");
    let gate = CipherGate::new();

    // 矩阵 a 的产物：无 key 建明文库。
    drop(
        open_store_with(&dir, &gate, &StoreKeyState::Absent)
            .await
            .expect("plaintext create"),
    );
    let head = std::fs::read(&db).expect("store.db");
    assert_eq!(&head[..16], PLAINTEXT_HEADER, "matrix a stays plaintext");

    execute(
        &db,
        "CREATE TABLE c_legacy_probe (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
    )
    .await;
    execute(
        &db,
        "INSERT INTO c_legacy_probe (k, v) VALUES ('probe', 'value')",
    )
    .await;

    // 矩阵 c：有 key 也按 legacy 打开（不设 key、不重写）。
    drop(
        open_store_with(&dir, &gate, &present(KEY_A_HEX))
            .await
            .expect("legacy open with a key present"),
    );
    drop(
        open_store_with(&dir, &gate, &present(KEY_A_HEX))
            .await
            .expect("legacy reopen"),
    );

    // 头仍是明文、存量数据仍可被无 key 连接读到（不破坏存量、不偷偷加密）。
    let head = std::fs::read(&db).expect("store.db after legacy open");
    assert_eq!(
        &head[..16],
        PLAINTEXT_HEADER,
        "matrix c must not convert the store in place"
    );
    assert_eq!(
        query_text(&db, None, "SELECT v FROM c_legacy_probe WHERE k = 'probe'")
            .await
            .expect("legacy rows readable without a key"),
        "value"
    );
    assert!(
        query_i64(&db, None, "SELECT count(*) FROM _sqlx_migrations")
            .await
            .expect("migrations still readable")
            > 0
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// 评审阻塞项 7a：legacy warn **由 open 触发**（不是测试先自己打闸），且
/// 每进程只一条。
///
/// 这是矩阵 c「停下来不迁移」的唯一可见信号：有 key 却有明文库 = 这批库
/// 等 D 阶段迁移器。以前那条用例先手工打闸再断言计数，等于把要证明的
/// 东西当成前提（自打闸恒成立，open 不触发也看不出来）。
#[tokio::test]
async fn legacy_warn_is_emitted_by_the_open_not_by_a_manual_gate() {
    let _serial = legacy_warn_serial().await;
    store_open::reset_legacy_plaintext_warn();
    assert_eq!(
        store_open::legacy_plaintext_warn_count(),
        0,
        "串行闸下计数可以确定地归零"
    );

    let root = sandbox("c-legacy-warn");
    let dir = root.join("accounts/lk-warn");
    let gate = CipherGate::new();

    // 建明文库这一步（无 key）**不**打 legacy warn：只有「有 key + 明文库」
    // 才是「这批库待迁移」。
    drop(
        open_store_with(&dir, &gate, &StoreKeyState::Absent)
            .await
            .expect("plaintext create"),
    );
    assert_eq!(
        store_open::legacy_plaintext_warn_count(),
        0,
        "无 key 打开不触发 legacy 提示"
    );

    // 第一次 legacy 打开 → warn 恰一次（**由 open 触发**）。
    drop(
        open_store_with(&dir, &gate, &present(KEY_A_HEX))
            .await
            .expect("legacy open"),
    );
    assert_eq!(
        store_open::legacy_plaintext_warn_count(),
        1,
        "legacy warn 必须由这次 open 触发"
    );

    // 再打开一次：复用已发出的那条，不刷屏。
    drop(
        open_store_with(&dir, &gate, &present(KEY_A_HEX))
            .await
            .expect("legacy reopen"),
    );
    assert_eq!(
        store_open::legacy_plaintext_warn_count(),
        1,
        "重复 legacy 打开不再发第二条"
    );
    // 闸本身幂等：手工再打也不增（信号是「每进程一条」而不是「每次一条」）。
    store_open::warn_plaintext_legacy_once();
    assert_eq!(store_open::legacy_plaintext_warn_count(), 1);

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// 密文 + 没有 key：`StoreKeyRequired`（终态），绝不静默新建明文库。
#[tokio::test]
async fn encrypted_store_without_key_reports_store_key_required() {
    let root = sandbox("c-key-required");
    let dir = root.join("accounts/lk-keyless");
    let db = dir.join("store.db");
    let gate = CipherGate::new();

    drop(
        open_store_with(&dir, &gate, &present(KEY_A_HEX))
            .await
            .expect("keyed create"),
    );
    let before = std::fs::read(&db).expect("store.db");

    let error = open_store_with(&dir, &gate, &StoreKeyState::Absent)
        .await
        .expect_err("an encrypted store without a key must be refused");
    assert_eq!(error, StoreOpenError::KeyRequired);
    assert_eq!(error.code(), "StoreKeyRequired");
    assert!(error.is_terminal());
    assert_eq!(
        std::fs::read(&db).expect("store.db after refusal"),
        before,
        "the refusal path must not replace the encrypted store with a plaintext one"
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// 矩阵 e：库损坏是**独立分类**（`StoreCorrupt`），且现场保留。
#[tokio::test]
async fn corrupt_store_is_classified_apart_from_key_failures() {
    // e2 走「明文头 + 有 key」分支，会触发进程级 legacy warn：拿串行锁，
    // 免得把专测那条用例的计数搅乱（本用例不断言计数）。
    let _serial = legacy_warn_serial().await;
    let gate = CipherGate::new();

    // e1：明文头 + 垃圾页（无 key）→ 打开/迁移时报 malformed/not-a-database。
    let root = sandbox("c-corrupt-plaintext");
    let dir = root.join("accounts/lk-corrupt");
    let db = dir.join("store.db");
    std::fs::create_dir_all(&dir).expect("dir");
    let mut junk = PLAINTEXT_HEADER.to_vec();
    junk.extend(std::iter::repeat_n(0_u8, 4096));
    std::fs::write(&db, &junk).expect("write junk store");

    let error = open_store_with(&dir, &gate, &StoreKeyState::Absent)
        .await
        .expect_err("a corrupt plaintext store must fail closed");
    assert_eq!(
        error.code(),
        "StoreCorrupt",
        "corruption is its own class, got {error:?}"
    );
    assert!(error.is_terminal());
    assert_eq!(
        std::fs::read(&db).expect("corrupt store preserved"),
        junk,
        "a corrupt store keeps its bytes for forensics"
    );

    // e2：同一份垃圾 + 有 key（明文头 → legacy 分支，打开仍失败）→ 同样
    // 归 StoreCorrupt，而不是「错 key」。
    let error = open_store_with(&dir, &gate, &present(KEY_A_HEX))
        .await
        .expect_err("corrupt store with a key must fail closed");
    assert_eq!(error.code(), "StoreCorrupt");

    // e3：非明文头但短于一页 → 结构性截断，不需要 key 就能判损。
    let root = sandbox("c-corrupt-truncated");
    let dir = root.join("accounts/lk-truncated");
    std::fs::create_dir_all(&dir).expect("dir");
    let db = dir.join("store.db");
    std::fs::write(&db, [0x42_u8; 100]).expect("write truncated store");

    let error = open_store_with(&dir, &gate, &present(KEY_A_HEX))
        .await
        .expect_err("a truncated store must fail closed");
    assert_eq!(error, StoreOpenError::Corrupt(CorruptDetail::Truncated));
    assert_eq!(error.code(), "StoreCorrupt");
    assert_ne!(
        StoreOpenError::Corrupt(CorruptDetail::Truncated).code(),
        StoreOpenError::KeyRejected.code(),
        "corruption and key rejection must stay distinguishable"
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// 门禁：能力门仍在 key 判定与一切磁盘动作之前（P0 评审第 1 条不回退）。
#[tokio::test]
async fn capability_gate_still_precedes_key_and_disk_actions() {
    let root = sandbox("c-gate-first");
    let target = root.join("accounts/lk-gate");
    let gate = CipherGate::incapable_for_tests(CipherProbeError::Unavailable);

    // 有合法 key 也没用：门先判。
    let error = open_store_with(&target, &gate, &present(KEY_A_HEX))
        .await
        .expect_err("missing cipher capability must fail closed");
    assert_eq!(error, StoreOpenError::CipherUnavailable);
    assert!(error.is_terminal());

    // 形状非法也排在门之后（先报能力，不先报 key）。
    let error = open_store_with(&target, &gate, &StoreKeyState::Invalid)
        .await
        .expect_err("gate still wins over key shape");
    assert_eq!(error, StoreOpenError::CipherUnavailable);

    assert!(
        !target.exists(),
        "gate refusal must happen before create_dir_all"
    );
    assert_eq!(entry_count(&root), 0);

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// 评审阻塞项 1：**读盘动作失败**不是「库坏了」，必须落在可重试的
/// `StoreOpenFailed` 上——判成 `StoreCorrupt` 会让接收管线永久停摆。
///
/// 覆盖面是三种真实可复现的「非 NotFound」IO 失败：权限位（EACCES）、
/// 自指符号环（ELOOP）、路径中间不是目录（ENOTDIR）。EMFILE/EIO 没法在单
/// 进程里稳定造出来，但走同一条映射（`classify` 只有两个分支：NotFound →
/// `Absent`，其余 IO 错误 → `Failed`）。
#[cfg(unix)]
#[tokio::test]
async fn classify_keeps_io_failures_retryable() {
    use std::os::unix::fs::PermissionsExt;

    // ① EACCES：文件在、头读不到。
    let root = sandbox("c-io-eacces");
    let dir = root.join("accounts/lk-eacces");
    std::fs::create_dir_all(&dir).expect("dir");
    let db = dir.join("store.db");
    std::fs::write(&db, [0x41_u8; 4096]).expect("write store");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    // root（容器里常见）无视权限位：那这条分支没有可观测面，跳过断言。
    if std::fs::File::open(&db).is_err() {
        assert_eq!(
            store_open::classify(&db).expect_err("EACCES is not a store state"),
            StoreOpenError::Failed,
            "权限错误必须可重试，不能判成 Corrupt"
        );
        // 端到端同判：开库路径也只回可重试类（终态会把账号钉死）。
        let error = open_store_with(&dir, &CipherGate::new(), &StoreKeyState::Absent)
            .await
            .expect_err("EACCES must not open the store");
        assert_eq!(error, StoreOpenError::Failed);
        assert!(!error.is_terminal());
    }
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).expect("restore");
    let _ = std::fs::remove_dir_all(&root);

    // ② ELOOP：store.db 自指符号环（连元数据都读不到）。
    let root = sandbox("c-io-eloop");
    let dir = root.join("accounts/lk-eloop");
    std::fs::create_dir_all(&dir).expect("dir");
    std::os::unix::fs::symlink("store.db", dir.join("store.db")).expect("symlink loop");
    assert_eq!(
        store_open::classify(&dir.join("store.db")).expect_err("a loop is not a store state"),
        StoreOpenError::Failed,
        "符号环是 IO 失败，不是内容损坏"
    );
    let _ = std::fs::remove_dir_all(&root);

    // ③ ENOTDIR：账号目录的父级位置被普通文件占住。
    let root = sandbox("c-io-enotdir");
    let blocker = root.join("accounts");
    std::fs::write(&blocker, b"not a directory").expect("blocking file");
    assert_eq!(
        store_open::classify(&blocker.join("lk-x/store.db")).expect_err("ENOTDIR"),
        StoreOpenError::Failed,
        "父级不是目录同样是 IO 失败"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 评审阻塞项 7b：`Corrupt(Unreadable)` 路径——`store.db` 的位置被**目录**
/// 占住。它不是瞬时 IO、不会自愈、与 key 无关，属于布局损坏：终态 + 留现场。
#[tokio::test]
async fn corrupt_unreadable_layout_is_terminal_and_leaves_the_scene() {
    let root = sandbox("c-corrupt-unreadable");
    let dir = root.join("accounts/lk-layout");
    let store_path = dir.join("store.db");
    std::fs::create_dir_all(&store_path).expect("directory occupying store.db");

    let error = open_store_with(&dir, &CipherGate::new(), &present(KEY_A_HEX))
        .await
        .expect_err("a directory at store.db must fail closed");
    assert_eq!(error, StoreOpenError::Corrupt(CorruptDetail::Unreadable));
    assert_eq!(error.code(), "StoreCorrupt");
    assert!(
        error.is_terminal(),
        "布局损坏是终态（等人工），不是退避重试"
    );
    assert_ne!(
        error.code(),
        StoreOpenError::KeyRejected.code(),
        "布局损坏与 key 类必须可区分"
    );

    // 现场未动：占位目录还在、里面没被塞进任何文件、账号目录没有新增条目。
    assert!(store_path.is_dir(), "拒绝路径不得删除/替换占位目录");
    assert_eq!(entry_count(&store_path), 0);
    let mut entries: Vec<String> = std::fs::read_dir(&dir)
        .expect("read account dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, vec!["store.db".to_string()]);

    let _ = std::fs::remove_dir_all(&root);
}

/// 评审第 4 条（口径二选一 → 选「降级文档 + 对齐白名单」）：验真是**读写**
/// 连接，SQLite 在 WAL 模式下必须建 `-wal`/`-shm`，它不可能是「零落盘」。
///
/// 本用例把这条事实钉死：目录只读 → 验真失败并归 `StoreOpenFailed`
/// （可重试）；恢复可写后同一份文件照常打开（说明失败来自挂载/权限）。
/// 若将来真把探针改成只读/immutable 探测，这条会红——那时必须同步改文档
/// 口径与 whitelist，再改这里。
#[cfg(unix)]
#[tokio::test]
async fn probe_keyed_store_requires_write_access() {
    use std::os::unix::fs::PermissionsExt;

    let root = sandbox("c-probe-readonly");
    let dir = root.join("accounts/lk-probe-ro");
    let gate = CipherGate::new();
    let key = key(KEY_A_HEX);
    drop(
        open_store_with(&dir, &gate, &StoreKeyState::Present(key.clone()))
            .await
            .expect("keyed create"),
    );
    quiesce_store_teardown().await;
    for sidecar in ["store.db-wal", "store.db-shm"] {
        let _ = std::fs::remove_file(dir.join(sidecar));
    }

    let permissions = std::fs::metadata(&dir).expect("dir meta").permissions();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("chmod 0500");
    // root 无视目录权限：那这条分支没有可观测面。
    let enforced = std::fs::write(dir.join(".readonly-check"), b"x").is_err();
    let outcome = open_store_with(&dir, &gate, &StoreKeyState::Present(key.clone())).await;
    std::fs::set_permissions(&dir, permissions).expect("restore dir perms");
    let _ = std::fs::remove_file(dir.join(".readonly-check"));
    match (enforced, outcome) {
        (true, Err(error)) => {
            assert_eq!(
                error,
                StoreOpenError::Failed,
                "只读目录下验真失败是可重试类，不是 StoreCorrupt"
            );
            assert!(!error.is_terminal());
        }
        (true, Ok(_)) => panic!(
            "只读目录下的验真不该成功：SQLite 需要写 -wal/-shm（若改成只读探测，先改文档口径）"
        ),
        (false, _) => {
            // root：权限位不生效，跳过。
        }
    }

    drop(
        open_store_with(&dir, &gate, &StoreKeyState::Present(key))
            .await
            .expect("writable dir opens the same store again"),
    );
    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// 评审阻塞项 3 + C-1：固定 stderr 码面补齐——七个码**共用同一形状**
/// （`event=store_open_failed code=<Code> retryable=<bool>`），可重试类
/// （`StoreOpenFailed`）与缺能力（`StoreCipherUnavailable`）都在内；每进程
/// 每码一条（闸幂等），各码的闸互不压制。
#[test]
fn every_store_open_code_emits_the_fixed_line_once() {
    fn line(code: &str, retryable: bool) -> String {
        format!("kt-signal-engine: ERROR event=store_open_failed code={code} retryable={retryable}")
    }
    assert_eq!(
        StoreOpenError::CipherUnavailable.stderr_line(),
        line("StoreCipherUnavailable", false)
    );
    assert_eq!(
        StoreOpenError::KeyInvalid.stderr_line(),
        line("StoreKeyInvalid", false)
    );
    assert_eq!(
        StoreOpenError::KeyRequired.stderr_line(),
        line("StoreKeyRequired", false)
    );
    assert_eq!(
        StoreOpenError::KeyRejected.stderr_line(),
        line("StoreKeyRejected", false)
    );
    assert_eq!(
        StoreOpenError::Corrupt(CorruptDetail::Truncated).stderr_line(),
        line("StoreCorrupt", false)
    );
    assert_eq!(
        StoreOpenError::PlaintextRequiresMigration.stderr_line(),
        line("StorePlaintextRequiresMigration", false),
        "C-1 的终态码用同一形状（connector 不必为它单开一种分类）"
    );
    assert_eq!(
        StoreOpenError::Failed.stderr_line(),
        line("StoreOpenFailed", true),
        "可重试类也必须落同形固定行（以前是一行自由文本，watchdog 分类不到）"
    );
    // 缺能力那条由能力门落：两侧必须逐字相等（形状漂移会被钉红）。
    assert_eq!(
        StoreOpenError::CipherUnavailable.stderr_line(),
        crate::store_cipher::unavailable_stderr_line()
    );

    // 「每进程每码一条 + 各码互不压制」的**可失败证据不在这里**：这段循环
    // 边打边读，`report_once()` 之后 `report_count() == 1` 无论闸共不共用
    // 都成立——把两个码合并成同一个槽，这里照样全绿（假绿）。
    // 真证据在换进程跑的两条用例：
    // - `each_store_open_code_emits_its_own_line`（两个码各落一行、顺序无关）；
    // - `create_dir_all_failure_reports_store_open_failed_once`（建目录失败
    //   的可重试类固定行）。
    // 新进程里闸归零，stderr 干净，合并闸只剩一行 → 必红。
    assert_eq!(
        StoreOpenError::CipherUnavailable.report_count(),
        0,
        "缺能力那一行由能力门落（同形同值），不走 StoreOpenError 的闸"
    );
}

/// 子用例产物（子进程 stdout/stderr 全量）。
struct ChildCase {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// 用**全新进程**跑一个 `#[ignore]` 子用例。
///
/// 为什么必须换进程：`report_once` 的闸是**进程级**一次性 `AtomicU8` 槽，
/// 同进程里任何先跑的用例都会把槽消费掉——那时再断言「stderr 恰一条」，读到
/// 的其实是别人落下的历史行，永远绿。换进程 = 闸归零 + stderr 干净，才有
/// 可能真的证明「这条失败路径落且只落一行」。
fn run_child_case(case: &str, envs: &[(&str, &str)]) -> ChildCase {
    let exe = std::env::current_exe().expect("current test binary");
    let output = std::process::Command::new(exe)
        .arg("--exact")
        .arg(format!("store_encryption_tests::{case}"))
        .args(["--ignored", "--nocapture", "--test-threads=1"])
        .envs(envs.iter().copied())
        .output()
        .expect("child test process spawns");
    ChildCase {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// 子用例必须**真的跑过**：`--exact` 名字漂了不会报错，只会「0 用例通过」。
fn assert_child_ran(child: &ChildCase, case: &str) {
    assert!(
        child.ok,
        "子用例 {case} 失败\n--- stdout ---\n{}\n--- stderr ---\n{}",
        child.stdout, child.stderr
    );
    assert!(
        child.stdout.contains("1 passed"),
        "子用例 {case} 没被跑到（--exact 名字漂了会静默 0 用例）\n--- stdout ---\n{}",
        child.stdout
    );
}

/// 只数固定证据行（其它 stderr 文本不参与计数）。
fn fixed_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.contains("event=store_open_failed"))
        .collect()
}

/// 收尾 1 的子用例：**只读父目录** → `create_dir_all` 失败（EACCES）→
/// 可重试类固定行。
///
/// 打在哪一步是契约：文件分类只读元数据（`stat` 只要求父目录有 `x` 位），
/// 所以 Absent 判定能过，失败落在 §6 建目录——正是以前裸 `?` 静默的那一步。
#[cfg(unix)]
#[ignore = "由 create_dir_all_failure_reports_store_open_failed_once 以新进程拉起"]
#[tokio::test]
async fn child_create_dir_all_failure_emits_one_line() {
    use std::os::unix::fs::PermissionsExt;

    let root = sandbox("child-dir-fail");
    let parent = root.join("accounts");
    std::fs::create_dir_all(&parent).expect("parent dir");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).expect("chmod 0500");
    // root 无视目录权限：那这条分支没有可观测面，明确报 SKIP，父用例据此跳过。
    let enforced = std::fs::write(parent.join(".readonly-check"), b"x").is_err();
    let _ = std::fs::remove_file(parent.join(".readonly-check"));

    let dir = parent.join("lk-readonly");
    let outcome = open_store_with(&dir, &CipherGate::new(), &StoreKeyState::Absent).await;
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).expect("restore dir");

    if enforced {
        assert_eq!(
            outcome.expect_err("只读父目录下建目录必须失败"),
            StoreOpenError::Failed,
            "建目录失败是可重试类（不进终态停机）"
        );
        assert!(!dir.exists(), "建目录失败不得留下账号目录");
        assert!(!dir.join("store.db").exists(), "建目录失败不得建库");
    } else {
        println!("SKIP_ROOT");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// **收尾 1**：`create_dir_all` 失败（EACCES/EROFS/ENOSPC）以前是裸 `?`——
/// stderr 零行，connector watchdog 的逐行分类看不见「盘写不了」，账号只会
/// 表现成「没反应」。现在必须落恰一条 `code=StoreOpenFailed retryable=true`。
///
/// 与路径非 UTF-8、连接串解析失败三处同批（`StoreOpenError::Failed.reported()`
/// 统一出口），闸仍是每进程每码一条——所以要新进程，而不是在
/// `every_store_open_code_emits_the_fixed_line_once` 里数计数。
#[cfg(unix)]
#[test]
fn create_dir_all_failure_reports_store_open_failed_once() {
    let case = "child_create_dir_all_failure_emits_one_line";
    let child = run_child_case(case, &[]);
    assert_child_ran(&child, case);
    if child.stdout.contains("SKIP_ROOT") {
        eprintln!(
            "跳过：当前以 root 运行，目录权限位不生效（与 probe_keyed_store_requires_write_access 同口径）"
        );
        return;
    }
    let lines = fixed_lines(&child.stderr);
    assert_eq!(
        lines.len(),
        1,
        "建目录失败必须**恰**落一条固定行（零行 = watchdog 分类不到）\n--- stderr ---\n{}",
        child.stderr
    );
    assert_eq!(
        lines[0],
        "kt-signal-engine: ERROR event=store_open_failed code=StoreOpenFailed retryable=true",
        "可重试类固定行形状逐字"
    );
}

/// 收尾 2 + C-1 的子用例：**同一个进程**里三个不同的码各触发一次（前两个
/// 的顺序可由 `KT_STORE_GATE_ORDER` 反转），并各重复触发一轮证明闸幂等。
///
/// 三个 fixture 都只靠文件内容造（不依赖权限位，root 下同样成立）：
/// - 非明文头（密文/垃圾）+ 无 key → `StoreKeyRequired`；
/// - 短于一页 → `StoreCorrupt(Truncated)`；
/// - 明文头 + 开关 on（父用例透传 `KT_SIGNAL_STORE_REQUIRE_ENCRYPTED=on`）→
///   `StorePlaintextRequiresMigration`。
#[ignore = "由 each_store_open_code_emits_its_own_line 以新进程拉起"]
#[tokio::test]
async fn child_two_codes_emit_two_lines() {
    async fn trigger(dir: &Path, expected: StoreOpenError) {
        let error = open_store_with(dir, &CipherGate::new(), &StoreKeyState::Absent)
            .await
            .expect_err("两个 fixture 都必须 fail closed");
        assert_eq!(error.code(), expected.code(), "fixture 必须触发期望的码");
        assert!(error.is_terminal(), "两条 fixture 都是终态码");
    }

    let root = sandbox("child-two-codes");
    let opaque = root.join("lk-required");
    std::fs::create_dir_all(&opaque).expect("opaque dir");
    std::fs::write(opaque.join("store.db"), [0xA5_u8; 4096]).expect("opaque store");
    let truncated = root.join("lk-truncated");
    std::fs::create_dir_all(&truncated).expect("truncated dir");
    std::fs::write(truncated.join("store.db"), [0_u8; 100]).expect("truncated store");
    let plaintext = root.join("lk-plaintext");
    std::fs::create_dir_all(&plaintext).expect("plaintext dir");
    let mut junk = PLAINTEXT_HEADER.to_vec();
    junk.extend(std::iter::repeat_n(0_u8, 4096));
    std::fs::write(plaintext.join("store.db"), &junk).expect("plaintext store");

    let required = StoreOpenError::KeyRequired;
    let corrupt = StoreOpenError::Corrupt(CorruptDetail::Truncated);
    let migration = StoreOpenError::PlaintextRequiresMigration;
    let order = if std::env::var("KT_STORE_GATE_ORDER").as_deref() == Ok("corrupt_first") {
        [
            (&truncated, corrupt),
            (&opaque, required),
            (&plaintext, migration),
        ]
    } else {
        [
            (&opaque, required),
            (&truncated, corrupt),
            (&plaintext, migration),
        ]
    };
    for (dir, expected) in order {
        trigger(dir, expected).await;
    }
    // 幂等：再触发一轮，闸不增（父用例仍只该看到两行）。
    for (dir, expected) in order {
        trigger(dir, expected).await;
    }
    assert_eq!(required.report_count(), 1, "KeyRequired 的闸只落一次");
    assert_eq!(corrupt.report_count(), 1, "StoreCorrupt 的闸只落一次");
    assert_eq!(
        migration.report_count(),
        1,
        "PlaintextRequiresMigration 的闸只落一次"
    );
    assert_eq!(
        StoreOpenError::Failed.report_count(),
        0,
        "没触发的码不该被别的码顺带点亮（共用槽会串）"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// **收尾 2 + C-1**：旧断言（`every_store_open_code_emits_the_fixed_line_once` 的
/// 循环）在同一次循环里边打边读——闸已被消费，`report_count() == 1` 无论
/// 共不共用槽都成立，**把两码合并成同一闸也不会红**。
///
/// 这里改成独立用例 + 真进程 + 真 stderr：三个不同码各触发一次，断言三行
/// 都在、各恰一条、前两个与顺序无关。`StoreOpenError::slot()` 一旦把两个码
/// 映射到同一个 `AtomicU8`，后一个码的 `report_once` 会被闸吃掉 → 少一行
/// → 本用例红（变异验证记录见 `docs/plan/engine-store-encryption.md` 的
/// C.12）。
#[test]
fn each_store_open_code_emits_its_own_line() {
    let case = "child_two_codes_emit_two_lines";
    for order in ["required_first", "corrupt_first"] {
        let child = run_child_case(
            case,
            &[
                ("KT_STORE_GATE_ORDER", order),
                (REQUIRE_ENCRYPTED_ENV, "on"),
            ],
        );
        assert_child_ran(&child, case);
        let lines = fixed_lines(&child.stderr);
        assert_eq!(
            lines.len(),
            3,
            "三个不同码必须各落一行（合并闸/共用槽会少一行）order={order}\n--- stderr ---\n{}",
            child.stderr
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("code=StoreKeyRequired"))
                .count(),
            1,
            "order={order}：StoreKeyRequired 恰一条\n--- stderr ---\n{}",
            child.stderr
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("code=StoreCorrupt"))
                .count(),
            1,
            "order={order}：StoreCorrupt 恰一条（被前一个码压掉就是共用闸）\n--- stderr ---\n{}",
            child.stderr
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("code=StorePlaintextRequiresMigration"))
                .count(),
            1,
            "order={order}：StorePlaintextRequiresMigration 恰一条（C-1 新槽不得被别的码压掉）\n--- stderr ---\n{}",
            child.stderr
        );
    }
}

/// sqlx 文本 → 码的归一：同一句 not-a-database 在带/不带 key 时含义不同。
#[test]
fn failure_text_classification_maps_not_a_database_to_key_rejection() {
    let not_a_database = "error returned from database: file is not a database";
    assert_eq!(
        store_open::classify_failure_message(not_a_database, true),
        StoreOpenError::KeyRejected
    );
    assert_eq!(
        store_open::classify_failure_message(not_a_database, false),
        StoreOpenError::Corrupt(CorruptDetail::Malformed),
        "on the plaintext path the same message can only mean corruption"
    );
    assert_eq!(
        store_open::classify_failure_message("database disk image is malformed", true),
        StoreOpenError::Corrupt(CorruptDetail::Malformed)
    );
    assert_eq!(
        store_open::classify_failure_message("unable to open database file", true),
        StoreOpenError::Failed
    );
    assert!(
        !StoreOpenError::Failed.is_terminal(),
        "transport/permission failures keep the retry semantics"
    );
}

/// 收尾 1 的第二条失败路径（子用例）：账号目录名非 UTF-8 → `path.to_str()`
/// 拿不到 `&str`。以前是裸 `return Err(StoreOpenError::Failed)`：可重试类
/// 里唯一「连错误文本都没有」的分支，stderr 零行。
#[cfg(unix)]
#[ignore = "由 non_utf8_store_path_reports_store_open_failed_once 以新进程拉起"]
#[tokio::test]
async fn child_non_utf8_store_path_emits_one_line() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let root = sandbox("child-nonutf8");
    let dir = root.join(OsStr::from_bytes(b"lk-\xff\xfe"));
    let outcome = open_store_with(&dir, &CipherGate::new(), &StoreKeyState::Absent).await;
    assert_eq!(
        outcome.expect_err("非 UTF-8 路径必须 fail closed"),
        StoreOpenError::Failed,
        "路径拿不到 &str 属于可重试类"
    );
    assert!(!dir.exists(), "非 UTF-8 路径不得留下账号目录");
    let _ = std::fs::remove_dir_all(&root);
}

/// **收尾 1（第二条）**：路径非 UTF-8 也必须落恰一条固定行——三处裸
/// `StoreOpenError::Failed` 的另一处（`create_dir_all` 那条见
/// `create_dir_all_failure_reports_store_open_failed_once`）。
#[cfg(unix)]
#[test]
fn non_utf8_store_path_reports_store_open_failed_once() {
    let case = "child_non_utf8_store_path_emits_one_line";
    let child = run_child_case(case, &[]);
    assert_child_ran(&child, case);
    let lines = fixed_lines(&child.stderr);
    assert_eq!(
        lines.len(),
        1,
        "非 UTF-8 路径必须**恰**落一条固定行（零行 = 这条失败路径不可观测）\n--- stderr ---\n{}",
        child.stderr
    );
    assert_eq!(
        lines[0],
        "kt-signal-engine: ERROR event=store_open_failed code=StoreOpenFailed retryable=true",
        "可重试类固定行形状逐字"
    );
}

// ——————————————————— C-1 终态开关（`KT_SIGNAL_STORE_REQUIRE_ENCRYPTED`） ———————————————————

/// 目录里的全部条目名（排序）：终态拒绝路径必须**不新建、不删除**任何文件。
fn entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// C-1 网格的明文库格：先按 **off** 建库（种子的行为不该被调用方 shell 里的
/// 开关影响），再落一张探针表 + 一行数据；返回 `store.db` 路径。
async fn seed_plaintext_store(dir: &Path) -> PathBuf {
    drop(
        open_store_with_requirement(
            dir,
            &CipherGate::new(),
            &StoreKeyState::Absent,
            RequireEncrypted::Off,
        )
        .await
        .expect("plaintext seed"),
    );
    let db = dir.join("store.db");
    execute(
        &db,
        "CREATE TABLE c1_probe (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
    )
    .await;
    execute(&db, "INSERT INTO c1_probe (k, v) VALUES ('probe', 'value')").await;
    quiesce_store_teardown().await;
    db
}

/// C-1 网格的密文库格：按 **off** + key 建加密库，返回 `store.db` 路径。
async fn seed_encrypted_store(dir: &Path) -> PathBuf {
    drop(
        open_store_with_requirement(
            dir,
            &CipherGate::new(),
            &present(KEY_A_HEX),
            RequireEncrypted::Off,
        )
        .await
        .expect("keyed seed"),
    );
    quiesce_store_teardown().await;
    dir.join("store.db")
}

/// 「开关 on + 明文库 → 终态」两格的公共断言：码 / 终态 / 固定行 / 零写入 /
/// 零新建与零删除（含 sidecar）/ 明文库本身还在。
async fn assert_plaintext_refused(
    dir: &Path,
    key_state: &StoreKeyState,
    label: &str,
) -> StoreOpenError {
    let db = dir.join("store.db");
    let before = std::fs::read(&db).expect("store.db");
    let entries = entry_names(dir);
    let error =
        match open_store_with_requirement(dir, &CipherGate::new(), key_state, RequireEncrypted::On)
            .await
        {
            Ok(_) => panic!("{label}：开关 on 必须拒绝明文库"),
            Err(error) => error,
        };
    assert_eq!(error, StoreOpenError::PlaintextRequiresMigration, "{label}");
    assert_eq!(error.code(), "StorePlaintextRequiresMigration", "{label}");
    assert!(error.is_terminal(), "{label}：终态不得退化成退避/网络重试");
    assert_eq!(
        error.stderr_line(),
        "kt-signal-engine: ERROR event=store_open_failed code=StorePlaintextRequiresMigration retryable=false",
        "{label}：固定行与既有六码同形"
    );
    assert_eq!(
        std::fs::read(&db).expect("store.db after refusal"),
        before,
        "{label}：终态拒绝不得写库"
    );
    assert_eq!(
        entry_names(dir),
        entries,
        "{label}：终态拒绝不得新建/删除任何文件（含 -wal/-shm）"
    );
    assert!(
        unexpected_entries(dir).is_empty(),
        "{label}：拒绝路径不得凭空造文件"
    );
    assert!(
        db.is_file(),
        "{label}：明文库必须原样留着（迁移器要拿它当源）"
    );
    error
}

/// C-1 开关只认显式词：`on` / `1` / `true` / `yes` 才开；`off` / `0` /
/// `false` / `no` / 空 / 缺席是 off；认不出的值**不**回落到 on（默认 off 是硬
/// 约束——笔误不该把一批账号全打成终态）。
#[test]
fn require_encrypted_switch_parses_only_explicit_words() {
    for raw in [
        None,
        Some(""),
        Some("   "),
        Some("off"),
        Some("OFF"),
        Some(" 0 "),
        Some("false"),
        Some("No"),
    ] {
        assert_eq!(
            require_encrypted_from_env_value(raw),
            RequireEncrypted::Off,
            "raw={raw:?} 必须被认成 off"
        );
        assert!(
            parse_require_encrypted(raw).is_some(),
            "raw={raw:?} 是合法取值（不是「认不出」）"
        );
    }
    for raw in ["on", "ON", " On ", "1", "true", "Yes"] {
        assert_eq!(
            require_encrypted_from_env_value(Some(raw)),
            RequireEncrypted::On,
            "raw={raw:?} 必须开启严格路径"
        );
        assert!(RequireEncrypted::On.is_on());
    }
    for raw in ["enable", "2", "on!", "flase", "yes please"] {
        assert_eq!(
            parse_require_encrypted(Some(raw)),
            None,
            "raw={raw:?} 是认不出的值"
        );
        assert_eq!(
            require_encrypted_from_env_value(Some(raw)),
            RequireEncrypted::Off,
            "raw={raw:?}：认不出 = off（绝不回落到 on）"
        );
    }
}

/// C-1 网格 ①：开关 off × 明文 × 无 key —— 现状逐字节不变（明文照旧打开、
/// 存量行可读、头仍是明文）。
#[tokio::test]
async fn switch_off_plaintext_without_key_still_opens() {
    let root = sandbox("c1-off-plain-keyless");
    let dir = root.join("accounts/lk-c1");
    let db = seed_plaintext_store(&dir).await;

    drop(
        open_store_with_requirement(
            &dir,
            &CipherGate::new(),
            &StoreKeyState::Absent,
            RequireEncrypted::Off,
        )
        .await
        .expect("off 必须保持兼容路径"),
    );
    assert_eq!(
        &std::fs::read(&db).expect("store.db")[..16],
        PLAINTEXT_HEADER,
        "off 不得改变明文库的形态"
    );
    assert_eq!(
        query_text(&db, None, "SELECT v FROM c1_probe WHERE k = 'probe'")
            .await
            .expect("legacy rows readable"),
        "value"
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// C-1 网格 ②：开关 off × 明文 × 有 key —— 现状逐字节不变（legacy 打开 +
/// warn 恰一次），与矩阵 c 同口径。
#[tokio::test]
async fn switch_off_plaintext_with_key_still_opens_legacy() {
    let _serial = legacy_warn_serial().await;
    store_open::reset_legacy_plaintext_warn();
    let root = sandbox("c1-off-plain-keyed");
    let dir = root.join("accounts/lk-c1-keyed");
    let db = seed_plaintext_store(&dir).await;

    drop(
        open_store_with_requirement(
            &dir,
            &CipherGate::new(),
            &present(KEY_A_HEX),
            RequireEncrypted::Off,
        )
        .await
        .expect("off + 有 key 必须仍按 legacy 打开"),
    );
    assert_eq!(
        store_open::legacy_plaintext_warn_count(),
        1,
        "off 路径的 legacy 证据行照旧恰一条"
    );
    assert_eq!(
        &std::fs::read(&db).expect("store.db")[..16],
        PLAINTEXT_HEADER
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// C-1 网格 ③：开关 off × 密文 × 无 key —— 现状不变（`StoreKeyRequired` 终态）。
#[tokio::test]
async fn switch_off_encrypted_without_key_still_reports_key_required() {
    let root = sandbox("c1-off-cipher-keyless");
    let dir = root.join("accounts/lk-c1-cipher");
    let db = seed_encrypted_store(&dir).await;
    let before = std::fs::read(&db).expect("store.db");

    let error = open_store_with_requirement(
        &dir,
        &CipherGate::new(),
        &StoreKeyState::Absent,
        RequireEncrypted::Off,
    )
    .await
    .expect_err("密文 + 无 key 照旧终态");
    assert_eq!(error, StoreOpenError::KeyRequired, "off 不得改变这一格");
    assert_eq!(std::fs::read(&db).expect("store.db"), before);

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// C-1 网格 ④：开关 off × 密文 × 有 key —— 现状不变（验真后正常打开）。
#[tokio::test]
async fn switch_off_encrypted_with_key_still_opens() {
    let root = sandbox("c1-off-cipher-keyed");
    let dir = root.join("accounts/lk-c1-cipher-keyed");
    let db = seed_encrypted_store(&dir).await;

    drop(
        open_store_with_requirement(
            &dir,
            &CipherGate::new(),
            &present(KEY_A_HEX),
            RequireEncrypted::Off,
        )
        .await
        .expect("off + 有 key 的密文库照常打开"),
    );
    assert!(
        query_i64(
            &db,
            Some(&key(KEY_A_HEX)),
            "SELECT count(*) FROM _sqlx_migrations"
        )
        .await
        .expect("keyed read")
            > 0
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// C-1 网格 ⑤：开关 on × 明文 × **无 key** —— 终态。这一格是开关的意义所在：
/// 放它过去等于「删掉 key 就能读明文」。
#[tokio::test]
async fn switch_on_plaintext_without_key_is_plaintext_requires_migration() {
    let root = sandbox("c1-on-plain-keyless");
    let dir = root.join("accounts/lk-c1-on-keyless");
    seed_plaintext_store(&dir).await;

    assert_plaintext_refused(&dir, &StoreKeyState::Absent, "on/明文/无 key").await;

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// C-1 网格 ⑥：开关 on × 明文 × **有 key** —— 同一终态，且**先于**任何
/// `probe` / legacy warn（终态闸在「决策是否带 key 打开」之前）。
#[tokio::test]
async fn switch_on_plaintext_with_key_is_plaintext_requires_migration() {
    let _serial = legacy_warn_serial().await;
    store_open::reset_legacy_plaintext_warn();
    let root = sandbox("c1-on-plain-keyed");
    let dir = root.join("accounts/lk-c1-on-keyed");
    seed_plaintext_store(&dir).await;

    assert_plaintext_refused(&dir, &present(KEY_A_HEX), "on/明文/有 key").await;
    assert_eq!(
        store_open::legacy_plaintext_warn_count(),
        0,
        "终态闸先于 legacy 打开，不该先打「按明文打开」的证据行"
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// C-1 网格 ⑦：开关 on × 密文 × 无 key —— 与 off 同判（`StoreKeyRequired`），
/// 开关只改变了明文那一格。
#[tokio::test]
async fn switch_on_encrypted_without_key_still_reports_key_required() {
    let root = sandbox("c1-on-cipher-keyless");
    let dir = root.join("accounts/lk-c1-on-cipher");
    let db = seed_encrypted_store(&dir).await;
    let before = std::fs::read(&db).expect("store.db");

    let error = open_store_with_requirement(
        &dir,
        &CipherGate::new(),
        &StoreKeyState::Absent,
        RequireEncrypted::On,
    )
    .await
    .expect_err("密文 + 无 key 与开关无关，仍是终态");
    assert_eq!(error, StoreOpenError::KeyRequired);
    assert_eq!(std::fs::read(&db).expect("store.db"), before);

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// C-1 网格 ⑧：开关 on × 密文 × 有 key —— 正常打开（Orchestration apply 之后
/// 的现实形态：库已是密文，开关可以安全打开）。
#[tokio::test]
async fn switch_on_encrypted_with_key_still_opens() {
    let root = sandbox("c1-on-cipher-keyed");
    let dir = root.join("accounts/lk-c1-on-cipher-keyed");
    let db = seed_encrypted_store(&dir).await;

    drop(
        open_store_with_requirement(
            &dir,
            &CipherGate::new(),
            &present(KEY_A_HEX),
            RequireEncrypted::On,
        )
        .await
        .expect("迁移完成后开关 on 不得挡住正常的加密库"),
    );
    assert_ne!(
        &std::fs::read(&db).expect("store.db")[..16],
        PLAINTEXT_HEADER
    );
    assert!(
        query_i64(
            &db,
            Some(&key(KEY_A_HEX)),
            "SELECT count(*) FROM _sqlx_migrations"
        )
        .await
        .expect("keyed read")
            > 0
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// **C-1 网格 ⑨（补漏，2026-11-12）**：开关 on × **库不存在** × 无 key —— 终态
/// `StoreKeyRequired`，不建目录、不建库、不写一个字节。
///
/// 这一格原来的落点是矩阵 a 的 `(None, Absent)` → 明文创建：开关 on 时它会在
/// `create_if_missing` 里现造一个**新的明文库**。它比 ⑤（存量明文库）更糟：
/// 存量至少是迁移源，这一格是开关打开之后才开始的明文生产。语义上也不是
/// 「要迁移」（盘上连目录都没有），而是「缺 key 就建不出密文库」。
#[tokio::test]
async fn switch_on_absent_without_key_refuses_before_creating_plaintext() {
    let root = sandbox("c1-on-absent-keyless");
    let dir = root.join("accounts/lk-c1-on-absent");

    let error = open_store_with_requirement(
        &dir,
        &CipherGate::new(),
        &StoreKeyState::Absent,
        RequireEncrypted::On,
    )
    .await
    .expect_err("开关 on + 无 key + 库不存在必须终态，不得新建明文库");
    assert_eq!(error, StoreOpenError::KeyRequired);
    assert_eq!(error.code(), "StoreKeyRequired");
    assert!(error.is_terminal(), "缺 key 是终态：重试一万次还是缺 key");
    assert_eq!(
        error.stderr_line(),
        "kt-signal-engine: ERROR event=store_open_failed code=StoreKeyRequired retryable=false",
        "缺 key 的固定行与既有码逐字同形"
    );
    assert_ne!(
        error.code(),
        StoreOpenError::PlaintextRequiresMigration.code(),
        "盘上没有明文库可迁移：这一格是「缺 key」，不是「要迁移」"
    );

    // 零落盘：连账号目录都不建，更不建 store.db（矩阵 a 的产物一个都不许出现）。
    assert!(!dir.exists(), "拒绝路径不得创建账号目录");
    assert_eq!(entry_count(&root), 0, "拒绝路径不得落任何文件");

    // 再拒一次仍同码（闸幂等；「每进程恰一条」由真进程用例断言）。
    let again = open_store_with_requirement(
        &dir,
        &CipherGate::new(),
        &StoreKeyState::Absent,
        RequireEncrypted::On,
    )
    .await
    .expect_err("第二次仍是终态");
    assert_eq!(again, error);
    assert!(
        !dir.join("store.db").exists(),
        "两次拒绝都不得建出 store.db"
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// **C-1 网格 ⑨ 的另一半**：开关 on × 库不存在 × **有 key** —— 照常建加密库。
/// 新闸必须只拦「无 key」，否则开关一开新账号就永远建不出来。
#[tokio::test]
async fn switch_on_absent_with_key_still_creates_encrypted_store() {
    let root = sandbox("c1-on-absent-keyed");
    let dir = root.join("accounts/lk-c1-on-absent-keyed");
    let db = dir.join("store.db");
    let key = key(KEY_A_HEX);

    drop(
        open_store_with_requirement(
            &dir,
            &CipherGate::new(),
            &StoreKeyState::Present(key.clone()),
            RequireEncrypted::On,
        )
        .await
        .expect("开关 on + 有 key 的首次创建必须成功"),
    );

    let bytes = std::fs::read(&db).expect("store.db");
    assert!(bytes.len() >= 512, "密钥库至少一页");
    assert_ne!(
        &bytes[..16],
        PLAINTEXT_HEADER,
        "开关 on 下建出来的必须是密文库"
    );
    assert!(
        query_i64(&db, None, "SELECT count(*) FROM _sqlx_migrations")
            .await
            .is_err(),
        "新库无 key 打不开"
    );
    assert!(
        query_i64(&db, Some(&key), "SELECT count(*) FROM _sqlx_migrations")
            .await
            .expect("keyed read")
            > 0,
        "带 key 读得到迁移账本（库真的被打开了）"
    );

    // 重开走「密文 + 有 key」那一格（⑧），不被新闸误伤。
    drop(
        open_store_with_requirement(
            &dir,
            &CipherGate::new(),
            &StoreKeyState::Present(key.clone()),
            RequireEncrypted::On,
        )
        .await
        .expect("开关 on 下重开同一份密文库"),
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// **C-1 网格 ⑨ 的 off 对照**：开关 off × 库不存在 × 无 key —— 明文创建照旧
/// （矩阵 a 逐字节不变）。新闸只在 on 时成立，off 一格都不能动。
#[tokio::test]
async fn switch_off_absent_without_key_still_creates_plaintext_store() {
    let root = sandbox("c1-off-absent-keyless");
    let dir = root.join("accounts/lk-c1-off-absent");
    let db = dir.join("store.db");

    drop(
        open_store_with_requirement(
            &dir,
            &CipherGate::new(),
            &StoreKeyState::Absent,
            RequireEncrypted::Off,
        )
        .await
        .expect("off + 无 key + 无库仍是兼容的明文创建（矩阵 a）"),
    );

    let bytes = std::fs::read(&db).expect("store.db");
    assert_eq!(
        &bytes[..16],
        PLAINTEXT_HEADER,
        "off 造出来的仍是明文库（逐字节与矩阵 a 同形）"
    );
    assert!(
        query_i64(&db, None, "SELECT count(*) FROM _sqlx_migrations")
            .await
            .expect("plaintext read")
            > 0,
        "无 key 能读到迁移账本（真·明文库）"
    );
    assert!(
        unexpected_entries(&dir).is_empty(),
        "明文创建不额外造文件：{:?}",
        unexpected_entries(&dir)
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// C-1 的顺序契约：开关 on 时**文件头是权威**——「明文头 + 垃圾内容」报
/// 「要迁移」而不是 `StoreCorrupt`（与迁移器同口径：先看头再谈内容）。
/// 两条都是终态，区别只在运维读到的码。
#[tokio::test]
async fn switch_on_classifies_plaintext_by_header_before_opening() {
    let root = sandbox("c1-on-plain-junk");
    let dir = root.join("accounts/lk-c1-junk");
    let db = dir.join("store.db");
    std::fs::create_dir_all(&dir).expect("dir");
    let mut junk = PLAINTEXT_HEADER.to_vec();
    junk.extend(std::iter::repeat_n(0_u8, 4096));
    std::fs::write(&db, &junk).expect("write junk store");

    let error = open_store_with_requirement(
        &dir,
        &CipherGate::new(),
        &present(KEY_A_HEX),
        RequireEncrypted::On,
    )
    .await
    .expect_err("明文头 = 先要迁移");
    assert_eq!(error, StoreOpenError::PlaintextRequiresMigration);
    assert_ne!(
        error.code(),
        StoreOpenError::Corrupt(CorruptDetail::Malformed).code(),
        "头优先，不与「内容坏了」混码"
    );
    assert_eq!(
        std::fs::read(&db).expect("junk store preserved"),
        junk,
        "拒绝路径不得改动明文库"
    );

    quiesce_store_teardown().await;
    let _ = std::fs::remove_dir_all(&root);
}

/// C-1 子用例（新进程）：`KT_SIGNAL_STORE_REQUIRE_ENCRYPTED=on` 由 env 进
/// 进程，明文库 + 无 key → 终态；两次调用只落一条固定行；文件不动。
#[ignore = "由 switch_on_plaintext_is_terminal_and_reports_once_per_process 以新进程拉起"]
#[tokio::test]
async fn child_switch_on_plaintext_requires_migration() {
    let root = sandbox("child-c1-switch-on");
    let dir = root.join("accounts/lk-c1-switch");
    let db = seed_plaintext_store(&dir).await;
    let before = std::fs::read(&db).expect("store.db");
    let entries = entry_names(&dir);

    let first = open_store_with(&dir, &CipherGate::new(), &StoreKeyState::Absent)
        .await
        .expect_err("env 开关 on + 明文库必须终态");
    assert_eq!(first, StoreOpenError::PlaintextRequiresMigration);
    assert!(first.is_terminal());
    // 第二次：闸幂等（父用例仍只该看到一行）。
    let second = open_store_with(&dir, &CipherGate::new(), &StoreKeyState::Absent)
        .await
        .expect_err("仍然终态");
    assert_eq!(second, first);
    assert_eq!(first.report_count(), 1, "每进程每码恰一条");
    assert_eq!(
        StoreOpenError::Failed.report_count(),
        0,
        "新码不得与可重试类共用闸（共用会互相压制）"
    );
    assert_eq!(
        std::fs::read(&db).expect("store.db"),
        before,
        "终态拒绝不得写库"
    );
    assert_eq!(entry_names(&dir), entries, "终态拒绝不得新建/删除文件");
    let _ = std::fs::remove_dir_all(&root);
}

/// **C-1 真进程**：开关由 **env** 进入（不是测试注入的枚举），明文库 +
/// 无 key 在新进程里恰落一条
/// `code=StorePlaintextRequiresMigration retryable=false`。
///
/// 换进程的理由与收尾 2 相同：`report_once` 的闸是进程级一次性槽，
/// 同进程里前面的用例早把它消费掉了（边打边读永远绿）。
#[test]
fn switch_on_plaintext_is_terminal_and_reports_once_per_process() {
    let case = "child_switch_on_plaintext_requires_migration";
    let child = run_child_case(case, &[(REQUIRE_ENCRYPTED_ENV, "on")]);
    assert_child_ran(&child, case);
    let lines = fixed_lines(&child.stderr);
    assert_eq!(
        lines.len(),
        1,
        "开关 on 的明文库必须恰落一条固定行（零行 = watchdog 分类不到；多行 = 闸漏）\n--- stderr ---\n{}",
        child.stderr
    );
    assert_eq!(
        lines[0],
        "kt-signal-engine: ERROR event=store_open_failed code=StorePlaintextRequiresMigration retryable=false",
        "C-1 固定行与既有六码逐字同形"
    );
}

/// C-1 ⑨ 的子用例（**新进程**）：env 开关 on + 无 key + 库不存在 → 终态
/// `StoreKeyRequired`；两次调用只落一条固定行；账号目录始终不存在。
#[ignore = "由 switch_on_absent_without_key_reports_once_per_process 以新进程拉起"]
#[tokio::test]
async fn child_switch_on_absent_keyless_emits_key_required_once() {
    let root = sandbox("child-c1-absent");
    let dir = root.join("accounts/lk-c1-absent");

    let first = open_store_with(&dir, &CipherGate::new(), &StoreKeyState::Absent)
        .await
        .expect_err("env 开关 on + 无 key + 无库必须终态");
    assert_eq!(first, StoreOpenError::KeyRequired);
    assert!(first.is_terminal(), "缺 key 不得退化成退避/网络重试");
    // 第二次：闸幂等（父用例仍只该看到一行）。
    let second = open_store_with(&dir, &CipherGate::new(), &StoreKeyState::Absent)
        .await
        .expect_err("仍然终态");
    assert_eq!(second, first);
    assert_eq!(first.report_count(), 1, "每进程每码恰一条");
    assert_eq!(
        StoreOpenError::PlaintextRequiresMigration.report_count(),
        0,
        "这一格不走「要迁移」的闸（盘上无库可迁）"
    );
    assert!(!dir.exists(), "终态路径不得建账号目录");
    assert!(!dir.join("store.db").exists(), "终态路径不得建库");
    let _ = std::fs::remove_dir_all(&root);
}

/// **C-1 ⑨ 真进程**：开关由 **env** 进入（不是测试注入的枚举），
/// `code=StoreKeyRequired retryable=false` 恰一条；这一格的「终态 + 零文件 +
/// 固定行一条」三条要求一次钉死。
#[test]
fn switch_on_absent_without_key_reports_once_per_process() {
    let case = "child_switch_on_absent_keyless_emits_key_required_once";
    let child = run_child_case(case, &[(REQUIRE_ENCRYPTED_ENV, "on")]);
    assert_child_ran(&child, case);
    let lines = fixed_lines(&child.stderr);
    assert_eq!(
        lines.len(),
        1,
        "开关 on + 无 key + 无库必须**恰**落一条固定行（零行 = watchdog 看不到；多行 = 闸漏）\n--- stderr ---\n{}",
        child.stderr
    );
    assert_eq!(
        lines[0],
        "kt-signal-engine: ERROR event=store_open_failed code=StoreKeyRequired retryable=false",
        "缺 key 的固定行与既有码逐字同形"
    );
}
