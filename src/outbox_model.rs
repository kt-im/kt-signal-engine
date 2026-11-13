// SPDX-License-Identifier: AGPL-3.0-only

//! Duplicate-envelope wire key: `dup:{sender}:{client_timestamp}`.
//!
//! Phase2 (2026-11-10, receive-duplicate decision B): the key is a wire-level
//! label only. Store rows are located by thread+ts (receive arm) or sender+ts
//! (legacy `receive.replay` face), never by this key, so the pre-persistence
//! `OutboxModel` that once mirrored the presage `kt_outbox` table is gone
//! together with the table.

pub(crate) fn duplicate_dedupe_key(sender: &str, timestamp: u64) -> String {
    format!("dup:{sender}:{timestamp}")
}

/// Phase2：dup key 只作为 wire 标识解析，不再是 store 行键。返回
/// `(sender, client_timestamp)`；格式不符（缺前缀、空 sender、ts 非整数）
/// 一律 None，调用方按 Failed 收口（无法定位行 ≠ 确认无行，不能 flush
/// 后 ack）。
pub(crate) fn parse_duplicate_dedupe_key(key: &str) -> Option<(String, u64)> {
    let rest = key.strip_prefix("dup:")?;
    let (sender, timestamp) = rest.rsplit_once(':')?;
    if sender.is_empty() {
        return None;
    }
    Some((sender.to_owned(), timestamp.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_key_is_stable_and_identity_scoped() {
        assert_eq!(duplicate_dedupe_key("aci-a", 42), "dup:aci-a:42");
        assert_ne!(
            duplicate_dedupe_key("aci-a", 42),
            duplicate_dedupe_key("aci-b", 42)
        );
    }

    #[test]
    fn duplicate_key_roundtrips_and_rejects_garbage() {
        let key = duplicate_dedupe_key("11111111-2222-4333-8444-555555555555", 1_700_000_000_000);
        assert_eq!(
            parse_duplicate_dedupe_key(&key),
            Some((
                "11111111-2222-4333-8444-555555555555".to_owned(),
                1_700_000_000_000
            ))
        );
        assert_eq!(parse_duplicate_dedupe_key("aci-a:42"), None);
        assert_eq!(parse_duplicate_dedupe_key("dup::42"), None);
        assert_eq!(parse_duplicate_dedupe_key("dup:aci-a:not-a-ts"), None);
        assert_eq!(parse_duplicate_dedupe_key("dup:aci-a"), None);
    }
}
