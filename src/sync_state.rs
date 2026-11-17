// SPDX-License-Identifier: AGPL-3.0-only

//! 阶段 1-1：`getSyncState` 的只读投影簿（SIGNAL-STAGE1-DESIGN §二 P-B-3，
//! 契约面见 docs/jsonrpc-compat.md 的 `getSyncState` 行与 §5 块）。
//!
//! **零新状态机**：本模块不跑定时器、不重试、不做任何业务决策，只把引擎登录
//! 窗 sync 臂**已经在跟踪**的三块簿记观测下来并投影成一把只读查询：
//!
//! - **窗口起算**：注册表本身就是窗口簿。`EngineState::add_account` = 新
//!   链路窗口开窗；进程启动时 `post_registration_syncs_complete == false` 的
//!   账号 = 首流窗口（`initial_sync_due(None) == true` 同门，收流臂一定会
//!   安排这一窗）；`remove_account` = 窗口作废。已完成注册的账号启动时不再
//!   跑本窗（收流臂 `sync_pending` 恒 None），如实报 idle。
//! - **contacts 半边**：`post_registration_syncs_complete`（`contactSync`
//!   的 `complete` 到达时收流臂落盘的官方口径）——state 层落盘钩子同点观测。
//! - **storage 半边**：`engine.storagePull`（trigger=login，T-9b 结果事件，
//!   两腿 join 后的如实布尔）+ `engine.initialSyncComplete.manifestOk` 的
//!   兜底面（登录拉超时被上层 30s 上限掐断时没有 storagePull 事件，join 仍
//!   会以 `manifestOk=false` 落定）。
//! - **profile 半边**：`engine.initialSyncComplete.profilesOk`（首轮 profile
//!   刷新腿的 join 结论）。
//!
//! **观测点唯一**：`service::outbound::OutboundHub::notify`（全部通知的唯一
//! 出口）。只有两个低频事件被解析（storagePull / initialSyncComplete），
//! 逐消息的 `receive` 通知只比一次方法名，不做任何深解析。
//!
//! **查询是纯读**：`snapshot` 不写簿记，重复调用逐字节一致（幂等验收）。
//! 相位词表 `idle|inflight|done|failed`：
//! - `inflight`：窗口已开窗且该半边尚未落定（窗口 TTL 120s 内）；
//! - `done` / `failed`：观测到的落定面（首次落定优先，迟到重复不改写）；
//! - `idle`：没有开窗，或开窗已过 TTL 仍未落定（一次性窗已过，不再假装
//!   在飞——不发明 failed）。
//!
//! contacts 半边没有 failed 面：三请求上行是 best-effort（失败只落结构化
//! warn 并等下次链路建立重补），进程内无可观测的失败事件；如实缺席，消费方
//! 用它自己的半边超时兜底。fake 模式（`KT_ENGINE_FAKE_SERVICE=1`）没有
//! 真实同步臂，恒答 idle。
//!
//! `sinceMs` = 当前窗口的开窗时刻（epoch ms）。消费方以它判新鲜度：窗口开在
//! 本次 gate 之前 = 上一次链接/启动留下的旧面，按陈旧处理。
//!
//! **已知窄窗（如实登记，不在本批修）**：重链瞬间旧收流臂的 detached
//! join/拉取任务仍可能发出上一窗口的 `engine.initialSyncComplete`；通知不带
//! 窗口标识（wire 面零变更纪律），本簿无法归因，会把它落进新窗口。影响面 =
//! 最多提前落定一次（下次开窗复位）；与引擎既有的 pending-join 语义同源，
//! 消费方仍以 `sinceMs` 判窗口新鲜度。要彻底关闭需要通知携带窗口号（改收流
//! 臂，属另一批）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::state::EngineState;

/// 一次性窗的在飞寿命（毫秒）：登录拉 30s 上限 + 首轮 profile 刷新 10s 沉降
/// 之后仍无落定的半边不再报 in-flight。与收流臂的 60s 一次性窗同量级，
/// 给足两个独立任务的上限余量（不做调度用，只做投影降级）。
const WINDOW_TTL_MS: u64 = 120_000;

/// 半边相位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Inflight,
    Done,
    Failed,
}

impl Phase {
    fn word(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Inflight => "inflight",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// 一个账号的同步窗口簿（进程内存，不落盘不跨重启）。
#[derive(Clone, Debug)]
struct WindowBook {
    /// 开窗时刻（epoch ms）——wire `sinceMs` 的唯一来源。
    started_ms: u64,
    /// 窗口是否还"在飞"（real 模式且注册未完成；fake 模式恒 false）。
    active: bool,
    /// 已观测到的落定面（首次落定优先）。
    contacts: Option<Phase>,
    storage: Option<Phase>,
    profile: Option<Phase>,
}

impl WindowBook {
    fn new(started_ms: u64, active: bool, contacts_done: bool) -> Self {
        Self {
            started_ms,
            active,
            contacts: contacts_done.then_some(Phase::Done),
            storage: None,
            profile: None,
        }
    }

    /// 半边当前相位：落定面优先，否则窗口在飞且未过 TTL = inflight，否则 idle。
    fn half_word(&self, settle: Option<Phase>, now_ms: u64) -> &'static str {
        if let Some(phase) = settle {
            return phase.word();
        }
        if self.active && now_ms.saturating_sub(self.started_ms) < WINDOW_TTL_MS {
            Phase::Inflight.word()
        } else {
            Phase::Idle.word()
        }
    }

    fn render(&self, now_ms: u64) -> Value {
        json!({
            "contacts": self.half_word(self.contacts, now_ms),
            "storage": self.half_word(self.storage, now_ms),
            "profile": self.half_word(self.profile, now_ms),
            "sinceMs": self.started_ms,
        })
    }
}

fn book() -> &'static Mutex<HashMap<String, WindowBook>> {
    static BOOK: OnceLock<Mutex<HashMap<String, WindowBook>>> = OnceLock::new();
    BOOK.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// real 模式判定（与 `Service::new` 的 `real` 同一个环境开关；这里只读，
/// 不缓存——测试替身可以自由设置）。
fn real_mode() -> bool {
    !crate::service::mode_from_env()
}

/// 进程启动装载：注册表即窗口簿的初始快照（`main.rs` 在 `EngineState::load`
/// 之后、任何收流管线启动之前调用一次）。
pub(crate) fn init(state: &EngineState) {
    let started_ms = now_ms();
    let real = real_mode();
    if let Ok(mut book) = book().lock() {
        book.clear();
        for entry in state.accounts() {
            let complete = entry.post_registration_syncs_complete;
            book.insert(
                entry.number.clone(),
                WindowBook::new(started_ms, real && !complete, complete),
            );
        }
    }
}

/// 新链路/同号重链：注册表写入成功后开新窗（`EngineState::add_account` 钩子）。
/// 上一次链接留下的落定面随窗口一起作废——旧窗口的 done 不得冒充本次 gate。
pub(crate) fn note_account_linked(account: &str) {
    let mut book = book().lock().expect("sync state book lock");
    book.insert(
        account.to_owned(),
        WindowBook::new(now_ms(), real_mode(), false),
    );
}

/// 账号摘除/解绑：窗口簿随注册表一起消失（`EngineState::remove_account` 钩子）。
pub(crate) fn note_account_removed(account: &str) {
    if let Ok(mut book) = book().lock() {
        book.remove(account);
    }
}

/// contacts 半边落定（`contactSync.complete` 的持久化钩子，首次落定优先）。
pub(crate) fn note_contacts_complete(account: &str) {
    let mut book = book().lock().expect("sync state book lock");
    if let Some(window) = book.get_mut(account) {
        window.contacts = Some(Phase::Done);
    }
}

/// 通知观测（`OutboundHub::notify` 唯一钩子）：只有两个低频事件被解析，
/// 逐消息的 `receive` 通知在这里只比一次方法名。形状不对的观测一律忽略
/// （宁可不落定，也不拿坏形状编造落定面）。
pub(crate) fn observe_notification(account: &str, value: &Value) {
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return;
    };
    match method {
        "engine.storagePull" => {
            let Some(params) = value.get("params") else {
                return;
            };
            // 只有登录窗自动拉（trigger=login）是 gate 的 storage 半边；
            // fetchLatest 是收流期的补拉，不参与 gate 判定。
            if params.get("trigger").and_then(Value::as_str) != Some("login") {
                return;
            }
            let (Some(contacts_ok), Some(groups_ok)) = (
                params.get("contactsOk").and_then(Value::as_bool),
                params.get("groupsOk").and_then(Value::as_bool),
            ) else {
                return;
            };
            let phase = if contacts_ok && groups_ok {
                Phase::Done
            } else {
                Phase::Failed
            };
            settle_storage(account, phase);
        }
        "engine.initialSyncComplete" => {
            let Some(params) = value.get("params") else {
                return;
            };
            let Some(profiles_ok) = params.get("profilesOk").and_then(Value::as_bool) else {
                return;
            };
            {
                let mut book = book().lock().expect("sync state book lock");
                let Some(window) = book.get_mut(account) else {
                    return;
                };
                if window.profile.is_none() {
                    window.profile = Some(if profiles_ok {
                        Phase::Done
                    } else {
                        Phase::Failed
                    });
                }
            }
            // 登录拉被 30s 上限掐断时没有 storagePull 事件，join 的
            // manifestOk 是这一窗 storage 半边的兜底落定面。
            if let Some(manifest_ok) = params.get("manifestOk").and_then(Value::as_bool) {
                settle_storage(
                    account,
                    if manifest_ok {
                        Phase::Done
                    } else {
                        Phase::Failed
                    },
                );
            }
        }
        _ => {}
    }
}

/// storage 半边落定（首次落定优先：storagePull 先到，join 的 manifestOk 只兜底）。
fn settle_storage(account: &str, phase: Phase) {
    let mut book = book().lock().expect("sync state book lock");
    if let Some(window) = book.get_mut(account) {
        if window.storage.is_none() {
            window.storage = Some(phase);
        }
    }
}

/// `getSyncState` 只读查询面：未知账号（未登记/已摘除）= None，调用方折叠
/// -32603。纯读，不建簿、不改簿。
pub(crate) fn snapshot(account: &str) -> Option<Value> {
    snapshot_at(account, now_ms())
}

fn snapshot_at(account: &str, now_ms: u64) -> Option<Value> {
    let book = book().lock().expect("sync state book lock");
    book.get(account).map(|window| window.render(now_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例独占自己的号码，互不串簿（全局簿按账号键控）。
    fn seed(account: &str, started_ms: u64, active: bool) -> Value {
        book().lock().expect("book").insert(
            account.to_owned(),
            WindowBook::new(started_ms, active, false),
        );
        snapshot_at(account, started_ms).expect("snapshot")
    }

    /// 窗口在飞且未落定 = 三半边 inflight；形状是四个键的封闭词表，重复
    /// 调用逐字节一致（幂等验收）。
    #[test]
    fn fresh_window_reports_inflight_and_is_idempotent() {
        let account = "+15550001001";
        let started = 1_760_000_000_000;
        let first = seed(account, started, true);
        assert_eq!(
            first,
            json!({
                "contacts": "inflight",
                "storage": "inflight",
                "profile": "inflight",
                "sinceMs": started,
            })
        );
        assert_eq!(first, snapshot_at(account, started + 1_000).unwrap());
        assert_eq!(first, snapshot_at(account, started + 5_000).unwrap());
        note_account_removed(account);
        assert!(snapshot_at(account, started).is_none());
    }

    /// 落定面：storagePull/initialSyncComplete/contacts 钩子各定一半；迟到
    /// 的相反结论不改写首次落定（幂等）。
    #[test]
    fn settle_faces_are_first_wins() {
        let account = "+15550001002";
        let started = 1_760_000_100_000;
        seed(account, started, true);
        observe_notification(
            account,
            &json!({
                "jsonrpc": "2.0",
                "method": "engine.storagePull",
                "params": { "account": account, "trigger": "login",
                            "contactsOk": true, "groupsOk": false },
            }),
        );
        observe_notification(
            account,
            &json!({
                "jsonrpc": "2.0",
                "method": "engine.storagePull",
                "params": { "account": account, "trigger": "login",
                            "contactsOk": true, "groupsOk": true },
            }),
        );
        observe_notification(
            account,
            &json!({
                "jsonrpc": "2.0",
                "method": "engine.initialSyncComplete",
                "params": { "account": account, "manifestOk": true,
                            "profilesOk": false },
            }),
        );
        note_contacts_complete(account);
        let face = snapshot_at(account, started + 30_000).expect("snapshot");
        assert_eq!(face["contacts"], json!("done"));
        assert_eq!(
            face["storage"],
            json!("failed"),
            "首次落定优先（迟到 ok 不改写）"
        );
        assert_eq!(face["profile"], json!("failed"));
        assert_eq!(face["sinceMs"], json!(started));
        note_account_removed(account);
    }

    /// 非登录触发（fetchLatest）不参与 gate 的 storage 半边。
    #[test]
    fn fetch_latest_pulls_do_not_settle_the_login_half() {
        let account = "+15550001003";
        let started = 1_760_000_200_000;
        seed(account, started, true);
        observe_notification(
            account,
            &json!({
                "jsonrpc": "2.0",
                "method": "engine.storagePull",
                "params": { "account": account, "trigger": "fetchLatest",
                            "contactsOk": true, "groupsOk": true },
            }),
        );
        let face = snapshot_at(account, started + 1_000).expect("snapshot");
        assert_eq!(face["storage"], json!("inflight"));
        note_account_removed(account);
    }

    /// TTL 之后窗口不再报 in-flight（一次性窗已过）——不发明 failed。
    #[test]
    fn window_ttl_degrades_inflight_to_idle() {
        let account = "+15550001004";
        let started = 1_760_000_300_000;
        seed(account, started, true);
        let expired = snapshot_at(account, started + WINDOW_TTL_MS + 1).expect("snapshot");
        assert_eq!(expired["contacts"], json!("idle"));
        assert_eq!(expired["storage"], json!("idle"));
        assert_eq!(expired["profile"], json!("idle"));
        assert_eq!(expired["sinceMs"], json!(started));
        note_account_removed(account);
    }

    /// 无窗口的账号（启动时注册已完成、链路后未开窗）= idle；contacts 半边
    /// 沿用持久化 done 面。
    #[test]
    fn completed_registration_reads_idle_with_contacts_done() {
        let account = "+15550001005";
        let started = 1_760_000_400_000;
        book()
            .lock()
            .expect("book")
            .insert(account.to_owned(), WindowBook::new(started, false, true));
        let face = snapshot_at(account, started + 1_000).expect("snapshot");
        assert_eq!(face["contacts"], json!("done"));
        assert_eq!(face["storage"], json!("idle"));
        assert_eq!(face["profile"], json!("idle"));
        note_account_removed(account);
    }

    /// 坏形状观测不落定（宁缺勿造）。
    #[test]
    fn malformed_observations_are_ignored() {
        let account = "+15550001006";
        let started = 1_760_000_500_000;
        seed(account, started, true);
        observe_notification(account, &json!({ "method": "engine.storagePull" }));
        observe_notification(
            account,
            &json!({ "method": "engine.storagePull",
                     "params": { "trigger": "login", "contactsOk": "yes" } }),
        );
        observe_notification(
            account,
            &json!({ "method": "engine.initialSyncComplete", "params": {} }),
        );
        let face = snapshot_at(account, started + 1_000).expect("snapshot");
        assert_eq!(face["storage"], json!("inflight"));
        assert_eq!(face["profile"], json!("inflight"));
        note_account_removed(account);
    }
}
