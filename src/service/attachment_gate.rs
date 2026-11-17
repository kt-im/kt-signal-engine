// SPDX-License-Identifier: AGPL-3.0-only

//! 附件上行全局闸（M5 稳定性批）：并发上限 + 全局字节预算。
//!
//! 背景：send/sendStory/sendContact 的附件以 base64 data URI 内联在单帧
//! 里，解码后全部驻留内存直到 CDN 上行完成。单请求 32 条 × 100MB 在服务
//! 层闸内合法，但多个并发请求叠加会把引擎 RSS 推到 GiB 级。本模块给进程
//! 级两道闸：
//!
//! - 并发槽（MAX_CONCURRENT_UPLOADS）：同时最多 N 个附件上行批次；
//!   第 N+1 个等待（有界 UPLOAD_SLOT_WAIT），等不到回结构化
//!   SendError::AttachmentBusy——排队有界，不无限堆积。
//! - 字节预算（GLOBAL_BYTE_BUDGET）：在 base64 decode 之前按 payload
//!   长度（解码字节上界）预留；不足立即拒绝
//!   SendError::AttachmentBudgetExceeded，不先解一份大内存再发现超限。
//!   预留随 lease drop 释放（上传结束/失败/取消都覆盖）。
//!
//! 单附件 100MB、单请求 32 条的服务层闸保留，受本全局闸约束：请求先过
//! 条数/单条闸，再按整批 payload 预留字节；预算不足时整批拒绝。

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::SendError;

/// 同时进行的附件上行批次数（2–4 区间；取 2 与 CDN/加密 CPU 预算一致）。
pub(crate) const MAX_CONCURRENT_UPLOADS: usize = 2;

/// 全局解码字节预算：2 个 160MiB 满帧解出的附件（各约 120MiB）可并存，
/// 第 3 个请求在 decode 前被拒；单请求超预算同样立即拒绝。
pub(crate) const GLOBAL_BYTE_BUDGET: u64 = 320 * 1024 * 1024;

/// 等上行槽的上限：等不到即回 busy（不把请求无限挂在队列里）。
pub(crate) const UPLOAD_SLOT_WAIT: Duration = Duration::from_secs(10);

/// 进程级附件闸（Service 单实例持有）。
pub(crate) struct AttachmentGate {
    slots: Arc<Semaphore>,
    used_bytes: StdMutex<u64>,
    budget_bytes: u64,
    slot_wait: Duration,
}

/// 已预留字节 + 已占用上行槽的租约：drop 时自动释放预算与槽位
/// （正常返回、上传失败、dispatch 超时取消都覆盖）。
pub(crate) struct AttachmentLease {
    gate: Arc<AttachmentGate>,
    _permit: OwnedSemaphorePermit,
    reserved_bytes: u64,
}

impl AttachmentGate {
    pub(crate) fn new(concurrency: usize, budget_bytes: u64) -> Arc<Self> {
        Self::with_wait(concurrency, budget_bytes, UPLOAD_SLOT_WAIT)
    }

    fn with_wait(concurrency: usize, budget_bytes: u64, slot_wait: Duration) -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(concurrency.max(1))),
            used_bytes: StdMutex::new(0),
            budget_bytes,
            slot_wait,
        })
    }

    /// 预留 bytes（base64 payload 长度 = 解码字节上界）并占用一个上行槽。
    ///
    /// 顺序：先判单请求是否注定超预算（立即拒，不白等槽）→ 等槽（有界）
    /// → 原子预留（并发请求在锁内判总量）。任何一步失败都不留残留。
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        bytes: u64,
    ) -> Result<AttachmentLease, SendError> {
        if bytes > self.budget_bytes {
            return Err(SendError::AttachmentBudgetExceeded);
        }
        let permit =
            match tokio::time::timeout(self.slot_wait, Arc::clone(&self.slots).acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                // 槽关闭（Service 收尾）或等待超时：都按可重试 busy 回。
                Ok(Err(_)) | Err(_) => return Err(SendError::AttachmentBusy),
            };
        if !self.try_reserve(bytes) {
            return Err(SendError::AttachmentBudgetExceeded);
        }
        Ok(AttachmentLease {
            gate: Arc::clone(self),
            _permit: permit,
            reserved_bytes: bytes,
        })
    }

    fn try_reserve(&self, bytes: u64) -> bool {
        let mut used = self.used_bytes.lock().expect("attachment budget lock");
        match used.checked_add(bytes) {
            Some(total) if total <= self.budget_bytes => {
                *used = total;
                true
            }
            _ => false,
        }
    }

    fn release(&self, bytes: u64) {
        let mut used = self.used_bytes.lock().expect("attachment budget lock");
        *used = used.saturating_sub(bytes);
    }

    /// 当前已预留字节（测试/观测用；纯读不改状态）。
    #[cfg(test)]
    pub(crate) fn in_use_bytes(&self) -> u64 {
        *self.used_bytes.lock().expect("attachment budget lock")
    }
}

impl Drop for AttachmentLease {
    fn drop(&mut self) {
        self.gate.release(self.reserved_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_a_request_larger_than_the_whole_budget_without_waiting() {
        let gate = AttachmentGate::new(2, 100);
        match gate.acquire(101).await {
            Err(SendError::AttachmentBudgetExceeded) => {}
            _ => panic!("oversized request must be rejected before decoding"),
        }
        assert_eq!(gate.in_use_bytes(), 0);
    }

    #[tokio::test]
    async fn enforces_the_global_byte_budget_across_requests() {
        let gate = AttachmentGate::new(2, 100);
        let first = gate.acquire(60).await.expect("first fits");
        assert_eq!(gate.in_use_bytes(), 60);
        match gate.acquire(60).await {
            Err(SendError::AttachmentBudgetExceeded) => {}
            _ => panic!("second request must be rejected while 60 bytes are held"),
        }
        // 被拒的请求不能泄漏预算或槽位。
        assert_eq!(gate.in_use_bytes(), 60);
        drop(first);
        assert_eq!(gate.in_use_bytes(), 0);
        let second = gate.acquire(60).await.expect("budget released on drop");
        drop(second);
        assert_eq!(gate.in_use_bytes(), 0);
    }

    #[tokio::test]
    async fn reports_busy_when_upload_slots_stay_saturated() {
        let gate = AttachmentGate::with_wait(1, 100, Duration::from_millis(30));
        let held = gate.acquire(10).await.expect("slot available");
        match gate.acquire(10).await {
            Err(SendError::AttachmentBusy) => {}
            _ => panic!("second request must backpressure with a structured busy error"),
        }
        drop(held);
        let after = gate.acquire(10).await.expect("slot released");
        drop(after);
    }

    #[tokio::test]
    async fn waiting_requests_do_not_reserve_bytes_before_a_slot() {
        let gate = AttachmentGate::new(1, 100);
        let held = gate.acquire(90).await.expect("slot available");
        let waiter = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.acquire(90).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(gate.in_use_bytes(), 90);
        drop(held);
        let second = waiter.await.expect("waiter joins").expect("slot released");
        assert_eq!(gate.in_use_bytes(), 90);
        drop(second);
        assert_eq!(gate.in_use_bytes(), 0);
    }
}
