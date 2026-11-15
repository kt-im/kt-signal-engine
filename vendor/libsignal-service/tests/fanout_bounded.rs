// SPDX-License-Identifier: AGPL-3.0-only

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use libsignal_service::sender::fanout_bounded;

fn tracked(
    item: usize,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
) -> impl Future<Output = usize> {
    async move {
        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        tokio::task::yield_now().await;
        in_flight.fetch_sub(1, Ordering::SeqCst);
        item * 10
    }
}

#[tokio::test]
async fn respects_limit_and_preserves_order() {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let sends = (0..12usize).map(|item| tracked(item, in_flight.clone(), peak.clone()));
    let out = fanout_bounded(sends, 4).await;

    assert_eq!(out, (0..12usize).map(|item| item * 10).collect::<Vec<_>>());
    assert_eq!(peak.load(Ordering::SeqCst), 4, "应跑满并发上限");
}

#[tokio::test]
async fn limit_one_is_sequential() {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let sends = (0..5usize).map(|item| tracked(item, in_flight.clone(), peak.clone()));
    let out = fanout_bounded(sends, 1).await;

    assert_eq!(out, (0..5usize).map(|item| item * 10).collect::<Vec<_>>());
    assert_eq!(peak.load(Ordering::SeqCst), 1, "limit=1 必须串行");
}