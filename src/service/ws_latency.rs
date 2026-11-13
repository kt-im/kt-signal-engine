// SPDX-License-Identifier: AGPL-3.0-only

//! WebSocket 收帧 → 接收循环的只读延迟探针。
//!
//! 目的：把「服务端晚到」与「本地排队」分开。服务器时间戳差值只能说明
//! 服务端到本进程的总延迟；本模块记录帧进入传输栈的本地 Instant，并把它
//! 随信封带到 receive loop，处理点再算一次差值：
//!
//! - `ws_frame_received`：收帧处一拍（seq / frame_bytes），证明帧已到本机；
//! - `ws_frame_to_loop`：信封到达 receive loop 时的一拍（delay_ms），
//!   这段差值就是 presage 批窗/解密/落库等本地排队时间。
//!
//! 纪律：warn 级（滚动文件订阅器只收 warn+）；只打序号、字节数与毫秒差，
//! 不含账号、会话、正文；采样由 [FrameLatencySampler] 统一裁定（前 50 帧
//! 恒打，此后 `ws_frame_to_loop` 仅 delay_ms >= 200 且距上次 >= 200ms 才打），
//! 任何采样失败都只返回 None，不影响接收主流程。

use std::time::Instant;

use presage::libsignal_service::messagepipe::{FrameLatencySampler, WsFrameStamp};
use presage::model::messages::Received;

/// 单位：毫秒。与 `FrameLatencySampler` 的阈值保持单一来源。
pub(crate) struct WsLatencySample {
    pub(crate) delay_ms: u64,
    pub(crate) frame_bytes: usize,
    pub(crate) seq: u64,
}

/// 每条接收流的探针状态（重连重开即重新热身，符合「新流新现场」）。
pub(crate) struct WsLatencyProbe {
    sampler: FrameLatencySampler,
}

impl WsLatencyProbe {
    pub(crate) fn new() -> Self {
        Self {
            sampler: FrameLatencySampler::new(),
        }
    }

    /// 读取收流 item 携带的帧戳；没有戳（QueueEmpty / GroupSyncFailed 等
    /// 无独立 ack 的条目）直接返回 None。
    pub(crate) fn observe(&mut self, item: &Option<Received>) -> Option<WsLatencySample> {
        self.observe_at(received_frame_stamp(item), Instant::now())
    }

    /// 纯函数核心：给定帧戳与当前时刻，按采样门槛决定是否产出一条样本。
    /// 单独暴露只为单测钉定 delay_ms 与采样边界，不参与生产分支。
    pub(crate) fn observe_at(
        &mut self,
        stamp: Option<WsFrameStamp>,
        now: Instant,
    ) -> Option<WsLatencySample> {
        let stamp = stamp?;
        let delay_ms = stamp.delay_ms_until(now);
        if !self.sampler.should_log_loop(delay_ms, now) {
            return None;
        }
        Some(WsLatencySample {
            delay_ms,
            frame_bytes: stamp.frame_bytes(),
            seq: stamp.seq(),
        })
    }
}

/// 从 receive loop 的 item 上取帧戳。所有带 EnvelopeAck 的变体都携带收帧
/// 时刻；QueueEmpty/GroupSyncFailed 没有独立 ack，返回 None。
fn received_frame_stamp(item: &Option<Received>) -> Option<WsFrameStamp> {
    match item.as_ref()? {
        Received::Content(_, ack)
        | Received::Duplicate(_, ack)
        | Received::IdentityChange(_, _, ack)
        | Received::DecryptionError(_, _, _, ack) => ack.frame_stamp(),
        Received::Contacts(_, Some(ack)) => ack.frame_stamp(),
        Received::Contacts(_, None) | Received::QueueEmpty | Received::GroupSyncFailed { .. } => {
            None
        }
    }
}

/// 处理点日志：固定 stage + 毫秒差 + 帧字节数 + 单调序号，无账号/内容。
pub(crate) fn log_frame_to_loop(sample: &WsLatencySample) {
    tracing::warn!(
        target: "engine.latency",
        stage = "ws_frame_to_loop",
        delay_ms = sample.delay_ms,
        frame_bytes = sample.frame_bytes,
        seq = sample.seq,
        "websocket frame reached receive loop"
    );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use presage::libsignal_service::messagepipe::{
        LATENCY_PROBE_DELAY_THRESHOLD_MS, LATENCY_PROBE_WARMUP_FRAMES,
    };

    fn stamp(received_at: Instant, seq: u64, frame_bytes: usize) -> WsFrameStamp {
        WsFrameStamp::from_parts(received_at, seq, frame_bytes)
    }

    #[test]
    fn envelope_stamp_delay_is_measured_from_frame_receipt() {
        let now = Instant::now();
        let received_at = now
            .checked_sub(Duration::from_millis(250))
            .expect("clock is far enough from the epoch");
        let mut probe = WsLatencyProbe::new();

        let sample = probe
            .observe_at(Some(stamp(received_at, 9, 4096)), now)
            .expect("warmup frame is always sampled");

        assert_eq!(sample.delay_ms, 250);
        assert_eq!(sample.frame_bytes, 4096);
        assert_eq!(sample.seq, 9);
    }

    #[test]
    fn sampler_warmup_then_delay_threshold_and_interval() {
        let start = Instant::now();
        let mut probe = WsLatencyProbe::new();

        // 前 50 帧恒采样，哪怕延迟为 0。
        for _ in 0..LATENCY_PROBE_WARMUP_FRAMES {
            assert!(probe.observe_at(Some(stamp(start, 1, 1)), start).is_some());
        }

        // 热身结束：亚阈值延迟直接丢弃。
        assert!(probe.observe_at(Some(stamp(start, 2, 1)), start).is_none());
        assert!(probe
            .observe_at(
                Some(stamp(start, 3, 1)),
                start + Duration::from_millis(LATENCY_PROBE_DELAY_THRESHOLD_MS - 1)
            )
            .is_none());

        // 达到阈值即产出一条，随后 200ms 内重复事件被节流。
        let at_threshold = start + Duration::from_millis(LATENCY_PROBE_DELAY_THRESHOLD_MS);
        assert!(probe
            .observe_at(Some(stamp(start, 4, 1)), at_threshold)
            .is_some());
        assert!(probe
            .observe_at(
                Some(stamp(start, 5, 1)),
                at_threshold + Duration::from_millis(199)
            )
            .is_none());
        assert!(probe
            .observe_at(
                Some(stamp(start, 6, 1)),
                at_threshold + Duration::from_millis(200)
            )
            .is_some());
    }

    #[test]
    fn missing_frame_stamp_is_silent() {
        let mut probe = WsLatencyProbe::new();
        assert!(probe.observe_at(None, Instant::now()).is_none());
    }
}
