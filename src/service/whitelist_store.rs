// SPDX-License-Identifier: AGPL-3.0-only

//! profileSharing 闸 whitelist 位 map 的磁盘层（发送长尾修4，2026-11-10）。
//!
//! 官方语义（行为对齐，不抄码）：`sendNormalMessage` 以**本地会话**
//! `profileSharing` 位闸本机 profile_key 注入（官方桌面读本地库，无网络
//! 面）。本引擎无本地会话库，该位读面 = Storage Service 联系人记录全量
//! 拉取（`storage_sync::contact_whitelist_map`，实测 1.5-2.3s = send 暗区
//! 根因）。进程内缓存（`service/mod.rs` 的 `whitelist_cache`，TTL 600s）
//! 消掉 TTL 内重复发送的读，但**冷启动/TTL 过期后的首条发送仍付全量拉
//! 取**——本模块把「最近一次成功全量读」落账号目录，让发送路径冷启动即
//! 命中磁盘、过期只做后台 best-effort 刷新。
//!
//! 落盘形状（账号 store 目录伴生，`identity-trust.json` 同位同纪律）：
//! `profile-sharing-whitelist.json`，`{version, accountHash, updatedAtMs, denied}`：
//!
//! - `version`：格式版本，未知版本/坏 JSON 一律当「无 map」（宁可 bootstrap
//!   一次全量读，不复用不可信旧格式）。
//! - `accountHash`：账号不可逆短哈希（`methods::probe_account8` 同源）——
//!   账号目录换用/手工复制后不得把他人 map 当本人用；**不落号码明文**。
//! - `updatedAtMs`：最近一次成功全量读的墙钟毫秒；0 = 写面失效标记（下次
//!   发送后台重拉，绝不阻塞）；**越过未来偏差窗（现刻 + `MAX_FUTURE_SKEW_MS`）
//!   的时刻一律判陈旧**（系统时钟回拨 / 手工改表 / 复制粘贴来的异常时间戳），
//!   旧 map 仍应答、后台刷新（[\`is_fresh\`]）。
//! - `denied`：显式 `whitelisted == false` 的 ACI（小写）集合。**只落拒集**：
//!   位 true 与缺记录在闸判定（`include_profile_key_for`）上同义（fail-open
//!   注入），落全集等于把联系人台账写进引擎（AGENTS.md 边界），落拒集信息量
//!   等价、暴露面最小；读回后 map 只含 false 条目，判定式零变化。
//!
//! 纪律：读面失败一律 `None`（调用方按「无 map」降级，fail-open 语义零
//! 变化）；写面固定「同目录临时文件 + rename」原子替换；**拉取失败绝不落
//! 盘、绝不删除旧 map**（失败不毒缓存——后台刷新失败保留上一份好快照）；
//! 读/写全是同步小文件 IO，LocalSet 单线程内无 await，不会互相踩半写。

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 磁盘 map 文件（账号 store 目录伴生）。
pub(crate) const PERSIST_FILE: &str = "profile-sharing-whitelist.json";
/// 格式版本：未知版本 = 无 map（bootstrap 一次全量读，不复用不可信格式）。
pub(crate) const PERSIST_VERSION: u32 = 1;
/// 磁盘 map 新鲜度窗（毫秒）= 内存缓存 TTL 单源常量：
/// `service/mod.rs` 的 `WHITELIST_CACHE_TTL` 引用本值，二者永不同步漂移。
pub(crate) const FRESH_WINDOW_MS: u64 = 600_000;
/// 未来时刻容差窗（毫秒）：`updatedAtMs` 比现刻晚这么多以内按「新鲜」处理
/// （跨主机小抖动 / 写面与读面之间的毫秒级漂移，不该为它重拉网络）；**超出
/// 即判陈旧**——未来时间戳不能当新鲜用（时钟回拨、手工改表、旧机快照复制
/// 都会造出这种时刻，认可它们等于让陈旧拒集无限续命）。取 5min：远小于
/// 新鲜度窗（10min），抖动余量足够而「异常未来」不可能混进来。
pub(crate) const MAX_FUTURE_SKEW_MS: u64 = 300_000;
/// 写面失效标记值（`updatedAtMs` = 0）：语义 =「map 已知但陈旧，下次发送
/// 后台重拉」，不是「无 map」（无 map 才允许 bootstrap 同步拉取）。
pub(crate) const STALE_MARK_MS: u64 = 0;

/// 落盘 JSON 形状（字段名 camelCase，`identity-trust.json` 先例）。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedFile {
    version: u32,
    account_hash: String,
    updated_at_ms: u64,
    denied: Vec<String>,
}

/// 读面结果：位 map（只含 false 条目）+ 最近成功全量读时刻。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedWhitelist {
    pub(crate) map: HashMap<String, bool>,
    pub(crate) updated_at_ms: u64,
}

/// 账号短哈希（不可逆，复用探针单点口径；空账号 = 空串，不会被接受）。
fn account_hash(account: &str) -> String {
    crate::methods::probe_account8(Some(account))
}

/// 位 map → 落盘字节（只留 `whitelisted == false`；ACI 小写 + 排序去重，
/// 同内容恒同字节——便于对账与测试钉定）。
pub(crate) fn encode(
    account: &str,
    map: &HashMap<String, bool>,
    updated_at_ms: u64,
) -> Option<Vec<u8>> {
    let hash = account_hash(account);
    if hash.is_empty() {
        return None;
    }
    let mut denied: Vec<String> = map
        .iter()
        .filter(|(_, whitelisted)| !**whitelisted)
        .map(|(aci, _)| aci.to_lowercase())
        .filter(|aci| !aci.is_empty())
        .collect();
    denied.sort();
    denied.dedup();
    serde_json::to_vec(&PersistedFile {
        version: PERSIST_VERSION,
        account_hash: hash,
        updated_at_ms,
        denied,
    })
    .ok()
}

/// 落盘字节 → 位 map。版本不符 / 坏 JSON / 账号哈希不符 / 字段缺失 → None
/// （调用方按「无 map」降级 = 允许一次 bootstrap，fail-open 语义不变）。
pub(crate) fn decode(account: &str, bytes: &[u8]) -> Option<PersistedWhitelist> {
    let parsed: PersistedFile = serde_json::from_slice(bytes).ok()?;
    if parsed.version != PERSIST_VERSION {
        return None;
    }
    let hash = account_hash(account);
    if hash.is_empty() || parsed.account_hash != hash {
        return None;
    }
    let mut map = HashMap::new();
    for aci in parsed.denied {
        let aci = aci.to_lowercase();
        if !aci.is_empty() {
            // 只落拒集的读回：false = 显式不分享；缺记录 = 注入（fail-open，
            // 与原读面含 true 条目的 map 判定逐个同义）。
            map.insert(aci, false);
        }
    }
    Some(PersistedWhitelist {
        map,
        updated_at_ms: parsed.updated_at_ms,
    })
}

/// 读磁盘 map（文件/解析失败 → None）。
pub(crate) fn load(account_dir: &Path, account: &str) -> Option<PersistedWhitelist> {
    let bytes = std::fs::read(account_dir.join(PERSIST_FILE)).ok()?;
    decode(account, &bytes)
}

/// 写磁盘 map（临时文件 + rename 原子替换；写失败 false，调用方静默降级）。
pub(crate) fn save(
    account_dir: &Path,
    account: &str,
    map: &HashMap<String, bool>,
    updated_at_ms: u64,
) -> bool {
    let Some(bytes) = encode(account, map, updated_at_ms) else {
        return false;
    };
    if std::fs::create_dir_all(account_dir).is_err() {
        return false;
    }
    let path = account_dir.join(PERSIST_FILE);
    let tmp = account_dir.join(format!("{PERSIST_FILE}.tmp"));
    if std::fs::write(&tmp, bytes).is_err() {
        return false;
    }
    std::fs::rename(&tmp, &path).is_ok()
}

/// 全量拉取结果 → 落盘：`Some(map)` = 成功（原子替换）；`None` = 拉取失败
/// ——**不写不删**（旧 map 原样保留，绝不用空 map 毒化磁盘）。返回是否落盘。
pub(crate) fn persist_fetch(
    account_dir: &Path,
    account: &str,
    fetched: Option<&HashMap<String, bool>>,
    now_ms: u64,
) -> bool {
    match fetched {
        Some(map) => save(account_dir, account, map, now_ms),
        None => false,
    }
}

/// 写面失效标记：map 内容原样保留，`updatedAtMs` 置 0（陈旧）——下次发送
/// 命中磁盘 map 后触发**后台**重拉，不阻塞。无 map（从未成功全量读）=
/// false（保持 bootstrap 语义不动）。
pub(crate) fn mark_stale(account_dir: &Path, account: &str) -> bool {
    let Some(persisted) = load(account_dir, account) else {
        return false;
    };
    save(account_dir, account, &persisted.map, STALE_MARK_MS)
}

/// 新鲜度（三面判据，任一不满足 = 陈旧）：
///
/// 1. `updatedAtMs` 非 0（0 = 写面陈旧标记，**不是**无 map）；
/// 2. 事件在过去：距今 ≤ [`FRESH_WINDOW_MS`]；
/// 3. 事件不得在未来超过 [`MAX_FUTURE_SKEW_MS`]——**未来时间戳不能当新鲜**
///    （时钟回拨 / 异常写入），只有小抖动窗内的未来时刻按新鲜处理，免得为
///    毫秒级漂移白拉一次网络。
///
/// 判陈旧的唯一后果是「排后台刷新」，旧 map 照常应答（调用方零阻塞）。
pub(crate) fn is_fresh(updated_at_ms: u64, now_ms: u64) -> bool {
    if updated_at_ms == STALE_MARK_MS {
        return false;
    }
    if updated_at_ms > now_ms {
        // 未来时刻：只容忍偏差窗内的抖动。
        return updated_at_ms - now_ms <= MAX_FUTURE_SKEW_MS;
    }
    now_ms - updated_at_ms <= FRESH_WINDOW_MS
}

/// 磁盘 map 剩余新鲜寿命（毫秒）：新鲜 → 窗内剩余，陈旧 → 0。内存缓存
/// deadline 按本值对齐磁盘新鲜度，避免「内存续命」把陈旧 map 拖过新鲜窗。
/// 偏差窗内的未来时刻按「刚写完」计（满窗剩余）；越界未来时刻 = 陈旧 = 0。
pub(crate) fn freshness_left_ms(updated_at_ms: u64, now_ms: u64) -> u64 {
    if !is_fresh(updated_at_ms, now_ms) {
        return 0;
    }
    FRESH_WINDOW_MS.saturating_sub(now_ms.saturating_sub(updated_at_ms))
}

/// 单点增量：写面已知的 (ACI, 位) 应用到 map（位 true = 从拒集移除，false
/// = 入拒集）。空 ACI 忽略。
pub(crate) fn apply(map: &mut HashMap<String, bool>, aci: &str, whitelisted: bool) {
    let aci = aci.to_lowercase();
    if aci.is_empty() {
        return;
    }
    if whitelisted {
        map.remove(&aci);
    } else {
        map.insert(aci, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let unique = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kt-engine-whitelist-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    const ACCOUNT: &str = "+8618827970450";

    fn map_of(entries: &[(&str, bool)]) -> HashMap<String, bool> {
        entries
            .iter()
            .map(|(aci, bit)| ((*aci).to_string(), *bit))
            .collect()
    }

    /// 落盘只留拒集：位 true 条目丢弃（缺记录 = 注入，判定同义），ACI 小写
    /// 排序确定；读回 map 只含 false 条目。
    #[test]
    fn persist_keeps_only_deny_set_and_normalizes() {
        let dir = temp_dir("roundtrip");
        let map = map_of(&[
            ("AAAAAAAA-1111", false),
            ("bbbbbbbb-2222", true),
            ("cccccccc-3333", false),
        ]);
        assert!(save(&dir, ACCOUNT, &map, 1_700_000_000_000));
        let raw = std::fs::read_to_string(dir.join(PERSIST_FILE)).expect("file");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(parsed["version"], serde_json::json!(PERSIST_VERSION));
        assert_eq!(
            parsed["updatedAtMs"],
            serde_json::json!(1_700_000_000_000u64)
        );
        assert_eq!(
            parsed["denied"],
            serde_json::json!(["aaaaaaaa-1111", "cccccccc-3333"]),
            "只落拒集 + 小写 + 排序"
        );
        let back = load(&dir, ACCOUNT).expect("load");
        assert_eq!(
            back.map,
            map_of(&[("aaaaaaaa-1111", false), ("cccccccc-3333", false)])
        );
        assert_eq!(back.updated_at_ms, 1_700_000_000_000);
        // 拒集读回在闸判定上与原 map 逐条同义：缺记录与位 true 都注入。
        assert!(!back.map.contains_key("bbbbbbbb-2222"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空 map 也落盘（真实的「零拒集」≠「没有 map」）——冷启动不得再
    /// bootstrap 同步拉取。
    #[test]
    fn empty_deny_set_still_persists_a_usable_map() {
        let dir = temp_dir("empty");
        assert!(save(&dir, ACCOUNT, &HashMap::new(), 1_700_000_000_000));
        let back = load(&dir, ACCOUNT).expect("empty map is still a map");
        assert!(back.map.is_empty());
        assert!(is_fresh(back.updated_at_ms, 1_700_000_000_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 不可信格式一律当无 map：版本不符 / 坏 JSON / 账号哈希不符 / 字段缺失。
    #[test]
    fn untrusted_formats_decode_to_none() {
        let dir = temp_dir("untrusted");
        assert!(save(&dir, ACCOUNT, &map_of(&[("aaaaaaaa-1111", false)]), 1));
        let path = dir.join(PERSIST_FILE);
        let good = std::fs::read(&path).expect("read");

        // 版本不符。
        let mut value: serde_json::Value = serde_json::from_slice(&good).expect("json");
        value["version"] = serde_json::json!(PERSIST_VERSION + 1);
        assert!(decode(ACCOUNT, &serde_json::to_vec(&value).expect("bytes")).is_none());
        // 账号哈希不符（目录换用/复制）。
        let mut value: serde_json::Value = serde_json::from_slice(&good).expect("json");
        value["accountHash"] = serde_json::json!("deadbeef");
        assert!(decode(ACCOUNT, &serde_json::to_vec(&value).expect("bytes")).is_none());
        // 坏 JSON / 空字节。
        assert!(decode(ACCOUNT, b"not json").is_none());
        assert!(decode(ACCOUNT, b"").is_none());
        // 字段缺失。
        assert!(decode(ACCOUNT, br#"{"version":1}"#).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「失败不毒缓存」：拉取失败（None）不写不删，旧 map 原样；成功才替换。
    #[test]
    fn failed_fetch_never_poisons_the_previous_map() {
        let dir = temp_dir("failure");
        let old = map_of(&[("aaaaaaaa-1111", false)]);
        assert!(save(&dir, ACCOUNT, &old, 1_700_000_000_000));
        // 失败面：文件字节逐字不动。
        let before = std::fs::read(dir.join(PERSIST_FILE)).expect("read");
        assert!(!persist_fetch(&dir, ACCOUNT, None, 1_700_000_999_000));
        assert_eq!(
            std::fs::read(dir.join(PERSIST_FILE)).expect("read"),
            before,
            "失败绝不改写磁盘"
        );
        assert_eq!(load(&dir, ACCOUNT).expect("old stays").map, old);
        // 成功面：替换。
        let new = map_of(&[("bbbbbbbb-2222", false)]);
        assert!(persist_fetch(&dir, ACCOUNT, Some(&new), 1_700_001_000_000));
        let back = load(&dir, ACCOUNT).expect("new");
        assert_eq!(back.map, new);
        assert_eq!(back.updated_at_ms, 1_700_001_000_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新鲜度窗：窗内新鲜、窗外陈旧、0 标记陈旧、未来小抖动新鲜、越界未来
    /// 陈旧；剩余寿命按窗对齐。
    #[test]
    fn freshness_window_and_remaining_lifetime() {
        let now = 1_700_000_000_000u64;
        assert!(is_fresh(now, now));
        assert!(is_fresh(now - FRESH_WINDOW_MS, now));
        assert!(!is_fresh(now - FRESH_WINDOW_MS - 1, now));
        assert!(!is_fresh(STALE_MARK_MS, now), "写面失效标记 = 陈旧");
        assert!(is_fresh(now + 5_000, now), "毫秒级抖动不触发无谓重拉");
        assert!(is_fresh(now + MAX_FUTURE_SKEW_MS, now), "偏差窗边界仍新鲜");
        assert!(
            !is_fresh(now + MAX_FUTURE_SKEW_MS + 1, now),
            "越过偏差窗的未来时刻 = 陈旧"
        );
        assert_eq!(freshness_left_ms(now, now), FRESH_WINDOW_MS);
        assert_eq!(
            freshness_left_ms(now - 100_000, now),
            FRESH_WINDOW_MS - 100_000
        );
        assert_eq!(freshness_left_ms(now - FRESH_WINDOW_MS - 1, now), 0);
        assert_eq!(freshness_left_ms(STALE_MARK_MS, now), 0);
        assert_eq!(
            freshness_left_ms(now + 5_000, now),
            FRESH_WINDOW_MS,
            "偏差窗内未来时刻按刚写完计"
        );
        assert_eq!(
            freshness_left_ms(now + MAX_FUTURE_SKEW_MS + 1, now),
            0,
            "越界未来无剩余寿命"
        );
    }

    /// 未来时间戳边界（时钟回拨 / 异常写入）：`updatedAtMs` 远在未来不得当
    /// 新鲜——偏差窗内外逐毫秒分界，且指向未来的时刻只影响新鲜度，不改变
    /// 拒集内容（旧 map 仍可读、位判定同义）。
    #[test]
    fn future_timestamp_beyond_skew_is_never_fresh() {
        let now = 1_700_000_000_000u64;
        // 一天后的未来：典型时钟回拨面。
        assert!(!is_fresh(now + 86_400_000, now), "远未来 = 陈旧");
        assert_eq!(freshness_left_ms(now + 86_400_000, now), 0);
        // 窗内外的分界逐个钉定（不靠近似）。
        let edge = now + MAX_FUTURE_SKEW_MS;
        assert!(is_fresh(edge, now), "偏差窗闭区间内 = 新鲜");
        assert!(!is_fresh(edge + 1, now), "偏差窗外 = 陈旧");
        // u64 上界附近不得 panic（saturating 口径）。
        assert!(!is_fresh(u64::MAX, now));
        assert_eq!(freshness_left_ms(u64::MAX, now), 0);
        // 未来时刻不影响拒集落盘/读回：内容面与新鲜度解耦。
        let dir = temp_dir("future");
        let map = map_of(&[("aaaaaaaa-1111", false)]);
        assert!(save(&dir, ACCOUNT, &map, now + 86_400_000));
        let back = load(&dir, ACCOUNT).expect("map readable regardless of skew");
        assert_eq!(back.map, map);
        assert!(!is_fresh(back.updated_at_ms, now), "读回仍判陈旧");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写面失效：map 原样、时刻归 0（陈旧）、拒绝下一次「新鲜」判定；无 map
    /// 时不凭空造文件（bootstrap 语义不动）。
    #[test]
    fn mark_stale_keeps_map_but_expires_freshness() {
        let dir = temp_dir("stale");
        assert!(!mark_stale(&dir, ACCOUNT), "无 map 不造文件");
        assert!(load(&dir, ACCOUNT).is_none());
        let map = map_of(&[("aaaaaaaa-1111", false)]);
        assert!(save(&dir, ACCOUNT, &map, 1_700_000_000_000));
        assert!(mark_stale(&dir, ACCOUNT));
        let back = load(&dir, ACCOUNT).expect("map kept");
        assert_eq!(back.map, map, "只改时刻不丢位");
        assert!(!is_fresh(back.updated_at_ms, 1_700_000_000_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 增量单点：位 true 出拒集，位 false 入拒集；空 ACI 忽略。
    #[test]
    fn delta_apply_moves_aci_in_and_out_of_deny_set() {
        let mut map = HashMap::new();
        apply(&mut map, "AAAAAAAA-1111", false);
        assert_eq!(map.get("aaaaaaaa-1111"), Some(&false));
        apply(&mut map, "aaaaaaaa-1111", true);
        assert!(!map.contains_key("aaaaaaaa-1111"), "位 true = 回注入面");
        apply(&mut map, "", false);
        assert!(map.is_empty());
    }
}
