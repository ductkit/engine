//! Event projection (spec section 8): each truth-log entry becomes one snapshot
//! event and one thin event, with a hidden `visible_at`.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::engine::{ChangeKind, Scope, StateUpdate};
use crate::rng::{RngStreams, SimRng};
use crate::stripe;
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
    pub account: Option<String>,
    pub object_type: ObjectType,
    pub object_id: String,
    pub object_version: u64,
    pub truth_seq: u64,
    /// The full event as served by the API.
    pub payload: Value,
    /// The other format's event ID; empty for a thin-only (`v2.`) event.
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
    /// The other format's event ID; empty for a thin-only (`v2.`) event.
    pub pair_id: String,
    /// The Connect label: the account the event is about, `None` for the platform's own.
    /// An Account object's v1 events carry the account here even though its truth-log
    /// entries (`StateUpdate::account`) are platform scope (spec 10.3).
    pub account: Option<String>,
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
            account: r.account.clone(),
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

    /// Events of `f` that satisfy `keep`, newest `created` first, then tiebreak, then
    /// later insertion first. `keep` runs before the sort, so only the events a request
    /// can see are ordered.
    pub(crate) fn list_order(
        &self,
        f: EventFormat,
        keep: impl Fn(&EventRecord) -> bool,
    ) -> Vec<&EventRecord> {
        let mut v: Vec<(usize, &EventRecord)> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.format == f && keep(r))
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
        r.visible_at <= now && r.created.0 > now.0.saturating_sub(retention_ms)
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
    /// The platform's application, the `data.object` of `account.application.*` events.
    pub application: Option<stripe::Application>,
}

impl Projector {
    pub(crate) fn new(config: &Config, streams: &RngStreams) -> Self {
        Projector {
            config: config.events.clone(),
            retention_ms: config.event_retention.window.ms(),
            ids: streams.stream("events.ids"),
            visibility: streams.stream("events.visibility"),
            shuffle: streams.stream("events.shuffle"),
            application: None,
        }
    }

    /// One snapshot and one thin event per snapshot type in `types`, in order; a
    /// `v2.` type gets its thin event only. `previous` is the state before the
    /// mutation. Draws nothing for an empty `types`.
    pub(crate) fn project(
        &mut self,
        u: &StateUpdate,
        previous: Option<&Value>,
        types: &[&str],
    ) -> Vec<(Option<EventRecord>, EventRecord)> {
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
    ) -> (Option<EventRecord>, EventRecord) {
        let thin_only = snapshot_type.starts_with("v2.");
        let snapshot_id = if thin_only {
            String::new()
        } else {
            self.ids.id_len("evt", 24)
        };
        let thin_id = self.ids.id_len("evt_test", 46);
        let mut lag = self.visibility.sample_ms(&self.config.visibility_lag);
        let tail = &self.config.late_tail;
        if self.visibility.chance(tail.probability) {
            lag = lag.saturating_add(self.visibility.uniform_ms(tail.min.ms(), tail.max.ms()));
        }
        let visible_at = u.at.plus_ms(lag);
        let tiebreak = if self.config.same_second_shuffle {
            self.shuffle.next_u64()
        } else {
            0
        };
        let name = u.object_type.as_str();
        let snapshot_type = snapshot_type.to_string();
        let thin_type = if thin_only {
            snapshot_type.clone()
        } else {
            format!("v1.{snapshot_type}")
        };

        // `data.object` and `previous_attributes` are free-form in the schema, so they stay
        // the stored JSON; the envelope around them is typed.
        let object = match (u.kind, previous) {
            _ if snapshot_type.starts_with("account.application.") => {
                let app = self
                    .application
                    .as_ref()
                    .expect("account events need connect, which creates the application");
                to_map(to_json(app))
            }
            // Ruling R2: the last live object, unchanged (no live schema has `deleted`).
            (ChangeKind::Deleted, Some(prev)) => to_map(prev.clone()),
            _ => to_map(u.state.clone()),
        };
        let (previous_attributes, changes) = match previous {
            Some(prev) if !matches!(u.kind, ChangeKind::Created | ChangeKind::Deleted) => (
                Some(to_map(previous_attributes(prev, &u.state))),
                Some(before_after(prev, &u.state)),
            ),
            _ => (None, None),
        };
        // The one place an event's Connect label is assigned. An account's own events
        // are labelled with the account, except thin-only `v2.` events, which are
        // platform events. Every other event carries the scope it was recorded in.
        let account: Option<String> = if u.object_type == ObjectType::Account {
            (!thin_only).then(|| u.object_id.clone())
        } else {
            u.account.clone()
        };
        // Presence (ruling R1): `account` and `context` carry the Connect label and are
        // absent for the platform's own events (neither is nullable). The mock's API is
        // read-only, so no request causes an event: `request` has null `id` and
        // `idempotency_key`.
        let snapshot_payload = to_json(&stripe::Event {
            account: account.clone(),
            api_version: Some(Some(PINNED_API_VERSION.to_string())),
            context: account.clone(),
            created: u.at.unix_seconds(),
            data: stripe::NotificationEventData {
                object,
                previous_attributes,
            },
            id: snapshot_id.clone(),
            livemode: false,
            object: stripe::EventObject::Event,
            pending_webhooks: 0,
            request: Some(Some(stripe::NotificationEventRequest {
                id: Some(None),
                idempotency_key: Some(None),
            })),
            type_: snapshot_type.clone(),
        });
        // Thin-only `v2.core.account.*` events point at the v2 account resource, which
        // still serves the account after it closes; the v1 URL would 404.
        let related = if thin_only && u.object_type == ObjectType::Account {
            stripe::V2CoreEventRelatedObject {
                id: Some(u.object_id.clone()),
                type_: "v2.core.account".to_string(),
                url: format!("/v2/core/accounts/{}", u.object_id),
            }
        } else {
            stripe::V2CoreEventRelatedObject {
                id: Some(u.object_id.clone()),
                type_: name.to_string(),
                // A child of an account lives in that account's scope, so its URL nests
                // under the scope's account.
                url: u
                    .object_type
                    .api_path(&u.object_id)
                    .or_else(|| {
                        u.account
                            .as_deref()
                            .and_then(|a| u.object_type.sub_path(a, &u.object_id))
                    })
                    .unwrap_or_else(|| {
                        panic!("{} has no API path, so it cannot announce events", name)
                    }),
            }
        };
        // Presence: none of `context`, `reason`, `snapshot_event`, `changes` and `data` is
        // nullable, so each is absent when the mock has no value: `context` for the
        // platform, `reason` always (no request causes an event), `snapshot_event` for a
        // thin-only event, `changes` for a create or delete, and `data` always (v1 thin
        // events carry no extra data).
        let thin_payload = to_json(&stripe::V2CoreEvent {
            changes,
            context: account.clone(),
            created: u.at.to_string(),
            data: None,
            id: thin_id.clone(),
            livemode: false,
            object: stripe::V2CoreEventObject::V2CoreEvent,
            reason: None,
            related_object: Some(related),
            snapshot_event: (!thin_only).then(|| snapshot_id.clone()),
            type_: thin_type.clone(),
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
            (!thin_only).then(|| {
                record(
                    &snapshot_id,
                    &thin_id,
                    EventFormat::Snapshot,
                    snapshot_type.clone(),
                    snapshot_payload,
                )
            }),
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

fn to_json(v: &impl Serialize) -> Value {
    serde_json::to_value(v).expect("an event serializes")
}

/// A JSON object as the map the generated free-form fields hold.
fn to_map(v: Value) -> BTreeMap<String, Value> {
    match v {
        Value::Object(m) => m.into_iter().collect(),
        other => panic!("an event's object is a JSON object, not {other}"),
    }
}

/// A stored event payload as its generated type; a payload that does not deserialize is a
/// bug in the mock.
fn typed<T: DeserializeOwned>(payload: &Value) -> T {
    serde_path_to_error::deserialize(payload).unwrap_or_else(|e| {
        panic!(
            "stored event is not a valid event at {}: {}",
            e.path(),
            e.inner()
        )
    })
}

/// A snapshot event as served or delivered: its stored payload with `pending_webhooks` now.
pub(crate) fn with_pending_webhooks(payload: &Value, pending: usize) -> Value {
    let mut e: stripe::Event = typed(payload);
    e.pending_webhooks = pending as i64;
    to_json(&e)
}

/// The thin event as delivered: the full event without `snapshot_event`, `changes` and `data`.
pub(crate) fn thin_notification(full: &Value) -> Value {
    let mut e: stripe::V2CoreEvent = typed(full);
    e.snapshot_event = None;
    e.changes = None;
    e.data = None;
    to_json(&e)
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
        let u = e.create_raw(
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
        e.create_raw(
            ObjectType::Customer,
            "cus_1".into(),
            cust("cus_1"),
            c.clone(),
            &["customer.created"],
        );
        let u = e
            .update_raw(
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
        e.create_raw(
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
                &json!({"id": "cus_1", "object": "customer", "deleted": true}),
                c,
                &["customer.deleted"],
            )
            .unwrap();
        let s = e.events().get(&u.events.snapshot[0]).unwrap();
        assert_eq!(s.event_type, "customer.deleted");
        assert_eq!(s.payload["data"]["object"]["email"], "a@example.com");
        assert!(s.payload["data"]["object"].get("deleted").is_none());
    }

    #[test]
    fn visibility_lag_and_listing_window() {
        let mut e = engine_with(
            "[events]\nvisibility_lag = { distribution = \"fixed\", ms = 5000 }\n[event_retention]\nwindow = \"1h\"",
        );
        let c = e.begin_action("t");
        let u = e.create_raw(
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
        let full = json!({
            "id": "evt_test_1", "object": "v2.core.event", "type": "v1.customer.updated",
            "created": "2026-01-01T00:00:00.000Z", "livemode": false,
            "related_object": {"id": "cus_1", "type": "customer", "url": "/v1/customers/cus_1"},
            "snapshot_event": "evt_1", "changes": {"before": {}, "after": {}}, "data": {},
        });
        let mut want = full.clone();
        for k in ["snapshot_event", "changes", "data"] {
            want.as_object_mut().unwrap().remove(k);
        }
        assert_eq!(thin_notification(&full), want);
    }

    #[test]
    fn platform_events_omit_what_they_lack() {
        let mut e = engine_with("");
        let c = e.begin_action("t");
        let u = e.create_raw(
            ObjectType::Customer,
            "cus_1".into(),
            cust("cus_1"),
            c,
            &["customer.created"],
        );
        let t = &e.events().get(&u.events.thin[0]).unwrap().payload;
        for absent in ["context", "reason", "changes", "data"] {
            assert!(t.get(absent).is_none(), "{absent}");
        }
        let s = &e.events().get(&u.events.snapshot[0]).unwrap().payload;
        for absent in ["account", "context"] {
            assert!(s.get(absent).is_none(), "{absent}");
        }
        assert_eq!(with_pending_webhooks(s, 3)["pending_webhooks"], 3);
    }

    #[test]
    fn unannounced_mutation_has_no_events() {
        let mut e = engine_with("");
        let c = e.begin_action("t");
        let u = e.create_raw(
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
        e.create_raw(
            ObjectType::Invoice,
            "in_x".into(),
            json!({"id": "in_x", "object": "invoice", "created": 1767225600, "status": "open"}),
            c.clone(),
            &["invoice.created"],
        );
        let u = e
            .update_raw(
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
        let u = e.create_raw(
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
