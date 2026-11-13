// SPDX-License-Identifier: AGPL-3.0-only

//! M6 C 阶段（engine 半边）：store key 通道 —— `KT_SIGNAL_STORE_KEY` 的
//! 形状解析与进程内三态（设计：docs/plan/engine-store-encryption.md
//! §7「C 阶段」）。
//!
//! 契约（C 阶段已定，见设计文档 §7 C 阶段矩阵）：
//!
//! - env `KT_SIGNAL_STORE_KEY`，两种合法编码：**64 个十六进制字符**（大小写
//!   都收，等价于 connector `StoreKey::from_hex` 的小写规范形），或 **base64
//!   的 32 字节**（标准/base64url、带不带 `=` 补位都收）；
//! - 解码结果必须恰 32 字节；其余一切形状（长度不对、字符集不对、base64
//!   解出 31/33 字节、值非 UTF-8、**空串**）→ [`StoreKeyState::Invalid`]，
//!   fail-closed（错误码 `StoreKeyInvalid`，一个库都不建）。空串按非法处理
//!   而不是按缺席：`KT_SIGNAL_STORE_KEY=` 是「想给 key 但没给成」的形态，
//!   静默回落明文创建正是设计 §6.2 F1 要拦的那种事故；
//! - key 材料**永不进日志/错误文本/指标**：本模块所有错误类型都不携带原文，
//!   `Display` 只有固定字面量，`Debug` 打码；key 只允许交给
//!   `SqliteConnectOptions::pragma("key", …)`；
//! - 进程内只读一次 env（[`StoreKeyState::process`] 的 `OnceLock`）：env 是
//!   启动期常量，重复读取只会在并发路径上制造抖动；三态（缺席/有效/非法）
//!   全部缓存，非法的证据行也只落一次（report 由调用方做）。
//!
//! 内存卫生（诚实版，照设计 §3.5）：key 会经 `SqliteConnectOptions` 常驻堆
//! （sqlx 池把 options 放在 `Arc` 里且不做 zeroize，连接每 10–30 分钟回收
//! 重建还要用它），所以本模块只承诺「env 原值读完即覆盖、不进日志、不进
//! argv」。
//!
//! **不承诺进程内清零**：进程退出时这些字节由 OS 回收，进程存活期间
//! `StoreKey` 的 `Drop` 只覆盖它自己那一份 `[u8; 32]` 副本（善意覆盖，
//! 不是安全边界：调用方可能已 clone、`pragma_literal()` 造出来的字面量
//! 会被 sqlx 移进 options 常驻到进程结束），本模块也拿不回那些副本的所有权。

use std::fmt;
use std::sync::OnceLock;

/// connector 侧同名 env（kt-signal-connector src/store.rs:98）。C 阶段它由
/// dev/测试覆盖或后续 connector 接线时由 launcher 注入：connector 现行实现
/// 仍会 `env_remove` 它（src/engine.rs:2695-2697），所以本阶段生产路径的
/// 现实取值恒为「缺席」→ 明文兼容路径（矩阵 a）。
pub(crate) const STORE_KEY_ENV: &str = "KT_SIGNAL_STORE_KEY";

/// key 字节数（SQLCipher raw key = 32 字节；connector 同值）。
const STORE_KEY_BYTES: usize = 32;

/// hex 编码长度（64 字符）。
const STORE_KEY_HEX_CHARS: usize = STORE_KEY_BYTES * 2;

/// key 形状非法。**不携带原文**：被拒的候选值仍然是密钥材料。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreKeyError {
    /// 既不是 64 hex，也不是 32 字节的 base64。
    InvalidShape,
}

impl fmt::Display for StoreKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "store key must be 64 hexadecimal characters or a base64 encoding of 32 bytes",
        )
    }
}

/// 32 字节 store key（SQLCipher raw key）。
///
/// `Debug` 打码（任何路径都不会把字节渲染进文本）；`Drop` 覆盖自己这份
/// 副本，但**进程内不保证清零**（不构成安全边界，见模块头「内存卫生」）。
#[derive(Clone)]
pub(crate) struct StoreKey([u8; STORE_KEY_BYTES]);

impl StoreKey {
    /// 解析 env/CI 传进来的字符串：64 hex（大小写均可）或 32 字节 base64。
    ///
    /// 前后空白先剥掉（`export KEY=$(cat keyfile)` 会带尾换行）；剥完为空 = 非法。
    pub(crate) fn parse(raw: &str) -> Result<Self, StoreKeyError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(StoreKeyError::InvalidShape);
        }
        let bytes = decode_hex(trimmed)
            .or_else(|| decode_base64(trimmed))
            .ok_or(StoreKeyError::InvalidShape)?;
        Ok(Self(bytes))
    }

    /// 由字节直接构造（单测与后续 DEK 包装面用）。
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: [u8; STORE_KEY_BYTES]) -> Self {
        Self(bytes)
    }

    /// SQLCipher raw-key 规格 `x'<64hex>'`，再按 sqlx 的渲染要求包一层双引号。
    ///
    /// sqlx 的 `pragma()` 原样拼 `PRAGMA key = <值>`（不加引号，见
    /// sqlx-sqlite 0.9 `options/connect.rs` 的 `pragma_string`），而 SQLCipher
    /// 的 raw key 形态要带引号才被解析器认（既有 P0 断言
    /// `src/store_cipher_tests.rs` 的 `keyed_store_is_unreadable_without_key`
    /// 同款形状）。返回值是**密钥材料**：只许交给 `.pragma("key", …)`，
    /// 不许进日志/错误/指标/任何 `format!` 后的文本。
    ///
    /// 这份 `String` **不可清零**：所有权随 `.pragma("key", …)` 移进
    /// `SqliteConnectOptions`（`Cow::Owned`）并常驻到进程结束，本模块拿不
    /// 回来；复制一份再清零只会让密钥材料多出一份，更差。所以这里不做
    /// 「用完即焚」的承诺（见模块头「内存卫生」）。
    pub(crate) fn pragma_literal(&self) -> String {
        let mut literal = String::with_capacity(STORE_KEY_HEX_CHARS + 5);
        literal.push('"');
        literal.push_str("x'");
        encode_hex_into(&mut literal, &self.0);
        literal.push_str("'\"");
        literal
    }
}

impl fmt::Debug for StoreKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoreKey(<redacted 32 bytes>)")
    }
}

impl Drop for StoreKey {
    /// best-effort：只覆盖本结构自己的字节副本（其它 clone / `pragma_literal`
    /// 字面量副本不受影响）。**不构成「进程内清零」保障**，进程退出由 OS 回收。
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// 进程内 key 三态（环境面结论）。
///
/// 放进 `OnceLock` 后进程内恒定：env 不会在运行期变，重复解析只会让
/// 「非法形状」在不同线程上得到不同解释。
pub(crate) enum StoreKeyState {
    /// 没有 key：矩阵 a（无 key + 无库 → 明文创建，兼容现状）。
    Absent,
    /// 有合法 key：矩阵 b/c/d 的分支由 store 文件状态决定（见 service）。
    Present(StoreKey),
    /// 形状非法：fail-closed，`StoreKeyInvalid`，一个库都不建。
    Invalid,
}

impl StoreKeyState {
    /// env 原始值 → 三态（`None` = 变量缺席；`Some` 走形状校验）。
    pub(crate) fn from_env_value(raw: Option<&str>) -> Self {
        match raw {
            None => Self::Absent,
            Some(value) => match StoreKey::parse(value) {
                Ok(key) => Self::Present(key),
                Err(_) => Self::Invalid,
            },
        }
    }

    /// 进程唯一结论（读 env 一次；正负都缓存）。
    ///
    /// 非 UTF-8 的 env 值按非法处理（拿不到可校验的字节序列）。
    ///
    /// 读入的这份拷贝在解析完**立刻覆盖**（best-effort：`String` → `Vec<u8>`
    /// 后 `fill(0)`，全程安全代码；不是「进程内清零」保障，见模块头）——
    /// env 原值是本模块唯一能拿回所有权的那份 key 材料，能清就清。
    pub(crate) fn process() -> &'static Self {
        static PROCESS_KEY: OnceLock<StoreKeyState> = OnceLock::new();
        PROCESS_KEY.get_or_init(|| match std::env::var_os(STORE_KEY_ENV) {
            None => Self::Absent,
            Some(raw) => Self::from_owned_env_value(raw),
        })
    }

    /// 由**本进程拥有的** env 值构造三态：解析后把这份拷贝覆盖掉再丢。
    fn from_owned_env_value(raw: std::ffi::OsString) -> Self {
        let Ok(value) = raw.into_string() else {
            // 非 UTF-8：拿不到可校验的字节序列 → 非法（fail-closed）。
            return Self::Invalid;
        };
        let mut bytes = value.into_bytes();
        let state = match std::str::from_utf8(&bytes) {
            Ok(text) => Self::from_env_value(Some(text)),
            Err(_) => Self::Invalid,
        };
        // best-effort：覆盖这一份；解析出的 `StoreKey` 是另一份副本。
        bytes.fill(0);
        state
    }

    /// 日志标签（不含任何材料）：`absent` / `present` / `invalid`。
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Present(_) => "present",
            Self::Invalid => "invalid",
        }
    }
}

/// 64 hex → 32 字节（大小写都收）。
fn decode_hex(raw: &str) -> Option<[u8; STORE_KEY_BYTES]> {
    let bytes = raw.as_bytes();
    if bytes.len() != STORE_KEY_HEX_CHARS {
        return None;
    }
    let mut key = [0_u8; STORE_KEY_BYTES];
    for (slot, pair) in key.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *slot = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(key)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// base64 → 32 字节：标准/base64url × 带/不带补位四种写法都收（跨仓调用
/// 方的编码习惯不同，形状校验的实质是「必须恰 32 字节」）。
fn decode_base64(raw: &str) -> Option<[u8; STORE_KEY_BYTES]> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    use base64::Engine as _;

    let decoded = STANDARD
        .decode(raw)
        .or_else(|_| STANDARD_NO_PAD.decode(raw))
        .or_else(|_| URL_SAFE.decode(raw))
        .or_else(|_| URL_SAFE_NO_PAD.decode(raw))
        .ok()?;
    decoded.as_slice().try_into().ok()
}

fn encode_hex_into(target: &mut String, bytes: &[u8]) {
    const HEX: [u8; 16] = *b"0123456789abcdef";
    for byte in bytes {
        target.push(HEX[usize::from(byte >> 4)] as char);
        target.push(HEX[usize::from(byte & 0x0f)] as char);
    }
}
