// SPDX-License-Identifier: AGPL-3.0-only

//! 进程装配：argv 解析（严格模式）+ 出口代理解析链 + 异步 stdio 主循环。
//!
//! 启动契约见 `docs/jsonrpc-compat.md` §1：connector 以
//! `--data-dir <dir> jsonRpc [--receive-mode on-start] [--ignore-attachments]
//! --ignore-stories --ignore-stickers` 拉起本进程；JVM 相关环境变量必须被
//! 忽略。
//!
//! 出口代理解析链（compat §1.1，同步段一次定案，首个命中生效）：① 显式
//! `KT_SIGNAL_SOCKS_PROXY=host:port`（非空非法 exit 2）→ 覆写
//! `ALL_PROXY=socks5h://…`；② 标准 `ALL_PROXY`/`all_proxy`（大写优先，scheme
//! 白名单 socks5/socks5h/http，命中即原样生效不改写）；③ macOS `scutil
//! --proxy` 系统代理（HTTP 路优先于 SOCKS 路，字段缺失/畸形逐路降级，scutil
//! 进程失败整体降级，非 macOS 跳过此级）→ 写 `ALL_PROXY=http://…` 或
//! `socks5h://…`；④ 直连。socks5h 必须用（远端解析）而非 socks5（本地解析）：
//! 被墙环境下本地 DNS 对 Signal 域名投毒，本地解析拿到假 IP 再经代理 CONNECT
//! 仍连到假地址（JVM signal-cli 同坑）；远端解析把域名交给代理出口侧解析。
//! 显式/系统两级统一写 `ALL_PROXY`，由 libsignal-service 的 reqwest 传输面
//! （HTTP + WebSocket 同一 client）统一走该出口。最终网络路径在启动同步段以
//! 恰一条 stderr INFO 行声明（direct/explicit-env/std-env/system-http/
//! system-socks + 具体地址），供排障对账；解析失败只降级不 panic。stdin EOF
//! 是唯一干净退出路径（exit 0）；argv 非法 exit 2；内部致命 exit 1（含
//! stdout writer 死亡——写失败/任务 panic 记 error 日志后 exit 1，交给
//! connector supervision 重启，不留「进程活着但出口已死」的僵尸）。stderr
//! 除该 INFO 行与代理配置错误行外保持安静（connector watchdog 只把
//! websocket 异常形状的行判死活，INFO 行刻意避开该形状；不落消息正文/号码/
//! URI/密钥）。
//!
//! P1b 装配：stdin 有界读搬到阻塞线程（P1a 逻辑原样），经 channel 桥进
//! tokio；响应与 receive 通知共用单一 stdout writer 队列（行序由队列保证，
//! 连接器按 id/有无 id 路由）；请求 dispatch 与接收管线跑在本线程 LocalSet
//! （presage future 持有非 Send 状态，进不了 tokio::spawn），finishLink
//! 阻塞不饿死其他方法；启动即为每个已登记账号拉起接收管线。

// M6 P0 编译期能力门禁（设计：docs/plan/engine-store-encryption.md §7 P0）。
// `PRAGMA key` 在纯 SQLite 上是静默 no-op——漏开 feature 会得到「代码在跑、
// 测试全绿、磁盘仍是明文」的半实施（该文档失败模式 F5）。本仓是 bin-only
// crate（无 lib.rs），main.rs 即 crate 根，门禁放这里覆盖全部 target；
// `cargo build --no-default-features` 直接编译失败，运行时兜底见
// src/store_cipher.rs + open_store。
#[cfg(not(feature = "sqlcipher"))]
compile_error!(
    "engine requires the sqlcipher feature: PRAGMA key is a silent no-op on plain SQLite"
);

/// 契约修订单源（version / engine.protocolCapabilities 两面共取）。
mod contract;
#[allow(dead_code)]
mod dispatch_model;
mod history;
mod jsonrpc;
mod logfile;
mod methods;
mod outbox_model;
mod proto;
mod service;
mod state;
/// M6 P0 store 加密能力门禁（运行时半边：cipher_version 探测 + fail-closed
/// 证据行；设计见 docs/plan/engine-store-encryption.md §7 P0）。本文件被
/// `src/bin/backup5-spike.rs` 以 `#[path]` 同源引入，所以它不引用 `crate::`
/// 下的其它模块。
mod store_cipher;
/// 能力门禁断言（依赖 `service::open_store`，因此只编进主 bin 的测试面）。
#[cfg(test)]
mod store_cipher_tests;
/// C 阶段 store 加密矩阵断言（同样依赖 `service::open_store_with`）。
#[cfg(test)]
mod store_encryption_tests;
/// M6 C 阶段：store key 通道（`KT_SIGNAL_STORE_KEY` 的形状解析 + 进程内三态；
/// 设计见 docs/plan/engine-store-encryption.md §7「C 阶段」）。
mod store_key;
/// M6 C 阶段：store 文件状态分类 + 打开失败的结构化错误码（connector 可区分
/// 的固定码面；同一份设计文档 §3.4 决策表 / §7 C 阶段）。
mod store_open;

// Android 本地备份导出面（engine-compat 备份批）：内部 Rust API，装配链由
// 消费批任务层（service::backup_task，契约 §10.3/§10.4）接线。
mod backup;

use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use methods::Inbound;
use state::EngineState;

/// 入站单帧字节上限（160 MiB，含行尾换行）：与 connector 的帧预算对齐
/// （`DEFAULT_HOST_FRAME_LIMIT` / `DEFAULT_UPSTREAM_LINE_LIMIT` = 160 MiB，
/// 其契约要求一条最大附件的 base64 恒能装进单个请求帧）。send 的附件
/// 以 `data:` URI 内联在 JSON-RPC 帧里（单附件上限 100MB，见
/// `service::ATTACHMENT_MAX_BYTES`），base64 膨胀 4/3 后约 127 MiB，信封
/// 余量足够。旧值 1 MiB 与该契约冲突：4.4MB 视频（≈5.9MB 单帧）会被拒成
/// parse error，connector 把无 id 的 parse error 判为协议故障、标记
/// engine faulted 并杀掉子进程（2026-10-19 实证）。
/// 超限行吞到行尾后按 -32700 拒绝（进程不死），拒绝面落结构化 warn
/// （帧字节数 + 上限，不含任何内容字节）。
///
/// M4 评估（与桌面/connector 帧合约漂移）：不下调到历史
/// 参考值 16 MiB——100MB 附件的 base64 单帧约 133 MiB，下调会
/// 直接切断附件发送；也不上调（不放大帧面）。内存上界改由
/// INBOUND_QUEUE_CAPACITY + INBOUND_PENDING_BYTES_BUDGET 兜底。
pub(crate) const MAX_LINE_BYTES: usize = 160 * 1024 * 1024;

/// 入站通道容量（帧数，M4）：stdin 读线程与 dispatch 环之间的有界桥。
/// 队列满时读线程 blocking_send 阻塞（OS 管道 + connector 写端背压），
/// 不再像无界 channel 一样把整帧无限堆进内存。
pub(crate) const INBOUND_QUEUE_CAPACITY: usize = 64;

/// 入站队列在途字节上限（M4/M5 内存面）：帧数有界之外再给内存上界。
/// 160MiB 单帧帽 × 64 槽的朴素组合可达 10GiB，故按字节再兜一层——
/// 队列里最多积压 2 个满帧；读线程在预算用尽时等待 dispatch 环取走帧。
pub(crate) const INBOUND_PENDING_BYTES_BUDGET: usize = 2 * MAX_LINE_BYTES;

/// 同刻在飞请求上限（M4）：超限帧立即回结构化 busy 错误（不静默丢、
/// 不用 spawn 无界膨胀请求队列）。
pub(crate) const INFLIGHT_REQUEST_LIMIT: usize = 8;

const USAGE: &str = "usage: kt-signal-engine --data-dir <dir> jsonRpc \
[--receive-mode <mode>] [--ignore-attachments] [--ignore-stories] [--ignore-stickers]";

struct LaunchArgs {
    data_dir: PathBuf,
}

/// 严格 argv 解析：未知参数一律失败（与上游对未知参数 fail-closed 同口径）。
/// JVM 模式的 `-D…` 前缀参数是 native 启动器形态，引擎非 JVM，同样拒绝。
fn parse_launch_args<I: Iterator<Item = std::ffi::OsString>>(args: I) -> Result<LaunchArgs, ()> {
    let mut data_dir: Option<PathBuf> = None;
    let mut saw_json_rpc = false;
    let mut iter = args;
    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("--data-dir") => {
                let value = iter.next().ok_or(())?;
                data_dir = Some(PathBuf::from(value));
            }
            Some("--receive-mode") => {
                iter.next().ok_or(())?;
            }
            Some("--ignore-attachments" | "--ignore-stories" | "--ignore-stickers") => {}
            Some("jsonRpc") => saw_json_rpc = true,
            _ => return Err(()),
        }
    }
    if !saw_json_rpc || data_dir.is_none() {
        return Err(());
    }
    Ok(LaunchArgs {
        data_dir: data_dir.unwrap_or_default(),
    })
}

/// 出口代理注入变量：connector 以同一变量把代理组代理传给引擎子进程（与
/// signal-cli JVM 形态共用环境面），桌面探测链路也写它。解析链中优先级最高
/// （compat §1.1 第 1 级）。
const SOCKS_PROXY_ENV: &str = "KT_SIGNAL_SOCKS_PROXY";

/// 标准代理环境变量（compat §1.1 第 2 级）：大写优先，小写兜底——与
/// reqwest/hyper-util 的 `get_first_env(["ALL_PROXY", "all_proxy"])` 同序，
/// 引擎决策与传输面实际行为不脱节。
const STD_PROXY_ENV: &str = "ALL_PROXY";
const STD_PROXY_ENV_FALLBACK: &str = "all_proxy";

/// 标准环境变量 scheme 白名单（大小写不敏感）。`https://`/`socks4://` 等
/// hyper-util 虽认但不在本链采纳范围：引擎只对自己能验证形状的值声明路径。
const STD_PROXY_SCHEMES: [&str; 3] = ["socks5", "socks5h", "http"];

/// 代理 host 校验口径（socks_proxy_url 与 scutil 系统代理解析共用）：非空、
/// 无空白与 ':'、≤253 字节——与 connector 侧 `SocksProxy::parse` 一致。
fn is_valid_proxy_host(host: &str) -> bool {
    !host.is_empty() && host.len() <= 253 && !host.chars().any(|c| c.is_whitespace() || c == ':')
}

/// 校验 host:port 并返回 `socks5h://host:port` 代理 URL。socks5h = 域名在
/// 代理侧远端解析（reqwest 语义），防本地 DNS 污染把 CONNECT 指到假 IP。
fn socks_proxy_url(value: &str) -> Result<String, &'static str> {
    let (host, port) = value.trim().split_once(':').ok_or("expected host:port")?;
    if !is_valid_proxy_host(host) {
        return Err("invalid proxy host");
    }
    let port: u16 = port.parse().map_err(|_| "invalid proxy port")?;
    if port == 0 {
        return Err("invalid proxy port");
    }
    Ok(format!("socks5h://{host}:{port}"))
}

/// 标准环境变量命中判定：非空、全值无空白（URI 不含裸空白，含则 reqwest 必
/// 拒——命中会造成日志路径失真）、scheme ∈ 白名单且 `://` 后余量非空。命中
/// 返回 trim 后的原值（原样生效，引擎不改写——reqwest 原生读同一变量）。
fn standard_proxy_env_hit(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    let (scheme, rest) = value.split_once("://")?;
    if rest.is_empty() {
        return None;
    }
    STD_PROXY_SCHEMES
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
        .then_some(value)
}

/// `scutil --proxy` 输出解析出的单条系统代理：scheme 取 "http"（→
/// `http://host:port`）或 "socks5h"（→ `socks5h://host:port`）。
struct ScutilProxy {
    scheme: &'static str,
    host: String,
    port: u16,
}

/// 解析 `scutil --proxy` 输出（`key : value` 行的平面字典）。只认六键：
/// HTTPEnable/HTTPProxy/HTTPPort 与 SOCKSEnable/SOCKSProxy/SOCKSPort。HTTP 与
/// SOCKS 两路同时有效时优先 HTTP（http 代理经 CONNECT 同样覆盖 https/wss 流
/// 量）。使能位非 "1"、字段缺失、host 非法（is_valid_proxy_host 口径）或端
/// 口非法（非数值/0）→ 该路作废；HTTP 路作废落 SOCKS 路，两路皆废 → None
/// （调用方降级到链的下一级）。字典包裹行（`<dictionary> {`/`}`）、空白行与
/// 未知键（含 ProxyAutoConfig* PAC 面）自然忽略。
fn parse_scutil_proxy(output: &str) -> Option<ScutilProxy> {
    let mut http_enabled = false;
    let mut http_proxy: Option<&str> = None;
    let mut http_port: Option<&str> = None;
    let mut socks_enabled = false;
    let mut socks_proxy: Option<&str> = None;
    let mut socks_port: Option<&str> = None;
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "HTTPEnable" => http_enabled = value == "1",
            "HTTPProxy" => http_proxy = Some(value),
            "HTTPPort" => http_port = Some(value),
            "SOCKSEnable" => socks_enabled = value == "1",
            "SOCKSProxy" => socks_proxy = Some(value),
            "SOCKSPort" => socks_port = Some(value),
            _ => {}
        }
    }
    scutil_leg(http_enabled, http_proxy, http_port, "http")
        .or_else(|| scutil_leg(socks_enabled, socks_proxy, socks_port, "socks5h"))
}

/// scutil 单路（HTTP 或 SOCKS）命中判定：使能 + host/port 双合法，任一缺失
/// 或畸形即整路作废（None），不发明部分命中。
fn scutil_leg(
    enabled: bool,
    host: Option<&str>,
    port: Option<&str>,
    scheme: &'static str,
) -> Option<ScutilProxy> {
    if !enabled {
        return None;
    }
    let host = host?;
    if !is_valid_proxy_host(host) {
        return None;
    }
    let port: u16 = port?.parse().ok()?;
    (port != 0).then_some(ScutilProxy {
        scheme,
        host: host.to_owned(),
        port,
    })
}

/// 代理解析链的最终决策：来源 + 生效的代理地址（Direct 无地址）。
enum ProxyDecision {
    Direct,
    ExplicitEnv(String),
    StdEnv(String),
    SystemHttp(String),
    SystemSocks(String),
}

impl ProxyDecision {
    /// 日志标记词——compat §1.1 契约面，与文档逐一对应。
    fn label(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::ExplicitEnv(_) => "explicit-env",
            Self::StdEnv(_) => "std-env",
            Self::SystemHttp(_) => "system-http",
            Self::SystemSocks(_) => "system-socks",
        }
    }

    /// 日志明细：代理地址或直连说明。
    fn detail(&self) -> &str {
        match self {
            Self::Direct => "no proxy configured",
            Self::ExplicitEnv(url)
            | Self::StdEnv(url)
            | Self::SystemHttp(url)
            | Self::SystemSocks(url) => url,
        }
    }
}

/// 代理解析链（compat §1.1，优先级从高到低，首个命中生效）。入参全部显式
/// 注入（env 读取与 scutil 读取留在 `setup_proxy_env`），纯函数可单测：
/// - `explicit`：`KT_SIGNAL_SOCKS_PROXY` 原值（None = 未设置）。非空非法 →
///   Err（启动 exit 2，fail-closed）；空值视为未配置。
/// - `all_proxy`/`all_proxy_fallback`：标准变量原值，大写优先。
/// - `scutil_output`：`scutil --proxy` 原始输出；None = 非 macOS 平台或读取
///   失败（该级整体跳过）。
fn resolve_proxy_chain(
    explicit: Option<&str>,
    all_proxy: Option<&str>,
    all_proxy_fallback: Option<&str>,
    scutil_output: Option<&str>,
) -> Result<ProxyDecision, &'static str> {
    if let Some(value) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(ProxyDecision::ExplicitEnv(socks_proxy_url(value)?));
    }
    if let Some(hit) = all_proxy
        .and_then(standard_proxy_env_hit)
        .or_else(|| all_proxy_fallback.and_then(standard_proxy_env_hit))
    {
        return Ok(ProxyDecision::StdEnv(hit.to_owned()));
    }
    if let Some(proxy) = scutil_output.and_then(parse_scutil_proxy) {
        let url = format!("{}://{}:{}", proxy.scheme, proxy.host, proxy.port);
        return Ok(match proxy.scheme {
            "http" => ProxyDecision::SystemHttp(url),
            _ => ProxyDecision::SystemSocks(url),
        });
    }
    Ok(ProxyDecision::Direct)
}

/// `scutil --proxy` 读取（仅 macOS 编译面）。带 2s 上限：scutil 挂死（罕见
/// 的 configd 卡死面）不得阻塞引擎启动——超时杀进程降级。spawn 失败（无
/// scutil）、非零退出、输出非 UTF-8（lossy 兜底后由解析面判废）均降级，
/// 不 panic。
#[cfg(target_os = "macos")]
fn read_scutil_proxy() -> Option<String> {
    use std::io::Read;
    const SCUTIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    let mut child = std::process::Command::new("scutil")
        .arg("--proxy")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        // 读失败按 EOF 处理：拿到多少算多少，废形交给解析面降级。
        let _ = stdout.read_to_end(&mut buffer);
        let _ = tx.send(buffer);
    });
    let buffer = match rx.recv_timeout(SCUTIL_TIMEOUT) {
        Ok(buffer) => {
            // stdout EOF 即进程已终（scutil 不 fork），就地回收避免僵尸。
            let exited = child.wait().map(|status| status.success()).unwrap_or(false);
            if !exited {
                return None;
            }
            buffer
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// 非 macOS：无 scutil 解析面，恒缺（链上第 3 级整体跳过）。
#[cfg(not(target_os = "macos"))]
fn read_scutil_proxy() -> Option<String> {
    None
}

/// 代理解析链执行 + `ALL_PROXY` 落位 + 启动 INFO 行。必须在任何
/// libsignal-service reqwest client 构建之前生效（reqwest 缺省读环境代理
/// 面，HTTP 与 WebSocket 共用同一 client），因此放在两段式 `main` 的同步
/// 段：此刻进程单线程（tokio worker 与 stdin 读线程都尚未启动），set_var
/// 无并发读者。StdEnv 命中不写（变量已在环境里，reqwest 原生读取同语义）；
/// Direct 不写也不清（维持现状语义）。
fn setup_proxy_env() -> Result<(), &'static str> {
    let explicit = std::env::var(SOCKS_PROXY_ENV).ok();
    let all_proxy = std::env::var(STD_PROXY_ENV).ok();
    let all_proxy_fallback = std::env::var(STD_PROXY_ENV_FALLBACK).ok();
    let scutil_output = read_scutil_proxy();
    let decision = resolve_proxy_chain(
        explicit.as_deref(),
        all_proxy.as_deref(),
        all_proxy_fallback.as_deref(),
        scutil_output.as_deref(),
    )?;
    match &decision {
        ProxyDecision::ExplicitEnv(url)
        | ProxyDecision::SystemHttp(url)
        | ProxyDecision::SystemSocks(url) => {
            std::env::set_var(STD_PROXY_ENV, url);
        }
        ProxyDecision::StdEnv(_) | ProxyDecision::Direct => {}
    }
    eprintln!(
        "kt-signal-engine: INFO network path: {} ({})",
        decision.label(),
        decision.detail()
    );
    Ok(())
}

fn main() -> ExitCode {
    if let Err(message) = setup_proxy_env() {
        eprintln!("kt-signal-engine: invalid {SOCKS_PROXY_ENV}: {message}");
        return ExitCode::from(2);
    }
    // Debug builds carry much larger async-state frames than release. Group
    // typing (send_typing_group -> send_message_to_group -> new_message_sender)
    // overflowed the 8 MiB main-thread stack and aborted the whole engine with
    // SIGABRT. Give the single-thread runtime a bounded 32 MiB stack so debug
    // and release use the same runtime semantics; if the host refuses the
    // thread, fall back to the process main stack rather than failing startup.
    const ENGINE_STACK_BYTES: usize = 32 * 1024 * 1024;
    match std::thread::Builder::new()
        .name("kt-signal-engine-main".into())
        .stack_size(ENGINE_STACK_BYTES)
        .spawn(amain)
    {
        Ok(handle) => handle.join().unwrap_or(ExitCode::FAILURE),
        Err(_) => amain(),
    }
}

/// 单账号护栏开关（S5）：池化后每个引擎只应承载一个账号，防止误把多账号
/// data-dir 指给池槽（设计 §9 S5.4 / 失败模式 I3）。仅显式 `=1` 开启；
/// 其他值/缺席视为关闭（保留今天多账号兼容路径）。
const SINGLE_ACCOUNT_ENV: &str = "KT_ENGINE_SINGLE_ACCOUNT";

/// 纯判定：开关打开且注册表账号数 > 1 才拒绝启动。
fn single_account_guard_violation(enabled: bool, account_count: usize) -> bool {
    enabled && account_count > 1
}

/// 开关解析（去空白后恒等 `"1"`；与 `KT_ENGINE_FAKE_SERVICE` 同款严格口径）。
fn single_account_guard_enabled_from(raw: Option<&str>) -> bool {
    raw.map(str::trim) == Some("1")
}

fn single_account_guard_enabled() -> bool {
    single_account_guard_enabled_from(std::env::var(SINGLE_ACCOUNT_ENV).ok().as_deref())
}

/// 启动期单账号护栏失败：结构化落滚动日志 + 一条固定形状 stderr 错误
/// （connector 会转发 stderr；不含号码/正文/密钥）。退出码 1 = 致命状态。
fn report_single_account_violation(accounts: usize) {
    tracing::error!(
        target: "engine.startup",
        stage = "single_account_guard",
        code = "SingleAccountRegistryViolation",
        accounts,
        limit = 1,
        "KT_ENGINE_SINGLE_ACCOUNT=1 but the registry contains more than one account; refusing to start"
    );
    eprintln!(
        "kt-signal-engine: ERROR event=startup_failed code=SingleAccountRegistryViolation accounts={accounts} limit=1 (KT_ENGINE_SINGLE_ACCOUNT=1 expects one account per data-dir)"
    );
}

// S5：池化后每个引擎一账号，多线程 runtime 的 per-core worker 是纯固定成本
// （空引擎 20–21 线程）。装配本来就是单线程 LocalSet + 一个 stdout writer /
// 一个证书维护 tokio::spawn 任务（均为 Send future），current_thread 足以
// 驱动；阻塞 stdin 读仍在独立 std::thread。长任务与收流共享同一线程是已知
// 取舍（设计 §13 决策 8）。
#[tokio::main(flavor = "current_thread")]
async fn amain() -> ExitCode {
    let args = match parse_launch_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(()) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    // 引擎自身滚动文件日志（send 可观测批 2026-10-16，compat §1）：先于
    // 任何服务活动安装；失败静默降级（无 stderr 输出，IPC 服务不受影响）。
    logfile::init(&args.data_dir);
    // M6 P0：链接的 sqlite 必须是 SQLCipher 构建（`cipher_version` 只在这个
    // 构建里存在）。每进程打一次证据（`cipher=SQLCipher <version>`，不含
    // 密钥）；缺能力时证据行由能力门在本次判定里落一条（error + stderr），
    // 真正的闸在 open_store，且前置在任何磁盘动作/打开动作之前（fail closed，
    // 缺能力时不落 store.db、不建账号目录）。
    store_cipher::report_startup_capability().await;
    // M6 C 阶段：store key 证据（`KT_SIGNAL_STORE_KEY` 的形状结论）。有效/
    // 缺席落一条滚动日志 warn；形状非法落**固定 stderr 行**
    //（`event=store_open_failed code=StoreKeyInvalid`），每进程一次——connector
    // 不必等第一次 store 打开就能把这类故障与网络故障分开。
    store_open::report_startup_key_state();
    // vendor 发送路径分段计时回注（send 性能批 2026-10-16，compat §5）：
    // 单向钩子一次性安装，缺省 no-op；键词表仍由 perf_metrics 封闭管理
    // （vendor 字面量漂移被 perf_metrics 单测钉红）。
    presage::libsignal_service::perf_hook::install(|key, us| {
        service::perf_metrics::record_us(key, us)
    })
    .expect("perf hook installs exactly once per process");
    if let Err(error) = service::cleanup_abandoned_link_stores(&args.data_dir).await {
        eprintln!("kt-signal-engine: abandoned link store cleanup failed: {error}");
        return ExitCode::from(1);
    }
    let state = match EngineState::load(&args.data_dir) {
        Ok(state) => state,
        Err(_) => return ExitCode::from(1),
    };
    // S5 单账号护栏：注册表 >1 账号 = 该 data-dir 属于多账号进程，池槽
    // 误指会破坏「一账号一进程」的隔离前提，fail closed（P0-B 的清扫逻辑
    // 不参与：这里在清扫之前退出，不动任何账号目录）。
    if single_account_guard_violation(single_account_guard_enabled(), state.accounts().len()) {
        report_single_account_violation(state.accounts().len());
        return ExitCode::from(1);
    }
    let state = Arc::new(AsyncMutex::new(state));

    // stdout writer：响应与 receive 通知的唯一出口（行序 = 队列序）。Notice
    // 逐条写+flush；Flush 是背压栅栏（此前全部 Notice 已写进 stdout 并 flush
    // ——连接器至少收到字节——才解决 oneshot）。receive 管线 ack 前等栅栏
    // （时序审计 G1：ack 晚于「连接器可读」，连接器死亡窗口内引擎崩溃也不
    // 丢已投影消息——服务器队列未耗，重启重放）。
    // 全局队列有界（稳定性批，2026-10-20）：连接器读得慢时 writer 反压
    // 到 send().await，而不是把响应/投影堆进无界内存；每账号队列的隔离
    // 背压见 service::outbound。
    //
    // writer 死亡监视线程（僵尸出口批）：stdout writer 是唯一出口，写失败/
    // panic 后不能留「进程活着但永不再产出」的假活——入站环 select 该
    // JoinHandle，死亡即记录日志并以 exit 1 退出，由 connector supervision
    // 重启整条引擎（顺带回收 WS 连接与 store）。
    let (outbound_tx, outbound_rx) =
        mpsc::channel::<OutboundFrame>(service::outbound::OUTBOUND_GLOBAL_CAPACITY);
    let outbound = service::outbound::OutboundHub::new(outbound_tx);
    let mut writer = tokio::spawn(run_outbound_writer(
        outbound_rx,
        tokio::io::BufWriter::new(tokio::io::stdout()),
    ));

    // stdin 有界读（阻塞线程）→ 有界桥进 tokio（M4）：队列容量 64 帧 +
    // 在途字节预算 2 帧，队列/预算满时读线程阻塞（背压到 OS 管道与
    // connector 写端），不再无界堆帧。
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<Inbound>(INBOUND_QUEUE_CAPACITY);
    let inbound_budget = Arc::new(InboundBudget::new(INBOUND_PENDING_BYTES_BUDGET));
    let reader_budget = Arc::clone(&inbound_budget);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line: Vec<u8> = Vec::new();
        loop {
            line.clear();
            let mut handle = stdin.lock();
            match read_bounded_line(&mut handle, &mut line) {
                ReadOutcome::Eof => break,
                ReadOutcome::Line => {
                    while matches!(line.last(), Some(b'\n' | b'\r')) {
                        line.pop();
                    }
                    let frame = std::mem::take(&mut line);
                    let queued_bytes = frame.len();
                    // 先占字节预算再入队：预算满时阻塞在 condvar（不是丢帧）。
                    reader_budget.reserve(queued_bytes);
                    if inbound_tx.blocking_send(Inbound::Line(frame)).is_err() {
                        reader_budget.release(queued_bytes);
                        break;
                    }
                }
                ReadOutcome::Overlong { bytes } => {
                    if inbound_tx
                        .blocking_send(Inbound::Overlong { bytes })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    // presage 的 Manager/接收流 future 持有非 Send 状态（ThreadRng、
    // dyn ProtocolStore），请求 dispatch 与接收管线统一跑在本线程 LocalSet。
    let local = tokio::task::LocalSet::new();
    // writer 死亡结局（None = stdin EOF 的干净收尾）。fatal = 需要 exit 1
    // 让 connector supervision 重启；日志在 report_writer_exit 单点落。
    let mut writer_exit: Option<WriterExit> = None;
    let mut writer_fatal = false;
    local
        .run_until(async {
            let service =
                service::Service::new(args.data_dir, Arc::clone(&state), Arc::clone(&outbound));
            service.spawn_receive_pipelines().await;

            writer_exit = run_engine_input_loop(
                Arc::clone(&state),
                Arc::clone(&service),
                Arc::clone(&outbound),
                &mut inbound_rx,
                &inbound_budget,
                &mut writer,
            )
            .await;

            match &writer_exit {
                Some(exit) => {
                    writer_fatal = report_writer_exit(exit);
                    if writer_fatal {
                        // stderr 固定形状（无正文/号码）：给连接器一个立即的
                        // 异常退出证据；退出码 1 才触发 supervision 重启。
                        eprintln!(
                            "kt-signal-engine: ERROR event=engine_exit code=StdoutWriterDead (stdout writer died; expecting supervisor restart)"
                        );
                    }
                }
                None => {
                    tracing::warn!(
                        target: "engine.lifecycle",
                        stage = "stdin_eof",
                        "engine stdin reached EOF; shutting down"
                    );
                }
            }

            // A4 干净退出：停全部接收管线并等待收口（在飞信封处理臂完成、
            // Manager drop 时 SQLite store 刷盘），再退出 LocalSet。
            service.shutdown_all().await;
        })
        .await;

    // 收尾：丢弃 outbound 发送端并 abort writer（在途通知/响应与等待许可的
    // notify 立即落错，不悬挂；有界队列语义见 service::outbound）。
    // stdin EOF 干净退出 0；writer 死亡（写失败/panic）exit 1 → supervision
    // 重启，绝不以「进程活着、出口已死」的形态继续持写端。
    drop(outbound);
    writer.abort();
    if writer_fatal {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// JSON-RPC 入站分派环（LocalSet 内）：select 同时监视线程 stdout writer。
///
/// 返回 Some(exit) = writer 先死（写失败/panic/通道关闭），主装配据此记录
/// 日志并决定是否 exit 1；返回 None = stdin EOF 的干净收尾。writer 单独 spawn
/// 且只在这里被 await，死亡不再静默——否则 stdout 写失败后仅 flush 栅栏
/// 超时、接收管线停摆，进程仍持写端假活。
async fn run_engine_input_loop(
    state: Arc<AsyncMutex<EngineState>>,
    service: Arc<service::Service>,
    outbound: Arc<service::outbound::OutboundHub>,
    inbound_rx: &mut mpsc::Receiver<Inbound>,
    inbound_budget: &InboundBudget,
    writer: &mut tokio::task::JoinHandle<WriterExit>,
) -> Option<WriterExit> {
    // 在飞请求许可（M4）：取不到许可的帧立即回结构化 busy，绝不排队等
    // 待、绝不丢弃。许可随 dispatch 任务持有到应答入队，写出口慢时在飞
    // 数也不会超过上限。
    let inflight = Arc::new(tokio::sync::Semaphore::new(INFLIGHT_REQUEST_LIMIT));
    loop {
        tokio::select! {
            inbound = inbound_rx.recv() => {
                let inbound = inbound?;
                inbound_budget.release(inbound.queued_bytes());

                // 解析只做一次：原始帧在产出 Prepared 后立即释放（160MiB
                // 大帧不再横跨整段 dispatch 生命周期）。
                let prepared = methods::prepare(&inbound);
                drop(inbound);

                // 坏行/超帧是廉价的解析错误面：不占在飞许可、
                // 不受 busy 拒绝，也不为它们 spawn 无限任务。
                if !matches!(prepared, methods::Prepared::Request(_)) {
                    let response = methods::respond_prepared(
                        prepared,
                        Arc::clone(&state),
                        Arc::clone(&service),
                    )
                    .await;
                    let _ = outbound
                        .send_global(OutboundFrame::notice(response))
                        .await;
                    continue;
                }

                let permit = match Arc::clone(&inflight).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!(
                            target: "engine.dispatch",
                            stage = "inflight_limit",
                            limit = INFLIGHT_REQUEST_LIMIT,
                            "request rejected: in-flight limit reached"
                        );
                        // 背压优先：busy 应答走同一 stdout 队列；队列关闭
                        // 说明 writer 已死，下一轮 select 的 writer 臂接管。
                        let response = methods::busy_response(&prepared);
                        let _ = outbound
                            .send_global(OutboundFrame::notice(response))
                            .await;
                        continue;
                    }
                };

                let state = Arc::clone(&state);
                let service = Arc::clone(&service);
                let outbound = Arc::clone(&outbound);
                tokio::task::spawn_local(async move {
                    let budget = methods::request_timeout_budget(&prepared);
                    let method = methods::prepared_method(&prepared).unwrap_or("-").to_owned();
                    let timeout_id = methods::prepared_id(&prepared);
                    let response = respond_with_budget(
                        budget,
                        methods::respond_prepared(prepared, state, service),
                        || {
                            tracing::warn!(
                                target: "engine.dispatch",
                                stage = "request_timeout",
                                method = %method,
                                budget_ms = budget.as_millis() as u64,
                                "rpc request timed out; dispatch future cancelled"
                            );
                            methods::timeout_response(&timeout_id, budget)
                        },
                    )
                    .await;
                    let _ = outbound.send_global(OutboundFrame::notice(response)).await;
                    // 应答已入队（或出口已死）后才释放在飞许可。
                    drop(permit);
                });
            }
            joined = &mut *writer => {
                return Some(join_writer(joined));
            }
        }
    }
}

/// 超时包裹单点（M5）：预算内正常返回；超时则 drop（取消）dispatch
/// future 并调用 on_timeout 造结构化错误面。抽成泛型便于单测钉定
/// 「超时确实取消 future」而不依赖真实 Service。
async fn respond_with_budget<F>(
    budget: std::time::Duration,
    future: F,
    on_timeout: impl FnOnce() -> Value,
) -> Value
where
    F: std::future::Future<Output = Value>,
{
    match tokio::time::timeout(budget, future).await {
        Ok(response) => response,
        Err(_) => on_timeout(),
    }
}

/// 入站队列字节预算（M4）：读线程与 LocalSet 分属两个线程，阻塞等待用
/// std condvar（不能在阻塞线程上 await tokio Semaphore）。
struct InboundBudget {
    pending: std::sync::Mutex<usize>,
    drained: std::sync::Condvar,
    capacity: usize,
}

impl InboundBudget {
    fn new(capacity: usize) -> Self {
        Self {
            pending: std::sync::Mutex::new(0),
            drained: std::sync::Condvar::new(),
            capacity,
        }
    }

    /// 预留字节；预算不足时等待 dispatch 环取走帧（保留 1 帧的推进余量：
    /// 即使单帧本身等于容量，也不让读写互锁）。
    fn reserve(&self, bytes: usize) {
        let bytes = bytes.min(self.capacity);
        let mut pending = self.pending.lock().expect("inbound budget lock");
        while *pending > 0 && pending.saturating_add(bytes) > self.capacity {
            pending = self.drained.wait(pending).expect("inbound budget wait");
        }
        *pending += bytes;
    }

    /// 帧被 dispatch 环取走后释放预算。
    fn release(&self, bytes: usize) {
        let mut pending = self.pending.lock().expect("inbound budget lock");
        *pending = pending.saturating_sub(bytes.min(self.capacity));
        self.drained.notify_all();
    }
}

enum ReadOutcome {
    Line,
    Eof,
    /// 超限帧：吞到行尾后返回。`bytes` 是该帧实际消耗的字节数（含行尾
    /// 换行），只进拒绝日志；帧内容不进任何 IPC 面（本地读缓冲由下一帧
    /// 的 clear 复位）。
    Overlong {
        bytes: u64,
    },
}

/// stdout 出站帧。`Notice` 是一行 JSON（响应或 receive 通知）；`Flush` 是
/// 时序审计 G1 的背压栅栏——writer 在此前全部 Notice 已写并 flush 后解决
/// oneshot，消费方（receive 管线 ack）据此把「连接器至少收到」变成显式
/// 等待点。
///
/// `permit` 是单账号在途许可（service::outbound 信号量）；writer 写完并
/// flush 后帧析构才释放，保证单账号洪峰不会无限占用全局队列。响应/测试
/// 直发帧无许可（`None`）。
pub(crate) enum OutboundFrame {
    Notice {
        value: Value,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    },
    Flush {
        done: tokio::sync::oneshot::Sender<()>,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    },
}

impl OutboundFrame {
    /// 无账号许可的 Notice（JSON-RPC 响应 / writer 单测直发）。
    pub(crate) fn notice(value: Value) -> Self {
        Self::Notice {
            value,
            permit: None,
        }
    }
}

/// stdout writer 退出原因（supervision 面：writer 是进程唯一出口，死亡不
/// 得静默——否则连接器会看到一个「进程还活着但永不再产出」的僵尸，
/// flush 栅栏失败后接收管线停摆而进程仍在持写端假活）。
#[derive(Debug)]
pub(crate) enum WriterExit {
    /// 出站队列发送端全部弃置（引擎正常收尾）。
    ChannelClosed,
    /// 写 stdout 失败（连接器死亡/管道破裂/半开写不出去）。stage 区分
    /// write_all 与 flush 两段。
    WriteFailed {
        stage: &'static str,
        error: std::io::Error,
    },
    /// writer 任务 panic（JoinError；不落 payload，防正文入日志）。
    Panicked,
    /// writer 任务被 abort（正常收尾的主动取消）。
    Aborted,
}

/// JoinHandle 收口：panic / 取消归类（不静默丢弃 JoinError）。
fn join_writer(joined: Result<WriterExit, tokio::task::JoinError>) -> WriterExit {
    match joined {
        Ok(exit) => exit,
        Err(error) if error.is_panic() => WriterExit::Panicked,
        Err(_) => WriterExit::Aborted,
    }
}

/// writer 死亡结局 → 日志 + 是否请求进程退出（exit 1，交给 connector
/// supervision 重启整条引擎；writer 已死时原地不退等于持写端假活）。
fn report_writer_exit(exit: &WriterExit) -> bool {
    match exit {
        WriterExit::ChannelClosed => {
            tracing::warn!(
                target: "engine.lifecycle",
                stage = "writer_channel_closed",
                "stdout writer stopped after the outbound queue closed"
            );
            false
        }
        WriterExit::WriteFailed { stage, error } => {
            tracing::error!(
                target: "engine.lifecycle",
                stage = "writer_failed",
                write_stage = *stage,
                error = %error,
                "stdout writer failed; exiting for connector supervision restart"
            );
            true
        }
        WriterExit::Panicked => {
            tracing::error!(
                target: "engine.lifecycle",
                stage = "writer_panicked",
                "stdout writer task panicked; exiting for connector supervision restart"
            );
            true
        }
        WriterExit::Aborted => {
            tracing::warn!(
                target: "engine.lifecycle",
                stage = "writer_aborted",
                "stdout writer task was aborted"
            );
            false
        }
    }
}

/// writer 主循环：Notice 逐条 write+flush（任一步失败即携原因退出，栅栏
/// oneshot 随帧弃置自然落在等待方 false）；Flush 就地解决（FIFO 保证此前
/// Notice 已 flush）。`permit` 在本臂结束（写完+flush 之后）才随帧释放。
async fn run_outbound_writer<W>(mut rx: mpsc::Receiver<OutboundFrame>, mut out: W) -> WriterExit
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    while let Some(frame) = rx.recv().await {
        match frame {
            OutboundFrame::Notice { value, permit } => {
                let mut text = serde_json::to_string(&value).expect("line serializes");
                text.push('\n');
                if let Err(error) = out.write_all(text.as_bytes()).await {
                    return WriterExit::WriteFailed {
                        stage: "write_all",
                        error,
                    };
                }
                if let Err(error) = out.flush().await {
                    return WriterExit::WriteFailed {
                        stage: "flush",
                        error,
                    };
                }
                drop(permit);
            }
            OutboundFrame::Flush { done, permit } => {
                let _ = done.send(());
                drop(permit);
            }
        }
    }
    WriterExit::ChannelClosed
}

/// flush 栅栏的通道侧实现（`Service::flush_barrier` 委托；自由函数便于
/// 单测直打协议面）。出口关闭立即 false；writer 已死则栅栏 oneshot 随
/// 接收端弃置，等待落 false。
#[cfg(test)]
pub(crate) async fn outbound_flush_barrier(tx: &mpsc::Sender<OutboundFrame>) -> bool {
    let (done, wait) = tokio::sync::oneshot::channel();
    if tx
        .send(OutboundFrame::Flush { done, permit: None })
        .await
        .is_err()
    {
        return false;
    }
    wait.await.is_ok()
}

/// 有界按行读：不预分配无界缓冲；超限行吞到行尾后报 `Overlong`（携带该帧
/// 实际字节数供拒绝日志）。按 `fill_buf` 分块读写，避免逐字节读取在大帧
/// （附件 data URI 常达数 MB）上的调用开销。
fn read_bounded_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> ReadOutcome {
    read_bounded_line_with_limit(reader, buf, MAX_LINE_BYTES)
}

fn read_bounded_line_with_limit(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    limit: usize,
) -> ReadOutcome {
    let mut total: u64 = 0;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                return if buf.is_empty() {
                    ReadOutcome::Eof
                } else {
                    ReadOutcome::Line
                }
            }
        };
        if available.is_empty() {
            return if buf.is_empty() {
                ReadOutcome::Eof
            } else {
                ReadOutcome::Line
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let chunk = newline.unwrap_or(available.len());
        total += chunk as u64 + u64::from(newline.is_some());
        if buf.len().saturating_add(chunk) > limit {
            let consumed = chunk + usize::from(newline.is_some());
            reader.consume(consumed);
            return match newline {
                Some(_) => ReadOutcome::Overlong { bytes: total },
                None => drain_to_line_end(reader, total),
            };
        }
        buf.extend_from_slice(&available[..chunk]);
        let consumed = chunk + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            return ReadOutcome::Line;
        }
    }
}

/// 超限帧的丢弃尾段：继续按块吞到行尾（或 EOF），只累计字节数。
fn drain_to_line_end(reader: &mut impl BufRead, mut total: u64) -> ReadOutcome {
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(_) => return ReadOutcome::Overlong { bytes: total },
        };
        if available.is_empty() {
            return ReadOutcome::Overlong { bytes: total };
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(newline) => {
                total += newline as u64 + 1;
                reader.consume(newline + 1);
                return ReadOutcome::Overlong { bytes: total };
            }
            None => {
                let len = available.len();
                total += len as u64;
                reader.consume(len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::socks_proxy_url;
    use super::{
        outbound_flush_barrier, parse_scutil_proxy, resolve_proxy_chain, run_outbound_writer,
        single_account_guard_enabled_from, single_account_guard_violation, standard_proxy_env_hit,
        OutboundFrame, ProxyDecision,
    };
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// S5 单账号护栏：只有「显式 =1 且注册表 >1 账号」才拒绝启动；
    /// 关闭态/单账号/空表均保持既有行为（多账号兼容路径不被误伤）。
    #[test]
    fn single_account_guard_only_rejects_multi_account_when_enabled() {
        assert!(!single_account_guard_violation(false, 0));
        assert!(!single_account_guard_violation(false, 2));
        assert!(!single_account_guard_violation(true, 0));
        assert!(!single_account_guard_violation(true, 1));
        assert!(single_account_guard_violation(true, 2));
        assert!(single_account_guard_violation(true, 9));
    }

    #[test]
    fn single_account_guard_env_is_strict_one() {
        assert!(single_account_guard_enabled_from(Some("1")));
        assert!(single_account_guard_enabled_from(Some(" 1 ")));
        for raw in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("yes"),
            Some("2"),
        ] {
            assert!(
                !single_account_guard_enabled_from(raw),
                "raw={raw:?} must not enable the guard"
            );
        }
    }

    #[test]
    fn socks_proxy_url_accepts_host_port() {
        assert_eq!(
            socks_proxy_url("127.0.0.1:7890").as_deref(),
            Ok("socks5h://127.0.0.1:7890")
        );
        assert_eq!(
            socks_proxy_url(" relay.example.com:1080 ").as_deref(),
            Ok("socks5h://relay.example.com:1080")
        );
        // host 上限 253 字节恰可过。
        assert!(socks_proxy_url(&format!("{}:7890", "a".repeat(253))).is_ok());
    }

    #[test]
    fn socks_proxy_url_rejects_bad_shapes() {
        assert!(socks_proxy_url("").is_err());
        assert!(socks_proxy_url("host").is_err());
        assert!(socks_proxy_url("host:notaport").is_err());
        assert!(socks_proxy_url("host:0").is_err());
        assert!(socks_proxy_url(":7890").is_err());
        assert!(socks_proxy_url("host with space:7890").is_err());
        assert!(socks_proxy_url("socks5://host:7890").is_err());
        // host 超 253 字节与 IPv6 裸字面量（含 ':'）同拒。
        assert!(socks_proxy_url(&format!("{}:7890", "a".repeat(254))).is_err());
        assert!(socks_proxy_url("::1:7890").is_err());
    }

    #[test]
    fn standard_proxy_env_accepts_whitelisted_schemes() {
        assert_eq!(
            standard_proxy_env_hit("socks5://h:1080"),
            Some("socks5://h:1080")
        );
        assert_eq!(
            standard_proxy_env_hit("socks5h://h:1080"),
            Some("socks5h://h:1080")
        );
        assert_eq!(
            standard_proxy_env_hit("http://h:8080"),
            Some("http://h:8080")
        );
        // scheme 大小写不敏感；trim 后原值返回（原样生效不改写）。
        assert_eq!(
            standard_proxy_env_hit("  SOCKS5H://relay.local:1080  "),
            Some("SOCKS5H://relay.local:1080")
        );
    }

    #[test]
    fn standard_proxy_env_rejects_out_of_whitelist_and_malformed() {
        // 白名单外 scheme：引擎链不采纳（reqwest 原生层行为不受链约束）。
        assert_eq!(standard_proxy_env_hit("https://h:443"), None);
        assert_eq!(standard_proxy_env_hit("socks4://h:1080"), None);
        assert_eq!(standard_proxy_env_hit("https://user:pass@h:443"), None);
        // 无 scheme / 空余量 / 空值 / 裸空白。
        assert_eq!(standard_proxy_env_hit("h:1080"), None);
        assert_eq!(standard_proxy_env_hit("socks5://"), None);
        assert_eq!(standard_proxy_env_hit(""), None);
        assert_eq!(standard_proxy_env_hit("   "), None);
        assert_eq!(
            standard_proxy_env_hit("socks5://host with space:1080"),
            None
        );
        // 内部空白（URI 不合法面）：不命中。
        assert_eq!(standard_proxy_env_hit("http://h :8080"), None);
    }

    /// scutil --proxy 真实输出形状（macOS Sequoia 系统代理开 + SOCKS 关）。
    const SCUTIL_HTTP_ON: &str = "<dictionary> {\n  HTTPEnable : 1\n  HTTPPort : 7890\n  HTTPProxy : 127.0.0.1\n  HTTPSEnable : 1\n  HTTPSPort : 7890\n  HTTPSProxy : 127.0.0.1\n  SOCKSEnable : 0\n  SOCKSPort : 1080\n  SOCKSProxy : 127.0.0.1\n}\n";

    #[test]
    fn scutil_parse_extracts_http_proxy() {
        let proxy = parse_scutil_proxy(SCUTIL_HTTP_ON).expect("http proxy parsed");
        assert_eq!(proxy.scheme, "http");
        assert_eq!(proxy.host, "127.0.0.1");
        assert_eq!(proxy.port, 7890);
    }

    #[test]
    fn scutil_parse_extracts_socks_proxy() {
        let output = "<dictionary> {\n  SOCKSEnable : 1\n  SOCKSProxy : socks.local\n  SOCKSPort : 1080\n}\n";
        let proxy = parse_scutil_proxy(output).expect("socks proxy parsed");
        assert_eq!(proxy.scheme, "socks5h");
        assert_eq!(proxy.host, "socks.local");
        assert_eq!(proxy.port, 1080);
    }

    #[test]
    fn scutil_parse_prefers_http_when_both_enabled() {
        let output = "HTTPEnable : 1\nHTTPProxy : h.local\nHTTPPort : 8080\nSOCKSEnable : 1\nSOCKSProxy : s.local\nSOCKSPort : 1080\n";
        let proxy = parse_scutil_proxy(output).expect("http leg wins");
        assert_eq!(proxy.scheme, "http");
        assert_eq!(proxy.host, "h.local");
    }

    #[test]
    fn scutil_parse_socks_fallback_when_http_leg_broken() {
        // HTTP 使能但端口缺失：该路作废，落 SOCKS 路。
        let output = "HTTPEnable : 1\nHTTPProxy : h.local\nSOCKSEnable : 1\nSOCKSProxy : s.local\nSOCKSPort : 1080\n";
        let proxy = parse_scutil_proxy(output).expect("socks fallback");
        assert_eq!(proxy.scheme, "socks5h");
        assert_eq!(proxy.host, "s.local");
    }

    #[test]
    fn scutil_parse_rejects_missing_and_malformed_fields() {
        // 使能位缺失 / 非 1。
        assert!(parse_scutil_proxy("HTTPProxy : h.local\nHTTPPort : 8080\n").is_none());
        assert!(
            parse_scutil_proxy("HTTPEnable : 0\nHTTPProxy : h.local\nHTTPPort : 8080\n").is_none()
        );
        // host 缺失 / 空值 / 含空白 / IPv6 裸字面量。
        assert!(parse_scutil_proxy("HTTPEnable : 1\nHTTPPort : 8080\n").is_none());
        assert!(parse_scutil_proxy("HTTPEnable : 1\nHTTPProxy : \nHTTPPort : 8080\n").is_none());
        assert!(
            parse_scutil_proxy("HTTPEnable : 1\nHTTPProxy : h local\nHTTPPort : 8080\n").is_none()
        );
        assert!(parse_scutil_proxy("HTTPEnable : 1\nHTTPProxy : ::1\nHTTPPort : 8080\n").is_none());
        // 端口缺失 / 非数值 / 0 / 超出 u16。
        assert!(parse_scutil_proxy("HTTPEnable : 1\nHTTPProxy : h.local\n").is_none());
        assert!(
            parse_scutil_proxy("HTTPEnable : 1\nHTTPProxy : h.local\nHTTPPort : notaport\n")
                .is_none()
        );
        assert!(
            parse_scutil_proxy("HTTPEnable : 1\nHTTPProxy : h.local\nHTTPPort : 0\n").is_none()
        );
        assert!(
            parse_scutil_proxy("HTTPEnable : 1\nHTTPProxy : h.local\nHTTPPort : 70000\n").is_none()
        );
        // 两路全废（SOCKS 端口畸形收尾）与空输出。
        assert!(parse_scutil_proxy("HTTPEnable : 1\nHTTPProxy : h local\nSOCKSEnable : 1\nSOCKSProxy : s.local\nSOCKSPort : x\n").is_none());
        assert!(parse_scutil_proxy("").is_none());
        // PAC 面（ProxyAutoConfig*）不构成命中。
        assert!(parse_scutil_proxy(
            "ProxyAutoConfigEnable : 1\nProxyAutoConfigURLString : http://pac.local/proxy.pac\n"
        )
        .is_none());
    }

    #[test]
    fn chain_explicit_env_wins_over_everything() {
        let decision = resolve_proxy_chain(
            Some("10.0.0.1:1080"),
            Some("http://ignored:8080"),
            None,
            Some(SCUTIL_HTTP_ON),
        )
        .expect("explicit env valid");
        assert!(
            matches!(decision, ProxyDecision::ExplicitEnv(ref url) if url == "socks5h://10.0.0.1:1080")
        );
    }

    #[test]
    fn chain_explicit_env_invalid_is_fatal() {
        // 显式配置非法 fail-closed（exit 2），不静默降级。
        assert!(resolve_proxy_chain(Some("host:notaport"), None, None, None).is_err());
        assert!(resolve_proxy_chain(Some("socks5://host:1080"), None, None, None).is_err());
    }

    #[test]
    fn chain_explicit_env_empty_falls_through_to_std_env() {
        let decision = resolve_proxy_chain(Some("   "), Some("http://fall.local:8080"), None, None)
            .expect("std env hit");
        assert!(matches!(decision, ProxyDecision::StdEnv(url) if url == "http://fall.local:8080"));
    }

    #[test]
    fn chain_std_env_uppercase_wins_over_lowercase() {
        let decision = resolve_proxy_chain(
            None,
            Some("socks5h://upper.local:1080"),
            Some("http://lower.local:8080"),
            None,
        )
        .expect("uppercase wins");
        assert!(matches!(decision, ProxyDecision::StdEnv(ref url) if url.contains("upper.local")));

        // 大写缺位 / 大写非白名单值时小写兜底。
        let decision = resolve_proxy_chain(
            None,
            Some("https://upper.local:443"),
            Some("socks5://lower.local:1080"),
            None,
        )
        .expect("lowercase fallback");
        assert!(matches!(decision, ProxyDecision::StdEnv(ref url) if url.contains("lower.local")));
    }

    #[test]
    fn chain_std_env_beats_scutil() {
        let decision = resolve_proxy_chain(
            None,
            None,
            Some("socks5://std.local:1080"),
            Some(SCUTIL_HTTP_ON),
        )
        .expect("std env wins over system");
        assert!(matches!(decision, ProxyDecision::StdEnv(ref url) if url.contains("std.local")));
    }

    #[test]
    fn chain_scutil_http_then_socks_and_degrades_to_direct() {
        let decision =
            resolve_proxy_chain(None, None, None, Some(SCUTIL_HTTP_ON)).expect("system http");
        assert!(
            matches!(decision, ProxyDecision::SystemHttp(ref url) if url == "http://127.0.0.1:7890")
        );

        // HTTP 路废、SOCKS 路有效 → system-socks。
        let scutil_socks = "HTTPEnable : 1\nHTTPProxy : h local\nSOCKSEnable : 1\nSOCKSProxy : s.local\nSOCKSPort : 1080\n";
        let decision =
            resolve_proxy_chain(None, None, None, Some(scutil_socks)).expect("system socks");
        assert!(
            matches!(decision, ProxyDecision::SystemSocks(ref url) if url == "socks5h://s.local:1080")
        );

        // scutil 输出全废 → 直连；scutil 缺席（非 macOS / 读取失败）→ 直连。
        assert!(matches!(
            resolve_proxy_chain(None, None, None, Some("HTTPEnable : 1\n")).expect("degrade"),
            ProxyDecision::Direct
        ));
        assert!(matches!(
            resolve_proxy_chain(None, None, None, None).expect("direct"),
            ProxyDecision::Direct
        ));
    }

    #[test]
    fn chain_decision_labels_match_compat_doc_tokens() {
        // compat §1.1 日志标记词是契约面：direct/explicit-env/std-env/
        // system-http/system-socks。
        assert_eq!(ProxyDecision::Direct.label(), "direct");
        assert_eq!(
            ProxyDecision::ExplicitEnv("socks5h://h:1".into()).label(),
            "explicit-env"
        );
        assert_eq!(
            ProxyDecision::StdEnv("http://h:1".into()).label(),
            "std-env"
        );
        assert_eq!(
            ProxyDecision::SystemHttp("http://h:1".into()).label(),
            "system-http"
        );
        assert_eq!(
            ProxyDecision::SystemSocks("socks5h://h:1".into()).label(),
            "system-socks"
        );
        assert_eq!(ProxyDecision::Direct.detail(), "no proxy configured");
        assert_eq!(
            ProxyDecision::StdEnv("http://h:1".into()).detail(),
            "http://h:1"
        );
    }

    /// G1 ack 时序面：栅栏在先入 Notice 落盘（write+flush）之后才解决——
    /// writer 逐条 flush，FIFO 队列序下 Flush 帧被弹出即代表此前全部通知
    /// 已出进程。栅栏等待方拿到 true 且字节已在 sink。
    #[tokio::test]
    async fn flush_barrier_resolves_only_after_prior_notices_flushed() {
        let (tx, rx) = tokio::sync::mpsc::channel::<OutboundFrame>(8);
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let writer_sink = std::sync::Arc::clone(&sink);
        let writer = tokio::spawn(async move {
            // Vec<u8> 直写无法断言中间态，包一层把 write 缓冲在 flush 时
            // 才并入共享 sink 的探针（flush 语义可断言）。
            struct Probe(std::sync::Arc<std::sync::Mutex<Vec<u8>>>, Vec<u8>);
            impl tokio::io::AsyncWrite for Probe {
                fn poll_write(
                    mut self: Pin<&mut Self>,
                    _cx: &mut Context<'_>,
                    buf: &[u8],
                ) -> Poll<std::io::Result<usize>> {
                    self.1.extend_from_slice(buf);
                    Poll::Ready(Ok(buf.len()))
                }
                fn poll_flush(
                    mut self: Pin<&mut Self>,
                    _cx: &mut Context<'_>,
                ) -> Poll<std::io::Result<()>> {
                    let inner = std::mem::take(&mut self.1);
                    self.0.lock().unwrap().extend(inner);
                    Poll::Ready(Ok(()))
                }
                fn poll_shutdown(
                    self: Pin<&mut Self>,
                    _cx: &mut Context<'_>,
                ) -> Poll<std::io::Result<()>> {
                    Poll::Ready(Ok(()))
                }
            }
            run_outbound_writer(rx, Probe(writer_sink, Vec::new())).await
        });
        tx.send(OutboundFrame::notice(serde_json::json!({"m": 1})))
            .await
            .unwrap();
        assert!(outbound_flush_barrier(&tx).await);
        let written = sink.lock().unwrap().clone();
        assert_eq!(written, b"{\"m\":1}\n".to_vec());
        drop(tx);
        let _ = writer.await;
    }

    /// G1 ack 时序面：出口死亡（writer 因写失败退出 / 通道关闭）→ 栅栏
    /// 落 false——receive 管线据此放弃 ack，信封留服务器队列待重启重放。
    #[tokio::test]
    async fn flush_barrier_fails_when_outbound_dead() {
        // 通道关闭臂：接收端已弃。
        let (tx, rx) = tokio::sync::mpsc::channel::<OutboundFrame>(8);
        drop(rx);
        assert!(!outbound_flush_barrier(&tx).await);

        // writer 死亡臂：sink 写失败 → writer 退出 → 在途栅栏 oneshot 弃置。
        struct BrokenSink;
        impl tokio::io::AsyncWrite for BrokenSink {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Ready(Err(std::io::Error::other("sink broken")))
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<OutboundFrame>(8);
        tokio::spawn(run_outbound_writer(rx, BrokenSink));
        tx.send(OutboundFrame::notice(serde_json::json!({"m": 1})))
            .await
            .unwrap();
        let mut died = false;
        for _ in 0..100 {
            if !outbound_flush_barrier(&tx).await {
                died = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(died, "barrier must fail once the writer has died");
    }

    /// writer 死亡（stdout 写失败）→ 退出原因可观测且被判 fatal（exit 1 /
    /// connector supervision 重启）；干净通道关闭不 fatal。
    #[tokio::test]
    async fn writer_failure_requests_process_exit() {
        struct FailingWriter;
        impl tokio::io::AsyncWrite for FailingWriter {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Ready(Err(std::io::Error::other("stdout broken")))
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<super::OutboundFrame>(8);
        let writer = tokio::spawn(super::run_outbound_writer(rx, FailingWriter));
        tx.send(super::OutboundFrame::notice(serde_json::json!({"m": 1})))
            .await
            .unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(2), writer)
            .await
            .expect("writer exits after a failed write")
            .expect("writer task joins");
        assert!(
            matches!(
                exit,
                super::WriterExit::WriteFailed {
                    stage: "write_all",
                    ..
                }
            ),
            "unexpected writer exit: {exit:?}"
        );
        assert!(
            super::report_writer_exit(&exit),
            "a dead stdout writer must request process exit for supervision restart"
        );

        // 干净收尾（发送端全弃）不是 fatal：不触发重启。
        let (tx, rx) = tokio::sync::mpsc::channel::<super::OutboundFrame>(1);
        drop(tx);
        let clean = tokio::spawn(super::run_outbound_writer(rx, FailingWriter));
        let exit = tokio::time::timeout(std::time::Duration::from_secs(2), clean)
            .await
            .expect("writer exits on channel close")
            .expect("writer task joins");
        assert!(matches!(exit, super::WriterExit::ChannelClosed));
        assert!(!super::report_writer_exit(&exit));
    }

    /// writer 任务 panic 不能被吞成静默退出：JoinError 归类为 Panicked 且 fatal。
    #[tokio::test]
    async fn writer_panic_requests_process_exit() {
        let handle = tokio::spawn(async { panic!("writer panic probe") });
        let exit = super::join_writer(handle.await);
        assert!(matches!(exit, super::WriterExit::Panicked));
        assert!(super::report_writer_exit(&exit));
    }

    /// writer 死亡必须在 stdin EOF 之前打断入站环（进程退出/重启信号）：
    /// 验证修复接线——stdout 写失败后引擎不再「活着但永不出帧」。
    #[tokio::test]
    async fn writer_death_breaks_input_loop_for_restart() {
        use tokio::sync::mpsc;

        struct FailingWriter;
        impl tokio::io::AsyncWrite for FailingWriter {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Ready(Err(std::io::Error::other("stdout broken")))
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let data_dir = std::env::temp_dir().join(format!(
                    "kt-engine-writer-death-{:016x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0)
                ));
                let state = std::sync::Arc::new(tokio::sync::Mutex::new(
                    super::EngineState::load(&data_dir).expect("state"),
                ));
                let (outbound, _rx) = crate::service::outbound::test_hub();
                let service = crate::service::Service::new(
                    data_dir.clone(),
                    std::sync::Arc::clone(&state),
                    std::sync::Arc::clone(&outbound),
                );
                let (inbound_tx, mut inbound_rx) =
                    mpsc::channel::<super::Inbound>(super::INBOUND_QUEUE_CAPACITY);
                let inbound_budget = super::InboundBudget::new(super::INBOUND_PENDING_BYTES_BUDGET);
                let (tx, rx) = mpsc::channel::<super::OutboundFrame>(8);
                let mut writer = tokio::spawn(super::run_outbound_writer(rx, FailingWriter));
                tx.send(super::OutboundFrame::notice(serde_json::json!({"m": 1})))
                    .await
                    .unwrap();
                let exit = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    super::run_engine_input_loop(
                        std::sync::Arc::clone(&state),
                        service,
                        outbound,
                        &mut inbound_rx,
                        &inbound_budget,
                        &mut writer,
                    ),
                )
                .await
                .expect("writer death must break the input loop before stdin EOF");
                assert!(matches!(exit, Some(super::WriterExit::WriteFailed { .. })));
                drop(inbound_tx);
                let _ = std::fs::remove_dir_all(&data_dir);
            })
            .await;
    }

    // —— 入站有界读帧（帧帽对齐批：超帽帧吞到行尾 + 字节计数 + 重同步）——

    use super::{read_bounded_line_with_limit, ReadOutcome};
    use std::io::Cursor;

    /// 按固定块长吐数据的 BufRead：让超限帧跨多次 fill_buf，钉定字节计数
    /// 与丢弃尾段在分块边界上的行为。
    struct Chunked<'a> {
        data: &'a [u8],
        chunk: usize,
    }

    impl std::io::Read for Chunked<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let len = self.data.len().min(self.chunk).min(out.len());
            out[..len].copy_from_slice(&self.data[..len]);
            self.data = &self.data[len..];
            Ok(len)
        }
    }

    impl std::io::BufRead for Chunked<'_> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let len = self.data.len().min(self.chunk);
            Ok(&self.data[..len])
        }

        fn consume(&mut self, amount: usize) {
            self.data = &self.data[amount.min(self.data.len())..];
        }
    }

    #[test]
    fn bounded_reader_accepts_a_line_at_the_limit_and_reads_the_next() {
        let mut input = Cursor::new(b"abcd\nnext\n".to_vec());
        let mut buf = Vec::new();
        assert!(matches!(
            read_bounded_line_with_limit(&mut input, &mut buf, 4),
            ReadOutcome::Line
        ));
        assert_eq!(buf, b"abcd");
        buf.clear();
        assert!(matches!(
            read_bounded_line_with_limit(&mut input, &mut buf, 4),
            ReadOutcome::Line
        ));
        assert_eq!(buf, b"next");
    }

    #[test]
    fn bounded_reader_reports_overlong_frame_bytes_and_resyncs() {
        let mut input = Cursor::new(b"abcdeXYZ\nok\n".to_vec());
        let mut buf = Vec::new();
        match read_bounded_line_with_limit(&mut input, &mut buf, 4) {
            ReadOutcome::Overlong { bytes } => assert_eq!(bytes, 9),
            _ => panic!("expected an overlong frame"),
        }
        // 该形状下同一块内已见行尾，超限帧内容未被写入 buf。
        assert!(buf.is_empty());
        buf.clear();
        assert!(matches!(
            read_bounded_line_with_limit(&mut input, &mut buf, 4),
            ReadOutcome::Line
        ));
        assert_eq!(buf, b"ok");
    }

    #[test]
    fn bounded_reader_counts_overlong_bytes_across_chunk_boundaries() {
        let mut reader = Chunked {
            data: b"abcdefghij\nok\n",
            chunk: 3,
        };
        let mut buf = Vec::new();
        match read_bounded_line_with_limit(&mut reader, &mut buf, 5) {
            ReadOutcome::Overlong { bytes } => assert_eq!(bytes, 11),
            _ => panic!("expected an overlong frame"),
        }
        buf.clear();
        assert!(matches!(
            read_bounded_line_with_limit(&mut reader, &mut buf, 5),
            ReadOutcome::Line
        ));
        assert_eq!(buf, b"ok");
    }

    #[test]
    fn bounded_reader_eof_partial_line_and_overlong_tail_at_eof() {
        let mut empty = Cursor::new(Vec::new());
        let mut buf = Vec::new();
        assert!(matches!(
            read_bounded_line_with_limit(&mut empty, &mut buf, 4),
            ReadOutcome::Eof
        ));

        // 无换行尾行照常投递（stdin EOF 语义与原实现一致）。
        let mut partial = Cursor::new(b"abc".to_vec());
        assert!(matches!(
            read_bounded_line_with_limit(&mut partial, &mut buf, 4),
            ReadOutcome::Line
        ));
        assert_eq!(buf, b"abc");
        buf.clear();
        assert!(matches!(
            read_bounded_line_with_limit(&mut partial, &mut buf, 4),
            ReadOutcome::Eof
        ));

        // EOF 前一直无换行的超限帧：报 Overlong（字节数 = 已吞字节）。
        let mut tail = Cursor::new(b"abcdef".to_vec());
        match read_bounded_line_with_limit(&mut tail, &mut buf, 2) {
            ReadOutcome::Overlong { bytes } => assert_eq!(bytes, 6),
            _ => panic!("expected an overlong frame at eof"),
        }
        buf.clear();
        assert!(matches!(
            read_bounded_line_with_limit(&mut tail, &mut buf, 2),
            ReadOutcome::Eof
        ));
    }

    // —— M4/M5：有界入站队列、在飞上限、请求超时、取消与正常路径 ——

    #[tokio::test]
    async fn inbound_queue_is_bounded_and_backpressures_without_dropping() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<super::Inbound>(2);
        tx.send(super::Inbound::Line(b"a".to_vec()))
            .await
            .expect("first slot");
        tx.send(super::Inbound::Line(b"b".to_vec()))
            .await
            .expect("second slot");

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            let sent = tx
                .blocking_send(super::Inbound::Line(b"c".to_vec()))
                .is_ok();
            let _ = done_tx.send(sent);
        });

        // 队列满只背压生产者；接收侧照常取帧（不阻塞、不丢弃）。
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "producer must block while the bounded queue is full"
        );
        match rx.recv().await {
            Some(super::Inbound::Line(line)) => assert_eq!(line, b"a"),
            _ => panic!("expected the first queued frame"),
        }
        assert!(done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("producer resumes after the consumer drains one slot"));
        producer.join().expect("producer joins");
        match rx.recv().await {
            Some(super::Inbound::Line(line)) => assert_eq!(line, b"b"),
            _ => panic!("expected the second queued frame"),
        }
        match rx.recv().await {
            Some(super::Inbound::Line(line)) => assert_eq!(line, b"c"),
            _ => panic!("the backpressured frame must not be dropped"),
        }
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn inbound_byte_budget_waits_for_drain_and_never_drops() {
        let budget = std::sync::Arc::new(super::InboundBudget::new(10));
        budget.reserve(6);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = {
            let budget = std::sync::Arc::clone(&budget);
            std::thread::spawn(move || {
                budget.reserve(6);
                let _ = done_tx.send(());
                budget.release(6);
            })
        };
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "second reservation must wait while the byte budget is exhausted"
        );
        budget.release(6);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("reservation completes after the dispatcher drains a frame");
        waiter.join().expect("waiter joins");
    }

    #[test]
    fn inflight_limit_answers_structured_busy_without_dropping_the_frame() {
        let permits =
            std::sync::Arc::new(tokio::sync::Semaphore::new(super::INFLIGHT_REQUEST_LIMIT));
        let mut held = Vec::new();
        for _ in 0..super::INFLIGHT_REQUEST_LIMIT {
            held.push(
                std::sync::Arc::clone(&permits)
                    .try_acquire_owned()
                    .expect("permit"),
            );
        }
        assert!(
            std::sync::Arc::clone(&permits).try_acquire_owned().is_err(),
            "in-flight cap reached"
        );
        let prepared = crate::methods::prepare(&super::Inbound::Line(
            br#"{"jsonrpc":"2.0","id":42,"method":"listAccounts","params":{}}"#.to_vec(),
        ));
        let response = crate::methods::busy_response(&prepared);
        assert_eq!(response["id"], serde_json::json!(42));
        assert_eq!(response["error"]["code"], serde_json::json!(-32000));
        assert_eq!(
            response["error"]["data"]["reason"],
            serde_json::json!("ENGINE_BUSY")
        );
        assert_eq!(
            response["error"]["data"]["inFlightLimit"],
            serde_json::json!(super::INFLIGHT_REQUEST_LIMIT)
        );
        assert!(response["error"]["message"]
            .as_str()
            .expect("message")
            .contains("EngineBusyException"));
        drop(held);
    }

    #[test]
    fn request_timeout_budget_classifies_plain_attachment_and_link_requests() {
        use crate::methods::{
            prepare, request_timeout_budget, ATTACHMENT_REQUEST_TIMEOUT, LINK_FINISH_TIMEOUT,
            REQUEST_TIMEOUT,
        };
        let budget = |body: serde_json::Value| {
            request_timeout_budget(&prepare(&super::Inbound::Line(
                body.to_string().into_bytes(),
            )))
        };
        assert_eq!(
            budget(serde_json::json!({"id": 1, "method": "listAccounts", "params": {}})),
            REQUEST_TIMEOUT
        );
        assert_eq!(
            budget(serde_json::json!({
                "id": 2, "method": "send",
                "params": {"account": "+1", "message": "hi"},
            })),
            REQUEST_TIMEOUT,
            "plain send is not an upload request"
        );
        assert_eq!(
            budget(serde_json::json!({
                "id": 3, "method": "send",
                "params": {
                    "account": "+1", "message": "hi",
                    "attachments": ["data:image/png;base64,aGVsbG8="],
                },
            })),
            ATTACHMENT_REQUEST_TIMEOUT
        );
        assert_eq!(
            budget(serde_json::json!({
                "id": 4, "method": "sendStory",
                "params": {
                    "account": "+1", "message": "hi",
                    "attachment": "data:image/png;base64,aGVsbG8=",
                },
            })),
            ATTACHMENT_REQUEST_TIMEOUT
        );
        assert_eq!(
            budget(serde_json::json!({"id": 5, "method": "finishLink", "params": {}})),
            LINK_FINISH_TIMEOUT
        );
        assert!(ATTACHMENT_REQUEST_TIMEOUT > REQUEST_TIMEOUT);
        assert!(LINK_FINISH_TIMEOUT > ATTACHMENT_REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn timed_out_dispatch_is_cancelled_and_answers_with_structured_error() {
        struct CancelProbe(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for CancelProbe {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = CancelProbe(std::sync::Arc::clone(&cancelled));
        let future = async move {
            let _probe = probe;
            std::future::pending::<serde_json::Value>().await
        };
        let id = serde_json::json!(9);
        let timeout = std::time::Duration::from_millis(20);
        let response = super::respond_with_budget(timeout, future, || {
            crate::methods::timeout_response(&id, timeout)
        })
        .await;

        assert!(
            cancelled.load(std::sync::atomic::Ordering::SeqCst),
            "timeout must drop (cancel) the dispatch future"
        );
        assert_eq!(response["id"], serde_json::json!(9));
        assert_eq!(response["error"]["code"], serde_json::json!(-32000));
        assert_eq!(
            response["error"]["data"]["reason"],
            serde_json::json!("REQUEST_TIMEOUT")
        );
        assert_eq!(
            response["error"]["data"]["timeoutMs"],
            serde_json::json!(20)
        );
    }

    #[tokio::test]
    async fn normal_dispatch_path_is_unchanged_by_the_timeout_wrapper() {
        let response = super::respond_with_budget(
            std::time::Duration::from_secs(5),
            async { serde_json::json!({"ok": true}) },
            || panic!("must not time out"),
        )
        .await;
        assert_eq!(response, serde_json::json!({"ok": true}));
    }
}
