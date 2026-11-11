// SPDX-License-Identifier: AGPL-3.0-only

//! 数据目录内引擎状态：`engine-` 命名空间，不与 signal-cli 布局混用
//! （compat §1）。
//!
//! P1b（v2）：账号注册表 `{number, dir}`——dir 指向 presage store 目录
//! （`accounts/lk-<hex>`，ADR 0004）；fake 模式账号 dir=None（无 store）。
//! v1（P1a 桩时代的合成号码）仅在确认 `accounts/` 下没有任何带
//! `store.db` 的目录时按旧语义重置为空；一旦存在 store 就 fail closed
//! （空表 + 清扫会删真实 store），恢复/迁移须人工处理。未知版本/坏 JSON
//! 同样 fail closed。发送调度日志纪律不变。
//!
//! 并发契约（D5，稳定性批 2026-10-20）：`EngineState` 只经
//! `Arc<AsyncMutex<EngineState>>` 访问（main/service 单实例），同一时刻只有
//! 一个 link/delete 变更在锁内执行——跨账号写天然串行。落盘固定
//! 「同目录临时文件 + rename」原子替换，临时文件名带进程内序号，即便未来
//! 出现绕过锁的调用也不会互相覆盖半写文件（最后 rename 赢，读者永远看到
//! 完整 JSON）。官方 Desktop 为单账号进程，无多账号并发对应面（`[KT 适配]`）。
//!
//! P0-B（2026-10-21，存储对账）：注册表是清扫的唯一权威，但「读不到」绝不
//! 等于「空表」。文件存在但损坏/版本未知、文件缺失但 accounts/ 下仍有带
//! `store.db` 的 `lk-*`（注册表被删/恢复不完整），一律 fail closed：
//! `load` 返回错误、进程退出、清扫不运行。只有「文件缺失且没有任何带
//! store 的账号目录」才按首次运行给空表。
//!
//! 批 W2（2026-10-24，§29.73 语义重构）：隔离区（`accounts/_quarantine/`）
//! 非空**不再**拒启动。fail-closed 只针对「注册数据本身不可信」；隔离件是
//! 「旁边的废件」，原位保留为待决区——未完成注册件由成功链接事件触发的
//! 弃置清扫自动带走（service 层 `engine.quarantineDiscarded`），含注册数
//! 据的件保留等显式恢复。官方无「隔离区常驻即全员停摆」的卡死态（§29.72
//! 实证：孤儿隔离件曾把健康账号绑架成每秒重启循环）。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Map;

const STATE_FILE: &str = "engine-state.json";
/// 发送调度日志目录（M5 多账号批）：按账号分文件
/// `<data_dir>/engine-send-log/<accountTag>.jsonl`（accountTag 见
/// `account_tag`；同目录 `.salt` 为安装级盐）。
pub(crate) const DISPATCH_LOG_DIR: &str = "engine-send-log";
/// 旧单文件全局调度日志（迁移前唯一位点；启动时一次性归属到分文件后删除）。
const LEGACY_DISPATCH_LOG: &str = "engine-send-log.jsonl";
/// 账号不可归属（缺参/形状非法）时的兜底桶：不属于账号命名空间，
/// 名称永不与合法 accountId 重合（合法 id 禁止以下划线开头）。
const UNATTRIBUTED_DISPATCH_LOG: &str = "_unattributed.jsonl";
/// 安装级盐（日志目录内）：首建 32 随机字节，文件 0600；accountTag 与
/// 文本摘要都用它做前缀，避免跨安装可关联与短文本枚举。
const DISPATCH_SALT_FILE: &str = ".salt";
const DISPATCH_SALT_BYTES: usize = 32;
/// accountTag 长度（hex 字符数 = 64 bit 截断；碰撞面在单机账号表尺度可忽略）。
const DISPATCH_ACCOUNT_TAG_CHARS: usize = 16;
/// 单账号活动日志上限：超过即轮转一次（当前段转 `.jsonl.1`，只保留最近
/// 一段），日志不无限增长。
const DISPATCH_LOG_LIMIT_BYTES: u64 = 5 * 1024 * 1024;
/// 注册表版本（v2 = P1b `{number, dir}`；ADR 0004）。
const STATE_VERSION: u32 = 2;

/// 账号目录 / presage store 文件名（state 加载与启动清扫共用同一判定源）。
pub(crate) const ACCOUNTS_DIR: &str = "accounts";
pub(crate) const STORE_DB_FILE: &str = "store.db";
/// 未登记凭证目录隔离区（P0-B）。位于 `accounts/_quarantine/`，不匹配
/// 清扫的 `lk-*` 直接子目录规则，二次启动不会被再次清扫。
pub(crate) const QUARANTINE_DIR: &str = "_quarantine";
/// 隔离审计日志（JSONL，一行一条；恢复指引见 type `QuarantineRecord`）。
pub(crate) const QUARANTINE_LOG: &str = "quarantine-log.jsonl";

/// 进程内临时文件序号（D5：并发 persist 各写各的 tmp，rename 原子替换）。
static TMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AccountEntry {
    pub number: String,
    /// presage store 目录（相对 --data-dir）；fake 账号为 None。
    pub dir: Option<String>,
    /// Official postRegistrationSyncsComplete: the three link-time sync
    /// requests are sent once per registration, not once per process start.
    #[serde(default)]
    pub post_registration_syncs_complete: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Persisted {
    version: u32,
    accounts: Vec<AccountEntry>,
}

pub(crate) struct EngineState {
    data_dir: PathBuf,
    accounts: Vec<AccountEntry>,
}

impl EngineState {
    /// 载入注册表（P0-B fail closed）。
    ///
    /// - 文件存在但读失败/坏 JSON/未知版本：返回 `InvalidData`，**不**给
    ///   空表（调用方无法构造 `EngineState`，启动清扫自然不运行）；
    /// - v1 旧桩注册表：只有在 `accounts/lk-*/store.db` 全部不存在时才按
    ///   ADR 0004 重置为空；存在任一 store 同样返回错误；
    /// - 文件缺失：先看 `accounts/lk-*/store.db`。存在任一 store 即视为
    ///   「注册表被删/恢复不完整」，同样返回错误；不存在才是首次运行（空表）；
    /// - 隔离区非空：warn 待决区计数后照常加载（批 W2 语义重构——隔离件是
    ///   废件不是坏注册表；弃置/恢复见 service 层成功链接清扫）。
    pub(crate) fn load(data_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join(STATE_FILE);
        let accounts = match fs::read(&path) {
            Ok(bytes) => match parse_registry(&bytes)? {
                RegistryContents::V2(accounts) => accounts,
                RegistryContents::V1 => {
                    ensure_no_stores(data_dir, "RegistryV1WithStores", "engine-state.json is v1")?;
                    tracing::warn!(
                        cause = "RegistryV1ResetNoStores",
                        "engine-state.json is v1 with no account stores; resetting legacy stub registry"
                    );
                    Vec::new()
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                ensure_no_stores(
                    data_dir,
                    "RegistryMissingWithStores",
                    "engine-state.json is missing",
                )?;
                tracing::warn!(
                    cause = "RegistryAbsentFirstRun",
                    "engine-state.json absent with no account stores; treating as first run"
                );
                Vec::new()
            }
            Err(err) => {
                tracing::error!(
                    cause = "RegistryUnreadable",
                    error = %err,
                    "engine-state.json exists but is unreadable; refusing to start (fail closed, sweep disabled)"
                );
                return Err(io::Error::new(
                    err.kind(),
                    format!("engine-state.json unreadable: {err}"),
                ));
            }
        };
        note_quarantined(data_dir);
        // M5：旧全局调度日志一次性按行归属（能确定账号才迁移，否则删除并
        // warn）；在任何 append_dispatch 之前完成，避免新旧双写。
        migrate_legacy_dispatch_log(data_dir);
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            accounts,
        })
    }

    fn persist(&self) -> io::Result<()> {
        let path = self.data_dir.join(STATE_FILE);
        let persisted = Persisted {
            version: STATE_VERSION,
            accounts: self.accounts.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&persisted)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "serialize"))?;
        // 同目录临时文件 + rename：半写状态不可见。临时文件名带进程内序号
        // （D5）：并发调用各写各的临时文件，rename 原子替换，读者永远读到
        // 完整 JSON（不会读到交错半写）。
        let tmp = self.data_dir.join(format!(
            "{STATE_FILE}.{}.{}.tmp",
            std::process::id(),
            TMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if let Err(error) = fs::write(&tmp, bytes) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        if let Err(error) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn add_account(&mut self, number: String, dir: Option<String>) -> io::Result<()> {
        self.accounts.retain(|existing| existing.number != number);
        self.accounts.push(AccountEntry {
            number,
            dir,
            post_registration_syncs_complete: false,
        });
        self.persist()
    }

    /// 摘除并返回被删条目（fake 账号 dir=None；真实账号由调用方清目录）。
    pub(crate) fn remove_account(&mut self, account: &str) -> io::Result<Option<AccountEntry>> {
        let removed = self
            .accounts
            .iter()
            .position(|existing| existing.number == account)
            .map(|index| self.accounts.remove(index));
        if removed.is_some() {
            self.persist()?;
        }
        Ok(removed)
    }

    pub(crate) fn post_registration_syncs_complete(&self, account: &str) -> bool {
        self.accounts
            .iter()
            .find(|entry| entry.number == account)
            .is_some_and(|entry| entry.post_registration_syncs_complete)
    }

    pub(crate) fn mark_post_registration_syncs_complete(
        &mut self,
        account: &str,
    ) -> io::Result<()> {
        if let Some(entry) = self
            .accounts
            .iter_mut()
            .find(|entry| entry.number == account)
        {
            if !entry.post_registration_syncs_complete {
                entry.post_registration_syncs_complete = true;
                self.persist()?;
            }
        }
        Ok(())
    }

    pub(crate) fn accounts(&self) -> &[AccountEntry] {
        &self.accounts
    }

    /// 账号的 store 目录（相对路径）；fake 账号或未登记返回 None。
    pub(crate) fn account_dir(&self, account: &str) -> Option<String> {
        self.accounts
            .iter()
            .find(|existing| existing.number == account)
            .and_then(|entry| entry.dir.clone())
    }

    /// 发送调度日志（M5 多账号批）：按账号分文件
    /// `<data_dir>/engine-send-log/<accountTag>.jsonl`，一行一条 JSON。
    ///
    /// accountTag = `sha256(salt || lower(accountId))` 前 16 hex——安装级
    /// 盐（同目录 `.salt`，首建 32 随机字节、0600）保证同名账号在不同安装
    /// 上不可关联，也避免把短账号名/号码枚举回来；文件名不含账号/号码明文。
    /// 大小写归一（lower）保证 `ABC`/`abc` 在 APFS/NTFS 上落到同一个文件。
    ///
    /// 落盘前统一脱敏（`redact_dispatch_params`）：正文、群名、联系人姓名、
    /// 邀请链接/密码、举报 token 只留长度 + 加盐 SHA-256 前缀；recipient
    /// 只留计数；账号本身只留加盐哈希前缀（归属由文件名承载）、贴纸图片
    /// 一律字节数摘要；method/时间戳/类型/布尔/时间戳/群 key 等结构键保留。
    /// 账号形状不合法或缺失时写 `_unattributed.jsonl` 兜底桶——绝不把
    /// 参数值拼进路径（路径穿越面归零）。real/fake 两模式同写：日志记录的是
    /// 「引擎收到了什么调度」。写入失败静默降级（可观测面不得反噬发送路径）。
    /// 调度层只在参数校验通过后调用（被拒请求不写日志）。
    pub(crate) fn append_dispatch(&self, method: &str, params: &Map<String, serde_json::Value>) {
        let salt = match load_or_create_dispatch_salt(&self.data_dir) {
            Ok(salt) => salt,
            Err(error) => {
                tracing::warn!(
                    cause = "DispatchLogSaltUnavailable",
                    error = %error,
                    "dispatch log salt unavailable; dropping this dispatch entry"
                );
                return;
            }
        };
        let account = params
            .get("account")
            .and_then(serde_json::Value::as_str)
            .filter(|value| is_valid_account_id(value));
        if account.is_none() {
            tracing::warn!(
                cause = "DispatchAccountInvalid",
                method = %method,
                "dispatch log account id missing or malformed; writing to unattributed bucket"
            );
        }
        let path = match account {
            Some(account) => dispatch_log_path(&self.data_dir, &salt, account)
                .unwrap_or_else(|| unattributed_dispatch_path(&self.data_dir)),
            None => unattributed_dispatch_path(&self.data_dir),
        };
        let entry = serde_json::json!({
            "method": method,
            "ts": dispatch_timestamp(),
            "params": redact_dispatch_params(params, &salt),
        });
        let _ = append_dispatch_line(&path, &entry.to_string());
    }

    /// 删除账号的调度日志文件与轮转段（deleteAccount 在 drain 之后收口，
    /// fail-soft：错误只落结构化 warn，绝不改删除主结局）。账号形状非法或
    /// 盐文件不存在时无文件可归属，跳过。
    pub(crate) fn remove_dispatch_log(&self, account: &str) {
        if !is_valid_account_id(account) {
            tracing::warn!(
                cause = "DispatchLogAccountInvalid",
                "deleteAccount dispatch log cleanup skipped: malformed account id"
            );
            return;
        }
        let salt = match read_dispatch_salt(&self.data_dir) {
            Ok(Some(salt)) => salt,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    cause = "DispatchLogSaltUnreadable",
                    error = %error,
                    "deleteAccount could not read the dispatch log salt; skipping log cleanup"
                );
                return;
            }
        };
        let Some(path) = dispatch_log_path(&self.data_dir, &salt, account) else {
            return;
        };
        for path in [path.clone(), rotated_dispatch_path(&path)] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(
                        cause = "DispatchLogRemoveFailed",
                        error = %error,
                        "deleteAccount could not remove the account dispatch log"
                    );
                }
            }
        }
    }
}

/// accountId 形状校验（M5）：只允许 `[A-Za-z0-9+_-]`、首字符为字母数字
/// 或 `+`、长度 1..=64；禁止 `.` / 路径分隔符 / 下划线开头的保留桶。
/// 文件名由加盐哈希生成，不含账号明文，因此 Windows 设备名（CON/NUL/COM1
/// 等）与大小写折叠不再构成路径风险；形状闸只用于判定归属。
pub(crate) fn is_valid_account_id(account: &str) -> bool {
    let Some(first) = account.chars().next() else {
        return false;
    };
    if account.len() > 64 {
        return false;
    }
    if !(first.is_ascii_alphanumeric() || first == '+') {
        return false;
    }
    account
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_'))
}

/// 账号日志文件路径；账号形状非法返回 None（调用方走兜底桶或跳过）。
fn dispatch_log_path(
    data_dir: &Path,
    salt: &[u8; DISPATCH_SALT_BYTES],
    account: &str,
) -> Option<PathBuf> {
    is_valid_account_id(account).then(|| {
        data_dir
            .join(DISPATCH_LOG_DIR)
            .join(format!("{}.jsonl", account_tag(salt, account)))
    })
}

/// 安装级盐路径（日志目录内，与账号文件同目录但点前缀不参与 jsonl 扫描）。
fn dispatch_salt_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DISPATCH_LOG_DIR).join(DISPATCH_SALT_FILE)
}

/// 只读盐（删除路径用：盐不存在 = 从未写过日志，无需创建）。
fn read_dispatch_salt(data_dir: &Path) -> io::Result<Option<[u8; DISPATCH_SALT_BYTES]>> {
    let path = dispatch_salt_path(data_dir);
    match fs::read(&path) {
        Ok(bytes) => {
            let salt: [u8; DISPATCH_SALT_BYTES] = bytes.as_slice().try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "dispatch log salt has unexpected length",
                )
            })?;
            set_dispatch_mode(&path, 0o600);
            Ok(Some(salt))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 读取或首建安装级盐：32 随机字节，文件 0600、目录 0700；`create_new`
/// 保证并发首建只有一方写入，另一方读回同一份盐。
fn load_or_create_dispatch_salt(data_dir: &Path) -> io::Result<[u8; DISPATCH_SALT_BYTES]> {
    if let Some(salt) = read_dispatch_salt(data_dir)? {
        return Ok(salt);
    }
    let dir = data_dir.join(DISPATCH_LOG_DIR);
    ensure_dispatch_dir(&dir)?;
    let path = dispatch_salt_path(data_dir);
    let mut salt = [0u8; DISPATCH_SALT_BYTES];
    {
        use rand::RngCore as _;
        rand::rng().fill_bytes(&mut salt);
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(&salt)?;
            file.flush()?;
            set_dispatch_mode(&path, 0o600);
            Ok(salt)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_dispatch_salt(data_dir)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "dispatch log salt disappeared after concurrent creation",
                )
            }),
        Err(error) => Err(error),
    }
}

/// accountTag：`sha256(salt || lower(accountId))` 前 16 hex。大小写归一，
/// 文件名不再承载账号/号码明文；盐使跨安装的同名账号不可关联。
fn account_tag(salt: &[u8; DISPATCH_SALT_BYTES], account: &str) -> String {
    let normalized = account.to_ascii_lowercase();
    let mut tag = salted_sha256_hex(salt, &normalized);
    tag.truncate(DISPATCH_ACCOUNT_TAG_CHARS);
    tag
}

fn unattributed_dispatch_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join(DISPATCH_LOG_DIR)
        .join(UNATTRIBUTED_DISPATCH_LOG)
}

/// 轮转段路径：`<tag>.jsonl.1`（只保留一段，见 `rotate_dispatch_log`）。
fn rotated_dispatch_path(path: &Path) -> PathBuf {
    let mut rotated = path.as_os_str().to_os_string();
    rotated.push(".1");
    PathBuf::from(rotated)
}

/// 日志目录：每次写入都校正为 0700（既建既改）。
fn ensure_dispatch_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    set_dispatch_mode(dir, 0o700);
    Ok(())
}

/// 追加一行（同目录 create_dir_all + 上限检查 + append；Unix 目录 0700、
/// 文件 0600，每次写入校正）。活动文件超过 5 MiB 时轮转一次：当前段改名
/// `.jsonl.1`（覆盖更早的轮转段），新段从下一行开始——磁盘最多保留
/// 「当前段 + 最近一段」，不无限增长。
fn append_dispatch_line(path: &Path, line: &str) -> io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dispatch log path has no parent",
        ));
    };
    ensure_dispatch_dir(dir)?;
    let entry_bytes = line.len() as u64 + 1;
    let current = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    if current > 0 && current.saturating_add(entry_bytes) > DISPATCH_LOG_LIMIT_BYTES {
        rotate_dispatch_log(path)?;
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    set_dispatch_mode(path, 0o600);
    use std::io::Write;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()
}

/// 单次轮转：删旧 `.1`、当前段改名 `.1`，新段由下一次 append 创建。
fn rotate_dispatch_log(path: &Path) -> io::Result<()> {
    let rotated = rotated_dispatch_path(path);
    match fs::remove_file(&rotated) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match fs::rename(path, &rotated) {
        Ok(()) => {
            set_dispatch_mode(&rotated, 0o600);
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn set_dispatch_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_dispatch_mode(_path: &Path, _mode: u32) {}

/// 调度日志脱敏：写入前一次完成「正文/名称/链接/密码/token 长度 + 加盐
/// 哈希前缀 / recipient 计数 / 账号加盐哈希前缀 / 号码抹除 / 媒体摘要」。
fn redact_dispatch_params(
    params: &Map<String, serde_json::Value>,
    salt: &[u8; DISPATCH_SALT_BYTES],
) -> Map<String, serde_json::Value> {
    params
        .iter()
        .map(|(key, value)| (key.clone(), redact_dispatch_value(key, value, None, salt)))
        .collect()
}

/// 文本类键：正文、群名/预览标题描述、联系人姓名、邀请链接/密码、举报
/// token、投票问项——一律只留字符长度 + 加盐 SHA-256 前缀（短文本不可
/// 通过裸哈希枚举）。`name` 是 updateContact 的联系人姓名键。
const DISPATCH_TEXT_KEYS: [&str; 16] = [
    "message",
    "text",
    "caption",
    "quoteText",
    "question",
    "options",
    "title",
    "previewTitle",
    "previewDescription",
    "givenName",
    "familyName",
    "nickname",
    "name",
    "inviteLink",
    "inviteLinkPassword",
    "token",
];
/// recipient 面键：一律只留计数（数组条目数/单值 1）。
const DISPATCH_RECIPIENT_KEYS: [&str; 9] = [
    "recipient",
    "recipients",
    "author",
    "sender",
    "number",
    "targetAuthor",
    "quoteAuthor",
    "storyAuthor",
    "updatedStoryRecipients",
];
/// 媒体 data URI 键：超过旧帧帽（1 MiB）的载荷只留字节数摘要（小载荷
/// 形状保持不变，connector 侧既有断言零冲击）。贴纸图片不受阈值保护，
/// 一律摘要（`sticker.image` 是内联图片字节，见 `redact_dispatch_value`）。
const DISPATCH_MEDIA_DIGEST_BYTES: usize = 1024 * 1024;

fn redact_dispatch_value(
    key: &str,
    value: &serde_json::Value,
    parent: Option<&str>,
    salt: &[u8; DISPATCH_SALT_BYTES],
) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(nested_key, nested)| {
                    (
                        nested_key.clone(),
                        redact_dispatch_value(nested_key, nested, Some(key), salt),
                    )
                })
                .collect(),
        ),
        Value::Array(items) if DISPATCH_RECIPIENT_KEYS.contains(&key) => {
            redacted_recipients_value(items.len())
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_dispatch_value(key, item, parent, salt))
                .collect(),
        ),
        Value::String(text) if key == "account" => redacted_account_value(text, salt),
        // 贴纸图片：不论大小一律摘要（媒体面审计必须项）。
        Value::String(text) if parent == Some("sticker") && key == "image" => {
            redacted_media_value("sticker image", text.len())
        }
        Value::String(text) if key == "avatar" => redacted_media_value("avatar", text.len()),
        Value::String(text)
            if matches!(key, "attachment" | "attachments" | "previewImage")
                && text.len() > DISPATCH_MEDIA_DIGEST_BYTES =>
        {
            let label = if key == "attachments" {
                "attachment"
            } else {
                key
            };
            redacted_media_value(label, text.len())
        }
        Value::String(_) if DISPATCH_RECIPIENT_KEYS.contains(&key) => redacted_recipient_value(),
        Value::String(text) if DISPATCH_TEXT_KEYS.contains(&key) => redacted_text_value(text, salt),
        Value::String(text) if looks_like_phone_number(text) => redacted_phone_value(),
        other => other.clone(),
    }
}

/// 文本摘要形状：长度按字符计（人读口径）+ 加盐 SHA-256 前 12 hex。
fn redacted_text_value(text: &str, salt: &[u8; DISPATCH_SALT_BYTES]) -> serde_json::Value {
    serde_json::json!({
        "redacted": "text",
        "length": text.chars().count(),
        "sha256Prefix": salted_sha256_prefix(salt, text, 12),
    })
}

fn redacted_recipients_value(count: usize) -> serde_json::Value {
    serde_json::json!({ "redacted": "recipients", "count": count })
}

fn redacted_recipient_value() -> serde_json::Value {
    serde_json::json!({ "redacted": "recipient", "count": 1 })
}

fn redacted_phone_value() -> serde_json::Value {
    serde_json::json!({ "redacted": "phone" })
}

fn redacted_media_value(label: &str, bytes: usize) -> serde_json::Value {
    serde_json::Value::String(format!("[{label} data uri: {bytes} bytes]"))
}

fn redacted_account_value(account: &str, salt: &[u8; DISPATCH_SALT_BYTES]) -> serde_json::Value {
    let normalized = account.to_ascii_lowercase();
    serde_json::json!({
        "redacted": "account",
        "sha256Prefix": salted_sha256_prefix(salt, &normalized, 12),
    })
}

/// 加盐 SHA-256（salt 前缀 + 值）：日志文件离开安装目录后无法靠彩虹表/
/// 短文本枚举还原正文。
fn salted_sha256_hex(salt: &[u8; DISPATCH_SALT_BYTES], value: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut encoded = String::with_capacity(64);
    for byte in digest.iter() {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn salted_sha256_prefix(salt: &[u8; DISPATCH_SALT_BYTES], value: &str, chars: usize) -> String {
    let mut encoded = salted_sha256_hex(salt, value);
    encoded.truncate(chars);
    encoded
}

/// E.164 形状（Signal 号码恒带 `+`）：`+1..9` 后跟 6..=15 位数字。
/// 任何位置（含嵌套对象/数组）命中的字符串都被抹成 `{redacted:"phone"}`，
/// 兜住未列举的号码键。
fn looks_like_phone_number(value: &str) -> bool {
    let Some(body) = value.strip_prefix('+') else {
        return false;
    };
    (6..=15).contains(&body.len())
        && body
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_digit() && first != '0')
        && body.chars().all(|c| c.is_ascii_digit())
}

fn dispatch_timestamp() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// 旧全局 `engine-send-log.jsonl` 启动时一次性迁移（M5）：
/// - 幂等：先原子 rename 为 `engine-send-log.jsonl.migrated`，再按行归属
///   追加到账号分文件。崩溃发生在追加中途时旧文件已不在原位，下次启动
///   只看到残留 `.migrated`（忽略、不重放），不会重复追加；
/// - 每行能解析出 method + params 且 `params.account` 形状合法 → 脱敏后
///   追加到该账号分文件（文件名加盐哈希）；
/// - 其余行（坏 JSON / 缺 method / 缺账号 / 账号形状非法）宁可不迁移：
///   直接丢弃并计入 dropped（warn 只落计数，不落行内容）；
/// - 处理完毕删除 `.migrated`（残留文件只在崩溃路径出现）。
///
/// fail-soft：任何读写失败只落结构化 warn，不阻塞启动。账号归属只按行内
/// account 判定（注册表可能早于/晚于日志，不拿注册表二次过滤）。
#[inline(never)]
fn migrate_legacy_dispatch_log(data_dir: &Path) {
    let legacy = data_dir.join(LEGACY_DISPATCH_LOG);
    if !legacy.exists() {
        // 崩溃残留的 .migrated 不重放（幂等的另一半）。
        return;
    }
    let staged = data_dir.join(format!("{LEGACY_DISPATCH_LOG}.migrated"));
    if staged.exists() {
        tracing::warn!(
            cause = "LegacyDispatchLogStaged",
            "legacy dispatch log left in place: a .migrated staging file already exists"
        );
        return;
    }
    if let Err(error) = fs::rename(&legacy, &staged) {
        tracing::warn!(
            cause = "LegacyDispatchLogRenameFailed",
            error = %error,
            "could not stage the legacy dispatch log; leaving it in place"
        );
        return;
    }
    let salt = match load_or_create_dispatch_salt(data_dir) {
        Ok(salt) => salt,
        Err(error) => {
            tracing::warn!(
                cause = "LegacyDispatchLogSaltUnavailable",
                error = %error,
                "legacy dispatch log staged but the salt is unavailable; not migrating lines"
            );
            return;
        }
    };
    let bytes = match fs::read(&staged) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(
                cause = "LegacyDispatchLogUnreadable",
                error = %error,
                "legacy dispatch log staged but unreadable; leaving it staged"
            );
            return;
        }
    };
    let mut migrated = 0usize;
    let mut dropped = 0usize;
    for line in String::from_utf8_lossy(&bytes).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            dropped += 1;
            continue;
        };
        let method = entry.get("method").and_then(serde_json::Value::as_str);
        let params = entry.get("params").and_then(serde_json::Value::as_object);
        let account = params
            .and_then(|params| params.get("account"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| is_valid_account_id(value));
        let (Some(method), Some(params), Some(account)) = (method, params, account) else {
            dropped += 1;
            continue;
        };
        let migrated_entry = serde_json::json!({
            "method": method,
            "migrated": true,
            "params": redact_dispatch_params(params, &salt),
        });
        match dispatch_log_path(data_dir, &salt, account) {
            Some(path) if append_dispatch_line(&path, &migrated_entry.to_string()).is_ok() => {
                migrated += 1;
            }
            _ => dropped += 1,
        }
    }
    match fs::remove_file(&staged) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            cause = "LegacyDispatchLogRemoveFailed",
            error = %error,
            "legacy dispatch log migrated but could not remove the staging file"
        ),
    }
    if dropped > 0 {
        tracing::warn!(
            cause = "LegacyDispatchLogLinesDropped",
            dropped,
            "legacy dispatch log lines without a determinable account were deleted"
        );
    }
    tracing::warn!(
        cause = "LegacyDispatchLogMigrated",
        migrated,
        dropped,
        "legacy dispatch log handled; only attributable lines were migrated"
    );
}

/// 注册表解析结果：v2 = 正常账号表；v1 = 旧桩注册表（是否允许重置由调用方
/// 按 `accounts/lk-*/store.db` 是否存在决定）。
enum RegistryContents {
    V2(Vec<AccountEntry>),
    V1,
}

/// 注册表解析：坏 JSON 返回 `InvalidData`（P0-B：绝不降级成空表）；v1 返回
/// legacy 标记（调用方确认无 store 后才可重置为空）；其他版本 fail closed。
fn parse_registry(bytes: &[u8]) -> io::Result<RegistryContents> {
    let persisted = serde_json::from_slice::<Persisted>(bytes).map_err(|error| {
        tracing::error!(
            cause = "RegistryCorrupt",
            error = %error,
            "engine-state.json is corrupt; refusing to start (fail closed, sweep disabled)"
        );
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("engine-state.json is corrupt: {error}"),
        )
    })?;
    match persisted.version {
        STATE_VERSION => Ok(RegistryContents::V2(persisted.accounts)),
        1 => Ok(RegistryContents::V1),
        version => {
            tracing::error!(
                cause = "RegistryVersionUnsupported",
                version,
                "engine-state.json version is unsupported; refusing to start (fail closed, sweep disabled)"
            );
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "engine-state.json version {version} is unsupported (expected {STATE_VERSION} or legacy 1)"
                ),
            ))
        }
    }
}

/// 注册表缺位/仅 v1 时：只要 `accounts/` 下仍有带 `store.db` 的 `lk-*`，
/// 就 fail closed（空表 + 清扫会删凭证）。`cause` 为结构化日志分类。
fn ensure_no_stores(data_dir: &Path, cause: &'static str, context: &str) -> io::Result<()> {
    let stores = account_store_dirs(data_dir)?;
    if stores.is_empty() {
        return Ok(());
    }
    tracing::error!(
        cause = cause,
        stores = stores.len(),
        "account stores exist while the registry cannot be trusted; refusing to start (fail closed, sweep disabled)"
    );
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{context} but {} account store {}; refusing to start without sweeping",
            stores.len(),
            if stores.len() == 1 {
                "directory exists"
            } else {
                "directories exist"
            }
        ),
    ))
}

/// `accounts/lk-*` 中带 `store.db` 的目录名（排序后返回，日志/测试确定性）。
/// 只用于「注册表缺失/仅 v1」时的恢复完整性判定；accounts/ 缺失 = 无 store；
/// 其他读取错误向上抛（无法证明安全就必须 fail closed）。
fn account_store_dirs(data_dir: &Path) -> io::Result<Vec<String>> {
    let accounts = data_dir.join(ACCOUNTS_DIR);
    let entries = match fs::read_dir(&accounts) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            tracing::error!(
                cause = "AccountsDirUnreadable",
                error = %err,
                "cannot inspect accounts dir; refusing to start (fail closed)"
            );
            return Err(err);
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        // 非 UTF-8 名同样按 lossy 名参与判定（与清扫同口径）：宁可误判
        // 「可能有 store」而 fail closed，也不把它当首次运行放行清扫。
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("lk-") || !entry.file_type()?.is_dir() {
            continue;
        }
        if store_db_present(&entry.path())? {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// `store.db` 是否存在。探测错误向上抛（调用方必须按「可能有 store」处理）。
pub(crate) fn store_db_present(dir: &Path) -> io::Result<bool> {
    dir.join(STORE_DB_FILE).try_exists()
}

/// 隔离区里真正的 `lk-*` 目录数（不把审计日志/空时间戳桶当凭证）。
fn quarantined_store_count(data_dir: &Path) -> io::Result<usize> {
    let root = data_dir.join(ACCOUNTS_DIR).join(QUARANTINE_DIR);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => {
            tracing::error!(
                cause = "QuarantineUnreadable",
                error = %err,
                "cannot inspect quarantine dir; refusing to start (fail closed)"
            );
            return Err(err);
        }
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == QUARANTINE_LOG || !entry.file_type()?.is_dir() {
            continue;
        }
        if name.starts_with("lk-") {
            count += 1;
            continue;
        }
        // 时间戳桶：只数桶内真正的 lk-* 目录；空桶（rename 失败残留）不拦启动。
        for nested in fs::read_dir(entry.path())? {
            let nested = nested?;
            if nested.file_name().to_string_lossy().starts_with("lk-")
                && nested.file_type()?.is_dir()
            {
                count += 1;
            }
        }
    }
    Ok(count)
}

/// 隔离区非空 = 有待决件（废件/待恢复凭证），原位保留不拦启动（批 W2）。
/// 结构化 warn 留恢复入口指针，不打印账号号码/密钥；检查失败同样只 warn——
/// 清扫永不触碰隔离区，「看不清废件」不升级成「全员停摆」。
fn note_quarantined(data_dir: &Path) {
    let count = match quarantined_store_count(data_dir) {
        Ok(count) => count,
        Err(error) => {
            tracing::warn!(
                cause = "QuarantineUnreadable",
                error = %error,
                "cannot inspect quarantine dir; continuing startup (sweep never touches the quarantine zone)"
            );
            return;
        }
    };
    if count == 0 {
        return;
    }
    tracing::warn!(
        cause = "QuarantinePending",
        quarantined = count,
        recovery_log = %format!("{ACCOUNTS_DIR}/{QUARANTINE_DIR}/{QUARANTINE_LOG}"),
        "quarantined account stores retained in place as a pending zone; unregistered leftovers are auto-discarded after the next successful link, stores holding registration data await explicit recovery"
    );
}

#[cfg(test)]
mod tests {
    use super::{
        account_tag, dispatch_log_path, dispatch_salt_path, rotated_dispatch_path,
        salted_sha256_prefix, EngineState, ACCOUNTS_DIR, DISPATCH_ACCOUNT_TAG_CHARS,
        DISPATCH_LOG_DIR, DISPATCH_LOG_LIMIT_BYTES, DISPATCH_SALT_BYTES, LEGACY_DISPATCH_LOG,
        QUARANTINE_DIR, QUARANTINE_LOG, STATE_FILE, STORE_DB_FILE,
    };
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;

    fn temp_dir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kt-engine-state-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn temp_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read state dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    fn dispatch_file_names(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir.join(DISPATCH_LOG_DIR)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".jsonl"))
            .collect();
        names.sort();
        names
    }

    fn account_path(dir: &Path, salt: &[u8; DISPATCH_SALT_BYTES], account: &str) -> PathBuf {
        dispatch_log_path(dir, salt, account).expect("valid account id")
    }

    /// 加盐文件名形状：16 位小写 hex + .jsonl（不含账号/号码明文）。
    fn assert_hashed_log_name(name: &str) {
        let stem = name
            .strip_suffix(".jsonl")
            .unwrap_or_else(|| panic!("dispatch log name must end in .jsonl: {name}"));
        assert_eq!(
            stem.len(),
            DISPATCH_ACCOUNT_TAG_CHARS,
            "accountTag 长度: {name}"
        );
        assert!(
            stem.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "accountTag 必须是 16 位小写 hex: {name}"
        );
    }

    /// D5 落盘面：add/remove 后 engine-state.json 可重载一致，临时文件不残留。
    #[test]
    fn persist_roundtrips_and_leaves_no_temp_file() {
        let dir = temp_dir("roundtrip");
        let mut state = EngineState::load(&dir).expect("load");
        state
            .add_account("+15550000001".to_owned(), Some("accounts/a".to_owned()))
            .expect("add");
        state
            .add_account("+15550000002".to_owned(), None)
            .expect("add fake");
        assert!(dir.join(STATE_FILE).is_file());
        assert!(temp_leftovers(&dir).is_empty());

        state.remove_account("+15550000001").expect("remove");
        let reloaded = EngineState::load(&dir).expect("reload");
        assert_eq!(reloaded.accounts(), state.accounts());
        assert_eq!(reloaded.accounts().len(), 1);
        assert!(temp_leftovers(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D5 跨账号串行：并发 link（add）与 delete（remove）都经同一把
    /// AsyncMutex 串行化；结束态与磁盘逐条一致、无半写临时文件。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_link_and_delete_are_serialized() {
        let dir = temp_dir("concurrent");
        let state = Arc::new(AsyncMutex::new(EngineState::load(&dir).expect("load")));
        let mut tasks = Vec::new();
        const WORKERS: u32 = 4;
        const ROUNDS: u32 = 25;
        for worker in 0..WORKERS {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                for round in 0..ROUNDS {
                    let number = format!("+1555{worker:03}{round:04}");
                    state
                        .lock()
                        .await
                        .add_account(number.clone(), None)
                        .expect("add");
                    if round % 3 == 0 {
                        state.lock().await.remove_account(&number).expect("remove");
                    }
                }
            }));
        }
        for task in tasks {
            task.await.expect("worker task");
        }

        let memory = {
            let state = state.lock().await;
            state.accounts().to_vec()
        };
        let reloaded = EngineState::load(&dir).expect("reload");
        assert_eq!(memory, reloaded.accounts(), "内存态与磁盘态逐条一致");
        // 每 worker 25 次 add、9 次 remove（round 0,3,…,24）→ 16 条/worker。
        assert_eq!(reloaded.accounts().len(), (WORKERS * 16) as usize);
        assert!(
            temp_leftovers(&dir).is_empty(),
            "并发 persist 后无半写临时文件残留"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_store(dir: &Path, name: &str) -> PathBuf {
        let store_dir = dir.join(ACCOUNTS_DIR).join(name);
        std::fs::create_dir_all(&store_dir).expect("store dir");
        std::fs::write(store_dir.join(STORE_DB_FILE), b"credential-bytes").expect("store db");
        store_dir
    }

    fn load_err(dir: &Path) -> std::io::Error {
        match EngineState::load(dir) {
            Ok(_) => panic!("expected EngineState::load to fail closed"),
            Err(error) => error,
        }
    }

    /// P0-B：坏 JSON 绝不能被当作空表（旧语义），必须 fail closed。
    #[test]
    fn load_fails_closed_on_corrupt_registry() {
        let dir = temp_dir("corrupt-registry");
        std::fs::write(dir.join(STATE_FILE), b"{not-json").expect("write corrupt registry");
        let err = load_err(&dir);
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "cause: {err}");
        assert!(err.to_string().contains("corrupt"), "cause: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未知版本 fail closed（绝不降级为空表）。
    #[test]
    fn load_fails_closed_on_unknown_registry_version() {
        let dir = temp_dir("version-registry");
        std::fs::write(dir.join(STATE_FILE), br#"{"version":3,"accounts":[]}"#)
            .expect("write v3 registry");
        let err = load_err(&dir);
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "cause: {err}");
        assert!(err.to_string().contains("version 3"), "cause: {err}");
        assert!(err.to_string().contains("unsupported"), "cause: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v1 旧桩注册表 + 无 store：保留 ADR 0004 的「重置为空」迁移语义。
    #[test]
    fn load_resets_v1_registry_when_no_stores_present() {
        let dir = temp_dir("v1-no-stores");
        std::fs::write(dir.join(STATE_FILE), br#"{"version":1,"accounts":[]}"#)
            .expect("write v1 registry");
        let state = EngineState::load(&dir).expect("v1 without stores can reset");
        assert!(state.accounts().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v1 旧桩注册表 + 真实 store：空表 + 清扫会删凭证，必须 fail closed。
    #[test]
    fn load_fails_closed_when_v1_registry_has_store() {
        let dir = temp_dir("v1-with-store");
        std::fs::write(dir.join(STATE_FILE), br#"{"version":1,"accounts":[]}"#)
            .expect("write v1 registry");
        let store_dir = write_store(&dir, "lk-old-stub");
        let err = load_err(&dir);
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "cause: {err}");
        assert!(err.to_string().contains("v1"), "cause: {err}");
        assert!(
            store_dir.join(STORE_DB_FILE).is_file(),
            "store 必须原样保留"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件存在但读失败（目录当文件 / 权限拒绝）与坏 JSON 同属 fail closed。
    #[test]
    fn load_fails_closed_when_registry_path_is_unreadable() {
        let dir = temp_dir("unreadable-registry");
        std::fs::create_dir(dir.join(STATE_FILE)).expect("registry path as dir");
        let err = load_err(&dir);
        assert!(err.to_string().contains("unreadable"), "cause: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-B 核心：注册表不存在但仍有带 store.db 的 lk-* = 注册表被删/恢复
    /// 不完整，fail closed；不得返回空表（否则启动清扫会删凭证）。
    #[test]
    fn load_fails_closed_when_registry_missing_but_store_present() {
        let dir = temp_dir("missing-with-store");
        let store_dir = write_store(&dir, "lk-restore-me");
        let err = load_err(&dir);
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "cause: {err}");
        assert!(err.to_string().contains("missing"), "cause: {err}");
        assert!(
            store_dir.join(STORE_DB_FILE).is_file(),
            "store 必须原样保留"
        );
        assert!(
            !dir.join(ACCOUNTS_DIR).join(QUARANTINE_DIR).exists(),
            "加载阶段不得移动/隔离任何目录"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Official postRegistrationSyncsComplete: persisted across restarts and
    /// reset when the account is linked again.
    #[test]
    fn post_registration_syncs_status_persists_and_resets_on_new_link() {
        let dir = temp_dir("post-registration-syncs");
        let mut state = EngineState::load(&dir).expect("first run");
        state
            .add_account("+15550001111".into(), Some("lk-sync".into()))
            .expect("add");
        assert!(!state.post_registration_syncs_complete("+15550001111"));
        state
            .mark_post_registration_syncs_complete("+15550001111")
            .expect("mark");
        assert!(state.post_registration_syncs_complete("+15550001111"));
        drop(state);

        let state = EngineState::load(&dir).expect("reload");
        assert!(
            state.post_registration_syncs_complete("+15550001111"),
            "completion must survive restart"
        );
        drop(state);

        let mut state = EngineState::load(&dir).expect("reload for relink");
        state
            .remove_account("+15550001111")
            .expect("remove")
            .expect("account existed");
        state
            .add_account("+15550001111".into(), Some("lk-sync".into()))
            .expect("relink");
        assert!(
            !state.post_registration_syncs_complete("+15550001111"),
            "new link must start incomplete"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 首次运行：无注册表、无账号目录 → 正常空表启动。
    #[test]
    fn load_treats_absent_registry_without_stores_as_first_run() {
        let dir = temp_dir("first-run");
        let state = EngineState::load(&dir).expect("first run must load");
        assert!(state.accounts().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无注册表但只有空 lk-* 目录（无 store.db）→ 仍属首次运行/可清扫面，
    /// 不因「目录存在」就 fail closed。
    #[test]
    fn load_ignores_empty_lk_dirs_when_registry_absent() {
        let dir = temp_dir("missing-empty-lk");
        std::fs::create_dir_all(dir.join(ACCOUNTS_DIR).join("lk-empty")).expect("empty lk dir");
        let state = EngineState::load(&dir).expect("empty lk dir is not a credential");
        assert!(state.accounts().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 批 W2 四象限②（只有孤儿）：隔离区有未登记废件、注册表缺位 → 照常
    /// 启动（待决区语义），隔离内容原位保留不移动/不删除。旧语义（§29.72
    /// 事故）此处 fail closed 循环拒启。
    #[test]
    fn load_continues_with_quarantine_pending_entries() {
        let dir = temp_dir("quarantine-pending");
        let quarantined = dir
            .join(ACCOUNTS_DIR)
            .join(QUARANTINE_DIR)
            .join("1700000000000")
            .join("lk-old");
        std::fs::create_dir_all(&quarantined).expect("quarantine dir");
        std::fs::write(quarantined.join(STORE_DB_FILE), b"credential-bytes").expect("store db");
        let state = EngineState::load(&dir).expect("quarantine leftovers must not block startup");
        assert!(state.accounts().is_empty(), "注册表缺位仍按首次运行给空表");
        assert!(
            quarantined.join(STORE_DB_FILE).is_file(),
            "隔离内容必须原位保留"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 批 W2 四象限①（孤儿 + 健康账号并存）：健康注册账号不被旁边的隔离
    /// 废件绑架——load 成功、账号表逐条保真、隔离件不动。
    #[test]
    fn load_starts_with_quarantine_and_healthy_accounts_coexisting() {
        let dir = temp_dir("quarantine-coexist");
        let mut state = EngineState::load(&dir).expect("load");
        state
            .add_account(
                "+15550000001".to_owned(),
                Some("accounts/lk-healthy".to_owned()),
            )
            .expect("add healthy");
        let quarantined = dir
            .join(ACCOUNTS_DIR)
            .join(QUARANTINE_DIR)
            .join("1700000000000")
            .join("lk-old");
        std::fs::create_dir_all(&quarantined).expect("quarantine dir");
        std::fs::write(quarantined.join(STORE_DB_FILE), b"credential-bytes").expect("store db");
        let reloaded = EngineState::load(&dir).expect("healthy accounts must start normally");
        assert_eq!(
            reloaded.accounts(),
            state.accounts(),
            "健康账号表不受隔离件影响"
        );
        assert!(
            quarantined.join(STORE_DB_FILE).is_file(),
            "隔离件待决区原位保留"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M5 多账号日志面：文件名 = 加盐 accountTag（16 hex，无账号/号码
    /// 明文）、写入前脱敏（正文/群名/联系人姓名/邀请链接/密码/token 长度+
    /// 加盐哈希、recipients 计数、号码抹除、贴纸图片一律摘要），结构键保留；
    /// Unix 下目录 0700 / 文件与盐 0600。
    #[test]
    fn dispatch_log_is_per_account_hashed_and_redacted() {
        let dir = temp_dir("dispatch-per-account");
        let state = EngineState::load(&dir).expect("load");
        let body = "top secret body";
        let params = json!({
            "account": "+15550000000",
            "message": body,
            "recipients": ["+15555550101", "+15555550102"],
            "timestamp": 42,
            "stop": false,
            "mentions": [{ "number": "+15555550101", "start": 0, "length": 2 }],
            "attachments": ["data:text/plain;base64,aGk="],
            "previewUrl": "https://example.com/a",
            "title": "group title secret",
            "previewTitle": "preview title secret",
            "previewDescription": "preview description secret",
            "contact": {
                "givenName": "Alice",
                "familyName": "Smith",
                "nickname": "Al",
                "avatar": "data:image/png;base64,AAAA",
            },
            "inviteLink": "https://signal.group/#SECRET",
            "inviteLinkPassword": "cGFzc3dvcmQ=",
            "token": "spam-token-secret",
            "sticker": {
                "packId": "00112233445566778899aabbccddeeff",
                "packKey": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "stickerId": 1,
                "image": "data:image/webp;base64,AAAA",
            },
        });
        state.append_dispatch("send", params.as_object().expect("object"));
        let other = json!({ "account": "+15550001111", "message": "other body" });
        state.append_dispatch("sendTyping", other.as_object().expect("object"));

        let salt_bytes = std::fs::read(dispatch_salt_path(&dir)).expect("salt written");
        assert_eq!(salt_bytes.len(), DISPATCH_SALT_BYTES, "盐长度 32B");
        let salt: [u8; DISPATCH_SALT_BYTES] = salt_bytes.as_slice().try_into().expect("salt");
        let first = account_path(&dir, &salt, "+15550000000");
        let second = account_path(&dir, &salt, "+15550001111");
        assert!(first.is_file(), "账号 A 独立文件");
        assert!(second.is_file(), "账号 B 独立文件");
        let names = dispatch_file_names(&dir);
        assert_eq!(names.len(), 2, "两个账号文件: {names:?}");
        for name in &names {
            assert_hashed_log_name(name);
            assert!(
                !name.contains("+15550000000"),
                "文件名不得含号码明文: {name}"
            );
        }
        assert_eq!(
            first.file_name().and_then(|name| name.to_str()),
            Some(format!("{}.jsonl", account_tag(&salt, "+15550000000")).as_str()),
            "文件名 = 加盐 tag"
        );
        let raw = std::fs::read_to_string(&first).expect("read account log");
        for secret in [
            body,
            "+1555",
            "group title secret",
            "preview title secret",
            "preview description secret",
            "Alice",
            "Smith",
            "signal.group/#SECRET",
            "cGFzc3dvcmQ=",
            "spam-token-secret",
        ] {
            assert!(!raw.contains(secret), "敏感值不得落盘 {secret}: {raw}");
        }
        let entry: Value = serde_json::from_str(raw.lines().next().expect("line")).expect("json");
        assert_eq!(entry["method"], json!("send"));
        assert_eq!(entry["ts"].as_str().map(str::len), Some(24), "RFC3339 毫秒");
        assert_eq!(entry["params"]["account"]["redacted"], json!("account"));
        assert_eq!(
            entry["params"]["account"]["sha256Prefix"]
                .as_str()
                .map(str::len),
            Some(12)
        );
        assert_eq!(entry["params"]["message"]["redacted"], json!("text"));
        assert_eq!(
            entry["params"]["message"]["length"],
            json!(body.chars().count())
        );
        assert_eq!(
            entry["params"]["message"]["sha256Prefix"]
                .as_str()
                .map(str::len),
            Some(12)
        );
        assert_eq!(
            entry["params"]["recipients"],
            json!({ "redacted": "recipients", "count": 2 })
        );
        assert_eq!(entry["params"]["timestamp"], json!(42), "结构键保留");
        assert_eq!(entry["params"]["stop"], json!(false), "类型键保留");
        assert_eq!(
            entry["params"]["previewUrl"],
            json!("https://example.com/a")
        );
        assert_eq!(
            entry["params"]["mentions"][0]["number"],
            json!({ "redacted": "recipient", "count": 1 })
        );
        assert_eq!(
            entry["params"]["attachments"],
            json!(["data:text/plain;base64,aGk="]),
            "小载荷保持既有形状"
        );
        for key in [
            "title",
            "previewTitle",
            "previewDescription",
            "inviteLink",
            "inviteLinkPassword",
            "token",
        ] {
            assert_eq!(
                entry["params"][key]["redacted"],
                json!("text"),
                "{key} 摘要化"
            );
        }
        for key in ["givenName", "familyName", "nickname"] {
            assert_eq!(
                entry["params"]["contact"][key]["redacted"],
                json!("text"),
                "contact.{key} 摘要化"
            );
        }
        assert_eq!(
            entry["params"]["contact"]["avatar"],
            json!("[avatar data uri: 26 bytes]")
        );
        assert_eq!(
            entry["params"]["sticker"]["image"],
            json!("[sticker image data uri: 27 bytes]"),
            "sticker.image 一律摘要（小图不留原文）"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&first)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600, "账号日志文件 0600");
            let dir_mode = std::fs::metadata(dir.join(DISPATCH_LOG_DIR))
                .expect("dir meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "日志目录 0700");
            let salt_mode = std::fs::metadata(dispatch_salt_path(&dir))
                .expect("salt meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(salt_mode, 0o600, "盐文件 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M5 大小写归一：同一账号的大小写变体落到同一个加盐文件（APFS/NTFS
    /// 大小写折叠面不产生两个文件），日志三行。
    #[test]
    fn dispatch_log_account_case_is_folded() {
        let dir = temp_dir("dispatch-case");
        let state = EngineState::load(&dir).expect("load");
        for account in ["Acct-Abc", "acct-abc", "ACCT-ABC"] {
            state.append_dispatch(
                "send",
                json!({ "account": account, "message": "x" })
                    .as_object()
                    .expect("object"),
            );
        }
        let salt: [u8; DISPATCH_SALT_BYTES] = std::fs::read(dispatch_salt_path(&dir))
            .expect("salt")
            .as_slice()
            .try_into()
            .expect("salt");
        let path = account_path(&dir, &salt, "acct-abc");
        assert!(path.is_file(), "小写归一后的文件");
        assert_eq!(
            std::fs::read_to_string(&path).expect("log").lines().count(),
            3,
            "三个大小写变体共用一个文件"
        );
        assert_eq!(
            dispatch_file_names(&dir),
            vec![path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("name")
                .to_owned()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M5 文件名面：加盐哈希名不含账号/号码明文，也不会生成 Windows 保留
    /// 设备名（CON/NUL/COM1）——这些账号同样落到普通 hex 文件。
    #[test]
    fn dispatch_log_file_names_are_hashed_without_plaintext_or_reserved_names() {
        let dir = temp_dir("dispatch-names");
        let state = EngineState::load(&dir).expect("load");
        for account in ["+15550000000", "CON", "NUL", "COM1", "LPT9", "abc"] {
            state.append_dispatch(
                "send",
                json!({ "account": account, "message": "x" })
                    .as_object()
                    .expect("object"),
            );
        }
        let names = dispatch_file_names(&dir);
        assert_eq!(names.len(), 6, "每个账号一个文件: {names:?}");
        for name in &names {
            assert_hashed_log_name(name);
        }
        let lower: Vec<String> = names.iter().map(|name| name.to_ascii_lowercase()).collect();
        for reserved in ["con.jsonl", "nul.jsonl", "com1.jsonl", "lpt9.jsonl"] {
            assert!(
                !lower.iter().any(|name| name == reserved),
                "保留设备名不得作为文件名: {names:?}"
            );
        }
        assert!(
            !lower.iter().any(|name| name.contains("+15550000000")),
            "文件名不得含号码明文: {names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M5 加盐哈希：同一短文本在两个安装（不同盐）下前缀不同，且不等于
    /// 裸 SHA-256——日志离开安装目录后无法靠短文本枚举还原。
    #[test]
    fn dispatch_log_text_hash_is_salted_and_not_enumerable() {
        use sha2::{Digest, Sha256};
        let dir_a = temp_dir("dispatch-salt-a");
        let dir_b = temp_dir("dispatch-salt-b");
        let text = "ok";
        for dir in [&dir_a, &dir_b] {
            let state = EngineState::load(dir).expect("load");
            state.append_dispatch(
                "send",
                json!({ "account": "+15550000000", "message": text })
                    .as_object()
                    .expect("object"),
            );
        }
        let salt_a: [u8; DISPATCH_SALT_BYTES] = std::fs::read(dispatch_salt_path(&dir_a))
            .expect("salt a")
            .as_slice()
            .try_into()
            .expect("salt");
        let salt_b: [u8; DISPATCH_SALT_BYTES] = std::fs::read(dispatch_salt_path(&dir_b))
            .expect("salt b")
            .as_slice()
            .try_into()
            .expect("salt");
        assert_ne!(salt_a, salt_b, "每次首次创建随机盐");
        let prefix_a = salted_sha256_prefix(&salt_a, text, 12);
        let prefix_b = salted_sha256_prefix(&salt_b, text, 12);
        assert_ne!(prefix_a, prefix_b, "不同盐 → 不同摘要");
        let plain = {
            use std::fmt::Write as _;
            let digest = Sha256::digest(text.as_bytes());
            let mut encoded = String::new();
            for byte in digest.iter() {
                let _ = write!(encoded, "{byte:02x}");
            }
            encoded.truncate(12);
            encoded
        };
        assert_ne!(prefix_a, plain, "加盐前缀 != 裸 SHA-256 前缀");
        let entry: Value = serde_json::from_str(
            std::fs::read_to_string(account_path(&dir_a, &salt_a, "+15550000000"))
                .expect("log a")
                .lines()
                .next()
                .expect("line"),
        )
        .expect("json");
        assert_eq!(entry["params"]["message"]["sha256Prefix"], json!(prefix_a));
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    /// M5 上限与轮转：活动日志超过 5 MiB 时轮转一次（旧段转 .jsonl.1，
    /// 只保留最近一段），新写入落在新文件。
    #[test]
    fn dispatch_log_rotates_once_at_size_limit() {
        let dir = temp_dir("dispatch-rotate");
        let state = EngineState::load(&dir).expect("load");
        state.append_dispatch(
            "send",
            json!({ "account": "+15550000000", "message": "seed" })
                .as_object()
                .expect("object"),
        );
        let salt: [u8; DISPATCH_SALT_BYTES] = std::fs::read(dispatch_salt_path(&dir))
            .expect("salt")
            .as_slice()
            .try_into()
            .expect("salt");
        let path = account_path(&dir, &salt, "+15550000000");
        let filler = format!(
            "{}\n",
            "x".repeat(usize::try_from(DISPATCH_LOG_LIMIT_BYTES - 8).expect("fits"))
        );
        std::fs::write(&path, filler).expect("near-limit log");
        state.append_dispatch(
            "send",
            json!({ "account": "+15550000000", "message": "after rotation" })
                .as_object()
                .expect("object"),
        );
        let rotated = rotated_dispatch_path(&path);
        assert!(rotated.is_file(), "旧段轮转到 .1");
        let current = std::fs::read_to_string(&path).expect("current log");
        assert_eq!(current.lines().count(), 1, "新段只含轮转后的写入");
        assert!(
            std::fs::metadata(&path).expect("meta").len() < 4096,
            "新段从零开始"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&rotated)
                    .expect("rotated meta")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "轮转段 0600"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M5 删除收口：只删该账号的当前段与轮转段（其他账号与兜底桶不受
    /// 影响），文件不存在时幂等无错；盐不存在时不创建目录。
    #[test]
    fn remove_dispatch_log_deletes_only_that_account() {
        let dir = temp_dir("dispatch-remove");
        let state = EngineState::load(&dir).expect("load");
        state.append_dispatch(
            "send",
            json!({ "account": "+15550000000", "message": "a" })
                .as_object()
                .expect("object"),
        );
        state.append_dispatch(
            "send",
            json!({ "account": "+15550001111", "message": "b" })
                .as_object()
                .expect("object"),
        );
        let salt: [u8; DISPATCH_SALT_BYTES] = std::fs::read(dispatch_salt_path(&dir))
            .expect("salt")
            .as_slice()
            .try_into()
            .expect("salt");
        let removed = account_path(&dir, &salt, "+15550000000");
        std::fs::write(rotated_dispatch_path(&removed), b"{}\n").expect("rotated segment");
        state.remove_dispatch_log("+15550000000");
        assert!(!removed.exists(), "当前段删除");
        assert!(!rotated_dispatch_path(&removed).exists(), "轮转段删除");
        assert!(
            account_path(&dir, &salt, "+15550001111").is_file(),
            "其他账号不受影响"
        );
        state.remove_dispatch_log("+15550000000");
        state.remove_dispatch_log("../../escape");
        state.remove_dispatch_log("_reserved");
        assert!(account_path(&dir, &salt, "+15550001111").is_file());
        let _ = std::fs::remove_dir_all(&dir);

        // 盐从未创建（没有写过日志）：删除不创建 engine-send-log 目录。
        let empty = temp_dir("dispatch-remove-empty");
        let state = EngineState::load(&empty).expect("load");
        state.remove_dispatch_log("+15550000000");
        assert!(
            !empty.join(DISPATCH_LOG_DIR).exists(),
            "无日志时删除不得重建目录/盐"
        );
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// M5 旧全局文件迁移：先 rename 为 .migrated 再按行归属（文件名加盐
    /// 哈希），不可归属行丢弃；成功后暂存文件删除，二次启动不重复。
    #[test]
    fn legacy_dispatch_log_migrates_attributable_lines_and_drops_rest() {
        let dir = temp_dir("dispatch-legacy");
        let legacy = dir.join(LEGACY_DISPATCH_LOG);
        let lines = [
            json!({
                "method": "send",
                "params": { "account": "+15550000000", "message": "legacy secret", "recipients": ["+15555550101"] }
            })
            .to_string(),
            json!({ "method": "sendTyping", "params": { "account": "+15550001111", "stop": true } })
                .to_string(),
            json!({ "method": "send", "params": { "message": "no account" } }).to_string(),
            json!({ "method": "send", "params": { "account": "../../evil", "message": "bad" } })
                .to_string(),
            "{not-json".to_owned(),
        ];
        std::fs::write(&legacy, lines.join("\n") + "\n").expect("legacy log");

        let state = EngineState::load(&dir).expect("load migrates");
        assert!(!legacy.exists(), "旧全局文件迁移后改名/清除");
        assert!(
            !dir.join(format!("{LEGACY_DISPATCH_LOG}.migrated")).exists(),
            "成功迁移后暂存文件不残留"
        );
        let salt: [u8; DISPATCH_SALT_BYTES] = std::fs::read(dispatch_salt_path(&dir))
            .expect("salt")
            .as_slice()
            .try_into()
            .expect("salt");
        let log_dir = dir.join(DISPATCH_LOG_DIR);
        let first = std::fs::read_to_string(account_path(&dir, &salt, "+15550000000"))
            .expect("migrated account A");
        assert!(first.contains(r#""migrated":true"#), "迁移标记: {first}");
        assert!(!first.contains("legacy secret"), "迁移同样脱敏: {first}");
        assert!(!first.contains("+15555550101"), "迁移号码零落盘: {first}");
        let entry: Value = serde_json::from_str(first.lines().next().expect("line")).expect("json");
        assert_eq!(entry["params"]["message"]["redacted"], json!("text"));
        assert_eq!(
            entry["params"]["recipients"],
            json!({ "redacted": "recipients", "count": 1 })
        );
        let second = std::fs::read_to_string(account_path(&dir, &salt, "+15550001111"))
            .expect("migrated account B");
        assert!(second.contains(r#""stop":true"#), "结构键保留: {second}");
        assert!(
            !log_dir.join("_unattributed.jsonl").exists(),
            "非法/缺账号行不迁移"
        );
        assert!(!dir.join("evil.jsonl").exists());
        // 第二次启动不重复迁移（旧文件已不存在）。
        drop(state);
        let reloaded = EngineState::load(&dir).expect("second load");
        assert_eq!(reloaded.accounts().len(), 0);
        let again =
            std::fs::read_to_string(account_path(&dir, &salt, "+15550000000")).expect("log");
        assert_eq!(again.lines().count(), 1, "一次性迁移");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M5 迁移幂等（崩溃面）：rename 已完成但追加中途崩溃时，残留
    /// .migrated 不重放；旧文件已被移走 → 下次启动不重复追加。
    #[test]
    fn legacy_dispatch_log_migration_does_not_replay_after_crash() {
        let dir = temp_dir("dispatch-legacy-crash");
        let staged = dir.join(format!("{LEGACY_DISPATCH_LOG}.migrated"));
        std::fs::write(
            &staged,
            json!({
                "method": "send",
                "params": { "account": "+15550000000", "message": "once" }
            })
            .to_string()
                + "\n",
        )
        .expect("staged legacy");
        // 模拟崩溃前已追加过一行（暂存文件尚未删除）。
        let state = EngineState::load(&dir).expect("load ignores staged file");
        assert!(
            dispatch_file_names(&dir).is_empty(),
            "暂存文件不重放（没有任何账号日志）"
        );
        assert!(staged.exists(), "崩溃残留保留（不重放、不静默删）");
        drop(state);
        let _ = EngineState::load(&dir).expect("reload");
        assert!(
            dispatch_file_names(&dir).is_empty(),
            "第二次启动同样不重放残留 .migrated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// M5 权限每次写入校正：文件被改成 0644、目录 0755 后，下一次 append
    /// 重新校正为 0600/0700。
    #[test]
    fn dispatch_log_permissions_are_corrected_on_every_write() {
        let dir = temp_dir("dispatch-modes");
        let state = EngineState::load(&dir).expect("load");
        state.append_dispatch(
            "send",
            json!({ "account": "+15550000000", "message": "x" })
                .as_object()
                .expect("object"),
        );
        let salt: [u8; DISPATCH_SALT_BYTES] = std::fs::read(dispatch_salt_path(&dir))
            .expect("salt")
            .as_slice()
            .try_into()
            .expect("salt");
        let path = account_path(&dir, &salt, "+15550000000");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_path = dir.join(DISPATCH_LOG_DIR);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("chmod file");
            std::fs::set_permissions(&dir_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod dir");
            state.append_dispatch(
                "send",
                json!({ "account": "+15550000000", "message": "y" })
                    .as_object()
                    .expect("object"),
            );
            assert_eq!(
                std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir_path)
                    .expect("dir meta")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 空时间戳桶与审计日志不算「有隔离凭证」：用户恢复/放弃后可正常启动。
    #[test]
    fn load_ignores_empty_quarantine_bucket_and_manifest() {
        let dir = temp_dir("quarantine-empty");
        let root = dir.join(ACCOUNTS_DIR).join(QUARANTINE_DIR);
        std::fs::create_dir_all(root.join("1700000000000")).expect("empty bucket");
        std::fs::write(root.join(QUARANTINE_LOG), b"").expect("manifest");
        let state = EngineState::load(&dir).expect("empty bucket/manifest must not block startup");
        assert!(state.accounts().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
