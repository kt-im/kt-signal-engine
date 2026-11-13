// SPDX-License-Identifier: AGPL-3.0-only

//! 引擎契约修订（`contractRevision`）单源。
//!
//! 契约修订是 engine 与 connector 共用的契约编号（connector
//! `docs/implementation-plan.md` 的 `Contract revision` 序列），不是本仓
//! `CARGO_PKG_VERSION`——包版本随每次构建漂移，契约修订只在 wire 契约
//! 变化时推进。引擎在两条自证面原样自报同一个值：
//!
//! 1. `version` 应答的 additive 键 `contractRevision`；
//! 2. `engine.protocolCapabilities` 应答的 additive 键
//!    `contractRevision`（connector 视为权威面，version 面是回退面）。
//!
//! connector O1 握手（`kt-signal-connector/src/engine_contract.rs`）以闭
//! 区间 `ENGINE_CONTRACT_MIN_REVISION = "1.200"` ..
//! `ENGINE_CONTRACT_MAX_REVISION = "1.285"` 做数字比较：面内 = `match`，
//! 无修订 = `legacy`（降级仍服务），面外/畸形 = 不可重试的
//! `ENGINE_CONTRACT_MISMATCH`。release service 的 `engineContractRevision`
//! 声明把引擎侧钉成 `min == max ==` 本常量；推进本常量必须同步 connector
//! 的区间上界与该项声明，否则握手只会在旧区间内 `legacy`/失配。
//!
//! 单源纪律：本常量是唯一事实源，两个自证面都从这里取；改契约修订只改
//! 这一处（外加 connector 侧区间声明），不得在应答体里各写一份字面量。

/// 引擎实现的共享契约修订。connector 当前支持区间 1.200-1.285，引擎侧
/// 取区间上界（release 声明 `min == max`）：本批 O1 握手引擎腿即按共享
/// 契约 1.285 落地。
pub(crate) const CONTRACT_REVISION: &str = "1.285";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (u64, u64, u64) {
        let mut parts = text.trim().split('.');
        let major: u64 = parts.next().expect("major").parse().expect("major u64");
        let minor: u64 = parts.next().expect("minor").parse().expect("minor u64");
        // connector 口径 major.minor[.patch]：patch 可省（省 = 0）。
        let patch: u64 = match parts.next() {
            Some(part) => part.parse().expect("patch u64"),
            None => 0,
        };
        assert!(parts.next().is_none(), "at most major.minor.patch");
        (major, minor, patch)
    }

    /// 常量形状 + 值钉定：改值必须同时改这里（撞闸即提醒复核 connector
    /// 区间与 release 声明），形状必须是 connector 可数字比较的
    /// `major.minor[.patch]`。
    #[test]
    fn contract_revision_is_the_pinned_shared_number() {
        assert_eq!(CONTRACT_REVISION, "1.285");
        let parsed = parse(CONTRACT_REVISION);
        assert_eq!(parsed, (1, 285, 0));
    }

    /// 与 connector 当前支持区间对齐（connector
    /// `ENGINE_CONTRACT_MIN_REVISION`/`ENGINE_CONTRACT_MAX_REVISION` 的
    /// 镜像断言）：常量落在 1.200-1.285 闭区间内，connector O1 判 `match`。
    /// connector 推进上界时本断言不拦（常量仍在区间内），只有常量被推到
    /// 区间外才红灯。
    #[test]
    fn contract_revision_sits_in_the_connector_supported_interval() {
        let revision = parse(CONTRACT_REVISION);
        assert!(revision >= parse("1.200"), "below the connector floor");
        assert!(revision <= parse("1.285"), "above the connector ceiling");
    }
}
