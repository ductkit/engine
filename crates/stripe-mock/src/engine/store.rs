//! Latest state of every object, plus creation order for lists.

use std::collections::{BTreeMap, HashMap};

use crate::ObjectType;
use crate::engine::{ChangeKind, StateUpdate};

/// One object's latest state and list position.
#[derive(Debug, Clone)]
pub(crate) struct StoredObject {
    pub latest: StateUpdate,
    /// `created` in Unix seconds, as shown in the API.
    pub created: i64,
    pub deleted: bool,
    /// Truth-log positions of every version, oldest first.
    pub versions: Vec<usize>,
}

#[derive(Debug, Default)]
struct TypeStore {
    /// Every object, in creation order.
    objects: Vec<StoredObject>,
    /// Position in `objects` by ID. Lookups only.
    index: HashMap<String, usize>,
    /// IDs not deleted, in creation order.
    live: Vec<String>,
    /// Positions of children by `(parent field, parent ID)`, in creation order.
    /// Lookups only.
    parents: HashMap<(&'static str, String), Vec<usize>>,
}

/// The parent fields indexed for each type. Parent fields never change.
fn parent_fields(t: ObjectType) -> &'static [&'static str] {
    match t {
        ObjectType::SubscriptionItem => &["subscription"],
        ObjectType::InvoiceLineItem => &["invoice"],
        ObjectType::Subscription | ObjectType::Invoice => &["customer"],
        ObjectType::Price => &["product"],
        ObjectType::Customer | ObjectType::Product => &[],
    }
}

/// All objects, by type. Deleted objects stay, marked `deleted`.
#[derive(Debug, Default)]
pub(crate) struct ObjectStore {
    types: BTreeMap<ObjectType, TypeStore>,
}

impl ObjectStore {
    pub(crate) fn get(&self, t: ObjectType, id: &str) -> Option<&StoredObject> {
        let ts = self.types.get(&t)?;
        ts.index.get(id).map(|&i| &ts.objects[i])
    }

    /// Every object of `t` (deleted included), newest `created` first,
    /// ties broken by later creation first. `created` never decreases in
    /// creation order (checked on insert), so this is reverse creation order.
    pub(crate) fn list_order(&self, t: ObjectType) -> Vec<&StoredObject> {
        self.types
            .get(&t)
            .map_or_else(Vec::new, |ts| ts.objects.iter().rev().collect())
    }

    /// IDs of objects of `t` that are not deleted, in creation order.
    pub(crate) fn live_ids(&self, t: ObjectType) -> &[String] {
        self.types.get(&t).map_or(&[], |ts| ts.live.as_slice())
    }

    /// Every object of `t` (deleted included), in creation order.
    pub(crate) fn in_creation_order(&self, t: ObjectType) -> Vec<&StoredObject> {
        self.types
            .get(&t)
            .map_or_else(Vec::new, |ts| ts.objects.iter().collect())
    }

    /// Objects of `t` (deleted included) whose `parent_field` was `parent_id`
    /// at creation, in creation order.
    pub(crate) fn children(
        &self,
        t: ObjectType,
        parent_field: &str,
        parent_id: &str,
    ) -> Vec<&StoredObject> {
        let Some(field) = parent_fields(t).iter().find(|f| **f == parent_field) else {
            debug_assert!(false, "{t:?}.{parent_field} is not indexed");
            return self
                .in_creation_order(t)
                .into_iter()
                .filter(|o| o.latest.state[parent_field].as_str() == Some(parent_id))
                .collect();
        };
        let Some(ts) = self.types.get(&t) else {
            return Vec::new();
        };
        ts.parents
            .get(&(*field, parent_id.to_string()))
            .map_or_else(Vec::new, |pos| {
                pos.iter().map(|&i| &ts.objects[i]).collect()
            })
    }

    /// Types that have at least one object.
    pub(crate) fn all_types(&self) -> Vec<ObjectType> {
        self.types.keys().copied().collect()
    }

    /// Adds a new object; `log_pos` is its truth-log position.
    pub(crate) fn insert(&mut self, update: StateUpdate, created: i64, log_pos: usize) {
        let ts = self.types.entry(update.object_type).or_default();
        let id = update.object_id.clone();
        debug_assert!(!ts.index.contains_key(&id), "duplicate id {id}");
        debug_assert!(
            ts.objects.last().is_none_or(|last| last.created <= created),
            "created must not decrease in creation order"
        );
        let pos = ts.objects.len();
        ts.index.insert(id.clone(), pos);
        for field in parent_fields(update.object_type) {
            if let Some(parent) = update.state[*field].as_str() {
                ts.parents
                    .entry((field, parent.to_string()))
                    .or_default()
                    .push(pos);
            }
        }
        ts.live.push(id);
        ts.objects.push(StoredObject {
            latest: update,
            created,
            deleted: false,
            versions: vec![log_pos],
        });
    }

    /// Records a new version; `log_pos` is its truth-log position.
    pub(crate) fn replace(&mut self, update: StateUpdate, log_pos: usize) {
        let ts = self
            .types
            .get_mut(&update.object_type)
            .expect("replace of unknown object");
        let pos = *ts
            .index
            .get(&update.object_id)
            .expect("replace of unknown object");
        let obj = &mut ts.objects[pos];
        let deleted = update.kind == ChangeKind::Deleted;
        if deleted
            && !obj.deleted
            && let Some(i) = ts.live.iter().position(|id| *id == update.object_id)
        {
            ts.live.remove(i);
        }
        obj.deleted = deleted;
        obj.versions.push(log_pos);
        obj.latest = update;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::SimTime;
    use crate::engine::EventRefs;

    fn update(
        t: ObjectType,
        id: &str,
        version: u64,
        kind: ChangeKind,
        state: Value,
    ) -> StateUpdate {
        StateUpdate {
            seq: 0,
            at: SimTime(0),
            account: None,
            object_type: t,
            object_id: id.to_string(),
            version,
            kind,
            state,
            events: EventRefs::default(),
            cause: None,
        }
    }

    fn add(s: &mut ObjectStore, t: ObjectType, id: &str, created: i64, state: Value) {
        s.insert(update(t, id, 1, ChangeKind::Created, state), created, 0);
    }

    fn del(s: &mut ObjectStore, t: ObjectType, id: &str) {
        s.replace(
            update(
                t,
                id,
                2,
                ChangeKind::Deleted,
                json!({"id": id, "deleted": true}),
            ),
            0,
        );
    }

    fn ids<'a>(objs: impl IntoIterator<Item = &'a StoredObject>) -> Vec<String> {
        objs.into_iter()
            .map(|o| o.latest.object_id.clone())
            .collect()
    }

    /// Run with `cargo test -p ductkit-stripe-mock --release --lib -- --ignored list_order_is_cheap`.
    /// A backfill page lists the full order; 1 000 pages over 200 000 objects
    /// must not cost seconds of lock time.
    #[test]
    #[ignore]
    fn list_order_is_cheap() {
        let mut s = ObjectStore::default();
        for i in 0..200_000 {
            add(
                &mut s,
                ObjectType::Customer,
                &format!("cus_{i:014}"),
                i,
                json!({}),
            );
        }
        let started = std::time::Instant::now();
        let mut n = 0;
        for _ in 0..1_000 {
            n += std::hint::black_box(s.list_order(ObjectType::Customer)).len();
        }
        let took = started.elapsed();
        println!("1000 list_order calls over 200k objects: {took:?}");
        assert_eq!(n, 200_000_000);
        assert!(took < std::time::Duration::from_secs(1), "{took:?}");
    }

    #[test]
    fn indexes_match_scans() {
        let mut s = ObjectStore::default();
        for i in 0..50 {
            add(
                &mut s,
                ObjectType::Customer,
                &format!("cus_{i}"),
                i / 3,
                json!({}),
            );
        }
        for i in 0..10 {
            let c = format!("cus_{}", i % 50);
            add(
                &mut s,
                ObjectType::Subscription,
                &format!("sub_{i}"),
                i,
                json!({"customer": c}),
            );
        }
        for i in 0..30 {
            let sub = format!("sub_{}", i % 10);
            add(
                &mut s,
                ObjectType::SubscriptionItem,
                &format!("si_{i}"),
                100,
                json!({"subscription": sub}),
            );
        }
        for i in 0..40 {
            let c = format!("cus_{}", i % 7);
            add(
                &mut s,
                ObjectType::Invoice,
                &format!("in_{i}"),
                100 + i,
                json!({"customer": c}),
            );
        }
        for i in [1, 4, 9, 20, 33] {
            del(&mut s, ObjectType::Customer, &format!("cus_{i}"));
        }
        for i in [0, 3, 5, 11, 12, 28, 29] {
            del(&mut s, ObjectType::SubscriptionItem, &format!("si_{i}"));
        }
        for t in ObjectType::ALL {
            let scan: Vec<String> = s
                .in_creation_order(t)
                .into_iter()
                .filter(|o| !o.deleted)
                .map(|o| o.latest.object_id.clone())
                .collect();
            assert_eq!(s.live_ids(t), scan.as_slice(), "{t:?}");
            let mut sorted: Vec<(usize, &StoredObject)> =
                s.in_creation_order(t).into_iter().enumerate().collect();
            sorted.sort_by(|(ia, a), (ib, b)| b.created.cmp(&a.created).then(ib.cmp(ia)));
            assert_eq!(
                ids(s.list_order(t)),
                ids(sorted.into_iter().map(|(_, o)| o)),
                "{t:?}"
            );
        }
        // Tombstones drop the parent field, so the reference derives each
        // child's parent at creation from its number, as inserted above.
        let parent_at_creation = |id: &str| -> String {
            let (prefix, n) = id.split_once('_').unwrap();
            let n: usize = n.parse().unwrap();
            match prefix {
                "sub" => format!("cus_{}", n % 50),
                "si" => format!("sub_{}", n % 10),
                "in" => format!("cus_{}", n % 7),
                _ => unreachable!(),
            }
        };
        let check = |t: ObjectType, field: &str, parents: Vec<String>| {
            for p in parents {
                let scan = s
                    .in_creation_order(t)
                    .into_iter()
                    .filter(|o| parent_at_creation(&o.latest.object_id) == p);
                assert_eq!(ids(s.children(t, field, &p)), ids(scan), "{t:?} {p}");
            }
        };
        check(
            ObjectType::Subscription,
            "customer",
            (0..50).map(|i| format!("cus_{i}")).collect(),
        );
        check(
            ObjectType::Invoice,
            "customer",
            (0..50).map(|i| format!("cus_{i}")).collect(),
        );
        check(
            ObjectType::SubscriptionItem,
            "subscription",
            (0..12).map(|i| format!("sub_{i}")).collect(),
        );
    }
}
