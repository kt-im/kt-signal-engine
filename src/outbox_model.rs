//! E: protocol outbox semantics before persistence.
//! seq is assigned in-order; dedupe_key is unique; confirm only moves forward.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutboxRow {
    pub seq: u64,
    pub epoch: u64,
    pub dedupe_key: String,
    pub payload: String,
    pub confirmed: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct OutboxModel {
    next_seq: u64,
    rows: BTreeMap<u64, OutboxRow>,
}

impl OutboxModel {
    pub(crate) fn enqueue(&mut self, epoch: u64, dedupe_key: &str, payload: &str) -> u64 {
        if let Some(existing) = self.rows.values().find(|row| row.dedupe_key == dedupe_key) {
            return existing.seq;
        }
        self.next_seq += 1;
        let seq = self.next_seq;
        self.rows.insert(
            seq,
            OutboxRow {
                seq,
                epoch,
                dedupe_key: dedupe_key.to_string(),
                payload: payload.to_string(),
                confirmed: false,
            },
        );
        seq
    }

    pub(crate) fn confirm(&mut self, seq: u64) -> bool {
        match self.rows.get_mut(&seq) {
            Some(row) if !row.confirmed => {
                row.confirmed = true;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn pull_unconfirmed(&self, after: u64, limit: usize) -> Vec<OutboxRow> {
        self.rows
            .range(after.saturating_add(1)..)
            .filter(|(_, row)| !row.confirmed)
            .take(limit)
            .map(|(_, row)| row.clone())
            .collect()
    }

    pub(crate) fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        fs::write(&tmp, bytes)?;
        fs::rename(tmp, path)
    }

    pub(crate) fn load(path: &Path) -> std::io::Result<Self> {
        let bytes = fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }

    pub(crate) fn snapshot(&self) -> Vec<OutboxRow> {
        self.rows.values().cloned().collect()
    }

    pub(crate) fn restore(rows: Vec<OutboxRow>) -> Self {
        let next_seq = rows.iter().map(|row| row.seq).max().unwrap_or(0);
        Self {
            next_seq,
            rows: rows.into_iter().map(|row| (row.seq, row)).collect(),
        }
    }

    pub(crate) fn pull_unconfirmed_for_epoch(
        &self,
        epoch: u64,
        after: u64,
        limit: usize,
    ) -> Vec<OutboxRow> {
        self.pull_unconfirmed(after, usize::MAX)
            .into_iter()
            .filter(|row| row.epoch == epoch)
            .take(limit)
            .collect()
    }
}

pub(crate) fn duplicate_dedupe_key(sender: &str, timestamp: u64) -> String {
    format!("dup:{sender}:{timestamp}")
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
    fn outbox_sequences_dedupes_confirms_and_pulls() {
        let mut outbox = OutboxModel::default();
        assert_eq!(outbox.enqueue(1, "a", "{}"), 1);
        assert_eq!(outbox.enqueue(1, "a", "{}"), 1);
        assert_eq!(outbox.enqueue(1, "b", "{}"), 2);
        assert_eq!(outbox.pull_unconfirmed(0, 10).len(), 2);
        assert!(outbox.confirm(1));
        assert!(!outbox.confirm(1));
        let rest = outbox.pull_unconfirmed(0, 10);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].seq, 2);

        // Duplicate after confirm: same seq, still confirmed, no new row.
        assert_eq!(outbox.enqueue(1, "a", "{}"), 1);
        assert!(outbox
            .pull_unconfirmed(0, 10)
            .iter()
            .all(|row| row.seq != 1));
        assert_eq!(outbox.pull_unconfirmed_for_epoch(1, 0, 10).len(), 1);
        assert!(outbox.pull_unconfirmed_for_epoch(2, 0, 10).is_empty());

        let restored = OutboxModel::restore(outbox.snapshot());
        assert_eq!(restored.pull_unconfirmed(0, 10).len(), 1);
        assert_eq!(restored.next_seq, 2);

        let path = std::env::temp_dir().join(format!("kt-outbox-{}.json", std::process::id()));
        outbox.save(&path).unwrap();
        let loaded = OutboxModel::load(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(loaded.pull_unconfirmed(0, 10).len(), 1);
        assert_eq!(loaded.next_seq, 2);
    }
}
