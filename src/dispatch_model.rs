// SPDX-License-Identifier: AGPL-3.0-only

//! E: send dispatch reconciliation state.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DispatchState {
    Prepared,
    Sent,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchEntry {
    pub id: String,
    pub state: DispatchState,
    pub attempts: u32,
}

#[derive(Default)]
pub(crate) struct DispatchModel {
    entries: BTreeMap<String, DispatchEntry>,
}

impl DispatchModel {
    pub(crate) fn prepare(&mut self, id: &str) -> &DispatchEntry {
        self.entries
            .entry(id.to_string())
            .or_insert_with(|| DispatchEntry {
                id: id.to_string(),
                state: DispatchState::Prepared,
                attempts: 0,
            })
    }

    pub(crate) fn mark_sent(&mut self, id: &str) -> bool {
        match self.entries.get_mut(id) {
            Some(entry) if entry.state == DispatchState::Prepared => {
                entry.state = DispatchState::Sent;
                entry.attempts += 1;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn mark_failed(&mut self, id: &str) -> bool {
        match self.entries.get_mut(id) {
            Some(entry) if entry.state == DispatchState::Prepared => {
                entry.state = DispatchState::Failed;
                entry.attempts += 1;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn entry(&self, id: &str) -> Option<&DispatchEntry> {
        self.entries.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_is_idempotent_and_terminal() {
        let mut model = DispatchModel::default();
        assert_eq!(model.prepare("m1").state, DispatchState::Prepared);
        assert!(model.mark_sent("m1"));
        assert!(!model.mark_sent("m1"));
        assert!(!model.mark_failed("m1"));
        assert_eq!(model.entry("m1").unwrap().attempts, 1);

        model.prepare("m2");
        assert!(model.mark_failed("m2"));
        assert!(!model.mark_failed("m2"));
        assert!(!model.mark_sent("m2"));
    }
}
