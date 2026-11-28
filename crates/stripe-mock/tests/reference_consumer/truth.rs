//! The truth a consumer is judged against, and how its synced copy diverges.

use std::collections::{BTreeMap, BTreeSet};

use ductkit_stripe_mock::{ChangeKind, ObjectType, StateUpdate};
use serde_json::Value;

/// `(scope, type, id)`; scope `None` is the platform.
pub type Key = (Option<String>, ObjectType, String);
/// What a consumer holds.
pub type Synced = BTreeMap<Key, Value>;

/// The types the harness compares; every other type is left out on both sides.
pub const COMPARED: [ObjectType; 5] = [
    ObjectType::Customer,
    ObjectType::Product,
    ObjectType::Price,
    ObjectType::Subscription,
    ObjectType::Invoice,
];

pub struct Truth {
    /// The latest state of every live compared object.
    pub latest: Synced,
    /// Every non-tombstone state of each object, in truth-log order.
    history: BTreeMap<Key, Vec<Value>>,
    /// Keys whose last entry is a delete.
    deleted: BTreeSet<Key>,
    /// Per object, the index in `history` of its last change that announced a thin
    /// event. Later changes were silent (no event), so no event hints at them.
    last_announced: BTreeMap<Key, usize>,
}

/// How a consumer's copy compares with the truth, key by key.
#[derive(Debug, Default, PartialEq)]
pub struct Divergence {
    pub equal: usize,
    /// Present, but an earlier state of a live object.
    pub stale: usize,
    /// Live in truth, absent from the copy.
    pub missing: usize,
    /// Deleted in truth, present in the copy as one of its earlier states.
    pub resurrected: usize,
    /// Present but matching no state the object ever had.
    pub other: Vec<Key>,
}

impl Divergence {
    pub fn converged(&self) -> bool {
        self.stale == 0 && self.missing == 0 && self.resurrected == 0 && self.other.is_empty()
    }
}

impl Truth {
    pub fn from_log(log: &[StateUpdate]) -> Truth {
        let mut t = Truth {
            latest: Synced::new(),
            history: BTreeMap::new(),
            deleted: BTreeSet::new(),
            last_announced: BTreeMap::new(),
        };
        for u in log.iter().filter(|u| COMPARED.contains(&u.object_type)) {
            let k: Key = (u.account.clone(), u.object_type, u.object_id.clone());
            if u.kind == ChangeKind::Deleted {
                t.latest.remove(&k);
                t.deleted.insert(k);
            } else {
                t.deleted.remove(&k);
                t.latest.insert(k.clone(), u.state.clone());
                let h = t.history.entry(k.clone()).or_default();
                h.push(u.state.clone());
                if !u.events.thin.is_empty() {
                    t.last_announced.insert(k, h.len() - 1);
                }
            }
        }
        t
    }

    pub fn classify(&self, synced: &Synced) -> Divergence {
        let mut d = Divergence::default();
        let seen = |k: &Key, v: &Value| self.history.get(k).is_some_and(|h| h.contains(v));
        for (k, v) in synced {
            match self.latest.get(k) {
                Some(l) if l == v => d.equal += 1,
                Some(_) if seen(k, v) => d.stale += 1,
                None if self.deleted.contains(k) && seen(k, v) => d.resurrected += 1,
                _ => d.other.push(k.clone()),
            }
        }
        d.missing = self
            .latest
            .keys()
            .filter(|k| !synced.contains_key(*k))
            .count();
        d
    }

    /// Keys of live objects the copy holds at a state older than their last announced
    /// change. A consumer that refetches on events can only lag behind silent changes;
    /// anything older means it missed or mishandled an event.
    pub fn unexplained_staleness(&self, synced: &Synced) -> Vec<Key> {
        synced
            .iter()
            .filter(|(k, v)| {
                self.latest.get(*k).is_some_and(|l| l != *v) && {
                    let h = &self.history[*k];
                    let from = self.last_announced.get(*k).copied().unwrap_or(0);
                    !h[from..].contains(v)
                }
            })
            .map(|(k, _)| k.clone())
            .collect()
    }
}
