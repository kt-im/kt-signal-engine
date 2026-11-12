// SPDX-License-Identifier: AGPL-3.0-only

//! M6 P0：store 静态加密**能力门禁**的运行时半边（设计：
//! `docs/plan/engine-store-encryption.md` §2.2 / §7 P0）。
//!
//! 背景（该文档失败模式 F5「半实施」）：`PRAGMA key` 在纯 SQLite 上是
//! **静默 no-op**——代码加了 key、测试全绿、磁盘上仍是明文。编译期兜底是
//! 两个 bin 顶部的 `compile_error!`（`src/main.rs` 与
//! `src/bin/backup5-spike.rs`：bin target 各自独立编译，门要各写一份，
//! feature 没开就编译失败）；本模块是运行期兜底：链接进来的 sqlite 必须
//! 真的能回答 `PRAGMA cipher_version`（该 pragma 只存在于 SQLCipher 构建
//! 里，纯 SQLite 静默忽略未知 pragma，得到 0 行），否则 store 一律打不开
//! （fail closed，而不是「打开了但是明文」）。
//!
//! 纪律：
//! - 探测连 `:memory:`，不碰任何账号库文件、不建文件、不改迁移、不改连接
//!   池参数（P0 边界：不引入 KEK/DEK，不写密钥）；
//! - 只读 `cipher_version`（SQLCipher 编译期常量串），本模块全程没有密钥
//!   面，日志行也不含路径/号码/密钥；
//! - 判定结论进程内**正负都缓存**（[`CipherGate`]）：成功结论一个
//!   `OnceLock`，失败结论进 [`NegativeCache`]（同样是 `OnceLock`）。能力
//!   是**构建属性**，不是每次开库的可变状态；失败也缓存 → 后续 `open_store`
//!   只回同一个错误，不重复探测 `:memory:`、不重复摊连接；
//! - 失败证据行（tracing error + stderr）每进程**只落一次**（[`CipherGate`]
//!   的 `denial_reports` 一次性闸）；重试路径不再刷日志；
//! - 门必须**前置**在 `open_with_options` 之前：打开动作会 create_if_missing
//!   建 `store.db`、起连接池、跑 `sqlx::migrate!()` 落 schema，门后置就等于
//!   「先落明文库再报错」。判定位置由 `service::open_store` 保证。
//!
//! 本文件被两个 bin 同源引用：主 bin（`src/main.rs` 的 `mod store_cipher`）
//! 与开发 bin（`src/bin/backup5-spike.rs` 的
//! `#[path = "../store_cipher.rs"] mod store_cipher`）。所以本文件**不引用**
//! `crate::` 下的其它模块；依赖 `open_store` 的断言放在
//! `src/store_cipher_tests.rs`（只编进主 bin 的测试面）。

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};

/// 能力探测语句。SQLCipher 返回 `"<major>.<minor>.<patch> <build>"`（本图
/// 实测 `4.10.0 community`，产品名不在这串里）；纯 SQLite 静默忽略未知
/// pragma → 0 行。
pub(crate) const CIPHER_VERSION_PRAGMA: &str = "PRAGMA cipher_version";

/// 固定失败码（错误/日志面唯一形状；照 compat §1 只落字面量 cause）。
pub(crate) const CIPHER_UNAVAILABLE_CAUSE: &str = "StoreCipherUnavailable";

/// 能力探测失败。两类都按「不可用」fail closed，区分只为日志可读。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CipherProbeError {
    /// `PRAGMA cipher_version` 无值/空串 = 链接的是纯 SQLite（feature 未生效）。
    Unavailable,
    /// 探测连接或查询本身失败（底层文本只入日志）。
    Probe(String),
}

impl fmt::Display for CipherProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str(
                "PRAGMA cipher_version returned no value (linked sqlite is not a SQLCipher build)",
            ),
            Self::Probe(detail) => write!(formatter, "cipher_version probe failed: {detail}"),
        }
    }
}

/// 失败结论的**负缓存**（`OnceLock`，评审第 2 条）。
///
/// 缺能力是构建属性、不会自愈：失败一次即定终身。缓存住它，后续
/// `open_store` 直接回同一个错误——不重探 `:memory:`、不再刷证据行。
/// （一过性连接失败同样被缓存：同一个进程里重试的收益远小于每次开库摊一条
/// 探测连接 + 一行 error 的代价，且真正的判定证据首行已落盘。）
pub(crate) struct NegativeCache {
    slot: OnceLock<CipherProbeError>,
}

impl NegativeCache {
    const fn new() -> Self {
        Self {
            slot: OnceLock::new(),
        }
    }

    fn get(&self) -> Option<CipherProbeError> {
        self.slot.get().cloned()
    }

    fn set(&self, error: &CipherProbeError) {
        // 并发首探时后到的 `set` 失败属于预期（几个结论等价），忽略即可。
        let _ = self.slot.set(error.clone());
    }
}

/// 能力门：正缓存 + 负缓存 + 探测计数 + 一次性的失败证据行。
///
/// 生产路径只有进程唯一实例 [`gate`]（能力是构建属性，全局一份就够）；单测
/// 可以自建实例直打同一份逻辑，从而**不动**全局结论、不污染同进程其它断言。
pub(crate) struct CipherGate {
    /// 成功结论（去空白后的版本串）。
    granted: OnceLock<String>,
    /// 失败结论（负缓存）。
    negative: NegativeCache,
    /// 真正发起的探测次数（首个判定者探一次；结论落缓存后不再探测）。
    probes: AtomicU32,
    /// 失败证据行已落盘的次数（0/1；每进程只落一次）。
    denial_reports: AtomicU32,
    /// 仅测试：强制探测结论（造「缺能力」路径而不依赖坏构建）。
    #[cfg(test)]
    forced: Option<CipherProbeError>,
}

impl CipherGate {
    pub(crate) const fn new() -> Self {
        Self {
            granted: OnceLock::new(),
            negative: NegativeCache::new(),
            probes: AtomicU32::new(0),
            denial_reports: AtomicU32::new(0),
            #[cfg(test)]
            forced: None,
        }
    }

    /// 一次能力判定：命中任一缓存即返回，不建探测连接。
    ///
    /// 首次判定失败时：写负缓存 + 落**一条**证据行（后面再判定只回缓存错误）。
    pub(crate) async fn ensure(&self) -> Result<String, CipherProbeError> {
        if let Some(version) = self.granted.get() {
            return Ok(version.clone());
        }
        if let Some(error) = self.negative.get() {
            return Err(error);
        }
        match self.probe().await {
            Ok(version) => {
                let _ = self.granted.set(version.clone());
                Ok(version)
            }
            Err(error) => {
                self.negative.set(&error);
                self.report_denial_once(&error);
                Err(error)
            }
        }
    }

    /// 真探一次（`:memory:` 连接 + `PRAGMA cipher_version`）。
    async fn probe(&self) -> Result<String, CipherProbeError> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        #[cfg(test)]
        if let Some(forced) = &self.forced {
            return Err(forced.clone());
        }
        read_cipher_version().await
    }

    /// 失败证据行：`compare_exchange` 抢唯一名额，**每进程只落一次**
    /// （tracing error + stderr 各一行，同一事实只出现一遍）。
    fn report_denial_once(&self, error: &CipherProbeError) {
        if self
            .denial_reports
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        tracing::error!(
            target: "engine.store",
            stage = "store_cipher",
            code = CIPHER_UNAVAILABLE_CAUSE,
            cause = %error,
            "refusing to open store: linked sqlite has no cipher_version"
        );
        eprintln!(
            "kt-signal-engine: store open refused cause={CIPHER_UNAVAILABLE_CAUSE} (linked sqlite reports no cipher_version; build with feature \"sqlcipher\")"
        );
    }

    /// 已发起的探测次数（单测断言「失败不重探」）。
    #[cfg(test)]
    pub(crate) fn probes(&self) -> u32 {
        self.probes.load(Ordering::SeqCst)
    }

    /// 已落盘的失败证据行条数（单测断言「每进程只打一次」）。
    #[cfg(test)]
    pub(crate) fn denial_reports(&self) -> u32 {
        self.denial_reports.load(Ordering::SeqCst)
    }

    /// 仅测试：造一个「缺能力」的门（不依赖坏构建，也不动全局实例）。
    #[cfg(test)]
    pub(crate) fn incapable_for_tests(error: CipherProbeError) -> Self {
        Self {
            forced: Some(error),
            ..Self::new()
        }
    }
}

/// 进程唯一的能力门（`open_store` 与启动证据共用这一份结论）。
static GLOBAL_GATE: CipherGate = CipherGate::new();

/// 取进程唯一能力门（单测可直接传自建实例，见 `service::open_store_gated`）。
pub(crate) fn gate() -> &'static CipherGate {
    &GLOBAL_GATE
}

/// 真读一次 `PRAGMA cipher_version`（每次新建 `:memory:` 连接，不缓存；
/// 缓存语义在 [`CipherGate`]）。
///
/// 返回去空白后的非空版本串；空集/空串 = 链接的不是 SQLCipher。
pub(crate) async fn read_cipher_version() -> Result<String, CipherProbeError> {
    let options = SqliteConnectOptions::new().in_memory(true);
    let mut connection = SqliteConnection::connect_with(&options)
        .await
        .map_err(|error| CipherProbeError::Probe(error.to_string()))?;
    let version: Option<String> = sqlx::query_scalar(CIPHER_VERSION_PRAGMA)
        .fetch_optional(&mut connection)
        .await
        .map_err(|error| CipherProbeError::Probe(error.to_string()))?;
    match version.as_deref().map(str::trim) {
        Some(version) if !version.is_empty() => Ok(version.to_owned()),
        _ => Err(CipherProbeError::Unavailable),
    }
}

/// 能力结论（正负都缓存：进程内最多探测一次，之后直接回缓存）。
pub(crate) async fn ensure_cipher_capable() -> Result<String, CipherProbeError> {
    gate().ensure().await
}

/// 落盘证据行的唯一形状：`SQLCipher <cipher_version>`。
///
/// SQLCipher 自己的 `cipher_version` 是 `4.10.0 community`（不含产品名），
/// 所以「这是 SQLCipher 构建」的标识由本标签承担——日志对账时能直接 grep
/// `SQLCipher`。不含密钥、路径、号码。
pub(crate) fn capability_line(version: &str) -> String {
    format!("SQLCipher {version}")
}

/// 启动期打一次能力证据（`main.rs` 在 `logfile::init` 之后、任何 store
/// 打开之前调用）。
///
/// 级别取 `warn` 是**落盘契约**而不是严重性判断：滚动日志订阅器钉
/// `LevelFilter::WARN`（`src/logfile.rs` MAX_LEVEL，info 不落盘）。这里只
/// 落证据、不阻断启动；真正的闸在 `open_store`（fail closed，门在打开之前）。
///
/// 失败分支**不**再打日志：`CipherGate::report_denial_once` 已在本次（即本
/// 进程第一次）判定失败时落下唯一一条 error/stderr；启动阶段重复一遍会让同一
/// 事实在日志里出现两次（评审第 2 条：每进程只打一次）。
pub(crate) async fn report_startup_capability() {
    if let Ok(version) = ensure_cipher_capable().await {
        tracing::warn!(
            target: "engine.startup",
            stage = "store_cipher",
            cipher = %capability_line(&version),
            "store encryption capability verified"
        );
    }
}
