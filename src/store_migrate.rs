// SPDX-License-Identifier: AGPL-3.0-only

//! M6 D1：engine 明文 `accounts/lk-*/store.db` → **加密库**的正向迁移器
//! （设计：`docs/plan/engine-store-migration-design.md` §2–§5、§9、§10）。
//!
//! 位置（设计 §2.2）：**同一二进制的新 argv 模式**
//! `kt-signal-engine --data-dir <DIR> storeMigrate [--account <lk-hex>] [--dry-run] [--json]`，
//! 与 `jsonRpc` 互斥、未知参数仍 exit 2（`src/main.rs` 的严格口径不变）。不放
//! 进 `open_store`（72 处调用点 + 接收路径 90s 超时会把迁移判成卡死），也不做
//! 启动内联迁移（连接器 15s 请求 / 30s watchdog 会在导出中途杀掉引擎）。
//!
//! 不变量（逐条对应硬约束）：
//!
//! 1. **能力门在最前**：`store_cipher::ensure_cipher_capable()` 早于任何磁盘
//!    动作（连 `logs/` 都还没建）——纯 SQLite 上 `PRAGMA key` 是静默 no-op，
//!    门后置等于「先落一个"加密"的明文库」。
//! 2. **key 只从 env 读、绝不落日志**：复用 C 的 `StoreKeyState::process`
//!    （`KT_SIGNAL_STORE_KEY` 三态）。证据行只带
//!    `code/stage/account_tag/bytes/elapsed_ms/keyid`——没有 key、没有号码；
//!    `keyid`/`fp` 是域分离哈希的 8 hex 前缀（`key_digest`），不可逆。
//! 3. **明文库迁移前后都能被无 key 打开**：只有五步全部成功才 rename；
//!    任一步失败保留原库与 sidecar，只清属于本次的 staging/备份半成品。
//! 4. **文件头是唯一权威**：`store_open::classify`（16 字节头 + 长度）判
//!    `Absent`/`Plaintext`/`Opaque`；`store.format` 只作审计与指纹比对，
//!    **任何正确性判断都不读它**（设计 §5.2 对 C 文档 §4.6 的修正：rename
//!    与 marker 之间的窗口靠头吸收，不产生第三态）。
//! 5. **每账号独立、串行、崩溃可续**：枚举 `accounts/lk-*`（不是注册表），
//!    按目录名排序逐个迁；单账号失败不阻断其余；`store-migrate.lock`
//!    非阻塞 flock 保证同一 data-dir 只有一次迁移在跑。
//! 6. **只做正向**：`--direction to-plaintext` 在本批是用法错误（exit 2），
//!    反向迁移（含往返一致性）属 D2。
//!
//! 正向五步（设计 §3）：
//!
//! ```text
//! 0) 残留清理     staging(+sidecar)、marker tmp、明文主库上的旧备份
//! 1) 一致性快照   wal_checkpoint(TRUNCATE) 断言 busy=0 → integrity_check=ok
//!                 → VACUUM INTO store.db.plaintext-backup → fsync
//! 2) 导出加密库   ATTACH … AS encrypted KEY ?（绑定参数）→ sqlcipher_export
//!                 → 补 user_version/application_id → DETACH → fsync
//! 3) 校验矩阵     独立 keyed 连接对 staging 逐项比对（§4），不过不替换
//! 4) 替换         remove_sidecars(store.db) → rename(staging → store.db)
//!                 → fsync(目录) → store.format（tmp + rename）
//! 5) 保留窗口     .plaintext-backup 保留 7 天；**只在主库已加密且可打开时**
//!                 才按 mtime 清理（主库状态不明 → 永不删备份）
//! ```
//!
//! 崩溃断点（设计 §5.4，B0–B5）全部靠「头分类 + 先删 staging 再重做」收敛，
//! 绝不接管半成品 staging（没有提交标记就无法证明它完整）。debug 构建另有
//! `KT_ENGINE_STORE_MIGRATE_FAULT=after_backup|after_export|after_sidecar|
//! after_rename|before_marker` 五个确定性断点（release 编译掉），供
//! `tests/store_migrate.rs` 的真进程用例注入。
//!
//! 另有两条**注入钩子**（同样只在 debug 构建，release 编译掉）：
//! `corrupt_staging_rows` 在导出后改 staging 行数（验 §4/D-19 的失败清理），
//! `before_swap` 在 swap 前制造「外部读者 + 后写帧」（验裁决 #10 的 pre-swap
//! checkpoint）。

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row, SqliteConnection};

use crate::state::{ACCOUNTS_DIR, STORE_DB_FILE};
use crate::store_cipher;
use crate::store_key::{StoreKey, StoreKeyState};
use crate::store_open::{self, StoreFileState};

// —————————————————————————— 契约常量（连接器唯一可编程面） ——————————————————————————

/// 成功 / 无事可做。
pub(crate) const EXIT_OK: u8 = 0;
/// 用法错误（集成 bug，不重试）。
pub(crate) const EXIT_USAGE: u8 = 2;
/// 可重试：空间不足 / 库被占用 / 瞬时 I/O（退避重试 ≤3 次）。
pub(crate) const EXIT_RETRY: u8 = 10;
/// 终态：key 缺失/不匹配、源库损坏、校验不过、权限拒绝、账号目录消失（等人）。
pub(crate) const EXIT_TERMINAL: u8 = 11;

/// 明文备份后缀（与 connector 同源 `src/store.rs:62`）。
pub(crate) const BACKUP_SUFFIX: &str = ".plaintext-backup";
/// 加密 staging 后缀（connector 同源 `:64`）。
pub(crate) const STAGING_SUFFIX: &str = ".encrypting";
/// 审计 marker（**不参与正确性判断**，设计 §5.2）。
pub(crate) const MARKER_FILE: &str = "store.format";
/// data-dir 级迁移锁（0600，非阻塞 flock；设计 §5.1）。
pub(crate) const LOCK_FILE: &str = "store-migrate.lock";
/// 明文备份保留期（7 天；同时是「密钥事故」的补救窗口，设计 §3.5）。
const BACKUP_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// 空间预检的额外余量（32 MiB）与放大系数（2.1×L，设计 §9 D-01）。
const SPACE_SLACK_BYTES: u64 = 32 * 1024 * 1024;
/// 迁移连接与校验连接的 busy timeout（与 vendor 打开的 5s 同口径）。
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// 语义 probe 收口等待：sqlx 池的关闭是异步的，带 SQLCipher codec 的 worker
/// 线程收口需要一小段静默（C.6 已实测的退出窗口，测试面同款做法）。
const STORE_TEARDOWN_QUIET: Duration = Duration::from_millis(120);
/// staging WAL 收口重试次数（收口不了就丢弃 staging 重做，绝不在 WAL 还有
/// 内容时删 sidecar）。
const CHECKPOINT_RETRIES: usize = 3;

/// 证据码字面量（连接器与运维 grep 的唯一判据；不做前缀匹配）。
pub(crate) mod codes {
    /// 迁移成功。
    pub const MIGRATED: &str = "store_migrated";
    /// 已经是加密库 → 幂等跳过。
    pub const NOOP: &str = "store_migration_noop";
    /// 没有库 / 0 字节 → 新建是 C 的事。
    pub const ABSENT: &str = "store_migration_absent";
    /// 加密库 + key 打不开（换机/换 key/首页损坏；设计 D-10）。
    pub const KEY_REJECTED: &str = "store_key_rejected";
    /// key 与 marker 指纹不符（设计 D-09）。
    pub const KEY_MISMATCH: &str = "store_key_mismatch";
    /// 缺 key（env 未注入，设计 D-08；复用 C 的码面）。
    pub const KEY_REQUIRED: &str = "StoreKeyRequired";
    /// key 形状非法（复用 C 的码面）。
    pub const KEY_INVALID: &str = "StoreKeyInvalid";
    /// 链接的 sqlite 不是 SQLCipher（复用 C 的码面）。
    pub const CIPHER_UNAVAILABLE: &str = "StoreCipherUnavailable";
    /// 库被占用（checkpoint busy≠0 / rename EBUSY；设计 D-04/D-05/D-16）。
    pub const BUSY: &str = "store_busy";
    /// 空间不足（设计 D-01/D-02/D-03）。
    pub const NO_SPACE: &str = "store_migrate_no_space";
    /// 另一个迁移器持锁（设计 D-17）。
    pub const LOCKED: &str = "store_migrate_locked";
    /// 校验矩阵不过（设计 D-19）。
    pub const VERIFY_FAILED: &str = "store_migration_verify_failed";
    /// 源库损坏（设计 D-11）。
    pub const CORRUPT: &str = "store_corrupt";
    /// 账号目录/库在迁移途中消失（设计 D-18）。
    pub const ACCOUNT_VANISHED: &str = "account_vanished";
    /// 瞬时 I/O 失败（可重试）。
    pub const IO_FAILED: &str = "store_migration_io_failed";
    /// 其它迁移失败（兜底）。
    pub const FAILED: &str = "store_migration_failed";
    /// data-dir 不存在（路径打错 / 槽还没建）：终态，**绝不代建目录**。
    pub const DATA_DIR_MISSING: &str = "store_migrate_data_dir_missing";
    /// 干跑：只出计划，一个字节都没写（`action=dry-run`）。
    pub const PLANNED: &str = "store_migration_planned";
    /// 主库状态违规（设计 §5.5：头与内容自相矛盾——加密库
    /// `integrity_check` 或 `cipher_integrity_check` 不过）。
    pub const STATE_INVALID: &str = "store_state_invalid";
}

/// 正向五步的计划标签（`--json` 的 `plan[]`/`did[]` 逐项对账用）。
const STEPS: [&str; 5] = ["checkpoint", "backup", "export", "verify", "swap"];

/// `accounts[].action` 的封闭取值（设计 §2.2；连接器按字面量分桶）。
const ACTION_MIGRATE: &str = "migrate";
/// 干跑：**没有迁移发生**，只有计划（旧实现在这里报 `migrate`+备份，
/// 等于对连接器谎报一次已完成的替换）。
const ACTION_DRY_RUN: &str = "dry-run";
const ACTION_NOOP: &str = "noop";
const ACTION_ABSENT: &str = "absent";
const ACTION_FAILED: &str = "failed";

/// 解析 argv 时必须跳过其取值的选项（值可能字面等于子命令名，例如
/// `--data-dir storeMigrate`：那个 `storeMigrate` 是目录名，不是子命令）。
const VALUE_OPTIONS: [&str; 4] = ["--data-dir", "--receive-mode", "--account", "--direction"];

/// 反向迁移在本构建里的用法错误行（D2 待做）。
const DIRECTION_UNSUPPORTED_LINE: &str =
    "usage: --direction to-plaintext is not implemented in this build (phase D2)";

// —————————————————————————————— argv（严格模式） ——————————————————————————————

/// 迁移方向。D1 只落 `ToEncrypted`；`ToPlaintext` 保留在解析面（否则
/// `--direction to-plaintext` 会被当成未知参数，与「D2 待做」的语义混在一起），
/// 但由 `entry` 以 exit 2 明确拒绝。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    ToEncrypted,
    ToPlaintext,
}

impl Direction {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "to-encrypted" => Some(Self::ToEncrypted),
            "to-plaintext" => Some(Self::ToPlaintext),
            _ => None,
        }
    }
}

/// `storeMigrate` 子命令参数。
#[derive(Clone, Debug)]
pub(crate) struct MigrateArgs {
    pub(crate) data_dir: PathBuf,
    /// 只迁一个账号（`lk-<hex>`，目录名）。
    pub(crate) account: Option<String>,
    pub(crate) direction: Direction,
    /// 只算计划、不写一个字节。
    pub(crate) dry_run: bool,
    /// stdout 只输出一份结果 JSON（一行）。
    pub(crate) json: bool,
}

/// argv 路由的两种结局。
pub(crate) enum Route {
    /// 不是迁移模式：交给既有的 `jsonRpc` 严格解析器。
    JsonRpc,
    /// 迁移模式。
    Migrate(MigrateArgs),
}

/// 在既有 `parse_launch_args` 之前分流（`src/main.rs` 只加一处调用）。
///
/// - argv 里**没有** `storeMigrate`：返回 [`Route::JsonRpc`]（既有解析器
///   继续按原口径处理，未知参数仍 exit 2）；
/// - 有 `storeMigrate`：按迁移模式的严格白名单解析；同时出现 `jsonRpc`、
///   未知参数、缺 `--data-dir`、`--account/--direction` 缺值、`--direction`
///   取值非法，一律 `Err(())` → 调用方 exit 2；
/// - 迁移专属开关出现在 `jsonRpc` 模式（或反之）同样 `Err(())`。
pub(crate) fn route(argv: &[OsString]) -> Result<Route, ()> {
    let Some(migrate_at) = subcommand_position(argv) else {
        return Ok(Route::JsonRpc);
    };
    let mut data_dir: Option<PathBuf> = None;
    let mut account: Option<String> = None;
    let mut direction = Direction::ToEncrypted;
    let mut dry_run = false;
    let mut json = false;
    let mut index = 0_usize;
    while index < argv.len() {
        let Some(arg) = argv[index].to_str() else {
            return Err(());
        };
        match arg {
            "--data-dir" => {
                data_dir = Some(PathBuf::from(argv.get(index + 1).ok_or(())?));
                index += 2;
            }
            "--account" => {
                let name = argv.get(index + 1).ok_or(())?.to_str().ok_or(())?;
                if !is_account_name(name) {
                    return Err(());
                }
                account = Some(name.to_owned());
                index += 2;
            }
            "--direction" => {
                let raw = argv.get(index + 1).ok_or(())?.to_str().ok_or(())?;
                direction = Direction::parse(raw).ok_or(())?;
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "storeMigrate" if index == migrate_at => index += 1,
            _ => return Err(()),
        }
    }
    let Some(data_dir) = data_dir else {
        return Err(());
    };
    Ok(Route::Migrate(MigrateArgs {
        data_dir,
        account,
        direction,
        dry_run,
        json,
    }))
}

/// 子命令名在 argv 里的**非取值位置**（这是 argv 误吞的唯一防线）：
/// `--data-dir storeMigrate jsonRpc` 里的 `storeMigrate` 是目录值，不是子命令，
/// 必须让既有的 jsonRpc 解析器接手；否则一个叫 `storeMigrate` 的 data-dir
/// 会让引擎以迁移模式启动（甚至 exit 2）。
fn subcommand_position(argv: &[OsString]) -> Option<usize> {
    let mut index = 0_usize;
    while index < argv.len() {
        let arg = argv[index].to_str()?;
        if arg == "storeMigrate" {
            return Some(index);
        }
        index += if VALUE_OPTIONS.contains(&arg) { 2 } else { 1 };
    }
    None
}

/// 账号目录名形状：`lk-` + 全小写十六进制（ADR 0004 的链接槽命名）。
fn is_account_name(name: &str) -> bool {
    match name.strip_prefix("lk-") {
        Some(rest) if !rest.is_empty() => rest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        _ => false,
    }
}

// —————————————————————————————— 入口与报告 ——————————————————————————————

/// 迁移命令入口（`src/main.rs` 在**任何磁盘动作之前**调用：能力门是本入口
/// 的第一个动作，见模块头不变量 1）。
pub(crate) async fn entry(args: MigrateArgs) -> ExitCode {
    if args.direction == Direction::ToPlaintext {
        // D1 只做正向；反向迁移（含往返一致性）属 D2。
        eprintln!("kt-signal-engine: {DIRECTION_UNSUPPORTED_LINE}");
        return ExitCode::from(EXIT_USAGE);
    }
    // 1) 能力门：硬门，且**早于任何磁盘动作**（含 logs/ 的创建）。
    if store_cipher::ensure_cipher_capable().await.is_err() {
        let report = MigrateReport {
            accounts: Vec::new(),
            free_bytes: None,
            need_bytes: None,
            dry_run: args.dry_run,
            error: Some(Failure::terminal(
                codes::CIPHER_UNAVAILABLE,
                "linked sqlite is not a SQLCipher build",
            )),
        };
        emit(&args, &report);
        return ExitCode::from(report.exit_code());
    }
    // 2) 滚动日志（证据行落 `<data-dir>/logs/engine-stderr.log`，不写 stderr）。
    //    数据目录不存在时连 logs/ 都不建：没有库就没有可迁对象。
    if args.data_dir.is_dir() {
        crate::logfile::init(&args.data_dir);
    }
    let report = run(
        args.data_dir.clone(),
        &args,
        StoreKeyState::process(),
        &Hooks::default(),
    )
    .await;
    emit(&args, &report);
    ExitCode::from(report.exit_code())
}

/// 一次运行的汇总（`--json` 契约的唯一来源）。
struct MigrateReport {
    accounts: Vec<AccountOutcome>,
    free_bytes: Option<u64>,
    need_bytes: Option<u64>,
    dry_run: bool,
    error: Option<Failure>,
}

impl MigrateReport {
    /// 0 / 10 / 11（设计 §2.4、§5.1：有任一终态失败则整体 11；否则有可重试
    /// 失败则 10；全绿 0）。
    fn exit_code(&self) -> u8 {
        if let Some(error) = &self.error {
            return if error.terminal {
                EXIT_TERMINAL
            } else {
                EXIT_RETRY
            };
        }
        let mut code = EXIT_OK;
        for account in &self.accounts {
            if account.terminal {
                return EXIT_TERMINAL;
            }
            if account.action == "failed" {
                code = EXIT_RETRY;
            }
        }
        code
    }

    /// 单测断言用（生产面只暴露 exit code 与 JSON）。
    #[cfg(test)]
    fn error_code(&self) -> Option<&'static str> {
        self.error.as_ref().map(|error| error.code)
    }

    /// 跨账号的步骤并集（规范顺序 = [`STEPS`]）。
    fn step_union(
        &self,
        pick: impl Fn(&AccountOutcome) -> &Vec<&'static str>,
    ) -> Vec<&'static str> {
        STEPS
            .iter()
            .copied()
            .filter(|step| {
                self.accounts
                    .iter()
                    .any(|account| pick(account).contains(step))
            })
            .collect()
    }

    fn to_json(&self) -> Value {
        let accounts: Vec<Value> = self
            .accounts
            .iter()
            .map(|account| {
                json!({
                    "lk": account.lk,
                    "action": account.action,
                    "code": account.code,
                    "bytes": account.bytes,
                    "elapsedMs": account.elapsed_ms,
                    "keyid": account.keyid,
                    "backup": account.backup,
                    "checks": account.checks,
                })
            })
            .collect();
        // 设计 §2.2/§10.2：`plan[]/did[]` 是**步骤名数组**（不是按账号分组的
        // 对象数组）。多账号时取并集，并按 STEPS 的规范顺序输出——连接器按
        // 字面量逐项对账，顺序抖动等于误报。
        let plan = self.step_union(|account| &account.plan);
        let did = self.step_union(|account| &account.did);
        let mut value = json!({
            "ok": self.exit_code() == EXIT_OK,
            "dryRun": self.dry_run,
            "freeBytes": self.free_bytes,
            "needBytes": self.need_bytes,
            "accounts": accounts,
            "plan": plan,
            "did": did,
        });
        if let Some(error) = &self.error {
            value["error"] = json!({
                "code": error.code,
                "retryable": !error.terminal,
            });
        }
        value
    }
}

/// 单账号结果。
struct AccountOutcome {
    lk: String,
    /// `migrate` / `noop` / `absent` / `failed`。
    action: &'static str,
    code: &'static str,
    bytes: Option<u64>,
    elapsed_ms: u128,
    keyid: Option<String>,
    backup: Option<String>,
    /// 终态失败（整体 exit 11 的判据）。
    terminal: bool,
    plan: Vec<&'static str>,
    did: Vec<&'static str>,
    checks: Vec<&'static str>,
    free_bytes: Option<u64>,
    need_bytes: Option<u64>,
}

impl AccountOutcome {
    fn new(lk: &str, started: Instant) -> Self {
        Self {
            lk: lk.to_owned(),
            action: "noop",
            code: codes::ABSENT,
            bytes: None,
            elapsed_ms: started.elapsed().as_millis(),
            keyid: None,
            backup: None,
            terminal: false,
            plan: Vec::new(),
            did: Vec::new(),
            checks: Vec::new(),
            free_bytes: None,
            need_bytes: None,
        }
    }
}

/// 失败：码 + 是否终态 + 只进日志的明细（不含 key / 号码）。
///
/// Debug 派生只给测试断言用（生产面从不 Debug 打印它：detail 可能含错误文本）。
#[derive(Debug)]
struct Failure {
    code: &'static str,
    terminal: bool,
    detail: String,
}

impl Failure {
    fn retryable(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            terminal: false,
            detail: detail.into(),
        }
    }

    fn terminal(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            terminal: true,
            detail: detail.into(),
        }
    }

    /// errno → 退出码（设计 §9：D-15 权限/只读 = **10**（裁决 #14，与 C 同
    /// 口径）、D-16 EBUSY/共享冲突 = 10，D-18 ENOENT = 11 `account_vanished`）。
    fn io(error: &io::Error, retry_code: &'static str) -> Self {
        use io::ErrorKind;
        match error.kind() {
            ErrorKind::NotFound => Self::terminal(codes::ACCOUNT_VANISHED, error.to_string()),
            // 权限/只读按「可重试」分桶：灰度期这类失败多数是槽还没 chown 好、
            // 挂载还没就位，退避重试能自愈；归终态会把槽误标成 migration_blocked。
            ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem => {
                Self::retryable(codes::IO_FAILED, error.to_string())
            }
            ErrorKind::StorageFull => Self::retryable(codes::NO_SPACE, error.to_string()),
            _ => Self::retryable(retry_code, error.to_string()),
        }
    }
}

/// 测试注入点（生产恒为默认值：真实磁盘读数、真实流程）。
#[derive(Default)]
struct Hooks {
    #[cfg(test)]
    free_bytes: Option<u64>,
}

impl Hooks {
    fn free_bytes(&self) -> Option<u64> {
        #[cfg(test)]
        {
            self.free_bytes
        }
        #[cfg(not(test))]
        {
            None
        }
    }
}

/// 迁移上下文（一次运行的不变量：key / keyid / fp / 账号目录）。
struct Context<'a> {
    args: &'a MigrateArgs,
    accounts_dir: PathBuf,
    key: &'a StoreKey,
    keyid: String,
    fingerprint: String,
    hooks: &'a Hooks,
}

// —————————————————————————————— 主流程 ——————————————————————————————

async fn run(
    data_dir: PathBuf,
    args: &MigrateArgs,
    key_state: &StoreKeyState,
    hooks: &Hooks,
) -> MigrateReport {
    let mut report = MigrateReport {
        accounts: Vec::new(),
        free_bytes: None,
        need_bytes: None,
        dry_run: args.dry_run,
        error: None,
    };
    // 前置 P2：key 可得（缺席/形状非法 → 终态 11，**零写入**，连锁都不建）。
    let key = match key_state {
        StoreKeyState::Present(key) => key,
        StoreKeyState::Absent => {
            report.error = Some(Failure::terminal(
                codes::KEY_REQUIRED,
                "KT_SIGNAL_STORE_KEY is not set",
            ));
            return report;
        }
        StoreKeyState::Invalid => {
            report.error = Some(Failure::terminal(
                codes::KEY_INVALID,
                "KT_SIGNAL_STORE_KEY is not a valid 32-byte key",
            ));
            return report;
        }
    };
    if !data_dir.is_dir() {
        // 没有 data-dir 就没有账号，但**这不是「无事可做」**：路径打错、槽还
        // 没建、data-dir 被挂载丢了，全都长得和「迁移成功」一样（旧行为
        // ok:true/exit 0 会把集成错误吃成绿灯）。终态 11 + 专用码，且绝不
        // 代建目录（建一个空 data-dir 会让连接器以为槽是干净的）。
        report.error = Some(Failure::terminal(
            codes::DATA_DIR_MISSING,
            "data-dir does not exist",
        ));
        return report;
    }
    let accounts_dir = data_dir.join(ACCOUNTS_DIR);
    // 前置 P3：data-dir 级非阻塞 flock（取不到 = 另一个迁移器在跑 → 10）。
    let _lock = match acquire_lock(&data_dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            report.error = Some(Failure::retryable(
                codes::LOCKED,
                "another storeMigrate holds the data-dir lock",
            ));
            return report;
        }
        Err(failure) => {
            report.error = Some(failure);
            return report;
        }
    };
    let context = Context {
        args,
        accounts_dir: accounts_dir.clone(),
        key,
        keyid: key_digest(b"kt-store-keyid|", key),
        fingerprint: key_digest(b"kt-store-fp|", key),
        hooks,
    };
    let accounts = match enumerate_accounts(&accounts_dir, args.account.as_deref()) {
        Ok(accounts) => accounts,
        Err(failure) => {
            report.error = Some(failure);
            return report;
        }
    };
    if accounts.is_empty() && args.account.is_some() {
        report.error = Some(Failure::terminal(
            codes::ACCOUNT_VANISHED,
            "requested account directory does not exist",
        ));
        return report;
    }
    for lk in accounts {
        let outcome = migrate_account(&context, &lk).await;
        emit_evidence(&outcome);
        if report.free_bytes.is_none() {
            report.free_bytes = outcome.free_bytes;
        }
        if report.need_bytes.is_none() {
            report.need_bytes = outcome.need_bytes;
        }
        report.accounts.push(outcome);
    }
    report
}

/// 枚举待迁账号：`accounts/lk-*` 目录，按名字排序（**串行**，同一时刻只持
/// 有一个库连接）。`accounts/_quarantine/` 是取证区，永不参与迁移。
fn enumerate_accounts(accounts_dir: &Path, only: Option<&str>) -> Result<Vec<String>, Failure> {
    if let Some(only) = only {
        return Ok(if accounts_dir.join(only).is_dir() {
            vec![only.to_owned()]
        } else {
            Vec::new()
        });
    }
    let entries = match fs::read_dir(accounts_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Failure::io(&error, codes::IO_FAILED)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
        let file_type = entry
            .file_type()
            .map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with("lk-") {
            // `_quarantine` 与其它非账号目录：不在迭代集合里（设计 §5.1）。
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// 单账号迁移（三种输入 → 三种结局，设计 §5.3）。
async fn migrate_account(context: &Context<'_>, lk: &str) -> AccountOutcome {
    let started = Instant::now();
    let mut outcome = AccountOutcome::new(lk, started);
    outcome.keyid = Some(context.keyid.clone());
    let dir = context.accounts_dir.join(lk);
    let store = dir.join(STORE_DB_FILE);
    // 权威分类：只看文件头（16B）+ 长度。
    let state = match store_open::classify(&store) {
        Ok(state) => state,
        Err(error) => return fail_outcome(outcome, classify_open_error(error)),
    };
    match state {
        StoreFileState::Absent => {
            outcome.action = ACTION_ABSENT;
            outcome.code = codes::ABSENT;
            outcome
        }
        // 干跑对已加密库只做分类（不做 key 验真：验真会开连接、落 sidecar，
        // 与「不写一个字节」冲突；干跑是计划工具，不是诊断工具）。
        StoreFileState::Opaque if context.args.dry_run => {
            outcome.action = ACTION_NOOP;
            outcome.code = codes::NOOP;
            outcome
        }
        StoreFileState::Opaque => settle_encrypted(context, outcome, &dir, &store).await,
        StoreFileState::Plaintext => {
            let dry_run = context.args.dry_run;
            match migrate_plaintext(context, &mut outcome, &dir, &store).await {
                Ok(()) if dry_run => {
                    // 干跑：只有计划。action/code/backup 一个都不许报成「已迁移」
                    // ——连接器按 action 分桶，谎报 backup 会让它以为手里有回滚物。
                    outcome.action = ACTION_DRY_RUN;
                    outcome.code = codes::PLANNED;
                    outcome.backup = None;
                    outcome
                }
                Ok(()) => {
                    outcome.action = ACTION_MIGRATE;
                    outcome.code = codes::MIGRATED;
                    outcome.bytes = fs::metadata(&store).ok().map(|meta| meta.len());
                    outcome.backup = Some(format!("{STORE_DB_FILE}{BACKUP_SUFFIX}"));
                    outcome
                }
                Err(failure) => fail_outcome(outcome, failure),
            }
        }
    }
}

/// 已加密（或垃圾）库的收敛分支：key 打得开 → 幂等 no-op（补 marker、
/// 清 marker tmp）；打不开 → 终态 `store_key_rejected`；指纹不符 → 终态
/// `store_key_mismatch`（设计 D-09/D-10）。
async fn settle_encrypted(
    context: &Context<'_>,
    mut outcome: AccountOutcome,
    dir: &Path,
    store: &Path,
) -> AccountOutcome {
    outcome.action = ACTION_NOOP;
    outcome.code = codes::NOOP;
    // B5：marker 的 tmp 残留一律可抛弃。
    remove_marker_tmp(dir);
    if let Err(failure) = store_open::probe_keyed_store(store, context.key).await {
        return fail_outcome(outcome, classify_open_error(failure));
    }
    // 设计 §5.5 的谓词：头≠明文 ⇒ 必须能 key 打开**且** `integrity_check=ok`
    // **且** `cipher_integrity_check` 空。probe 只看首页，页 HMAC/结构坏了它
    // 照样返回 Ok——旧实现于是把「打得开但已损坏」的库报成 noop/exit 0（第三
    // 态被当成绿）。这里补齐后两层，坏了就报 `store_state_invalid` 保留现场。
    if let Err(failure) = verify_encrypted_integrity(store, context.key).await {
        return fail_outcome(outcome, failure);
    }
    outcome
        .checks
        .extend(["store_integrity_check", "store_cipher_integrity_check"]);
    // 验真连接是读写连接（WAL 下会建 -wal/-shm），且 sqlx 的关闭是异步的：
    // 等一小段静默把空 sidecar 回收掉，别让一次「零字节改动」的幂等跳过多出
    // 两个文件（非空 WAL 绝不动——那里面有已提交帧）。
    tokio::time::sleep(STORE_TEARDOWN_QUIET).await;
    remove_empty_sidecars(store);
    // 指纹比对（marker 只作证据：缺 marker 只是少一条诊断；存在且不符 =
    // 有人换过 key，绝不打开/重建）。
    if let Some(recorded) = read_marker_fingerprint(dir) {
        if recorded != context.fingerprint {
            return fail_outcome(
                outcome,
                Failure::terminal(
                    codes::KEY_MISMATCH,
                    "store key does not match the fingerprint recorded in store.format",
                ),
            );
        }
    }
    // B4：rename 与 marker 之间的窗口——库已加密可打开，补写 marker（审计）。
    if !dir.join(MARKER_FILE).is_file() {
        if let Err(failure) = write_marker(dir, &context.keyid, &context.fingerprint) {
            return fail_outcome(outcome, failure);
        }
        outcome.did.push("marker");
    }
    // 保留窗口：**只有主库被判为加密且可打开**时才允许按 mtime 清理备份。
    if prune_expired_backup(dir).is_some() {
        outcome.did.push("prune_backup");
    }
    outcome
}

/// 正向五步（明文 → 加密）。
async fn migrate_plaintext(
    context: &Context<'_>,
    outcome: &mut AccountOutcome,
    dir: &Path,
    store: &Path,
) -> Result<(), Failure> {
    let backup = dir.join(format!("{STORE_DB_FILE}{BACKUP_SUFFIX}"));
    let staging = dir.join(format!("{STORE_DB_FILE}{STAGING_SUFFIX}"));
    outcome.plan = STEPS.to_vec();
    outcome.bytes = fs::metadata(store).ok().map(|meta| meta.len());
    // 前置 P6（干跑也做）：空间 = L×2.1 + 32 MiB，L = store.db + -wal。
    let need = space_need(store_sizes(store));
    let free = free_bytes(dir, context.hooks)?;
    outcome.free_bytes = Some(free);
    outcome.need_bytes = Some(need);
    if free < need {
        return Err(Failure::retryable(
            codes::NO_SPACE,
            format!("free={free} need={need}"),
        ));
    }
    if context.args.dry_run {
        // 干跑：只读元数据 + 头分类 + 空间读数，不打开库、不写一个字节。
        return Ok(());
    }

    // 0) 残留清理（B0/B1/B2/B3 的收敛入口）：staging + 其 sidecar、marker
    //    tmp。明文主库 = 上一次迁移没完成，那种备份不可能是任何「完整迁移」
    //    的回滚物（真正回滚物是明文主库本身），所以它占着 VACUUM INTO 的目标
    //    路径时必须先删（设计 §5.4 B1）。
    remove_staging(&staging)?;
    remove_marker_tmp(dir);
    remove_file_if_exists(&backup)?;

    // 1) 一致性快照：无 key 打开（create_if_missing=false，偏离 D-1）。
    let mut source = connect_plain(store).await?;
    let checkpoint = checkpoint_truncate(&mut source).await?;
    if checkpoint.busy != 0 {
        return Err(Failure::retryable(
            codes::BUSY,
            format!("wal_checkpoint busy={}", checkpoint.busy),
        ));
    }
    outcome.did.push("checkpoint");
    // 前置 P5：源库健康（不过就不迁移——否则备份会被后续清理带走）。
    if !integrity_ok(&mut source).await? {
        return Err(Failure::terminal(
            codes::CORRUPT,
            "source PRAGMA integrity_check is not ok",
        ));
    }
    // 源库事实快照（schema 集合 / 行数 / 账本 / 序列 / 头字段 / 大表抽样）。
    let source_stats = collect_stats(&mut source).await?;
    // 注册事实也要在**源侧**取真值（设计 §4 的语义行是「Some 且 ACI/设备名
    // 与迁移前一致」——只验证 staging 自己能读出 Some 是空转）。
    let source_registration = read_registration_facts(&mut source).await?;

    // 2) 明文备份：VACUUM INTO（目标已存在会失败，第 0 步已删）。
    let backup_path = backup
        .to_str()
        .ok_or_else(|| Failure::terminal(codes::FAILED, "backup path is not valid UTF-8"))?;
    sqlx::query("VACUUM INTO ?")
        .bind(backup_path)
        .execute(&mut source)
        .await
        .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
    fsync_file(&backup).map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
    fsync_dir(dir).map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
    // VACUUM 的返回值本身不足以证明备份可用：SQLite 内核在个别页布局上会让
    // step 失败而 sqlx 仍把 execute 报成 Ok（本机复现：系统 sqlite3 3.54 的
    // 12B reserved 页布局 → worker step MISUSE、目标文件 0 字节）。备份是唯一
    // 回滚物，必须显式验「非空 + 明文头」；不过就删掉半个备份并退避重试
    // （设计 D-02 的纪律：不把半成品留给下一次）。
    ensure_plaintext_backup(&backup)?;
    outcome.did.push("backup");
    fault_point("after_backup");

    // 3) 导出加密库：ATTACH … KEY ?（绑定参数，绝不拼 SQL）+ sqlcipher_export。
    let staging_path = staging
        .to_str()
        .ok_or_else(|| Failure::terminal(codes::FAILED, "staging path is not valid UTF-8"))?;
    // ATTACH 的建文件权限跟随主库连接（SQLite ≥3.21：主库若不是 CREATE 打开
    // 的，ATTACH 不会建新文件 → SQLITE_CANTOPEN）。D-1 要求源库连接
    // create_if_missing(false)，所以 staging 先由本进程建一个**空文件**，再交给
    // ATTACH + KEY 写 SQLCipher 头（空库正是 SQLCipher 生成随机盐的形态）。
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
    attach_export(&mut source, staging_path, context.key, &source_stats).await?;
    close(source).await?;
    fsync_file(&staging).map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
    fsync_dir(dir).map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
    outcome.did.push("export");
    fault_point("after_export");
    fault_mutate_staging(&staging, context.key).await;

    // 4) 校验矩阵（fail → 删 staging，保留原库与备份；设计 §4/D-19）。
    outcome.checks = verify_staging(context, &staging, &source_stats, &source_registration).await?;
    outcome.did.push("verify");
    // 语义 probe 会以引擎真实打开路径跑一次 schema 迁移，因此排在结构比对
    // **之后**；跑完必须把 WAL 收口再删 sidecar，否则可能丢最后几页。
    checkpoint_staging(&staging, context.key).await?;
    remove_sidecars(&staging);

    // 5) 替换（顺序写死，任何时刻 store.db 要么完整明文、要么完整密文）。
    // 删 sidecar 之前**再 checkpoint 一次源库**：`remove_sidecars` 会无条件删
    // -wal/-shm/-journal，此刻 WAL 里若还有已提交帧，删掉就是静默丢数据。先
    // TRUNCATE 一次并断言 busy=0 且 WAL 已空：折得进去才安全删；折不进去
    // （长读事务 / 外部写者）报可重试 10，现场一个字节都不动。
    fault_inject_foreign_writer(store).await;
    checkpoint_source_quiet(store).await?;
    remove_sidecars(store);
    fault_point("after_sidecar");
    fs::rename(&staging, store).map_err(|error| Failure::io(&error, codes::BUSY))?;
    fsync_dir(dir).map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
    outcome.did.push("swap");
    fault_point("after_rename");
    fault_point("before_marker");
    write_marker(dir, &context.keyid, &context.fingerprint)?;
    outcome.did.push("marker");
    Ok(())
}

// —————————————————————————————— 五个动作的细节 ——————————————————————————————

/// `PRAGMA wal_checkpoint(TRUNCATE)` 的结果三列（偏离 D-2：参考实现把 busy
/// 丢掉了，这里必须读回来——「库被占用」的唯一廉价信号）。
struct Checkpoint {
    busy: i64,
}

async fn checkpoint_truncate(connection: &mut SqliteConnection) -> Result<Checkpoint, Failure> {
    let row = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .fetch_optional(&mut *connection)
        .await
        .map_err(|error| sql_failure(error, codes::BUSY))?;
    let Some(row) = row else {
        // 非 WAL 模式时 SQLite 回 (0,-1,-1)；没有行同样等价于「没有待截断
        // 的 WAL」，不把它当失败。
        return Ok(Checkpoint { busy: 0 });
    };
    let busy: i64 = row.try_get(0).unwrap_or(0);
    Ok(Checkpoint { busy })
}

async fn integrity_ok(connection: &mut SqliteConnection) -> Result<bool, Failure> {
    let rows: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut *connection)
        .await
        .map_err(|error| sql_failure(error, codes::CORRUPT))?;
    Ok(!rows.is_empty() && rows.iter().all(|row| row == "ok"))
}

/// 无 key 打开源库：`create_if_missing(false)`（偏离 D-1——现成选项会在路径
/// 打错时**悄悄建一个空库**，然后把它当源库导出）。journal mode 跟随文件头
/// （不在源库上写任何 pragma，避免迁移前的多余字节改动）。
async fn connect_plain(path: &Path) -> Result<SqliteConnection, Failure> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT)
        .pragma("cache_size", "-1024");
    SqliteConnection::connect_with(&options)
        .await
        .map_err(|error| classify_connect_error(&error))
}

/// 带 key 打开（staging 校验 / WAL 收口用）。
async fn connect_keyed(path: &Path, key: &StoreKey) -> Result<SqliteConnection, Failure> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT)
        .pragma("key", key.pragma_literal());
    SqliteConnection::connect_with(&options)
        .await
        .map_err(|error| classify_connect_error(&error))
}

async fn close(connection: SqliteConnection) -> Result<(), Failure> {
    connection
        .close()
        .await
        .map_err(|error| sql_failure(error, codes::IO_FAILED))
}

/// ATTACH + `sqlcipher_export` + 头字段补齐 + DETACH（设计 §3.2）。
///
/// 路径与 key 规格都用**绑定参数**（绝不拼 SQL）：key 的值就是
/// `pragma_literal()` 去掉外层双引号后的 raw key 形态 `x'<64hex>'`——PRAGMA
/// 路径上那对双引号是 sqlx 的渲染要求（SQL 语法），不是 key 值本身。
async fn attach_export(
    connection: &mut SqliteConnection,
    staging_path: &str,
    key: &StoreKey,
    source: &StoreStats,
) -> Result<(), Failure> {
    let literal = key.pragma_literal();
    let attach_key = literal.trim_matches('"').to_owned();
    sqlx::query("ATTACH DATABASE ? AS encrypted KEY ?")
        .bind(staging_path)
        .bind(attach_key)
        .execute(&mut *connection)
        .await
        .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
    sqlx::query("SELECT sqlcipher_export('encrypted')")
        .execute(&mut *connection)
        .await
        .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
    // user_version / application_id 是文件头字段，不在 sqlcipher_export 的
    // 覆盖面里（设计 §3.2 第 3 条）——必须显式补齐。
    for (pragma, value) in [
        ("user_version", source.user_version),
        ("application_id", source.application_id),
    ] {
        let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new("PRAGMA encrypted.");
        builder.push(pragma).push(" = ").push(value);
        builder
            .build()
            .execute(&mut *connection)
            .await
            .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
    }
    sqlx::query("DETACH DATABASE encrypted")
        .execute(&mut *connection)
        .await
        .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
    Ok(())
}

// —————————————————————————————— 校验矩阵（§4） ——————————————————————————————

/// 源库事实（迁移前后逐项比对；**不写死表名**——10 个迁移已增删过表）。
struct StoreStats {
    schema: Vec<String>,
    row_counts: Vec<(String, i64)>,
    ledger: Option<Vec<(i64, Vec<u8>, bool)>>,
    sequence: Option<Vec<(String, i64)>>,
    user_version: i64,
    application_id: i64,
    thread_messages: Option<(i64, i64, i64, i64)>,
}

async fn collect_stats(connection: &mut SqliteConnection) -> Result<StoreStats, Failure> {
    let schema: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
    let mut row_counts = Vec::with_capacity(schema.len());
    for table in &schema {
        // 表名来自 sqlite_master（不是用户输入），仍按标识符转义后再拼接：
        // 动态 SQL 一律走 QueryBuilder（sqlx 0.9 对裸 String 有注入审计门）。
        let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT COUNT(*) FROM ");
        builder.push(quote_identifier(table));
        let count: i64 = builder
            .build_query_scalar()
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
        row_counts.push((table.clone(), count));
    }
    let has = |name: &str| schema.iter().any(|table| table == name);
    let ledger = if has("_sqlx_migrations") {
        let rows =
            sqlx::query("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&mut *connection)
                .await
                .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
        let mut ledger = Vec::with_capacity(rows.len());
        for row in rows {
            let version: i64 = row.try_get("version").unwrap_or_default();
            let checksum: Vec<u8> = row.try_get("checksum").unwrap_or_default();
            let success: bool = row.try_get("success").unwrap_or_default();
            ledger.push((version, checksum, success));
        }
        Some(ledger)
    } else {
        None
    };
    let sequence = if has("sqlite_sequence") {
        let rows = sqlx::query("SELECT name, seq FROM sqlite_sequence ORDER BY name")
            .fetch_all(&mut *connection)
            .await
            .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
        let mut sequence = Vec::with_capacity(rows.len());
        for row in rows {
            let name: String = row.try_get("name").unwrap_or_default();
            let seq: i64 = row.try_get("seq").unwrap_or_default();
            sequence.push((name, seq));
        }
        Some(sequence)
    } else {
        None
    };
    let user_version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
    let application_id: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
    let thread_messages = if has("thread_messages") {
        let row = sqlx::query(
            "SELECT COUNT(*), COALESCE(MIN(rowid), 0), COALESCE(MAX(rowid), 0), COALESCE(SUM(LENGTH(content_body)), 0) FROM thread_messages",
        )
        .fetch_one(&mut *connection)
        .await
        .map_err(|error| sql_failure(error, codes::IO_FAILED))?;
        Some((
            row.try_get(0).unwrap_or_default(),
            row.try_get(1).unwrap_or_default(),
            row.try_get(2).unwrap_or_default(),
            row.try_get(3).unwrap_or_default(),
        ))
    } else {
        None
    };
    Ok(StoreStats {
        schema,
        row_counts,
        ledger,
        sequence,
        user_version,
        application_id,
        thread_messages,
    })
}

/// 校验矩阵（设计 §4）：任一项不过 → 删 staging + sidecar，保留原库与备份，
/// exit 11，**不替换**。
async fn verify_staging(
    context: &Context<'_>,
    staging: &Path,
    source: &StoreStats,
    registration: &Option<RegistrationFacts>,
) -> Result<Vec<&'static str>, Failure> {
    let mut checks = Vec::new();
    let discard = |checks: Vec<&'static str>, detail: String| {
        remove_staging(staging).ok();
        Failure::terminal(codes::VERIFY_FAILED, format!("{detail}; passed={checks:?}"))
    };
    // 头：staging 前 16 字节必须不是明文头（否则 `PRAGMA key` 是静默 no-op
    // 或 ATTACH 没带 KEY）。
    match store_open::classify(staging) {
        Ok(StoreFileState::Opaque) => checks.push("staging_header_not_plaintext"),
        other => {
            return Err(discard(
                checks,
                format!("staging header check failed: {other:?}"),
            ))
        }
    }
    // 负例：无 key 连接必须打不开（明文伪装成加密 → 直接中止）。
    if !no_key_open_fails(staging).await {
        return Err(discard(
            checks,
            "staging opens without a key (not encrypted)".to_owned(),
        ));
    }
    checks.push("staging_rejects_no_key_open");
    // 加密库先验真（与打开路径同一个函数，错 key 归一 StoreKeyRejected）。
    if let Err(failure) = store_open::probe_keyed_store(staging, context.key).await {
        return Err(discard(
            checks,
            format!("staging key probe failed: {}", failure.code()),
        ));
    }
    checks.push("staging_opens_with_key");
    let mut keyed = match connect_keyed(staging, context.key).await {
        Ok(connection) => connection,
        Err(failure) => {
            return Err(discard(
                checks,
                format!("staging open failed: {}", failure.code),
            ))
        }
    };
    if !integrity_ok(&mut keyed).await.unwrap_or(false) {
        return Err(discard(
            checks,
            "staging PRAGMA integrity_check is not ok".to_owned(),
        ));
    }
    checks.push("staging_integrity_check");
    // 外键断裂（导出顺序/触发器副作用）。
    let foreign_keys: Vec<(i64, i64, String, i64)> = match sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut keyed)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|row| {
                (
                    row.try_get(0).unwrap_or_default(),
                    row.try_get(1).unwrap_or_default(),
                    row.try_get::<String, _>(2).unwrap_or_default(),
                    row.try_get(3).unwrap_or_default(),
                )
            })
            .collect(),
        Err(error) => {
            return Err(discard(
                checks,
                format!("foreign_key_check failed: {error}"),
            ))
        }
    };
    if !foreign_keys.is_empty() {
        return Err(discard(
            checks,
            "staging PRAGMA foreign_key_check is not empty".to_owned(),
        ));
    }
    checks.push("staging_foreign_key_check");
    // 页 HMAC（写坏/撕裂）。
    let cipher_rows: Vec<String> = match sqlx::query_scalar("PRAGMA cipher_integrity_check")
        .fetch_all(&mut keyed)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return Err(discard(
                checks,
                format!("cipher_integrity_check failed: {error}"),
            ))
        }
    };
    if !cipher_rows.is_empty() {
        return Err(discard(
            checks,
            "staging PRAGMA cipher_integrity_check is not empty".to_owned(),
        ));
    }
    checks.push("staging_cipher_integrity_check");
    // 结构/数据/账本/序列/头字段逐项比对。
    let staging_stats = match collect_stats(&mut keyed).await {
        Ok(stats) => stats,
        Err(failure) => {
            return Err(discard(
                checks,
                format!("staging stats failed: {}", failure.code),
            ))
        }
    };
    close(keyed).await?;
    if let Err(detail) = diff_stats(source, &staging_stats) {
        return Err(discard(checks, detail));
    }
    checks.extend([
        "schema_set_equal",
        "row_counts_equal",
        "thread_messages_sample_equal",
        "migration_ledger_equal",
        "sqlite_sequence_equal",
        "user_version_equal",
        "application_id_equal",
    ]);
    // 语义 probe：引擎真实打开路径（schema 对但 presage 反序列化失败只有这里
    // 能发现）。
    if let Err(failure) = semantic_probe(staging, context.key, registration).await {
        return Err(discard(
            checks,
            format!("semantic probe failed: {}", failure.code),
        ));
    }
    checks.push("semantic_registration_readable");
    Ok(checks)
}

/// 迁移前的注册事实（**源库侧**真值：`kv` 里那行官方落库 JSON 的 ACI 与设备名）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RegistrationFacts {
    aci: Option<String>,
    device_name: Option<String>,
}

impl RegistrationFacts {
    fn from_json(value: &Value) -> Self {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        Self {
            // ServiceIds flatten 之后 ACI 的 JSON 键名是 "uuid"（上游 serde rename）。
            aci: text("uuid"),
            device_name: text("device_name"),
        }
    }
}

/// 从源库读注册事实（**只读 SELECT**，不跑 schema 迁移、不打开 presage——
/// 迁移前的源库一个字节都不许改）。
async fn read_registration_facts(
    connection: &mut SqliteConnection,
) -> Result<Option<RegistrationFacts>, Failure> {
    let row = match sqlx::query("SELECT value FROM kv WHERE key = 'registration'")
        .fetch_optional(&mut *connection)
        .await
    {
        Ok(row) => row,
        // 更老的 schema 没有 kv 表：没有注册事实可比（staging 也就不该多出来）。
        Err(error)
            if error
                .to_string()
                .to_ascii_lowercase()
                .contains("no such table") =>
        {
            return Ok(None)
        }
        Err(error) => return Err(sql_failure(error, codes::IO_FAILED)),
    };
    let Some(row) = row else {
        return Ok(None);
    };
    // 列类型是 Blob，但官方 `save_registration_data` 写的是 TEXT（SQLite 列
    // 类型不强制）：两种存储类都要读得出来。
    let bytes = match row.try_get::<Vec<u8>, _>("value") {
        Ok(bytes) => bytes,
        Err(_) => row
            .try_get::<String, _>("value")
            .map(String::into_bytes)
            .map_err(|error| {
                Failure::terminal(
                    codes::VERIFY_FAILED,
                    format!("source registration row is unreadable: {error}"),
                )
            })?,
    };
    let json: Value = serde_json::from_slice(&bytes).map_err(|error| {
        Failure::terminal(
            codes::VERIFY_FAILED,
            format!("source registration row is not JSON: {error}"),
        )
    })?;
    Ok(Some(RegistrationFacts::from_json(&json)))
}

/// 用引擎真实打开路径（`presage_store_sqlite::SqliteStore`）打开库并读
/// `load_registration_data`（schema 对但语义坏的唯一发现路径）。
///
/// 设计 §4 的语义行是「**`Some` 且 ACI/设备名与迁移前一致**」：只要求「读得
/// 出来」是空转——一个把 `kv` 整表丢掉的导出照样返回 `Ok(None)`，旧实现把它
/// 判成通过（`registration.map(|_| ())`）。
async fn semantic_probe(
    path: &Path,
    key: &StoreKey,
    expected: &Option<RegistrationFacts>,
) -> Result<(), Failure> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .pragma("cache_size", "-1024")
        .pragma("key", key.pragma_literal());
    let store = presage_store_sqlite::SqliteStore::open_with_options(
        options,
        presage_store_sqlite::OnNewIdentity::Trust,
    )
    .await
    .map_err(|_| Failure::terminal(codes::VERIFY_FAILED, "presage store open failed on staging"))?;
    let registration = {
        use presage::store::StateStore as _;
        store.load_registration_data().await
    };
    drop(store);
    // 池的关闭是异步的：给 SQLCipher worker 一小段静默，再收口 WAL。
    tokio::time::sleep(STORE_TEARDOWN_QUIET).await;
    let registration = registration
        .map_err(|_| Failure::terminal(codes::VERIFY_FAILED, "registration data unreadable"))?;
    match (expected, registration) {
        // 源库没有注册行（未注册的链接库）：staging 也不许凭空多出一条。
        (None, None) => Ok(()),
        (None, Some(_)) => Err(Failure::terminal(
            codes::VERIFY_FAILED,
            "staging has registration data the source does not have",
        )),
        (Some(expected), None) => Err(Failure::terminal(
            codes::VERIFY_FAILED,
            format!(
                "staging lost the registration row (source aci={:?} device_name={:?})",
                expected.aci, expected.device_name
            ),
        )),
        (Some(expected), Some(data)) => {
            let got = RegistrationFacts {
                aci: Some(data.service_ids.aci.to_string()),
                device_name: data.device_name().map(str::to_owned),
            };
            if got == *expected {
                return Ok(());
            }
            Err(Failure::terminal(
                codes::VERIFY_FAILED,
                format!(
                    "staging registration mismatch: source aci={:?} device_name={:?}, staging aci={:?} device_name={:?}",
                    expected.aci, expected.device_name, got.aci, got.device_name
                ),
            ))
        }
    }
}

/// staging 上把 WAL 收口（语义 probe 以 WAL 打开过它），失败按可重试处理
/// ——绝不在 WAL 还有内容时删 sidecar。
async fn checkpoint_staging(staging: &Path, key: &StoreKey) -> Result<(), Failure> {
    for attempt in 0..CHECKPOINT_RETRIES {
        let mut connection = connect_keyed(staging, key).await?;
        let checkpoint = checkpoint_truncate(&mut connection).await?;
        close(connection).await?;
        if checkpoint.busy == 0 {
            fsync_file(staging).map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
            return Ok(());
        }
        if attempt + 1 < CHECKPOINT_RETRIES {
            tokio::time::sleep(STORE_TEARDOWN_QUIET).await;
        }
    }
    remove_staging(staging).ok();
    Err(Failure::retryable(
        codes::BUSY,
        "staging WAL could not be truncated after the semantic probe",
    ))
}

/// 加密库的完整性闸（设计 §5.5）：`integrity_check` 必须全 `ok`，
/// `cipher_integrity_check` 必须空。
///
/// 打不开是 key 的事（`probe_keyed_store` 已判），这里只判「打得开但内容坏」
/// ——页结构损坏或页 HMAC 不符都算**第三态**，必须报 `store_state_invalid`
/// 保留现场，不能一路滑到 no-op/exit 0。
async fn verify_encrypted_integrity(path: &Path, key: &StoreKey) -> Result<(), Failure> {
    let mut connection = connect_keyed(path, key).await?;
    let integrity: Result<Vec<String>, sqlx::Error> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut connection)
        .await;
    let cipher: Result<Vec<String>, sqlx::Error> =
        sqlx::query_scalar("PRAGMA cipher_integrity_check")
            .fetch_all(&mut connection)
            .await;
    close(connection).await?;
    let integrity = integrity.map_err(|error| {
        Failure::terminal(
            codes::STATE_INVALID,
            format!("integrity_check failed: {error}"),
        )
    })?;
    if integrity.is_empty() || !integrity.iter().all(|row| row == "ok") {
        return Err(Failure::terminal(
            codes::STATE_INVALID,
            format!("integrity_check is not ok: {integrity:?}"),
        ));
    }
    let cipher = cipher.map_err(|error| {
        Failure::terminal(
            codes::STATE_INVALID,
            format!("cipher_integrity_check failed: {error}"),
        )
    })?;
    if !cipher.is_empty() {
        return Err(Failure::terminal(
            codes::STATE_INVALID,
            format!("cipher_integrity_check is not empty: {cipher:?}"),
        ));
    }
    Ok(())
}

/// swap 前的源库静默闸：WAL 必须能被 TRUNCATE，且事后确实为空。
///
/// `remove_sidecars` 是「不可逆地丢弃 WAL 里的一切」：有长读事务或外部写者
/// 时做这件事等于丢已提交帧。这里把「折不进去」变成可重试错误（10）而不是
/// 删掉它——现场一个字节都不动，等占用方退出去重试。
async fn checkpoint_source_quiet(store: &Path) -> Result<(), Failure> {
    let mut connection = connect_plain(store).await?;
    let checkpoint = checkpoint_truncate(&mut connection).await;
    close(connection).await?;
    let checkpoint = checkpoint?;
    if checkpoint.busy != 0 {
        return Err(Failure::retryable(
            codes::BUSY,
            format!("pre-swap WAL checkpoint busy={}", checkpoint.busy),
        ));
    }
    let wal = sidecar_path(store, "-wal");
    match fs::metadata(&wal) {
        Ok(metadata) if metadata.len() != 0 => Err(Failure::retryable(
            codes::BUSY,
            format!("pre-swap WAL still holds {} bytes", metadata.len()),
        )),
        _ => Ok(()),
    }
}
/// 无 key 打开 staging 必须失败（明文伪装的负例）。
async fn no_key_open_fails(path: &Path) -> bool {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false);
    match SqliteConnection::connect_with(&options).await {
        Ok(mut connection) => {
            let probe = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_master")
                .fetch_one(&mut connection)
                .await;
            let _ = connection.close().await;
            probe.is_err()
        }
        Err(_) => true,
    }
}

fn diff_stats(source: &StoreStats, staging: &StoreStats) -> Result<(), String> {
    let mut mismatches: Vec<&'static str> = Vec::new();
    if source.schema != staging.schema {
        mismatches.push("schema_set");
    }
    if source.row_counts != staging.row_counts {
        mismatches.push("row_counts");
    }
    if source.thread_messages != staging.thread_messages {
        mismatches.push("thread_messages_sample");
    }
    if source.ledger != staging.ledger {
        mismatches.push("migration_ledger");
    }
    if source.sequence != staging.sequence {
        mismatches.push("sqlite_sequence");
    }
    if source.user_version != staging.user_version {
        mismatches.push("user_version");
    }
    if source.application_id != staging.application_id {
        mismatches.push("application_id");
    }
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(format!("staging mismatches: {mismatches:?}"))
    }
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// —————————————————————————————— 文件系统与预检 ——————————————————————————————

/// 非阻塞 flock：`Ok(None)` = 另一个迁移器持锁（exit 10）。
fn acquire_lock(data_dir: &Path) -> Result<Option<File>, Failure> {
    fs::create_dir_all(data_dir).map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
    let path = data_dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(Failure::io(&error, codes::IO_FAILED)),
    }
}

fn store_sizes(store: &Path) -> u64 {
    let mut total = fs::metadata(store).map(|meta| meta.len()).unwrap_or(0);
    total += fs::metadata(sidecar_path(store, "-wal"))
        .map(|meta| meta.len())
        .unwrap_or(0);
    total
}

/// 预检：空闲 ≥ L × 2.1 + 32 MiB（设计 §9 D-01）。
fn space_need(length: u64) -> u64 {
    length.saturating_mul(21) / 10 + SPACE_SLACK_BYTES
}

/// 空闲字节：unix 用 `df -k -P`，Windows 用 `fsutil volume diskfree`
/// （零新增依赖；纯读数，不写盘）。
fn free_bytes(dir: &Path, hooks: &Hooks) -> Result<u64, Failure> {
    if let Some(free) = hooks.free_bytes() {
        return Ok(free);
    }
    platform_disk_free(dir).map_err(|error| Failure::io(&error, codes::IO_FAILED))
}

#[cfg(not(windows))]
fn platform_disk_free(dir: &Path) -> io::Result<u64> {
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg("-P")
        .arg(dir)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("df exited unsuccessfully"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_df_available_kib(&text)
        .map(|kib| kib.saturating_mul(1024))
        .ok_or_else(|| io::Error::other("df output has no available column"))
}

/// `df -k -P` 的输出解析（尾行第 4 列 = Available KiB）。
fn parse_df_available_kib(text: &str) -> Option<u64> {
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let columns: Vec<&str> = line.split_whitespace().collect();
    if columns.len() < 4 {
        return None;
    }
    columns[3].parse().ok()
}

#[cfg(windows)]
fn platform_disk_free(dir: &Path) -> io::Result<u64> {
    let root = dir
        .components()
        .next()
        .map(|component| component.as_os_str().to_owned());
    let Some(root) = root else {
        return Err(io::Error::other("no drive letter"));
    };
    let output = std::process::Command::new("fsutil")
        .arg("volume")
        .arg("diskfree")
        .arg(root)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("fsutil exited unsuccessfully"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_fsutil_free_bytes(&text)
        .ok_or_else(|| io::Error::other("fsutil output has no free bytes"))
}

#[cfg(windows)]
fn parse_fsutil_free_bytes(text: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.contains("Total free bytes"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|value| value.trim().parse().ok())
}

fn sidecar_path(store: &Path, suffix: &str) -> PathBuf {
    let mut name = store.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// 只回收**空** sidecar（WAL 有内容 = 还有已提交帧，删了就是丢数据）。
fn remove_empty_sidecars(store: &Path) {
    let wal = sidecar_path(store, "-wal");
    let Ok(metadata) = fs::metadata(&wal) else {
        return;
    };
    if metadata.len() != 0 {
        return;
    }
    let _ = fs::remove_file(&wal);
    let _ = fs::remove_file(sidecar_path(store, "-shm"));
}

fn remove_sidecars(store: &Path) {
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = fs::remove_file(sidecar_path(store, suffix));
    }
}

/// 备份可用性：非空 + 明文 SQLite 头。不过关就删掉半成品并报可重试。
fn ensure_plaintext_backup(backup: &Path) -> Result<(), Failure> {
    if plaintext_backup_usable(backup) {
        return Ok(());
    }
    let _ = fs::remove_file(backup);
    Err(Failure::retryable(
        codes::IO_FAILED,
        "VACUUM INTO produced an unusable backup (empty or non-plaintext)",
    ))
}

fn plaintext_backup_usable(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if metadata.len() < 512 {
        return false;
    }
    let mut head = [0_u8; store_open::PLAINTEXT_HEADER.len()];
    match File::open(path) {
        Ok(mut file) => file.read_exact(&mut head).is_ok() && &head == store_open::PLAINTEXT_HEADER,
        Err(_) => false,
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), Failure> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Failure::io(&error, codes::IO_FAILED)),
    }
}

/// 丢弃 staging 与它的 sidecar（**绝不接管半成品**，设计 §5.4 硬规则 1）。
fn remove_staging(staging: &Path) -> Result<(), Failure> {
    remove_file_if_exists(staging)?;
    remove_sidecars(staging);
    Ok(())
}

fn remove_marker_tmp(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(MARKER_FILE) && name.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn fsync_file(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// 目录 fsync：Unix 上 `File::open(dir).sync_all()`；Windows 的目录句柄语义
/// 未验证（设计 §13.2），此处降级为 no-op 并保持可编译。
fn fsync_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

// —————————————————————————————— marker / 指纹 / 保留窗口 ——————————————————————————————

/// 域分离指纹：`sha256(domain ‖ raw key pragma 字面量)` 前 4 字节 = 8 hex。
///
/// 材料只有 [`StoreKey`] 能给（`pragma_literal()` 是该结构唯一不外泄的出口），
/// 因此哈希的是「PRAGMA 字面量」而不是裸 32 字节——它是 key 字节的确定性
/// 函数，域分离后只用于「同一次槽启动的 key 是否就是写 marker 的那把」。
fn key_digest(domain: &[u8], key: &StoreKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(key.pragma_literal().as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(8);
    for byte in &digest[..4] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `store.format`（审计 + 指纹；**永不参与正确性判断**）：
/// `magic=KTSF ver=1 state=sqlcipher-v1 keyid=<8hex> fp=<8hex> at=<unixms> tool=<ver>`。
fn write_marker(dir: &Path, keyid: &str, fingerprint: &str) -> Result<(), Failure> {
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let content = format!(
        "magic=KTSF ver=1 state=sqlcipher-v1 keyid={keyid} fp={fingerprint} at={at} tool={}\n",
        env!("CARGO_PKG_VERSION")
    );
    let tmp = dir.join(format!("{MARKER_FILE}.{}.tmp", std::process::id()));
    fs::write(&tmp, content.as_bytes()).map_err(|error| Failure::io(&error, codes::IO_FAILED))?;
    if let Err(error) = fs::rename(&tmp, dir.join(MARKER_FILE)) {
        let _ = fs::remove_file(&tmp);
        return Err(Failure::io(&error, codes::IO_FAILED));
    }
    fsync_dir(dir).map_err(|error| Failure::io(&error, codes::IO_FAILED))
}

fn read_marker_fingerprint(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(MARKER_FILE)).ok()?;
    text.split_whitespace()
        .find_map(|field| field.strip_prefix("fp="))
        .map(str::to_owned)
}

/// 7 天保留窗口：只在「主库已加密且可打开」时才调用（设计 §3.5/§5.4）。
fn prune_expired_backup(dir: &Path) -> Option<PathBuf> {
    prune_expired_backup_at(dir, SystemTime::now())
}

/// 保留窗口的可测形状（now 显式传入，单测不必改文件 mtime）。
fn prune_expired_backup_at(dir: &Path, now: SystemTime) -> Option<PathBuf> {
    let backup = dir.join(format!("{STORE_DB_FILE}{BACKUP_SUFFIX}"));
    let metadata = fs::metadata(&backup).ok()?;
    let modified = metadata.modified().ok()?;
    let age = now.duration_since(modified).ok()?;
    if age <= BACKUP_RETENTION {
        return None;
    }
    fs::remove_file(&backup).ok().map(|()| backup)
}

// —————————————————————————————— 故障判定与证据 ——————————————————————————————

fn classify_open_error(error: store_open::StoreOpenError) -> Failure {
    use store_open::StoreOpenError as Open;
    match error {
        Open::KeyRejected => Failure::terminal(codes::KEY_REJECTED, "store key rejected"),
        Open::Corrupt(_) => Failure::terminal(codes::CORRUPT, "store is corrupt"),
        Open::KeyRequired => Failure::terminal(codes::KEY_REQUIRED, "store requires a key"),
        Open::KeyInvalid => Failure::terminal(codes::KEY_INVALID, "store key shape invalid"),
        Open::CipherUnavailable => {
            Failure::terminal(codes::CIPHER_UNAVAILABLE, "linked sqlite is not SQLCipher")
        }
        Open::Failed => Failure::retryable(codes::IO_FAILED, "store classification failed"),
    }
}

/// sqlx 错误：磁盘满/被占用按可重试分桶；其余归到调用方给的码。
fn sql_failure(error: sqlx::Error, retry_code: &'static str) -> Failure {
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("full") || text.contains("no space") {
        return Failure::retryable(codes::NO_SPACE, "disk full");
    }
    if text.contains("locked") || text.contains("busy") {
        return Failure::retryable(codes::BUSY, "store is busy");
    }
    Failure::retryable(retry_code, error.to_string())
}

fn classify_connect_error(error: &sqlx::Error) -> Failure {
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("not a database") || text.contains("code 26") || text.contains("encrypted") {
        return Failure::terminal(codes::KEY_REJECTED, "store key rejected");
    }
    if text.contains("full") || text.contains("no space") {
        return Failure::retryable(codes::NO_SPACE, "disk full");
    }
    if text.contains("locked") || text.contains("busy") {
        return Failure::retryable(codes::BUSY, "store is busy");
    }
    if text.contains("permission denied") || text.contains("read-only") {
        // 裁决 #14：权限/只读 IO 读失败统一 10（与 C 一致，见设计 D-15）。
        return Failure::retryable(codes::IO_FAILED, "permission denied");
    }
    Failure::retryable(codes::IO_FAILED, error.to_string())
}

/// 日志明细脱敏：任何 ≥32 连续 hex 字符一律打码（raw key 是 64 hex；底层
/// 错误文本偶尔会回显语句片段）。不改变 detail 的其余部分，便于定位。
fn redact_detail(detail: &str) -> String {
    let mut out = String::with_capacity(detail.len());
    let mut run = String::new();
    for ch in detail.chars() {
        if ch.is_ascii_hexdigit() {
            run.push(ch);
            continue;
        }
        push_redacted(&mut out, &mut run);
        out.push(ch);
    }
    push_redacted(&mut out, &mut run);
    out
}

fn push_redacted(out: &mut String, run: &mut String) {
    if run.chars().count() >= 32 {
        out.push_str("<redacted>");
    } else {
        out.push_str(run);
    }
    run.clear();
}

fn fail_outcome(mut outcome: AccountOutcome, failure: Failure) -> AccountOutcome {
    #[cfg(test)]
    eprintln!(
        "store_migrate failure for {}: code={} terminal={} detail={}",
        outcome.lk, failure.code, failure.terminal, failure.detail
    );
    outcome.action = ACTION_FAILED;
    outcome.code = failure.code;
    outcome.terminal = failure.terminal;
    // 终态失败也要把现场留着：不删原库、不删备份；staging 由各调用点负责。
    // detail 进日志前先过 redact_detail：底层错误文本有可能回显 SQL 片段，
    // 而 raw key 是 64 hex——K3 要求 key 的任何形态都不落日志。
    tracing::warn!(
        target: "engine.store",
        stage = "store_migrate",
        code = failure.code,
        retryable = !failure.terminal,
        account_tag = %outcome.lk,
        detail = %redact_detail(&failure.detail),
        "store migration failed"
    );
    outcome
}

/// 每账号一条 WARN 证据行：字段只有 `code/stage/account_tag/bytes/
/// elapsed_ms/keyid`（设计 §10.8 第 7 条），无 key、无号码。
fn emit_evidence(outcome: &AccountOutcome) {
    tracing::warn!(
        target: "engine.store",
        stage = "store_migrate",
        code = outcome.code,
        action = outcome.action,
        account_tag = %outcome.lk,
        bytes = outcome.bytes.unwrap_or_default(),
        elapsed_ms = u64::try_from(outcome.elapsed_ms).unwrap_or(u64::MAX),
        keyid = outcome.keyid.as_deref().unwrap_or("-"),
        "store migration evidence"
    );
}

/// 确定性崩溃断点（**只在 debug 构建存在**；release 编译成空函数）。
///
/// exit 而非 abort：此刻每一步都已显式 fsync，进程退出与 kill -9 的磁盘
/// 终态等价，而 exit 不会在测试里留 core dump。`std::process::exit` 不跑
/// 析构，锁由 OS 释放——正是要模拟的「迁移中途被杀」。
fn fault_point(name: &str) {
    #[cfg(debug_assertions)]
    {
        if std::env::var("KT_ENGINE_STORE_MIGRATE_FAULT").as_deref() == Ok(name) {
            std::process::exit(90);
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = name;
    }
}

/// 测试注入（**只在 debug 构建存在**；release 编译掉）：导出之后人为改
/// staging 的行数，用来验收 §4/D-19 的失败路径——verify 不过时必须删 staging、
/// 保留原库与备份、整体 exit 11。
async fn fault_mutate_staging(staging: &Path, key: &StoreKey) {
    #[cfg(debug_assertions)]
    {
        if std::env::var("KT_ENGINE_STORE_MIGRATE_FAULT").as_deref() != Ok("corrupt_staging_rows") {
            return;
        }
        // 故意不带 foreign_keys：从任意表删一行都不该被外键挡住（这里要造的
        // 是「行数对不上」，不是「外键断裂」）。
        let options = SqliteConnectOptions::new()
            .filename(staging)
            .create_if_missing(false)
            .pragma("key", key.pragma_literal());
        let Ok(mut connection) = SqliteConnection::connect_with(&options).await else {
            return;
        };
        let mut tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .fetch_all(&mut connection)
        .await
        .unwrap_or_default();
        // `_sqlx_migrations` 动了会变成账本不符而不是行数不符；优先大表。
        tables.retain(|table| table != "_sqlx_migrations" && table != "kv");
        if let Some(position) = tables.iter().position(|table| table == "thread_messages") {
            let first = tables.remove(position);
            tables.insert(0, first);
        }
        for table in tables {
            let mut builder = sqlx::QueryBuilder::<sqlx::Sqlite>::new("DELETE FROM ");
            builder.push(quote_identifier(&table));
            builder.push(" WHERE rowid IN (SELECT rowid FROM ");
            builder.push(quote_identifier(&table));
            builder.push(" LIMIT 1)");
            let deleted = builder
                .build()
                .execute(&mut connection)
                .await
                .map(|done| done.rows_affected())
                .unwrap_or(0);
            if deleted > 0 {
                break;
            }
        }
        let _ = close(connection).await;
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (staging, key);
    }
}

/// 测试注入（**只在 debug 构建存在**）：在 swap 前的 checkpoint **之前**制造
/// 「外部读者持快照 + 之后写入的帧」。用来验收 #10 的裁决：删 sidecar 之前
/// 必须先 checkpoint，折不进去就 exit 10；把这条 checkpoint 回滚掉，用例会
/// 从 10 变成 0（即证明它是活的）。
async fn fault_inject_foreign_writer(store: &Path) {
    #[cfg(debug_assertions)]
    {
        if std::env::var("KT_ENGINE_STORE_MIGRATE_FAULT").as_deref() != Ok("before_swap") {
            return;
        }
        let Ok(mut reader) = connect_plain(store).await else {
            return;
        };
        if sqlx::raw_sql("BEGIN; SELECT COUNT(*) FROM threads;")
            .execute(&mut reader)
            .await
            .is_err()
        {
            return;
        }
        // 泄漏连接 = 让读快照活到进程结束（这正是要模拟的「别人还开着库」）。
        std::mem::forget(reader);
        if let Ok(mut writer) = connect_plain(store).await {
            let _ =
                sqlx::raw_sql("INSERT INTO threads (recipient_id) VALUES ('aci-foreign-writer')")
                    .execute(&mut writer)
                    .await;
            let _ = close(writer).await;
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = store;
    }
}
fn emit(args: &MigrateArgs, report: &MigrateReport) {
    if args.json {
        match serde_json::to_string(&report.to_json()) {
            Ok(line) => println!("{line}"),
            Err(_) => eprintln!("kt-signal-engine: failed to serialize storeMigrate report"),
        }
        return;
    }
    for account in &report.accounts {
        println!(
            "storeMigrate {}: {} code={} bytes={} elapsed_ms={} backup={}",
            account.lk,
            account.action,
            account.code,
            account.bytes.unwrap_or_default(),
            account.elapsed_ms,
            account.backup.as_deref().unwrap_or("-")
        );
    }
    if let Some(free) = report.free_bytes {
        println!(
            "storeMigrate: free={free} need={}",
            report.need_bytes.unwrap_or_default()
        );
    }
    if let Some(error) = &report.error {
        eprintln!(
            "kt-signal-engine: ERROR event=store_migrate_failed code={} retryable={}",
            error.code, !error.terminal
        );
    }
    println!("storeMigrate: exit={}", report.exit_code());
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::store_open::PLAINTEXT_HEADER;

    /// 两把形状合法的测试 key（32B；A/B 第一字节不同，错 key 一定解不开）。
    const KEY_A_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const KEY_B_HEX: &str = "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f";
    const LK: &str = "lk-a1b2c3d4";

    fn sandbox(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kt-engine-storemig-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("sandbox");
        dir
    }

    fn key(hex: &str) -> StoreKey {
        StoreKey::parse(hex).expect("valid 64-hex key")
    }

    fn present(hex: &str) -> StoreKeyState {
        StoreKeyState::Present(key(hex))
    }

    fn migrate_args(data_dir: &Path) -> MigrateArgs {
        MigrateArgs {
            data_dir: data_dir.to_path_buf(),
            account: None,
            direction: Direction::ToEncrypted,
            dry_run: false,
            json: true,
        }
    }

    fn account_dir(data_dir: &Path) -> PathBuf {
        let dir = data_dir.join(ACCOUNTS_DIR).join(LK);
        fs::create_dir_all(&dir).expect("account dir");
        dir
    }

    /// 目录快照（文件名 → 字节）：用来断言「零字节改动」与「现场原样」。
    fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return files;
        };
        for entry in entries.flatten() {
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            files.insert(name, fs::read(entry.path()).unwrap_or_default());
        }
        files
    }

    fn is_plaintext_header(path: &Path) -> bool {
        let mut head = [0_u8; PLAINTEXT_HEADER.len()];
        match File::open(path) {
            Ok(mut file) => {
                use std::io::Read as _;
                file.read_exact(&mut head).is_ok() && &head == PLAINTEXT_HEADER
            }
            Err(_) => false,
        }
    }

    /// 用**引擎真实 schema**（presage 迁移）建一个明文库，再灌几行数据 ——
    /// 只有真 schema 才能让语义 probe 与结构比对都有意义。
    async fn seed_plaintext_store(data_dir: &Path) -> PathBuf {
        let dir = account_dir(data_dir);
        let path = dir.join(STORE_DB_FILE);
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
        tokio::time::sleep(STORE_TEARDOWN_QUIET).await;
        let mut connection = connect_plain(&path).await.expect("plain connection");
        sqlx::raw_sql(
            "INSERT INTO threads (group_master_key, recipient_id) VALUES (NULL, 'aci-one');
             INSERT INTO thread_messages (ts, thread_id, sender_service_id, sender_device_id,
                 destination_service_id, needs_receipt, unidentified_sender, content_body, was_plaintext)
             VALUES (1700000000000,
                 (SELECT id FROM threads WHERE recipient_id = 'aci-one'),
                 'aci-sender', 1, 'aci-one', 1, 0, X'00ff', 0);
             PRAGMA user_version = 7;
             PRAGMA application_id = 8397123;",
        )
        .execute(&mut connection)
        .await
        .expect("seed rows");
        close(connection).await.expect("close seeded store");
        path
    }

    async fn query_i64(
        path: &Path,
        key: Option<&StoreKey>,
        sql: &'static str,
    ) -> Result<i64, String> {
        let mut connection = match key {
            Some(key) => connect_keyed(path, key).await,
            None => connect_plain(path).await,
        }
        .map_err(|failure| failure.code.to_owned())?;
        let value = sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&mut connection)
            .await
            .map_err(|error| error.to_string());
        let _ = close(connection).await;
        value
    }

    /// 正向主链路：迁移成功 + §4 矩阵关键项 + 头字段/序列/账本原样。
    #[tokio::test]
    async fn migrates_plaintext_store_and_verifies_the_matrix() {
        let data = sandbox("migrate-ok");
        let store = seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let report = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(report.exit_code(), EXIT_OK, "{:?}", report.error_code());
        let outcome = &report.accounts[0];
        assert_eq!(outcome.action, "migrate");
        assert_eq!(outcome.code, codes::MIGRATED);
        for check in [
            "staging_header_not_plaintext",
            "staging_rejects_no_key_open",
            "staging_integrity_check",
            "staging_foreign_key_check",
            "staging_cipher_integrity_check",
            "schema_set_equal",
            "row_counts_equal",
            "thread_messages_sample_equal",
            "migration_ledger_equal",
            "sqlite_sequence_equal",
            "user_version_equal",
            "application_id_equal",
            "semantic_registration_readable",
        ] {
            assert!(
                outcome.checks.contains(&check),
                "missing check {check}; got {:?}",
                outcome.checks
            );
        }
        // 头：主库不再是明文；备份仍是明文（回滚物）。
        assert!(!is_plaintext_header(&store), "store.db must be encrypted");
        assert!(
            is_plaintext_header(&dir.join(format!("{STORE_DB_FILE}{BACKUP_SUFFIX}"))),
            "plaintext backup must stay plaintext"
        );
        // 无 key 打不开；有 key 数据完整（含 AUTOINCREMENT 高水位与头字段）。
        assert!(
            query_i64(&store, None, "SELECT COUNT(*) FROM sqlite_master")
                .await
                .is_err()
        );
        let key_a = key(KEY_A_HEX);
        assert_eq!(
            query_i64(&store, Some(&key_a), "SELECT COUNT(*) FROM thread_messages")
                .await
                .expect("keyed read"),
            1
        );
        assert_eq!(
            query_i64(
                &store,
                Some(&key_a),
                "SELECT seq FROM sqlite_sequence WHERE name = 'threads'"
            )
            .await
            .expect("sequence"),
            1
        );
        assert_eq!(
            query_i64(&store, Some(&key_a), "PRAGMA user_version")
                .await
                .expect("user_version"),
            7
        );
        assert_eq!(
            query_i64(&store, Some(&key_a), "PRAGMA application_id")
                .await
                .expect("application_id"),
            8397123
        );
        // 现场：staging 不残留；marker 落盘且只有指纹/keyid（没有 key）。
        assert!(!dir
            .join(format!("{STORE_DB_FILE}{STAGING_SUFFIX}"))
            .exists());
        let marker = fs::read_to_string(dir.join(MARKER_FILE)).expect("marker");
        assert!(
            marker.starts_with("magic=KTSF ver=1 state=sqlcipher-v1 "),
            "{marker}"
        );
        assert!(
            marker.contains("keyid=") && marker.contains("fp="),
            "{marker}"
        );
        assert!(
            !marker.contains(KEY_A_HEX),
            "marker must never carry key material: {marker}"
        );
        let _ = fs::remove_dir_all(&data);
    }

    /// 幂等：第二次运行 exit 0 + noop，且账号目录逐字节不变（§5.3 的可验收面）。
    #[tokio::test]
    async fn second_run_is_a_byte_identical_noop() {
        let data = sandbox("noop");
        let store = seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let first = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(first.exit_code(), EXIT_OK);
        let before = snapshot(&dir);
        let store_bytes = fs::read(&store).expect("store bytes");
        let second = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(second.exit_code(), EXIT_OK);
        assert_eq!(second.accounts[0].action, "noop");
        assert_eq!(second.accounts[0].code, codes::NOOP);
        assert_eq!(snapshot(&dir), before, "noop must not change one byte");
        assert_eq!(fs::read(&store).expect("store bytes"), store_bytes);
        let _ = fs::remove_dir_all(&data);
    }

    /// 缺 key：exit 11、零写入（连锁文件都不建），明文库原样。
    #[tokio::test]
    async fn missing_key_writes_nothing() {
        let data = sandbox("no-key");
        let store = seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let before = snapshot(&dir);
        let report = run(
            data.clone(),
            &migrate_args(&data),
            &StoreKeyState::Absent,
            &Hooks::default(),
        )
        .await;
        assert_eq!(report.exit_code(), EXIT_TERMINAL);
        assert_eq!(report.error_code(), Some(codes::KEY_REQUIRED));
        assert!(report.accounts.is_empty());
        assert_eq!(snapshot(&dir), before, "缺 key 必须零写入");
        assert!(is_plaintext_header(&store));
        assert!(!data.join(LOCK_FILE).exists(), "缺 key 不建锁文件");
        let _ = fs::remove_dir_all(&data);
    }

    /// key 形状非法：同样是终态 11、零写入。
    #[tokio::test]
    async fn invalid_key_shape_writes_nothing() {
        let data = sandbox("bad-key");
        seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let before = snapshot(&dir);
        let report = run(
            data.clone(),
            &migrate_args(&data),
            &StoreKeyState::Invalid,
            &Hooks::default(),
        )
        .await;
        assert_eq!(report.exit_code(), EXIT_TERMINAL);
        assert_eq!(report.error_code(), Some(codes::KEY_INVALID));
        assert_eq!(snapshot(&dir), before);
        let _ = fs::remove_dir_all(&data);
    }

    /// 错 key（跨 key 打开已加密库）：终态 11 + store_key_rejected，一个字节都不动。
    #[tokio::test]
    async fn wrong_key_is_terminal_and_leaves_the_store_alone() {
        let data = sandbox("wrong-key");
        let store = seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let first = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(first.exit_code(), EXIT_OK);
        let before = snapshot(&dir);
        let second = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_B_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(second.exit_code(), EXIT_TERMINAL);
        assert_eq!(second.accounts[0].code, codes::KEY_REJECTED);
        assert_eq!(snapshot(&dir), before, "错 key 不得重写/重建库");
        assert!(!is_plaintext_header(&store));
        let _ = fs::remove_dir_all(&data);
    }

    /// 设计 §5.5 谓词 + `store_state_invalid`：加密库**打得开但已损坏**时不许
    /// 报 noop/exit 0。旧实现只 `probe_keyed_store`（首页 sqlite_master），
    /// 页 HMAC 坏了照样返回 Ok → 幂等分支直接绿灯，损坏被当成「已迁移」。
    #[tokio::test]
    async fn corrupt_encrypted_store_is_state_invalid_not_noop() {
        let data = sandbox("state-invalid");
        let store = seed_plaintext_store(&data).await;
        let first = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(first.exit_code(), EXIT_OK, "{:?}", first.error_code());
        // 破坏**最后一页**：首页保持可读，于是 key probe 仍然成功，只有
        // integrity_check / cipher_integrity_check 能发现。
        let mut bytes = fs::read(&store).expect("store bytes");
        assert!(bytes.len() > 3 * 4096, "库里至少要有 3 页: {}", bytes.len());
        let start = bytes.len() - 4096 + 64;
        for byte in &mut bytes[start..start + 64] {
            *byte ^= 0xff;
        }
        fs::write(&store, &bytes).expect("corrupt store");
        let before = fs::read(&store).expect("store bytes");
        let second = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(
            second.exit_code(),
            EXIT_TERMINAL,
            "{:?}",
            second.error_code()
        );
        assert_eq!(second.accounts[0].code, codes::STATE_INVALID);
        assert_eq!(
            fs::read(&store).expect("store bytes"),
            before,
            "损坏现场不得被改写"
        );
        let _ = fs::remove_dir_all(&data);
    }
    /// D-04/D-05：还有读者时 checkpoint 回 busy≠0 → 可重试 10，且不动现场。
    #[tokio::test]
    async fn busy_checkpoint_is_retryable_and_touches_nothing() {
        let data = sandbox("busy");
        let store = seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        // 另一个连接持读事务 + 期间另写一帧：WAL 里存在「读者快照之后的帧」，
        // TRUNCATE checkpoint 必然截断不了 → busy=1（只有读者没有新帧时是 0，
        // 那样测不到占用信号）。
        let mut reader = connect_plain(&store).await.expect("reader");
        sqlx::raw_sql("BEGIN; SELECT COUNT(*) FROM kv;")
            .execute(&mut reader)
            .await
            .expect("read snapshot held");
        let mut writer = connect_plain(&store).await.expect("writer");
        sqlx::raw_sql("INSERT INTO threads (recipient_id) VALUES ('aci-two');")
            .execute(&mut writer)
            .await
            .expect("frame written past the reader snapshot");
        close(writer).await.expect("writer close");
        // 只比库文件本身：-shm 是 SQLite 的连接共享内存（每个连接进出都会改写），
        // 拿它做「零写入」判据会把连接自身的内务当成迁移写入。
        let before = fs::read(&store).expect("store bytes");
        let report = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(report.exit_code(), EXIT_RETRY, "{:?}", report.error_code());
        assert_eq!(report.accounts[0].code, codes::BUSY);
        assert_eq!(
            fs::read(&store).expect("store bytes"),
            before,
            "被占用时不得写库"
        );
        assert!(!dir.join(format!("{STORE_DB_FILE}{BACKUP_SUFFIX}")).exists());
        assert!(!dir
            .join(format!("{STORE_DB_FILE}{STAGING_SUFFIX}"))
            .exists());
        // 硬约束 4：失败路径必须让明文库**仍可被无 key 打开**。
        assert!(
            query_i64(&store, None, "SELECT COUNT(*) FROM threads")
                .await
                .is_ok(),
            "迁移失败后明文库必须仍可无 key 打开"
        );
        // 读者收口后同一份现场照常迁移（可重试语义成立）。
        let _ = sqlx::raw_sql("COMMIT").execute(&mut reader).await;
        close(reader).await.expect("reader close");
        let retry = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(retry.exit_code(), EXIT_OK, "{:?}", retry.error_code());
        assert_eq!(retry.accounts[0].action, "migrate");
        let _ = fs::remove_dir_all(&data);
    }

    /// D-01：空间不足 → 可重试 10，且一个字节都不写（含备份/staging）。
    #[tokio::test]
    async fn insufficient_space_is_retryable_and_writes_nothing() {
        let data = sandbox("no-space");
        seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let before = snapshot(&dir);
        let hooks = Hooks {
            free_bytes: Some(1024),
        };
        let report = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &hooks,
        )
        .await;
        assert_eq!(report.exit_code(), EXIT_RETRY);
        assert_eq!(report.accounts[0].code, codes::NO_SPACE);
        assert_eq!(report.free_bytes, Some(1024));
        assert!(report.need_bytes.unwrap_or_default() > 1024);
        assert_eq!(snapshot(&dir), before, "空间不足必须零写入");
        let _ = fs::remove_dir_all(&data);
    }

    /// 干跑：只出计划（checkpoint/backup/export/verify/swap），不写文件。
    ///
    /// 关键是**不许谎报**：旧实现把干跑报成 `action=migrate` +
    /// `code=store_migrated` + `backup=store.db.plaintext-backup`，连接器按
    /// action/code 分桶，会以为一次真实替换已经完成、手里还有回滚物。
    #[tokio::test]
    async fn dry_run_plans_without_writing() {
        let data = sandbox("dry-run");
        seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let before = snapshot(&dir);
        let mut args = migrate_args(&data);
        args.dry_run = true;
        let report = run(data.clone(), &args, &present(KEY_A_HEX), &Hooks::default()).await;
        assert_eq!(report.exit_code(), EXIT_OK);
        let outcome = &report.accounts[0];
        assert_eq!(outcome.action, ACTION_DRY_RUN, "干跑不得报 migrate");
        assert_eq!(outcome.code, codes::PLANNED);
        assert_eq!(outcome.backup, None, "干跑没有备份，不许报 backup");
        assert_eq!(outcome.plan, STEPS.to_vec());
        assert!(outcome.did.is_empty());
        // JSON 面同样不许有 migrate/backup 的痕迹。
        let json = report.to_json();
        assert_eq!(json["accounts"][0]["action"], ACTION_DRY_RUN);
        assert_eq!(json["accounts"][0]["code"], codes::PLANNED);
        assert!(json["accounts"][0]["backup"].is_null(), "{json}");
        assert_eq!(json["plan"], json!(STEPS.to_vec()));
        assert_eq!(json["did"], json!(Vec::<&str>::new()));
        assert_eq!(snapshot(&dir), before, "干跑必须零写入");
        let _ = fs::remove_dir_all(&data);
    }

    /// 数据目录不存在 = 终态 11 + 专用码（旧行为 ok:true/exit 0 会把路径打错
    /// 吃成绿灯），且**不代建目录**。
    #[tokio::test]
    async fn missing_data_dir_is_terminal_and_creates_nothing() {
        let data = sandbox("missing-dir");
        let absent = data.join("no-such-slot");
        let report = run(
            absent.clone(),
            &migrate_args(&absent),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(
            report.exit_code(),
            EXIT_TERMINAL,
            "缺 data-dir 不是无事可做"
        );
        assert_eq!(report.error_code(), Some(codes::DATA_DIR_MISSING));
        assert!(!report.to_json()["ok"].as_bool().unwrap_or(true));
        assert!(!absent.exists(), "缺 data-dir 不得代建目录");
        let _ = fs::remove_dir_all(&data);
    }

    /// 裁决 #14（设计 D-15 新口径）：权限/只读 IO 读失败归**可重试** 10，
    /// 与 C 一致；ENOENT 仍是终态 11 `account_vanished`。
    #[test]
    fn permission_failures_are_retryable_not_terminal() {
        use std::io::ErrorKind;
        for kind in [ErrorKind::PermissionDenied, ErrorKind::ReadOnlyFilesystem] {
            let failure = Failure::io(&io::Error::from(kind), codes::IO_FAILED);
            assert!(!failure.terminal, "{kind:?} 必须可重试");
            assert_eq!(failure.code, codes::IO_FAILED);
        }
        let vanished = Failure::io(&io::Error::from(ErrorKind::NotFound), codes::IO_FAILED);
        assert!(vanished.terminal, "ENOENT 仍是终态（账号目录没了）");
        assert_eq!(vanished.code, codes::ACCOUNT_VANISHED);
    }

    /// B1/B2/B3/B5：上一轮留下的 staging/备份/marker tmp 一律丢弃重做，
    /// 绝不「接管半成品」。
    #[tokio::test]
    async fn stale_residue_is_discarded_and_redone() {
        let data = sandbox("residue");
        let store = seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let staging = dir.join(format!("{STORE_DB_FILE}{STAGING_SUFFIX}"));
        let backup = dir.join(format!("{STORE_DB_FILE}{BACKUP_SUFFIX}"));
        fs::write(&staging, b"half written staging").expect("stale staging");
        fs::write(sidecar_path(&staging, "-wal"), b"stale").expect("stale staging wal");
        fs::write(&backup, b"stale backup").expect("stale backup");
        fs::write(dir.join(format!("{MARKER_FILE}.999.tmp")), b"tmp").expect("stale tmp");
        let report = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(report.exit_code(), EXIT_OK, "{:?}", report.error_code());
        assert_eq!(report.accounts[0].action, "migrate");
        assert!(!staging.exists(), "半成品 staging 必须被丢弃");
        assert!(!sidecar_path(&staging, "-wal").exists());
        assert!(is_plaintext_header(&backup), "备份必须被本次的真快照覆盖");
        assert!(!dir.join(format!("{MARKER_FILE}.999.tmp")).exists());
        assert!(!is_plaintext_header(&store));
        let _ = fs::remove_dir_all(&data);
    }

    /// B4：rename 之后、marker 之前被杀 → 判定加密 → 跳过并补写 marker（只审计）。
    #[tokio::test]
    async fn missing_marker_is_healed_without_touching_the_store() {
        let data = sandbox("marker-heal");
        let store = seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let first = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(first.exit_code(), EXIT_OK);
        fs::remove_file(dir.join(MARKER_FILE)).expect("drop marker");
        fs::write(dir.join(format!("{MARKER_FILE}.7.tmp")), b"x").expect("marker tmp");
        let store_bytes = fs::read(&store).expect("store bytes");
        let second = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(second.exit_code(), EXIT_OK);
        assert_eq!(second.accounts[0].action, "noop");
        assert!(dir.join(MARKER_FILE).is_file(), "marker 必须补写");
        assert!(!dir.join(format!("{MARKER_FILE}.7.tmp")).exists());
        assert_eq!(fs::read(&store).expect("store bytes"), store_bytes);
        let _ = fs::remove_dir_all(&data);
    }

    /// D-09：marker 指纹与本次 key 不符 → 终态 11，既不迁移也不打开。
    #[tokio::test]
    async fn marker_fingerprint_mismatch_is_terminal() {
        let data = sandbox("fp-mismatch");
        let store = seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let first = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(first.exit_code(), EXIT_OK);
        let marker = dir.join(MARKER_FILE);
        fs::write(
            &marker,
            b"magic=KTSF ver=1 state=sqlcipher-v1 keyid=deadbeef fp=deadbeef at=0 tool=test\n",
        )
        .expect("rewrite marker");
        let store_bytes = fs::read(&store).expect("store bytes");
        let second = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(second.exit_code(), EXIT_TERMINAL);
        assert_eq!(second.accounts[0].code, codes::KEY_MISMATCH);
        assert_eq!(fs::read(&store).expect("store bytes"), store_bytes);
        let _ = fs::remove_dir_all(&data);
    }

    /// 保留窗口：7 天前才可删；未过期保留（这是密钥事故的补救窗口）。
    #[tokio::test]
    async fn backup_retention_window_is_seven_days() {
        let data = sandbox("retention");
        let dir = account_dir(&data);
        let backup = dir.join(format!("{STORE_DB_FILE}{BACKUP_SUFFIX}"));
        fs::write(&backup, b"backup").expect("backup");
        let modified = fs::metadata(&backup)
            .expect("metadata")
            .modified()
            .expect("mtime");
        assert!(
            prune_expired_backup_at(&dir, modified + Duration::from_secs(24 * 3600)).is_none(),
            "未过期不得删备份"
        );
        assert!(backup.exists());
        assert!(
            prune_expired_backup_at(&dir, modified + BACKUP_RETENTION + Duration::from_secs(60))
                .is_some(),
            "过期必须删备份"
        );
        assert!(!backup.exists());
        let _ = fs::remove_dir_all(&data);
    }

    /// 官方 store 落库形态的注册数据（`kv` 表的 `registration` 行 JSON）。
    fn registration_json(aci: &str, device_name: &str) -> Value {
        use base64::Engine as _;
        use presage::libsignal_service::utils::{phonenumber_from_signal, TryIntoE164};
        let e164 = "+15551234567".try_into_e164().expect("e164");
        json!({
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
        let mut connection = connect_plain(path).await.expect("plain connection");
        let value = serde_json::to_vec(&registration_json(aci, device_name)).expect("json");
        sqlx::query("INSERT OR REPLACE INTO kv (key, value) VALUES ('registration', ?)")
            .bind(value)
            .execute(&mut connection)
            .await
            .expect("registration row");
        close(connection).await.expect("close");
    }

    fn failure_code(result: Result<(), Failure>) -> &'static str {
        match result {
            Ok(()) => panic!("expected a verification failure"),
            Err(failure) => failure.code,
        }
    }

    /// 设计 §4 的语义行是「**`Some` 且 ACI/设备名与迁移前一致**」。旧实现
    /// 只看 `load_registration_data()` 是否报错（`registration.map(|_| ())`），
    /// 于是「导出把 kv 整表丢了」(`Ok(None)`) 与「设备名漂移」都被判成通过。
    #[tokio::test]
    async fn semantic_probe_requires_matching_registration() {
        let data = sandbox("probe-registration");
        let store = seed_plaintext_store(&data).await;
        let dir = account_dir(&data);
        let aci = "6a2cb17f-ef7c-4a5d-9c69-6f8b2b1f88ae";
        seed_registration_row(&store, aci, "KT Signal Engine").await;
        let mut source = connect_plain(&store).await.expect("source");
        let expected = read_registration_facts(&mut source).await.expect("facts");
        close(source).await.expect("close source");
        let expected = expected.expect("源库必须读到注册事实");
        assert_eq!(expected.aci.as_deref(), Some(aci));
        assert_eq!(expected.device_name.as_deref(), Some("KT Signal Engine"));
        // 真跑一次，拿一份**合法的加密库**当 staging 的母本。
        let report = run(
            data.clone(),
            &migrate_args(&data),
            &present(KEY_A_HEX),
            &Hooks::default(),
        )
        .await;
        assert_eq!(report.exit_code(), EXIT_OK, "{:?}", report.error_code());
        let key_a = key(KEY_A_HEX);
        // 变体工厂：每次都从母本拷一份新的 staging（probe 会改写 sidecar）。
        let staged_copy = |name: &str| -> PathBuf {
            let path = dir.join(name);
            fs::copy(&store, &path).expect("copy staged");
            path
        };
        let rewrite_row = |path: &Path, value: Option<Vec<u8>>| {
            let path = path.to_path_buf();
            let key = key_a.clone();
            async move {
                let mut connection = connect_keyed(&path, &key).await.expect("keyed");
                match value {
                    Some(bytes) => {
                        sqlx::query("UPDATE kv SET value = ? WHERE key = 'registration'")
                            .bind(bytes)
                            .execute(&mut connection)
                            .await
                            .expect("rewrite");
                    }
                    None => {
                        sqlx::query("DELETE FROM kv WHERE key = 'registration'")
                            .execute(&mut connection)
                            .await
                            .expect("delete");
                    }
                }
                close(connection).await.expect("close");
            }
        };
        // 1) 完全一致：通过（不然这层校验会把正常迁移全判红）。
        let matching = staged_copy("staged-matching.db");
        assert!(
            semantic_probe(&matching, &key_a, &Some(expected.clone()))
                .await
                .is_ok(),
            "一致时必须通过"
        );
        // 2) 设备名漂移：必须红。
        let drifted = staged_copy("staged-drifted.db");
        rewrite_row(
            &drifted,
            Some(serde_json::to_vec(&registration_json(aci, "KT Signal Engine X")).expect("json")),
        )
        .await;
        assert_eq!(
            failure_code(semantic_probe(&drifted, &key_a, &Some(expected.clone())).await),
            codes::VERIFY_FAILED
        );
        // 3) 注册行整条丢了（`Ok(None)`）：这正是旧实现放过去的那种导出。
        let lost = staged_copy("staged-lost.db");
        rewrite_row(&lost, None).await;
        assert_eq!(
            failure_code(semantic_probe(&lost, &key_a, &Some(expected.clone())).await),
            codes::VERIFY_FAILED
        );
        // 4) 源库没注册、staging 却多出一条：同样不许过。
        assert_eq!(
            failure_code(semantic_probe(&matching, &key_a, &None).await),
            codes::VERIFY_FAILED
        );
        let _ = fs::remove_dir_all(&data);
    }
    /// argv 严格面：迁移模式的白名单 / 两模式互斥 / 未知参数 / 非法账号名。
    #[test]
    fn argv_route_is_strict() {
        fn route_of(argv: &[&str]) -> Result<Route, ()> {
            let args: Vec<OsString> = argv.iter().map(OsString::from).collect();
            route(&args)
        }
        for argv in [
            vec!["--data-dir", "/tmp/kt", "jsonRpc"],
            vec!["--data-dir", "/tmp/kt", "storeMigrate"],
            vec![
                "--data-dir",
                "/tmp/kt",
                "storeMigrate",
                "--account",
                LK,
                "--json",
                "--dry-run",
                "--direction",
                "to-encrypted",
            ],
        ] {
            assert!(route_of(&argv).is_ok(), "{argv:?}");
        }
        for argv in [
            vec!["--data-dir", "/tmp/kt", "storeMigrate", "jsonRpc"],
            vec!["--data-dir", "/tmp/kt", "storeMigrate", "--bogus"],
            vec!["storeMigrate"],
            vec![
                "--data-dir",
                "/tmp/kt",
                "storeMigrate",
                "--direction",
                "sideways",
            ],
            vec!["--data-dir", "/tmp/kt", "storeMigrate", "--account", "nope"],
            vec![
                "--data-dir",
                "/tmp/kt",
                "storeMigrate",
                "--account",
                "lk-AB",
            ],
        ] {
            assert!(route_of(&argv).is_err(), "{argv:?} must be rejected");
        }
        match route_of(&["--data-dir", "/tmp/kt", "jsonRpc"]) {
            Ok(Route::JsonRpc) => {}
            _ => panic!("jsonRpc 必须原样交给既有解析器"),
        }
        // 取值位置的 `storeMigrate` 是**目录名**，不是子命令：一个叫
        // storeMigrate 的 data-dir 不得把引擎带进迁移模式（旧实现按字面位置
        // 找子命令，于是这条合法的 jsonRpc 启动被误判成迁移模式并 exit 2）。
        match route_of(&["--data-dir", "storeMigrate", "jsonRpc"]) {
            Ok(Route::JsonRpc) => {}
            _ => panic!("--data-dir storeMigrate jsonRpc 必须按 jsonRpc 启动"),
        }
        match route_of(&["storeMigrate", "--data-dir", "storeMigrate", "--json"]) {
            Ok(Route::Migrate(args)) => assert_eq!(args.data_dir, PathBuf::from("storeMigrate")),
            _ => panic!("子命令 + 同名 data-dir 必须解析成迁移模式"),
        }
        match route_of(&["--data-dir", "/tmp/kt", "storeMigrate", "--dry-run"]) {
            Ok(Route::Migrate(args)) => {
                assert!(args.dry_run);
                assert_eq!(args.direction, Direction::ToEncrypted);
                assert_eq!(args.data_dir, PathBuf::from("/tmp/kt"));
            }
            _ => panic!("迁移模式解析失败"),
        }
    }

    /// df 输出解析（预检读数的唯一纯函数）。
    #[test]
    fn df_output_parsing_is_strict() {
        let sample = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                      /dev/disk3s5 482797652 441215552 12345678 98% /System/Volumes/Data\n";
        assert_eq!(parse_df_available_kib(sample), Some(12345678));
        assert_eq!(parse_df_available_kib(""), None);
        assert_eq!(parse_df_available_kib("Filesystem\n"), None);
    }

    /// VACUUM 之后的备份可用性闸：空文件 / 非明文头一律不算「备份成功」。
    #[test]
    fn backup_usability_requires_a_plaintext_non_empty_file() {
        let dir = sandbox("backup-usable");
        let path = dir.join("backup");
        assert!(!plaintext_backup_usable(&path), "不存在的文件不可用");
        fs::write(&path, b"").expect("empty");
        assert!(!plaintext_backup_usable(&path), "空备份不可用");
        fs::write(&path, vec![0_u8; 1024]).expect("garbage");
        assert!(!plaintext_backup_usable(&path), "非明文头不可用");
        let mut bytes = vec![0_u8; 1024];
        bytes[..store_open::PLAINTEXT_HEADER.len()].copy_from_slice(store_open::PLAINTEXT_HEADER);
        fs::write(&path, &bytes).expect("plaintext");
        assert!(plaintext_backup_usable(&path), "明文库备份可用");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 日志明细脱敏：64 hex 的 key（或任何长 hex 串）不得原样落盘。
    #[test]
    fn failure_detail_redacts_key_material() {
        let raw = format!("PRAGMA key = \"x'{KEY_A_HEX}'\" failed: not a database");
        let redacted = redact_detail(&raw);
        assert!(!redacted.contains(KEY_A_HEX), "{redacted}");
        assert!(redacted.contains("<redacted>"), "{redacted}");
        assert!(redacted.contains("not a database"), "{redacted}");
        // 短 hex（如线程 id / 错误码）不受影响。
        assert_eq!(redact_detail("code 26 at 0x1f"), "code 26 at 0x1f");
    }

    /// 空间预检口径 = L×2.1 + 32 MiB（设计 §9 D-01）。
    #[test]
    fn space_need_matches_design() {
        assert_eq!(space_need(0), SPACE_SLACK_BYTES);
        assert_eq!(
            space_need(10 * 1024 * 1024),
            21 * 1024 * 1024 + SPACE_SLACK_BYTES
        );
    }
}
