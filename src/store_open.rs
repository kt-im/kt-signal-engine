// SPDX-License-Identifier: AGPL-3.0-only

//! M6 C 阶段（engine 半边）：store 文件状态分类 + 打开失败的结构化错误码
//! （设计：docs/plan/engine-store-encryption.md §3.4 决策表 / §7「C 阶段」）。
//!
//! 三件事，互相独立：
//!
//! 1. [`classify`]：**读文件头**判明文/密文，不靠「打得开就算明文」
//!    （connector 先例 `db_file_state`）。分类只读元数据与 16 字节头，
//!    不建目录、不建库、不跑迁移；调用点必须把它排在能力门与 key 形状校验
//!    之后、`create_dir_all` 之前（顺序由 `service::open_store_with` 保证）。
//! 2. [`StoreOpenError`]：connector 可区分的**固定码面**——
//!    `code()` 给结构化码，`report_once` 落固定 stderr 行
//!    `kt-signal-engine: ERROR event=store_open_failed code=<Code> retryable=<bool>`
//!    （每进程每码恰一次；形状与 `SingleAccountRegistryViolation` 同款，
//!    只落字面量、无路径/号码/密钥）。`is_terminal()` 把**六类**终态
//!    （缺能力 / key 形状非法 / 缺 key / 错 key / 库损坏 / 开关 on 的明文库）
//!    与可重试失败分开：
//!    前者**不进**接收管线的 `Recoverable` 退避重连（错 key 重试一万次还是
//!    错 key，而且每次重试都会再碰一次盘）。**可重试面包括一切 IO 失败**
//!    （EACCES/EMFILE/EIO/只读挂载/路径不存在），它们不证明库内容坏了——
//!    把这类瞬时条件判成终态会让账号永久停接收，是本模块明确拆掉的误判。
//! 3. [`probe_keyed_store`]：加密库打开前的**强制首页读**（connector
//!    `open_encrypted` 同位）。错 key 在这里归一成 `StoreKeyRejected`，
//!    而不是等第一次业务查询才炸。
//!
//! 判定边界的诚实说明：
//!
//! - 「密文」没有 magic——SQLCipher 文件头是 16 字节随机盐，所以**非明文头
//!   一律归为 `Opaque`**：可能是加密库，也可能是垃圾文件。
//! - 错 key 与「第 1 页损坏」在 SQLCipher 里不可分辨（都是 HMAC 失败 →
//!   `not a database`）。本模块把「Opaque + 有 key + 首页读失败」判为
//!   `StoreKeyRejected`（与设计 §3.4 同口径），把**结构性**损坏
//!   （短于一页、明文头但内容非法、SQLite 报 malformed）判为 `StoreCorrupt`。
//!   第 1 页已损坏的加密库会被误判成错 key——两者都是终态、都要人工介入，
//!   现场靠 `code` 与文件本身区分。
//! - 只有 0 字节文件按 `Absent` 处理（OS 中断的 create 会留下空文件，
//!   SQLite 自己把它当空库）：没有数据可丢，交给打开路径新建。
//! - `Corrupt` 只在**稳定可证的结构事实**上成立：长度短于一页（铁定截断）、
//!   `store.db` 位置被非普通文件占住（目录/设备/FIFO——不会自愈的布局损坏）、
//!   SQLite 自己报 malformed/not-a-database。**读盘动作失败**（权限、句柄耗尽、
//!   EIO、只读挂载、并发替换导致的短读）一律 `Failed`（可重试）：这些条件
//!   外部一恢复就好，判终态等于把账号永久钉死在「不再接收」上。

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};

use crate::store_key::{StoreKey, StoreKeyState};

/// 明文 SQLite 文件头（前 16 字节）。密文库的前 16 字节是随机盐，不是它。
pub(crate) const PLAINTEXT_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// SQLite 允许的最小页大小（512B）：短于它的文件连一页都装不下，必然是
/// 截断件，不需要 key 就能判损。加密库默认页 4096（SQLCipher/sqlx 缺省），
/// 所以这个下界只用来拦「铁定坏了」的形态，不猜测具体页大小。
const MIN_SQLITE_FILE_BYTES: u64 = 512;

/// store.db 的磁盘形态（只看文件头，不看能否打开）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreFileState {
    /// 不存在或 0 字节。
    Absent,
    /// 明文 SQLite（头 = `SQLite format 3\0`）。
    Plaintext,
    /// 非明文头：加密库（或垃圾文件；两者都只能靠 key 试开区分）。
    Opaque,
}

/// 损坏的具体形态（只进 tracing 字段，不进固定 stderr 行）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CorruptDetail {
    /// 短于一页（铁定截断）。
    Truncated,
    /// 元数据/文件头读不出来，或 store.db 位置上不是普通文件。
    Unreadable,
    /// 头看着正常但 SQLite 自己报 malformed/not-a-database。
    Malformed,
}

impl CorruptDetail {
    fn label(self) -> &'static str {
        match self {
            Self::Truncated => "truncated",
            Self::Unreadable => "unreadable",
            Self::Malformed => "malformed",
        }
    }
}

/// C-1 终态开关的 env 名：`KT_SIGNAL_STORE_REQUIRE_ENCRYPTED`（**默认 off**）。
///
/// | 取值（去空白、大小写不敏感） | 结果 |
/// |---|---|
/// | 缺席 / 空串 / `off` / `0` / `false` / `no` | off：**今天的行为逐字节不变** |
/// | `on` / `1` / `true` / `yes` | on：明文库一律终态 |
/// | 其它（`enable` / `2` / `on!` …） | off + 一条滚动日志 warn |
///
/// 认不出的值**不**回落到 on：默认 off 是硬约束
/// （`docs/plan/engine-store-encryption.md` C-12），笔误不该把一批账号全打成
/// 终态；真要开的时候显式写 `on` / `1`。
pub(crate) const REQUIRE_ENCRYPTED_ENV: &str = "KT_SIGNAL_STORE_REQUIRE_ENCRYPTED";

/// 开关取值（第三态「认不出」由 [`parse_require_encrypted`] 的 `None` 表达，
/// 这里只留两个会改变行为的档）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequireEncrypted {
    /// 兼容路径：明文库照旧打开（有 key 时打一次 legacy warn）。
    Off,
    /// 严格路径：明文库（**无论有没有 key**）一律
    /// `StorePlaintextRequiresMigration` 终态。
    On,
}

impl RequireEncrypted {
    /// 唯一判据（别让调用点各写一遍 `== On`）。
    pub(crate) fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

/// 纯解析（单测直打）：`None` = 认不出，调用方按 off 兜底并留一条 warn。
pub(crate) fn parse_require_encrypted(raw: Option<&str>) -> Option<RequireEncrypted> {
    match raw.map(str::trim) {
        None | Some("") => Some(RequireEncrypted::Off),
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "off" | "0" | "false" | "no" => Some(RequireEncrypted::Off),
            "on" | "1" | "true" | "yes" => Some(RequireEncrypted::On),
            _ => None,
        },
    }
}

/// env 值 → 开关（认不出 = off）。
pub(crate) fn require_encrypted_from_env_value(raw: Option<&str>) -> RequireEncrypted {
    parse_require_encrypted(raw).unwrap_or(RequireEncrypted::Off)
}

/// 进程开关：每次打开读一次 env。没有 `OnceLock`——生产里没人中途改 env，
/// 而单测要能注入，读一次字符串比多一颗进程级状态便宜。
///
/// on 时落**一条**滚动日志 WARN（运维要能看出「不服务明文」是开关而不是
/// 故障）；认不出的值同样一条 WARN。两条都只进滚动日志，**不进 stderr**
/// ——stderr 的固定行只属于 [`StoreOpenError`] 的码面。
pub(crate) fn require_encrypted() -> RequireEncrypted {
    let raw = std::env::var(REQUIRE_ENCRYPTED_ENV).ok();
    let resolved = require_encrypted_from_env_value(raw.as_deref());
    if resolved.is_on() {
        require_encrypted_on_warn_once();
    } else if parse_require_encrypted(raw.as_deref()).is_none() {
        // 认不出 = off，但要让人在滚动日志里看见「你写的值没人认识」。
        warn_unrecognised_require_encrypted_once(raw.as_deref());
    }
    resolved
}

/// on 的证据行闸（每进程一条；`swap` 的返回值就是「我是不是第一个」）。
static REQUIRE_ENCRYPTED_ON_WARNS: AtomicU32 = AtomicU32::new(0);

fn require_encrypted_on_warn_once() {
    if REQUIRE_ENCRYPTED_ON_WARNS.swap(1, Ordering::SeqCst) == 1 {
        return;
    }
    tracing::warn!(
        target: "engine.store",
        stage = "store_open",
        switch = "require_encrypted",
        value = "on",
        "plaintext stores are refused with a terminal class; migrate them before turning this on"
    );
}

/// 认不出的值的闸（每进程一条；值本身只进滚动日志，不是密钥）。
static REQUIRE_ENCRYPTED_INVALID_WARNS: AtomicU32 = AtomicU32::new(0);

fn warn_unrecognised_require_encrypted_once(raw: Option<&str>) {
    if REQUIRE_ENCRYPTED_INVALID_WARNS.swap(1, Ordering::SeqCst) == 1 {
        return;
    }
    tracing::warn!(
        target: "engine.store",
        stage = "store_open",
        switch = "require_encrypted",
        value = raw.unwrap_or_default(),
        "unrecognised value for the require-encrypted switch; staying off"
    );
}

/// 打开 store 失败的分类（connector 面）。
///
/// 码是**契约字面量**：connector 侧按 `code=` 后的整词匹配，不做前缀/
/// 模糊匹配，所以这里的字符串与设计文档逐字一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreOpenError {
    /// 链接的 sqlite 不是 SQLCipher 构建（P0 能力门；证据行由门落，不重复）。
    CipherUnavailable,
    /// `KT_SIGNAL_STORE_KEY` 形状非法（fail-closed，不建库）。
    KeyInvalid,
    /// 没有 key 却必须走密文：终态，等 key 补齐，绝不静默新建明文库。
    ///
    /// 两个入口：① `store.db` 已是密文（`Opaque`）——没有 key 就打不开；
    /// ② C-1 开关 on 且库**不存在**（`Absent`）——开关要求密文，没 key 就建
    /// 不出密文库，此时放行 `create_if_missing` 等于开关 on 还在生产新的
    /// 明文库（比存量明文更糟：存量至少是迁移源）。
    ///
    /// 第二种入口归本码而不是 `PlaintextRequiresMigration`：盘上没有任何
    /// 东西要迁移（连账号目录都没建），缺的是 key。
    KeyRequired,
    /// 有 key 但解不开（错 key；或第 1 页损坏，见模块文档的边界说明）。
    KeyRejected,
    /// 库损坏（截断/不可读/malformed）。
    Corrupt(CorruptDetail),
    /// 明文库 + `KT_SIGNAL_STORE_REQUIRE_ENCRYPTED=on`（C-1）：**终态**，
    /// 等迁移器把库转成密文，或由运维显式把开关关回去。判定只看文件头，
    /// 与有没有 key 无关——无 key 的明文库放过去就等于开关没开（删掉 key
    /// 就能读明文，闸形同虚设）。拒绝路径不打开、不写盘、**不删明文库**
    /// （迁移器要拿它当源）。
    PlaintextRequiresMigration,
    /// 其余可重试失败（权限、句柄耗尽、EIO、只读挂载、路径、连接池、
    /// 迁移等）：保持既有重试语义，**不进**终态停机。
    Failed,
}

impl StoreOpenError {
    /// 结构化码（connector 与运维 grep 的唯一字面量）。
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::CipherUnavailable => "StoreCipherUnavailable",
            Self::KeyInvalid => "StoreKeyInvalid",
            Self::KeyRequired => "StoreKeyRequired",
            Self::KeyRejected => "StoreKeyRejected",
            Self::Corrupt(_) => "StoreCorrupt",
            Self::PlaintextRequiresMigration => "StorePlaintextRequiresMigration",
            Self::Failed => "StoreOpenFailed",
        }
    }

    /// 终态 = 不进退避重连。**实际六类**：`CipherUnavailable`（缺能力）、
    /// `KeyInvalid`（形状非法）、`KeyRequired`（缺 key）、`KeyRejected`
    /// （错 key / 首页损坏）、`Corrupt`（库损坏）、
    /// `PlaintextRequiresMigration`（开关 on + 明文库，与 key 无关）。
    ///
    /// `Failed` 是**唯一**可重试类：权限、句柄耗尽、EIO、只读挂载、路径
    /// 不存在、连接池/迁移失败都在这一类——它们不证明库内容坏了，等外部
    /// 条件恢复即可，判终态会让账号永久停接收。
    pub(crate) fn is_terminal(self) -> bool {
        !matches!(self, Self::Failed)
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Corrupt(detail) => detail.label(),
            Self::CipherUnavailable
            | Self::KeyInvalid
            | Self::KeyRequired
            | Self::KeyRejected
            | Self::PlaintextRequiresMigration
            | Self::Failed => "-",
        }
    }

    /// 固定 stderr 行（connector watchdog 逐行分类的**唯一契约形状**）。
    ///
    /// 七个码共用同一形状，只有 `code` 与 `retryable` 变化；只含字面量，
    /// 无路径/号码/密钥。C-1 的 `StorePlaintextRequiresMigration` 也在其中
    /// （`retryable=false`）：connector 的逐行分类不需要为它单开一种形状。
    /// `CipherUnavailable` 的这一行由能力门在首次判定时落
    /// （`src/store_cipher.rs` 的 `unavailable_stderr_line`，同一份形状），
    /// 本函数是两侧形状的唯一断言口径（`store_cipher_tests` 对拍）。
    pub(crate) fn stderr_line(self) -> String {
        format!(
            "kt-signal-engine: ERROR event=store_open_failed code={} retryable={}",
            self.code(),
            !self.is_terminal()
        )
    }

    /// 落**一次**证据（每进程每码一条）：tracing error 进滚动日志 + 一条
    /// 固定形状 stderr（connector watchdog 逐行分类的入口）。
    ///
    /// `CipherUnavailable` 是唯一例外：能力门在首次判定时已落过同形状那条
    /// （行由 `stderr_line()` 与门侧共享，见 `store_cipher`），这里再打就是
    /// 同一事实出现两遍。
    ///
    /// **可重试类也走这条固定行**（`retryable=true`）：权限/句柄/EIO 这类
    /// 失败同样要能被 connector 的逐行分类看见，否则「打不开盘」会退化成
    /// 只有滚动日志里的 warn。每码一次，重试不刷屏。
    pub(crate) fn report_once(self) {
        if self == Self::CipherUnavailable {
            return;
        }
        if self.slot().swap(1, Ordering::SeqCst) == 1 {
            return;
        }
        tracing::error!(
            target: "engine.store",
            stage = "store_open",
            code = self.code(),
            retryable = !self.is_terminal(),
            detail = self.detail(),
            "store open refused"
        );
        eprintln!("{}", self.stderr_line());
    }

    /// 落一次证据并把自己原样交回（`return Err(…)` / `map_err` 链上的统一
    /// 写法）：与 [`Self::report_once`] 同一道闸，重复调用仍只落一行。
    ///
    /// 存在的理由：`service::open_store_with` 里「路径非 UTF-8」「建目录失败
    /// （EACCES/EROFS/ENOSPC）」「连接串解析失败」以前是裸
    /// `StoreOpenError::Failed` 直接返回/ `?`，stderr **零行**——connector
    /// watchdog 按行分类，缺行等于这条失败路径根本不存在。改成
    /// `Err(StoreOpenError::Failed.reported())` 后每条失败路径都留同一条固定
    /// 行（只含字面量，无路径）。
    pub(crate) fn reported(self) -> Self {
        self.report_once();
        self
    }

    /// 每个码**各占一个**一次性闸（缺能力不走这里：门自己那道闸已经落过
    /// 同形状的行）。共用一个槽会让两个码互相压制，破坏「每进程每码一条」。
    fn slot(self) -> &'static AtomicU8 {
        static CIPHER: AtomicU8 = AtomicU8::new(0);
        static KEY_INVALID: AtomicU8 = AtomicU8::new(0);
        static KEY_REQUIRED: AtomicU8 = AtomicU8::new(0);
        static KEY_REJECTED: AtomicU8 = AtomicU8::new(0);
        static CORRUPT: AtomicU8 = AtomicU8::new(0);
        static PLAINTEXT_REQUIRES_MIGRATION: AtomicU8 = AtomicU8::new(0);
        static FAILED: AtomicU8 = AtomicU8::new(0);
        match self {
            Self::CipherUnavailable => &CIPHER,
            Self::KeyInvalid => &KEY_INVALID,
            Self::KeyRequired => &KEY_REQUIRED,
            Self::KeyRejected => &KEY_REJECTED,
            Self::Corrupt(_) => &CORRUPT,
            Self::PlaintextRequiresMigration => &PLAINTEXT_REQUIRES_MIGRATION,
            Self::Failed => &FAILED,
        }
    }

    /// 该码已落盘的条数（单测断言「每进程每码一条」；缺能力只可能是 0，
    /// 它那条由门的 `denial_reports` 计数）。
    #[cfg(test)]
    pub(crate) fn report_count(self) -> u8 {
        self.slot().load(Ordering::SeqCst)
    }
}

/// 明文库 + 有 key（矩阵 c）的 legacy 证据行：**每进程一条**。
///
/// 这是 D 阶段（迁移器）之前的有意停留：不设 key 打开（存量可读），
/// 也不悄悄加密/重写文件。日志只说明「这批库待迁移」，不含路径。
static LEGACY_PLAINTEXT_WARNS: AtomicU32 = AtomicU32::new(0);

pub(crate) fn warn_plaintext_legacy_once() {
    if LEGACY_PLAINTEXT_WARNS.swap(1, Ordering::SeqCst) == 1 {
        return;
    }
    tracing::warn!(
        target: "engine.store",
        stage = "store_open",
        code = "StorePlaintextLegacy",
        key = "present",
        store = "plaintext",
        "plaintext store opened without a key; encrypted migration lands in phase D"
    );
}

/// 已落盘的 legacy 证据行条数（单测断言「每进程只打一次」）。
#[cfg(test)]
pub(crate) fn legacy_plaintext_warn_count() -> u32 {
    LEGACY_PLAINTEXT_WARNS.load(Ordering::SeqCst)
}

/// 清零 legacy warn 闸（**只给测试**）。
///
/// 这个闸是进程级一次性的，而 cargo 并行跑用例：凡是会触发 legacy 路径的
/// 用例都得先拿串行锁再归零，否则「warn 由这次 open 触发」的断言会被并行
/// 用例提前推高的计数搅掉（见 `store_encryption_tests` 的 `LEGACY_WARN_SERIAL`）。
#[cfg(test)]
pub(crate) fn reset_legacy_plaintext_warn() {
    LEGACY_PLAINTEXT_WARNS.store(0, Ordering::SeqCst);
}

/// 读 store.db 的磁盘形态（只读；不建目录/不建库）。
///
/// 分类判据只有两条：**稳定的结构事实**判 `Corrupt`（不存在之外的
/// 元数据事实：长度短于一页、位置上不是普通文件）；**读盘动作失败**
/// 一律 `Failed`（可重试）。这条边界是硬约束：EACCES（ACL 改动）、
/// EMFILE（句柄回收）、EIO（介质抖动）、只读挂载（挂载参数变更）都是
/// 「这一次读不到」，不是「内容坏了」；判成终态会让接收管线永久停摆
/// （判错了没人会来重试，判成可重试最多多一次 stat）。
pub(crate) fn classify(path: &Path) -> Result<StoreFileState, StoreOpenError> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoreFileState::Absent);
        }
        // 权限/符号环/ENOTDIR/EIO/EMFILE…：读不到 ≠ 内容坏 → 可重试。
        Err(_) => return Err(StoreOpenError::Failed),
    };
    if !metadata.is_file() {
        // 目录/设备/FIFO 占住 store.db：不是瞬时 IO，也不会自愈（没有人会
        // 自动把目录换回文件），属于**布局损坏**——与 key 无关，也不需要靠
        // 重试等奇迹，判 `Corrupt(Unreadable)`（终态、留现场、等人工）。
        return Err(StoreOpenError::Corrupt(CorruptDetail::Unreadable));
    }
    if metadata.len() == 0 {
        return Ok(StoreFileState::Absent);
    }
    if metadata.len() < MIN_SQLITE_FILE_BYTES {
        // 长度是**元数据事实**：短于一页就是截断件，不需要读内容。
        return Err(StoreOpenError::Corrupt(CorruptDetail::Truncated));
    }
    let mut head = [0_u8; PLAINTEXT_HEADER.len()];
    let mut file = std::fs::File::open(path).map_err(|_| StoreOpenError::Failed)?;
    // 短读（UnexpectedEof）= 元数据说有 ≥512B、读的这一刻不够 16B：多半是
    // 并发替换/截断（写侧事故），下一次分类会按当时的长度重判。宁可多试
    // 一次也不把它钉成终态；EIO/EINTR 同理。
    file.read_exact(&mut head)
        .map_err(|_| StoreOpenError::Failed)?;
    Ok(if &head == PLAINTEXT_HEADER {
        StoreFileState::Plaintext
    } else {
        StoreFileState::Opaque
    })
}

/// 加密库打开前的验真：带 key 建一条短连接，强制读一页（`sqlite_master`）。
///
/// 只读、`create_if_missing(false)`（绝不因为 key 错就造一个新库）；连接
/// 用完即关，不进池。判定结论：
///
/// - 读成功 → key 可解，放行真正的打开动作；
/// - `not a database` / code 26 → 错 key（[StoreOpenError::KeyRejected]）；
/// - `malformed` 等 → 损坏（[StoreOpenError::Corrupt]）；
/// - 其余（路径/权限/连接）→ [StoreOpenError::Failed]，保持可重试。
pub(crate) async fn probe_keyed_store(path: &Path, key: &StoreKey) -> Result<(), StoreOpenError> {
    let path = path.to_str().ok_or(StoreOpenError::Failed)?;
    let options = path
        .parse::<SqliteConnectOptions>()
        .map_err(|_| StoreOpenError::Failed)?
        .create_if_missing(false)
        .foreign_keys(true)
        .pragma("key", key.pragma_literal());
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .map_err(|error| classify_failure_message(&error.to_string(), true))?;
    let probe = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_master")
        .fetch_one(&mut connection)
        .await;
    match probe {
        Ok(_) => Ok(()),
        Err(error) => Err(classify_failure_message(&error.to_string(), true)),
    }
}

/// 把 sqlx 的错误文本归一到 [StoreOpenError]（文本匹配是 sqlx 层唯一
/// 稳定的判据：`SqliteStoreError` 只透传 sqlite 的 message/code）。
///
/// `keyed` = 这次打开是否带了 key：同一句 `not a database` 在带 key 时
/// 说明 key 不对（或首页损坏），在明文路径上只能说明文件坏了。
pub(crate) fn classify_failure_message(message: &str, keyed: bool) -> StoreOpenError {
    let text = message.to_ascii_lowercase();
    let not_a_database =
        text.contains("not a database") || text.contains("code 26") || text.contains("encrypted");
    if not_a_database {
        return if keyed {
            StoreOpenError::KeyRejected
        } else {
            StoreOpenError::Corrupt(CorruptDetail::Malformed)
        };
    }
    if text.contains("malformed") || text.contains("corrupt") {
        return StoreOpenError::Corrupt(CorruptDetail::Malformed);
    }
    StoreOpenError::Failed
}

/// 启动期 key 证据（`main.rs` 在能力证据之后、任何 store 打开之前调用）。
///
/// - 有效/缺席：落一条 WARN 证据（落滚动日志，不进 stderr）；
/// - 非法：落**固定 stderr 行**（`code=StoreKeyInvalid`），每进程一次，
///   connector 不必等第一次 store 打开就能识别。
pub(crate) fn report_startup_key_state() {
    let state = StoreKeyState::process();
    match state {
        StoreKeyState::Invalid => StoreOpenError::KeyInvalid.report_once(),
        StoreKeyState::Absent => tracing::warn!(
            target: "engine.startup",
            stage = "store_key",
            key = state.label(),
            "store key absent; plaintext store creation stays the compatibility path"
        ),
        StoreKeyState::Present(_) => tracing::warn!(
            target: "engine.startup",
            stage = "store_key",
            key = state.label(),
            "store key present; new stores are created encrypted"
        ),
    }
}
