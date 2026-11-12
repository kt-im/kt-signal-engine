// SPDX-License-Identifier: AGPL-3.0-only

//! Storage 记录按账号缓存（BUG-6 收尾：sticker/folders/pinned 三类读面）。
//!
//! 背景：`StorageContextCache`（BUG-6 第一批）消掉的是**开库税**
//! （open_store + 注册数据 + PushService）。storage.* 读面仍逐次付
//! **网络税**：master key → manifest GET → item 读（/v1/storage/read）+
//! auth 换取，一次读 = 2~3 个往返；贴纸包/聊天文件夹/置顶三连查在桌面
//! 会话列表刷新链上被反复触发，单次实测 1.3~3.6s（见 BUG-6 台账）。
//!
//! 本模块把三类读面的**成功后投影**按账号常驻进程内存：
//! - 作用域：每账号一份（`StorageContext.records`），随解绑/换号/删库
//!   与 `StorageContext` 一起丢弃——**不做跨账号全局缓存**，A 的贴纸包/
//!   置顶/文件夹绝不漏给 B（用户护栏 1）。
//! - 失效：只认两条信号（用户护栏 2）——
//!   1) 「storage-pull-settled」：`pull_storage_manifest` 两腿落定后调用
//!      `invalidate()`（云上记录可能已被其他设备改写，下轮读懒回源）；
//!   2) 60s TTL 下限：条目自填充/写穿起 60s 内命中，超过即视为过期，
//!      下一次读才回源——**不做主动/定时刷新**。
//! - 写穿：本机写面成功即以返回的全量投影更新对应槽（写后真相 =
//!   云端真相），不额外回源。
//! - 错误不缓存：读面失败原样上抛，下一次读重新回源（幂等读面纪律）。
//!
//! 并发：命中走 std Mutex 短临界区（毫秒内完成 clone）；未命中由单飞
//! `AsyncMutex` 串行回源——同账号并发 miss 只打一次网络，其余调用等
//! 结果后按命中返回。锁序恒为「fill → inner」，`inner` 临界区内无 await。
//! 上界 = 账号数（惰性建立、随上下文销毁），无需额外 LRU。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use crate::service::storage_sticker::StickerPackSync;
use crate::service::storage_sync::{PinnedEntry, StorageRecordKind, StorageSyncError};

/// 条目新鲜度下限（用户裁决：60s TTL 下限；事件失效是另一条独立信号）。
pub(crate) const STORAGE_RECORD_CACHE_TTL_MS: u64 = 60_000;

/// `(填充时刻毫秒, 投影)`；时刻口径 = 引擎墙钟（`now_ms`）。
type Stamped<T> = (u64, T);

#[derive(Default)]
struct Inner {
    pinned: Option<Stamped<Arc<Vec<PinnedEntry>>>>,
    records: HashMap<StorageRecordKind, Stamped<Arc<Vec<Value>>>>,
    sticker_packs: Option<Stamped<Arc<Vec<StickerPackSync>>>>,
}

impl Inner {
    #[cfg(test)]
    fn slots(&self) -> usize {
        usize::from(self.pinned.is_some())
            + self.records.len()
            + usize::from(self.sticker_packs.is_some())
    }
}

/// 单账号 storage 记录缓存（挂在 `StorageContext` 上，clone 共享）。
pub(crate) struct StorageRecordsCache {
    ttl_ms: u64,
    inner: StdMutex<Inner>,
    /// 回源单飞：同一账号并发 miss 只让一个调用真正走网络。
    fill: AsyncMutex<()>,
    /// 命中/回源计数（观测点：测试断言复用，生产只做原子自增）。
    hits: AtomicU64,
    fills: AtomicU64,
    /// 写穿/失效代数：在飞回源若跨越一次写穿/失效，其结果不得覆盖更新的
    /// 槽（旧读结果晚到 = 把用户刚写的值打回旧态；TTL ≤60s 兜底但可避免）。
    generation: AtomicU64,
}

impl StorageRecordsCache {
    pub(crate) fn new() -> Self {
        Self::with_ttl(STORAGE_RECORD_CACHE_TTL_MS)
    }

    fn with_ttl(ttl_ms: u64) -> Self {
        Self {
            ttl_ms,
            inner: StdMutex::new(Inner::default()),
            fill: AsyncMutex::new(()),
            hits: AtomicU64::new(0),
            fills: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        }
    }

    /// 槽内容变更（写穿/失效）后：在飞回源不落缓存。
    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// 通用取用：命中（未过期）直接 clone 返回；未命中单飞回源后落缓存。
    async fn get_or_fill<T, F, Fut>(
        &self,
        now_ms: u64,
        probe: impl Fn(&Inner) -> Option<Stamped<Arc<Vec<T>>>>,
        insert: impl Fn(&mut Inner, u64, Arc<Vec<T>>),
        load: F,
    ) -> Result<Vec<T>, StorageSyncError>
    where
        T: Clone,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<T>, StorageSyncError>>,
    {
        if let Some(value) = self.fresh(now_ms, &probe) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok((*value).clone());
        }
        // 单飞：拿不到锁说明另一调用正在回源；等它落缓存后按命中返回。
        let _guard = self.fill.lock().await;
        if let Some(value) = self.fresh(now_ms, &probe) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok((*value).clone());
        }
        let generation = self.generation.load(Ordering::Relaxed);
        let loaded = load().await?;
        // 回源期间发生过写穿/失效：旧读结果不落缓存（调用方仍拿到本次
        // 读值，下一次读按新代回源/命中）。
        if self.generation.load(Ordering::Relaxed) != generation {
            return Ok(loaded);
        }
        let value = Arc::new(loaded);
        {
            let mut inner = self.inner.lock().expect("storage record cache poisoned");
            insert(&mut inner, now_ms, Arc::clone(&value));
        }
        self.fills.fetch_add(1, Ordering::Relaxed);
        Ok((*value).clone())
    }

    fn fresh<T>(
        &self,
        now_ms: u64,
        probe: &impl Fn(&Inner) -> Option<Stamped<Arc<Vec<T>>>>,
    ) -> Option<Arc<Vec<T>>> {
        let inner = self.inner.lock().expect("storage record cache poisoned");
        let (filled_at, value) = probe(&inner)?;
        (now_ms.saturating_sub(filled_at) < self.ttl_ms).then_some(value)
    }

    /// 置顶（getPinnedConversations 读面）命中/回源。
    pub(crate) async fn pinned<F, Fut>(
        &self,
        now_ms: u64,
        load: F,
    ) -> Result<Vec<PinnedEntry>, StorageSyncError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<PinnedEntry>, StorageSyncError>>,
    {
        self.get_or_fill(
            now_ms,
            |inner| inner.pinned.clone(),
            |inner, at, value| inner.pinned = Some((at, value)),
            load,
        )
        .await
    }

    /// 置顶写穿（setConversationPinned 成功返回写后全量投影）。
    pub(crate) fn store_pinned(&self, now_ms: u64, entries: Vec<PinnedEntry>) {
        self.inner
            .lock()
            .expect("storage record cache poisoned")
            .pinned = Some((now_ms, Arc::new(entries)));
        self.bump_generation();
    }

    /// Storage 记录读面（chatFolders.list / notificationProfiles.list /
    /// storyDistributionLists.list 共用；按 selector 分槽）。
    pub(crate) async fn records<F, Fut>(
        &self,
        kind: StorageRecordKind,
        now_ms: u64,
        load: F,
    ) -> Result<Vec<Value>, StorageSyncError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<Value>, StorageSyncError>>,
    {
        self.get_or_fill(
            now_ms,
            |inner| inner.records.get(&kind).cloned(),
            |inner, at, value| {
                inner.records.insert(kind, (at, value));
            },
            load,
        )
        .await
    }

    /// 记录写穿（setRecords 成功返回写后全量投影）。
    pub(crate) fn store_records(&self, kind: StorageRecordKind, now_ms: u64, values: Vec<Value>) {
        self.inner
            .lock()
            .expect("storage record cache poisoned")
            .records
            .insert(kind, (now_ms, Arc::new(values)));
        self.bump_generation();
    }

    /// 贴纸包安装状态（getStickerPackSyncs 读面）命中/回源。
    pub(crate) async fn sticker_packs<F, Fut>(
        &self,
        now_ms: u64,
        load: F,
    ) -> Result<Vec<StickerPackSync>, StorageSyncError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<StickerPackSync>, StorageSyncError>>,
    {
        self.get_or_fill(
            now_ms,
            |inner| inner.sticker_packs.clone(),
            |inner, at, value| inner.sticker_packs = Some((at, value)),
            load,
        )
        .await
    }

    /// 贴纸包写穿（setStickerPackSync 成功返回写后全量投影）。
    pub(crate) fn store_sticker_packs(&self, now_ms: u64, packs: Vec<StickerPackSync>) {
        self.inner
            .lock()
            .expect("storage record cache poisoned")
            .sticker_packs = Some((now_ms, Arc::new(packs)));
        self.bump_generation();
    }

    /// 「storage-pull-settled」失效：manifest 两腿落定 = 云上记录可能已被
    /// 其他设备改写，整账号记录缓存作废（下一轮读懒回源，不主动预取）。
    pub(crate) fn invalidate(&self) {
        *self.inner.lock().expect("storage record cache poisoned") = Inner::default();
        self.bump_generation();
    }

    /// 当前缓存槽数（解绑/失效/写穿测试的观测点）。
    #[cfg(test)]
    pub(crate) fn cached_slots(&self) -> usize {
        self.inner
            .lock()
            .expect("storage record cache poisoned")
            .slots()
    }

    #[cfg(test)]
    pub(crate) fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn fills(&self) -> u64 {
        self.fills.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::storage_sync::PinnedEntry;

    fn sticker_pack(id: u8, position: u32) -> StickerPackSync {
        StickerPackSync {
            pack_id: vec![id; 16],
            pack_key: Some(vec![id; 32]),
            position: Some(position),
            deleted_at_timestamp_ms: None,
        }
    }

    fn pinned_entry() -> PinnedEntry {
        PinnedEntry {
            id: None,
            raw: Default::default(),
        }
    }

    /// 命中：60s 内第二次读不再回源；TTL 下限到点后下一次读才回源。
    #[tokio::test]
    async fn hit_within_ttl_and_lazy_refill_after_floor() {
        let cache = StorageRecordsCache::new();
        let loads = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&loads);

        let first = cache
            .sticker_packs(1_000, || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(vec![sticker_pack(1, 1)])
                }
            })
            .await
            .expect("fill");
        assert_eq!(first.len(), 1);
        assert_eq!(loads.load(Ordering::Relaxed), 1);

        // 59,999ms 后仍新鲜：命中且不回源。
        let second = cache
            .sticker_packs(1_000 + STORAGE_RECORD_CACHE_TTL_MS - 1, || {
                let counter = Arc::clone(&loads);
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(Vec::new())
                }
            })
            .await
            .expect("hit");
        assert_eq!(second.len(), 1, "TTL 内命中返回缓存投影");
        assert_eq!(loads.load(Ordering::Relaxed), 1, "TTL 内不得回源");
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.fills(), 1);

        // 恰满 60s = 过期：懒回源（不是主动刷新）。
        let third = cache
            .sticker_packs(1_000 + STORAGE_RECORD_CACHE_TTL_MS, || {
                let counter = Arc::clone(&loads);
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(vec![sticker_pack(2, 1), sticker_pack(3, 2)])
                }
            })
            .await
            .expect("refill");
        assert_eq!(third.len(), 2);
        assert_eq!(loads.load(Ordering::Relaxed), 2);
    }

    /// 事件失效：invalidate 后同一时刻下一次读立即回源。
    #[tokio::test]
    async fn invalidate_forces_next_read_to_refill() {
        let cache = StorageRecordsCache::new();
        let loads = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&loads);
        let fill = |counter: Arc<AtomicU64>, value: Vec<PinnedEntry>| {
            move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(value)
                }
            }
        };

        cache
            .pinned(10, fill(Arc::clone(&counter), vec![pinned_entry()]))
            .await
            .expect("fill");
        assert_eq!(cache.cached_slots(), 1);

        cache.invalidate();
        assert_eq!(cache.cached_slots(), 0, "storage-pull-settled 清空记录槽");

        cache
            .pinned(10, fill(counter, Vec::new()))
            .await
            .expect("refill");
        assert_eq!(loads.load(Ordering::Relaxed), 2, "失效后必须重新回源");
    }

    /// 写穿：写面返回的全量投影落缓存，随后读命中且不回源。
    #[tokio::test]
    async fn write_through_updates_without_refetch() {
        let cache = StorageRecordsCache::new();
        let loads = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&loads);
        cache.store_pinned(5, vec![pinned_entry()]);
        let read = cache
            .pinned(5, || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Ok(Vec::new())
                }
            })
            .await
            .expect("hit");
        assert_eq!(read.len(), 1, "写穿投影优先");
        assert_eq!(loads.load(Ordering::Relaxed), 0, "写穿后无需回源");
    }

    /// 并发未命中单飞：两个调用只打一次回源，另一个等结果后命中。
    #[tokio::test]
    async fn concurrent_misses_fill_once() {
        let cache = Arc::new(StorageRecordsCache::new());
        let loads = Arc::new(AtomicU64::new(0));
        let first = {
            let cache = Arc::clone(&cache);
            let loads = Arc::clone(&loads);
            tokio::spawn(async move {
                cache
                    .sticker_packs(20, move || {
                        let loads = Arc::clone(&loads);
                        async move {
                            loads.fetch_add(1, Ordering::Relaxed);
                            tokio::task::yield_now().await;
                            Ok(vec![sticker_pack(7, 1)])
                        }
                    })
                    .await
            })
        };
        let second = {
            let cache = Arc::clone(&cache);
            let loads = Arc::clone(&loads);
            tokio::spawn(async move {
                cache
                    .sticker_packs(20, move || {
                        let loads = Arc::clone(&loads);
                        async move {
                            loads.fetch_add(1, Ordering::Relaxed);
                            tokio::task::yield_now().await;
                            Ok(vec![sticker_pack(7, 1)])
                        }
                    })
                    .await
            })
        };
        let (first, second) = (first.await.expect("join"), second.await.expect("join"));
        assert_eq!(first.expect("first").len(), 1);
        assert_eq!(second.expect("second").len(), 1);
        assert_eq!(loads.load(Ordering::Relaxed), 1, "并发 miss 只允许一次回源");
        assert_eq!(cache.hits(), 1);
    }

    /// 错误不缓存：失败后下一次读重新回源。
    #[tokio::test]
    async fn error_is_not_cached() {
        let cache = StorageRecordsCache::new();
        let attempts = Arc::new(AtomicU64::new(0));
        let first_attempt = Arc::clone(&attempts);
        let result = cache
            .records(StorageRecordKind::ChatFolder, 1, || {
                let attempts = Arc::clone(&first_attempt);
                async move {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    Err(StorageSyncError::ReadFailed)
                }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(cache.cached_slots(), 0, "失败不落缓存");

        let second_attempt = Arc::clone(&attempts);
        let result = cache
            .records(StorageRecordKind::ChatFolder, 1, || {
                let attempts = Arc::clone(&second_attempt);
                async move {
                    attempts.fetch_add(1, Ordering::Relaxed);
                    Ok(Vec::new())
                }
            })
            .await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
    }

    /// 记录槽按 selector 隔离（folder/profile/list 互不覆盖）。
    #[tokio::test]
    async fn record_slots_are_keyed_by_selector() {
        let cache = StorageRecordsCache::new();
        cache.store_records(
            StorageRecordKind::ChatFolder,
            1,
            vec![serde_json::json!({"a": 1})],
        );
        cache.store_records(
            StorageRecordKind::NotificationProfile,
            1,
            vec![serde_json::json!({"b": 2})],
        );
        let folders = cache
            .records(StorageRecordKind::ChatFolder, 1, || async {
                Ok(Vec::new())
            })
            .await
            .expect("folders");
        assert_eq!(folders, vec![serde_json::json!({"a": 1})]);
        let profiles = cache
            .records(StorageRecordKind::NotificationProfile, 1, || async {
                Ok(Vec::new())
            })
            .await
            .expect("profiles");
        assert_eq!(profiles, vec![serde_json::json!({"b": 2})]);
        assert_eq!(cache.cached_slots(), 2);
    }

    /// 每账号一份实例：两个账号的缓存互不可见（不做全局缓存）。
    #[tokio::test]
    async fn account_caches_do_not_share_state() {
        let a = StorageRecordsCache::new();
        let b = StorageRecordsCache::new();
        a.store_sticker_packs(1, vec![sticker_pack(1, 1)]);
        let from_b = b
            .sticker_packs(1, || async { Ok(vec![sticker_pack(2, 1)]) })
            .await
            .expect("b fill");
        assert_eq!(from_b[0].pack_id, vec![2u8; 16]);
        let from_a = a
            .sticker_packs(1, || async { Ok(Vec::new()) })
            .await
            .expect("a hit");
        assert_eq!(from_a[0].pack_id, vec![1u8; 16]);
        assert_eq!(b.fills(), 1);
        assert_eq!(a.fills(), 0);
    }

    /// 在飞回源期间发生写穿：旧读结果不得覆盖新写值（代数闸）。
    #[tokio::test]
    async fn inflight_fill_does_not_clobber_a_concurrent_write_through() {
        let cache = Arc::new(StorageRecordsCache::new());
        let writer = Arc::clone(&cache);
        let loaded = cache
            .pinned(1, move || async move {
                // 回源在飞时写面落定（写后全量投影 = 新值）。
                writer.store_pinned(1, vec![pinned_entry()]);
                Ok(Vec::new())
            })
            .await
            .expect("fill");
        assert!(loaded.is_empty(), "调用方拿本次（旧）读值");
        // 缓存保留写穿的新值；下一次读命中，不得回源。
        let reread = cache
            .pinned(1, || async {
                panic!("write-through 值应命中，不得回源")
            })
            .await
            .expect("hit");
        assert_eq!(reread.len(), 1);
        assert_eq!(cache.fills(), 0, "跨代回源不落缓存");
        assert_eq!(cache.hits(), 1);
    }
}
