//! Event projection (spec section 8): each truth-log entry becomes one snapshot
//! event and one thin event, with a hidden `visible_at`.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::engine::{ChangeKind, Scope, StateUpdate};
use crate::rng::{RngStreams, SimRng};
use crate::{Config, EventsConfig, ObjectType, PINNED_API_VERSION, SimTime};

/// Snapshot (v1) or thin (v2) event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventFormat {
    Snapshot,
    Thin,
}

/// One stored event.
#[derive(Debug, Clone)]
pub(crate) struct EventRecord {
    pub id: String,
    pub format: EventFormat,
    pub event_type: String,
    pub created: SimTime,
    pub visible_at: SimTime,
    pub tiebreak: u64,
    /// Where the event's object lives (`None` is the platform).
    pub scope: Scope,
    /// The Connect label the event carries (`account` / `context`); `None` for the platform.
    #[allow(dead_code)] // read by the Connect tasks
    pub account: Option<String>,
    pub object_type: ObjectType,
    pub object_id: String,
    pub object_version: u64,
    pub truth_seq: u64,
    /// The full event as served by the API.
    pub payload: Value,
    /// The other format's event ID.
    pub pair_id: String,
}

/// Public view of an event for the visibility timeline.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventInfo {
    pub id: String,
    pub format: EventFormat,
    pub event_type: String,
    pub created: SimTime,
    pub visible_at: SimTime,
    pub object_id: String,
    pub object_version: u64,
    /// The truth-log entry this event was projected from.
    pub truth_seq: u64,
    /// The other format's event ID.
    pub pair_id: String,
}

impl From<&EventRecord> for EventInfo {
    fn from(r: &EventRecord) -> Self {
        EventInfo {
            id: r.id.clone(),
            format: r.format,
            event_type: r.event_type.clone(),
            created: r.created,
            visible_at: r.visible_at,
            object_id: r.object_id.clone(),
            object_version: r.object_version,
            truth_seq: r.truth_seq,
            pair_id: r.pair_id.clone(),
        }
    }
}

/// All events, in creation order.
#[derive(Debug, Default)]
pub(crate) struct EventStore {
    records: Vec<EventRecord>,
    index: HashMap<String, usize>,
}

impl EventStore {
    pub(crate) fn push(&mut self, r: EventRecord) {
        self.index.insert(r.id.clone(), self.records.len());
        self.records.push(r);
    }

    pub(crate) fn get(&self, id: &str) -> Option<&EventRecord> {
        self.index.get(id).map(|i| &self.records[*i])
    }

    /// Events of `f`, newest `created` first, then tiebreak, then later insertion first.
    pub(crate) fn list_order(&self, f: EventFormat) -> Vec<&EventRecord> {
        let mut v: Vec<(usize, &EventRecord)> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.format == f)
            .collect();
        v.sort_by(|(ia, a), (ib, b)| {
            b.created
                .unix_seconds()
                .cmp(&a.created.unix_seconds())
                .then(b.tiebreak.cmp(&a.tiebreak))
                .then(ib.cmp(ia))
        });
        v.into_iter().map(|(_, r)| r).collect()
    }

    /// Whether `r` is visible and still retained at `now`.
    pub(crate) fn is_listed(&self, r: &EventRecord, now: SimTime, retention_ms: i64) -> bool {
        r.visible_at <= now && r.created.0 > now.0 - retention_ms
    }

    /// Makes every event visible by `now` at the latest.
    pub(crate) fn flush_visibility(&mut self, now: SimTime) {
        for r in &mut self.records {
            r.visible_at = r.visible_at.min(now);
        }
    }

    pub(crate) fn all(&self) -> &[EventRecord] {
        &self.records
    }
}

/// Turns truth-log entries into events.
#[derive(Debug)]
pub(crate) struct Projector {
    pub config: EventsConfig,
    pub retention_ms: i64,
    ids: SimRng,
    visibility: SimRng,
    shuffle: SimRng,
}

impl Projector {
    pub(crate) fn new(config: &Config, streams: &RngStreams) -> Self {
        Projector {
            config: config.events.clone(),
            retention_ms: config.event_retention.window.ms(),
            ids: streams.stream("events.ids"),
            visibility: streams.stream("events.visibility"),
            shuffle: streams.stream("events.shuffle"),
        }
    }

    /// One snapshot and one thin event per snapshot type in `types`, in order.
    /// `previous` is the state before the mutation. Draws nothing for an empty `types`.
    pub(crate) fn project(
        &mut self,
        u: &StateUpdate,
        previous: Option<&Value>,
        types: &[&str],
    ) -> Vec<(EventRecord, EventRecord)> {
        types
            .iter()
            .map(|t| self.project_one(u, previous, t))
            .collect()
    }

    fn project_one(
        &mut self,
        u: &StateUpdate,
        previous: Option<&Value>,
        snapshot_type: &str,
    ) -> (EventRecord, EventRecord) {
        let snapshot_id = self.ids.id_len("evt", 24);
        let thin_id = self.ids.id_len("evt_test", 46);
        let mut lag = self.visibility.sample_ms(&self.config.visibility_lag);
        let tail = &self.config.late_tail;
        if self.visibility.chance(tail.probability) {
            lag += self.visibility.uniform_ms(tail.min.ms(), tail.max.ms());
        }
        let visible_at = u.at.plus_ms(lag);
        let tiebreak = if self.config.same_second_shuffle {
            self.shuffle.next_u64()
        } else {
            0
        };
        let name = u.object_type.as_str();
        let snapshot_type = snapshot_type.to_string();
        let thin_type = format!("v1.{snapshot_type}");

        let object = match (u.kind, previous) {
            (ChangeKind::Deleted, Some(prev)) => {
                let mut o = prev.clone();
                if let Some(m) = o.as_object_mut() {
                    m.insert("deleted".into(), json!(true));
                }
                o
            }
            _ => u.state.clone(),
        };
        let mut data = Map::new();
        data.insert("object".into(), object);
        let mut changes = Value::Null;
        if !matches!(u.kind, ChangeKind::Created | ChangeKind::Deleted)
            && let Some(prev) = previous
        {
            data.insert(
                "previous_attributes".into(),
                previous_attributes(prev, &u.state),
            );
            changes = before_after(prev, &u.state);
        }
        // The one place an event's Connect label is assigned: it defaults to the
        // scope its object lives in.
        let account: Option<String> = u.account.clone();
        let mut snapshot_payload = json!({
            "id": snapshot_id,
            "object": "event",
            "api_version": PINNED_API_VERSION,
            "created": u.at.unix_seconds(),
            "data": Value::Object(data),
            "livemode": false,
            "pending_webhooks": 0,
            "request": {"id": null, "idempotency_key": null},
            "type": snapshot_type,
        });
        if let Some(acct) = &account {
            snapshot_payload["account"] = json!(acct);
        }
        let thin_payload = json!({
            "id": thin_id,
            "object": "v2.core.event",
            "type": thin_type,
            "created": u.at.to_string(),
            "livemode": false,
            "context": account,
            "reason": null,
            "related_object": {
                "id": u.object_id,
                "type": name,
                "url": u.object_type.api_path(&u.object_id),
            },
            "snapshot_event": snapshot_id,
            "changes": changes,
            "data": null,
        });
        let record = |id: &str, pair: &str, format, event_type: String, payload| EventRecord {
            id: id.to_string(),
            format,
            event_type,
            created: u.at,
            visible_at,
            tiebreak,
            scope: u.account.clone(),
            account: account.clone(),
            object_type: u.object_type,
            object_id: u.object_id.clone(),
            object_version: u.version,
            truth_seq: u.seq,
            payload,
            pair_id: pair.to_string(),
        };
        (
            record(
                &snapshot_id,
                &thin_id,
                EventFormat::Snapshot,
                snapshot_type.clone(),
                snapshot_payload,
            ),
            record(
                &thin_id,
                &snapshot_id,
                EventFormat::Thin,
                thin_type,
                thin_payload,
            ),
        )
    }
}

/// Old values of changed top-level keys; object values are diffed one level deep,
/// with `null` for keys that did not exist before.
fn previous_attributes(old: &Value, new: &Value) -> Value {
    let (Some(o), Some(n)) = (old.as_object(), new.as_object()) else {
        return json!({});
    };
    let mut out = Map::new();
    for (k, nv) in n {
        let ov = o.get(k).unwrap_or(&Value::Null);
        if ov == nv {
            continue;
        }
        match (ov.as_object(), nv.as_object()) {
            (Some(oi), Some(ni)) => {
                let mut inner = Map::new();
                for key in ni.keys().chain(oi.keys()) {
                    if oi.get(key) != ni.get(key) && !inner.contains_key(key) {
                        inner.insert(key.clone(), oi.get(key).cloned().unwrap_or(Value::Null));
                    }
                }
                out.insert(k.clone(), Value::Object(inner));
            }
            _ => {
                out.insert(k.clone(), ov.clone());
            }
        }
    }
    Value::Object(out)
}

/// `{"before": {...}, "after": {...}}` over changed top-level keys.
fn before_after(old: &Value, new: &Value) -> Value {
    let (Some(o), Some(n)) = (old.as_object(), new.as_object()) else {
        return Value::Null;
    };
    let mut before = Map::new();
    let mut after = Map::new();
    for (k, nv) in n {
        let ov = o.get(k).unwrap_or(&Value::Null);
        if ov != nv {
            before.insert(k.clone(), ov.clone());
            after.insert(k.clone(), nv.clone());
        }
    }
    json!({"before": before, "after": after})
}

/// The thin event as delivered: the full event without `snapshot_event`, `changes` and `data`.
pub(crate) fn thin_notification(full: &Value) -> Value {
    let mut v = full.clone();
    if let Some(m) = v.as_object_mut() {
        m.remove("snapshot_event");
        m.remove("changes");
        m.remove("data");
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::rng::RngStreams;
    use crate::{Config, ObjectType, SimTime};
    use serde_json::json;

    fn engine_with(toml: &str) -> Engine {
        let c = Config::from_toml_str(toml).unwrap();
        Engine::new(SimTime(1_767_225_600_000), &c, &RngStreams::new(c.seed))
    }

    fn cust(id: &str) -> Value {
        json!({"id": id, "object": "customer", "created": 1767225600, "email": "a@example.com", "metadata": {"tier": "gold"}})
    }

    #[test]
    fn every_mutation_yields_a_pair() {
        let mut e = engine_with("");
        let c = e.begin_action("t");
        let u = e.create(
            ObjectType::Customer,
            "cus_1".into(),
            cust("cus_1"),
            c,
            &["customer.created"],
        );
        assert_eq!((u.events.snapshot.len(), u.events.thin.len()), (1, 1));
        let s = e.events().get(&u.events.snapshot[0]).unwrap();
        let t = e.events().get(&u.events.thin[0]).unwrap();
        assert_eq!(
            (s.event_type.as_str(), t.event_type.as_str()),
            ("customer.created", "v1.customer.created")
        );
        assert!(s.id.starts_with("evt_") && s.id.len() == 28 && t.id.starts_with("evt_test_"));
        assert_eq!(s.payload["api_version"], "2026-11-18.endive");
        assert_eq!(s.payload["created"], 1767225600);
        assert_eq!(s.payload["data"]["object"]["id"], "cus_1");
        assert!(s.payload["data"].get("previous_attributes").is_none());
        assert_eq!(t.payload["created"], "2026-01-01T00:00:00.000Z");
        assert_eq!(t.payload["snapshot_event"], s.id.as_str());
        assert_eq!(
            t.payload["related_object"],
            json!({"id": "cus_1", "type": "customer", "url": "/v1/customers/cus_1"})
        );
        assert_eq!((s.visible_at, s.tiebreak), (t.visible_at, t.tiebreak));
    }

    #[test]
    fn update_has_previous_attributes_and_changes() {
        let mut e = engine_with("");
        let c = e.begin_action("t");
        e.create(
            ObjectType::Customer,
            "cus_1".into(),
            cust("cus_1"),
            c.clone(),
            &["customer.created"],
        );
        let u = e
            .update(
                ObjectType::Customer,
                "cus_1",
                |v| {
                    v["email"] = json!("b@example.com");
                    v["metadata"]["plan"] = json!("pro");
                },
                c,
                ChangeKind::Updated,
                &["customer.updated"],
            )
            .unwrap()
            .unwrap();
        let s = e.events().get(&u.events.snapshot[0]).unwrap();
        assert_eq!(
            s.payload["data"]["previous_attributes"],
            json!({"email": "a@example.com", "metadata": {"plan": null}})
        );
        let t = e.events().get(&u.events.thin[0]).unwrap();
        assert_eq!(t.payload["changes"]["before"]["email"], "a@example.com");
        assert_eq!(t.payload["changes"]["after"]["email"], "b@example.com");
    }

    #[test]
    fn delete_event_carries_full_last_state() {
        let mut e = engine_with("");
        let c = e.begin_action("t");
        e.create(
            ObjectType::Customer,
            "cus_1".into(),
            cust("cus_1"),
            c.clone(),
            &["customer.created"],
        );
        let u = e
            .delete(
                ObjectType::Customer,
                "cus_1",
                json!({"id": "cus_1", "object": "customer", "deleted": true}),
                c,
                &["customer.deleted"],
            )
            .unwrap();
        let s = e.events().get(&u.events.snapshot[0]).unwrap();
        assert_eq!(s.event_type, "customer.deleted");
        assert_eq!(s.payload["data"]["object"]["email"], "a@example.com");
        assert_eq!(s.payload["data"]["object"]["deleted"], true);
    }

    #[test]
    fn visibility_lag_and_listing_window() {
        let mut e = engine_with(
            "[events]\nvisibility_lag = { distribution = \"fixed\", ms = 5000 }\n[event_retention]\nwindow = \"1h\"",
        );
        let c = e.begin_action("t");
        let u = e.create(
            ObjectType::Customer,
            "cus_1".into(),
            cust("cus_1"),
            c,
            &["customer.created"],
        );
        let r = e.events().get(&u.events.snapshot[0]).unwrap().clone();
        assert_eq!(r.visible_at, r.created.plus_ms(5000));
        let ret = e.retention_ms();
        assert!(!e.events().is_listed(&r, r.created.plus_ms(4999), ret));
        assert!(e.events().is_listed(&r, r.created.plus_ms(5000), ret));
        assert!(!e.events().is_listed(&r, r.created.plus_ms(3_600_000), ret));
    }

    #[test]
    fn thin_notification_strips_full_fields() {
        let n = thin_notification(
            &json!({"id": "evt_test_1", "snapshot_event": "evt_1", "changes": null, "data": null, "type": "v1.customer.created"}),
        );
        assert_eq!(
            n,
            json!({"id": "evt_test_1", "type": "v1.customer.created"})
        );
    }

    #[test]
    fn unannounced_mutation_has_no_events() {
        let mut e = engine_with("");
        let c = e.begin_action("t");
        let u = e.create(
            ObjectType::SubscriptionItem,
            "si_1".into(),
            json!({"id": "si_1", "object": "subscription_item", "created": 1767225600}),
            c,
            &[],
        );
        assert_eq!(u.events, crate::engine::EventRefs::default());
        assert!(e.events().all().is_empty());
    }

    #[test]
    fn two_types_two_pairs() {
        let mut e = engine_with("");
        let c = e.begin_action("t");
        e.create(
            ObjectType::Invoice,
            "in_x".into(),
            json!({"id": "in_x", "object": "invoice", "created": 1767225600, "status": "open"}),
            c.clone(),
            &["invoice.created"],
        );
        let u = e
            .update(
                ObjectType::Invoice,
                "in_x",
                |v| v["status"] = json!("paid"),
                c,
                ChangeKind::StateChanged,
                &["invoice.paid", "invoice.payment_succeeded"],
            )
            .unwrap()
            .unwrap();
        assert_eq!((u.events.snapshot.len(), u.events.thin.len()), (2, 2));
        assert_eq!(u.kind, ChangeKind::StateChanged);
        let types: Vec<String> = u
            .events
            .thin
            .iter()
            .map(|id| e.events().get(id).unwrap().event_type.clone())
            .collect();
        assert_eq!(types, ["v1.invoice.paid", "v1.invoice.payment_succeeded"]);
        for id in &u.events.snapshot {
            let s = e.events().get(id).unwrap();
            assert_eq!(s.payload["data"]["previous_attributes"]["status"], "open");
        }
        let t = e.events().get(&u.events.thin[0]).unwrap();
        assert_eq!(t.payload["related_object"]["url"], "/v1/invoices/in_x");
        assert_eq!(t.payload["related_object"]["type"], "invoice");
    }

    #[test]
    fn subscription_related_object() {
        let mut e = engine_with("");
        let c = e.begin_action("t");
        let u = e.create(
            ObjectType::Subscription,
            "sub_1".into(),
            json!({"id": "sub_1", "object": "subscription", "created": 1767225600}),
            c,
            &["customer.subscription.created"],
        );
        let t = e.events().get(&u.events.thin[0]).unwrap();
        assert_eq!(t.event_type, "v1.customer.subscription.created");
        assert_eq!(
            t.payload["related_object"]["url"],
            "/v1/subscriptions/sub_1"
        );
    }
}
