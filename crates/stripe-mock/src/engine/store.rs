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
    /// Position in creation order within this type.
    pub creation_index: usize,
    pub deleted: bool,
}

#[derive(Debug, Default)]
struct TypeStore {
    order: Vec<String>,
    objects: HashMap<String, StoredObject>,
}

/// All objects, by type. Deleted objects stay, marked `deleted`.
#[derive(Debug, Default)]
pub(crate) struct ObjectStore {
    types: BTreeMap<ObjectType, TypeStore>,
}

impl ObjectStore {
    pub(crate) fn get(&self, t: ObjectType, id: &str) -> Option<&StoredObject> {
        self.types.get(&t)?.objects.get(id)
    }

    /// Every object of `t` (deleted included), newest `created` first,
    /// ties broken by later creation first.
    pub(crate) fn list_order(&self, t: ObjectType) -> Vec<&StoredObject> {
        let mut all = self.in_creation_order(t);
        all.sort_by(|a, b| {
            b.created
                .cmp(&a.created)
                .then(b.creation_index.cmp(&a.creation_index))
        });
        all
    }

    /// IDs of objects of `t` that are not deleted, in creation order.
    pub(crate) fn live_ids(&self, t: ObjectType) -> Vec<&str> {
        self.in_creation_order(t)
            .into_iter()
            .filter(|o| !o.deleted)
            .map(|o| o.latest.object_id.as_str())
            .collect()
    }

    /// Every object of `t` (deleted included), in creation order.
    pub(crate) fn in_creation_order(&self, t: ObjectType) -> Vec<&StoredObject> {
        match self.types.get(&t) {
            Some(ts) => ts.order.iter().map(|id| &ts.objects[id]).collect(),
            None => Vec::new(),
        }
    }

    /// Objects of `t` (deleted included) whose `parent_field` is `parent_id`,
    /// in creation order.
    pub(crate) fn children(
        &self,
        t: ObjectType,
        parent_field: &str,
        parent_id: &str,
    ) -> Vec<&StoredObject> {
        self.in_creation_order(t)
            .into_iter()
            .filter(|o| o.latest.state[parent_field].as_str() == Some(parent_id))
            .collect()
    }

    /// Types that have at least one object.
    pub(crate) fn all_types(&self) -> Vec<ObjectType> {
        self.types.keys().copied().collect()
    }

    pub(crate) fn insert(&mut self, update: StateUpdate, created: i64) {
        let ts = self.types.entry(update.object_type).or_default();
        let id = update.object_id.clone();
        debug_assert!(!ts.objects.contains_key(&id), "duplicate id {id}");
        let creation_index = ts.order.len();
        ts.order.push(id.clone());
        ts.objects.insert(
            id,
            StoredObject {
                latest: update,
                created,
                creation_index,
                deleted: false,
            },
        );
    }

    pub(crate) fn replace(&mut self, update: StateUpdate) {
        let obj = self
            .types
            .get_mut(&update.object_type)
            .and_then(|ts| ts.objects.get_mut(&update.object_id))
            .expect("replace of unknown object");
        obj.deleted = update.kind == ChangeKind::Deleted;
        obj.latest = update;
    }
}
