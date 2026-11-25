//! The truth log: every mutation, in order (spec section 2).

pub(crate) mod events;
pub(crate) mod store;

use serde::Serialize;
use serde_json::Value;

use crate::delivery::{
    AbandonReason, AttemptOutcome, Deliveries, DeliveryRecord, Destinations, PreparedAttempt,
};
use crate::rng::RngStreams;
use crate::stream::{self, SubscribeFilter, SubscribeOptions, Subscriber, Subscription};
use crate::{Config, MockError, ObjectType, SimTime};

pub(crate) use events::{EventStore, Projector};

pub(crate) use store::ObjectStore;

/// What kind of change a [`StateUpdate`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Part of a subscription's initial snapshot (live object).
    Snapshot,
    Created,
    Updated,
    /// A product or price became `active: false`.
    Archived,
    /// A `status` change.
    StateChanged,
    Deleted,
}

/// The business action that caused a mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Cause {
    pub action: String,
    pub action_id: String,
}

/// Stripe events a mutation produced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EventRefs {
    pub snapshot: Vec<String>,
    pub thin: Vec<String>,
}

/// One truth-log entry: an object's full state after a mutation (spec section 10.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StateUpdate {
    /// Global, gap-free, starting at 1.
    pub seq: u64,
    pub at: SimTime,
    /// `None` for the platform account.
    pub account: Option<String>,
    pub object_type: ObjectType,
    pub object_id: String,
    /// Per object, starting at 1.
    pub version: u64,
    pub kind: ChangeKind,
    pub state: Value,
    pub events: EventRefs,
    pub cause: Option<Cause>,
}

/// Authoritative state: current time, truth log and object store.
#[derive(Debug)]
pub(crate) struct Engine {
    now: SimTime,
    next_seq: u64,
    next_action: u64,
    log: Vec<StateUpdate>,
    store: ObjectStore,
    subscribers: Vec<Subscriber>,
    projector: Projector,
    events: EventStore,
    destinations: Destinations,
    deliveries: Deliveries,
}

impl Engine {
    pub(crate) fn new(start: SimTime, config: &Config, streams: &RngStreams) -> Self {
        Engine {
            now: start,
            next_seq: 1,
            next_action: 1,
            log: Vec::new(),
            store: ObjectStore::default(),
            subscribers: Vec::new(),
            projector: Projector::new(config, streams),
            events: EventStore::default(),
            destinations: {
                let mut d = Destinations::new(streams);
                for c in &config.destinations {
                    d.add(c.clone(), start)
                        .expect("config destinations are validated");
                }
                d
            },
            deliveries: Deliveries::new(&config.webhooks, streams),
        }
    }

    pub(crate) fn destinations(&self) -> &Destinations {
        &self.destinations
    }

    pub(crate) fn destinations_mut(&mut self) -> &mut Destinations {
        &mut self.destinations
    }

    pub(crate) fn events(&self) -> &EventStore {
        &self.events
    }

    /// Non-duplicate deliveries of the event that have not succeeded.
    pub(crate) fn pending_webhooks(&self, event_id: &str) -> usize {
        self.deliveries.pending_for_event(event_id)
    }

    /// The earliest queued delivery attempt.
    pub(crate) fn next_delivery_due(&self) -> Option<SimTime> {
        self.deliveries.next_due()
    }

    /// Attempts due at the earliest due time `<= upto`, marked in flight.
    pub(crate) fn due_batch(&mut self, upto: SimTime) -> Option<(SimTime, Vec<PreparedAttempt>)> {
        self.deliveries
            .due_batch(upto, &self.events, &self.destinations)
    }

    /// Logs a finished attempt at `now`.
    pub(crate) fn record_outcome(
        &mut self,
        a: &PreparedAttempt,
        outcome: AttemptOutcome,
        elapsed_ms: u64,
        now: SimTime,
    ) {
        let latest_version = self
            .events
            .get(&a.event_id)
            .and_then(|ev| self.store.get(ev.object_type, &ev.object_id))
            .map_or(a.object_version, |o| o.latest.version);
        self.deliveries.record_outcome(
            a,
            outcome,
            elapsed_ms,
            now,
            latest_version,
            &mut self.destinations,
        );
    }

    /// Enables or disables a destination; disabling abandons its queued deliveries.
    pub(crate) fn set_destination_enabled(
        &mut self,
        id: &str,
        enabled: bool,
    ) -> Result<(), MockError> {
        let d = self.destinations.get_mut(id)?;
        if d.info.enabled == enabled {
            return Ok(());
        }
        d.info.enabled = enabled;
        d.first_failure_since_success = None;
        if !enabled {
            self.deliveries.abandon_for_destination(
                id,
                AbandonReason::DestinationDisabled,
                self.now,
            );
        }
        Ok(())
    }

    /// Removes a destination and abandons its queued deliveries.
    pub(crate) fn remove_destination(&mut self, id: &str) -> Result<(), MockError> {
        self.destinations.remove(id)?;
        self.deliveries
            .abandon_for_destination(id, AbandonReason::DestinationRemoved, self.now);
        Ok(())
    }

    /// Quiesce support: retries go to `now`, every queued attempt is due now and
    /// every event is visible now.
    pub(crate) fn begin_quiesce(&mut self) {
        self.deliveries.quiescing = true;
        self.events.flush_visibility(self.now);
    }

    pub(crate) fn end_quiesce(&mut self) {
        self.deliveries.quiescing = false;
    }

    pub(crate) fn rush_deliveries(&mut self) {
        self.deliveries.rush_all(self.now);
    }

    pub(crate) fn has_pending_deliveries(&self) -> bool {
        self.deliveries.has_pending()
    }

    pub(crate) fn webhooks_config(&self) -> &crate::WebhooksConfig {
        &self.deliveries.config
    }

    pub(crate) fn set_webhooks_config(&mut self, c: crate::WebhooksConfig) {
        self.deliveries.config = c;
    }

    pub(crate) fn delivery_log(&self) -> &[DeliveryRecord] {
        self.deliveries.log()
    }

    pub(crate) fn retention_ms(&self) -> i64 {
        self.projector.retention_ms
    }

    pub(crate) fn projector_mut(&mut self) -> &mut Projector {
        &mut self.projector
    }

    pub(crate) fn now(&self) -> SimTime {
        self.now
    }

    pub(crate) fn set_now(&mut self, t: SimTime) {
        debug_assert!(t >= self.now, "time moved backwards");
        self.now = t;
    }

    /// Starts a business action; all its mutations share the returned cause.
    pub(crate) fn begin_action(&mut self, name: &str) -> Cause {
        let n = self.next_action;
        self.next_action += 1;
        Cause {
            action: name.to_string(),
            action_id: format!("act_{n}"),
        }
    }

    /// Creates an object, announcing `events` (snapshot event types; may be empty).
    pub(crate) fn create(
        &mut self,
        t: ObjectType,
        id: String,
        state: Value,
        cause: Cause,
        events: &[&str],
    ) -> StateUpdate {
        let created = state["created"].as_i64().unwrap_or(self.now.unix_seconds());
        let update = self.record(t, id, 1, ChangeKind::Created, state, cause, None, events);
        self.store.insert(update.clone(), created);
        self.notify(&update);
        update
    }

    /// Applies `f` to the object's state; records nothing if the state is unchanged.
    /// `kind` is `Updated`, `Archived` or `StateChanged`.
    #[allow(clippy::too_many_arguments)] // every argument is part of the entry
    pub(crate) fn update(
        &mut self,
        t: ObjectType,
        id: &str,
        f: impl FnOnce(&mut Value),
        cause: Cause,
        kind: ChangeKind,
        events: &[&str],
    ) -> Result<Option<StateUpdate>, MockError> {
        debug_assert!(matches!(
            kind,
            ChangeKind::Updated | ChangeKind::Archived | ChangeKind::StateChanged
        ));
        let latest = self.live(t, id)?;
        let mut state = latest.state.clone();
        f(&mut state);
        if state == latest.state {
            return Ok(None);
        }
        let version = latest.version + 1;
        let previous = latest.state.clone();
        let update = self.record(
            t,
            id.to_string(),
            version,
            kind,
            state,
            cause,
            Some(previous),
            events,
        );
        self.store.replace(update.clone());
        self.notify(&update);
        Ok(Some(update))
    }

    pub(crate) fn delete(
        &mut self,
        t: ObjectType,
        id: &str,
        tombstone: Value,
        cause: Cause,
        events: &[&str],
    ) -> Result<StateUpdate, MockError> {
        let latest = self.live(t, id)?;
        let version = latest.version + 1;
        let previous = latest.state.clone();
        let update = self.record(
            t,
            id.to_string(),
            version,
            ChangeKind::Deleted,
            tombstone,
            cause,
            Some(previous),
            events,
        );
        self.store.replace(update.clone());
        self.notify(&update);
        Ok(update)
    }

    pub(crate) fn latest(&self, t: ObjectType, id: &str) -> Option<&StateUpdate> {
        self.store.get(t, id).map(|o| &o.latest)
    }

    pub(crate) fn log(&self) -> &[StateUpdate] {
        &self.log
    }

    pub(crate) fn store(&self) -> &ObjectStore {
        &self.store
    }

    fn live(&self, t: ObjectType, id: &str) -> Result<&StateUpdate, MockError> {
        match self.store.get(t, id) {
            None => Err(MockError::NoSuchObject {
                object_type: t,
                id: id.to_string(),
            }),
            Some(o) if o.deleted => Err(MockError::Deleted {
                object_type: t,
                id: id.to_string(),
            }),
            Some(o) => Ok(&o.latest),
        }
    }

    #[allow(clippy::too_many_arguments)] // private helper; every field is part of the entry
    fn record(
        &mut self,
        t: ObjectType,
        id: String,
        version: u64,
        kind: ChangeKind,
        state: Value,
        cause: Cause,
        previous: Option<Value>,
        events: &[&str],
    ) -> StateUpdate {
        let mut update = StateUpdate {
            seq: self.next_seq,
            at: self.now,
            account: None,
            object_type: t,
            object_id: id,
            version,
            kind,
            state,
            events: EventRefs::default(),
            cause: Some(cause),
        };
        self.next_seq += 1;
        for (snapshot, thin) in self.projector.project(&update, previous.as_ref(), events) {
            update.events.snapshot.push(snapshot.id.clone());
            update.events.thin.push(thin.id.clone());
            self.deliveries
                .schedule_for_event(&snapshot, &self.destinations);
            self.deliveries
                .schedule_for_event(&thin, &self.destinations);
            self.events.push(snapshot);
            self.events.push(thin);
        }
        self.log.push(update.clone());
        update
    }

    /// Snapshot plus registration, atomically (the caller holds the sim lock).
    pub(crate) fn subscribe(
        &mut self,
        filter: SubscribeFilter,
        options: &SubscribeOptions,
    ) -> Subscription {
        let mut types: Vec<ObjectType> = self
            .store
            .all_types()
            .into_iter()
            .filter(|t| filter.matches(*t))
            .collect();
        types.sort();
        let snapshot = types
            .into_iter()
            .flat_map(|t| self.store.in_creation_order(t))
            .map(|o| StateUpdate {
                kind: if o.deleted {
                    ChangeKind::Deleted
                } else {
                    ChangeKind::Snapshot
                },
                events: EventRefs::default(),
                cause: None,
                ..o.latest.clone()
            })
            .collect();
        let (subscriber, subscription) = stream::pair(filter, options, snapshot);
        self.subscribers.push(subscriber);
        subscription
    }

    /// Ends every subscription (used on shutdown).
    pub(crate) fn clear_subscribers(&mut self) {
        self.subscribers.clear();
    }

    /// Offers the update to every subscriber without waiting; drops lagging or closed ones.
    fn notify(&mut self, update: &StateUpdate) {
        self.subscribers.retain(|s| s.offer(update));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_engine() -> Engine {
        Engine::new(SimTime(0), &Config::default(), &RngStreams::new(0))
    }

    fn cust(id: &str, created: i64) -> Value {
        json!({"id": id, "object": "customer", "created": created, "email": null})
    }

    #[test]
    fn seq_and_versions() {
        let mut e = test_engine();
        let c = e.begin_action("t");
        let a = e.create(
            ObjectType::Customer,
            "cus_a".into(),
            cust("cus_a", 0),
            c.clone(),
            &["customer.created"],
        );
        let b = e
            .update(
                ObjectType::Customer,
                "cus_a",
                |v| v["email"] = json!("x@example.com"),
                c.clone(),
                ChangeKind::Updated,
                &["customer.updated"],
            )
            .unwrap()
            .unwrap();
        let d = e
            .delete(
                ObjectType::Customer,
                "cus_a",
                json!({"id": "cus_a", "object": "customer", "deleted": true}),
                c,
                &["customer.deleted"],
            )
            .unwrap();
        assert_eq!((a.seq, b.seq, d.seq), (1, 2, 3));
        assert_eq!((a.version, b.version, d.version), (1, 2, 3));
        assert_eq!(
            (a.kind, b.kind, d.kind),
            (
                ChangeKind::Created,
                ChangeKind::Updated,
                ChangeKind::Deleted
            )
        );
        assert_eq!(e.log().len(), 3);
        assert!(
            e.store()
                .get(ObjectType::Customer, "cus_a")
                .unwrap()
                .deleted
        );
        assert!(e.store().live_ids(ObjectType::Customer).is_empty());
    }

    #[test]
    fn noop_update_records_nothing() {
        let mut e = test_engine();
        let c = e.begin_action("t");
        e.create(
            ObjectType::Customer,
            "cus_a".into(),
            cust("cus_a", 0),
            c.clone(),
            &["customer.created"],
        );
        assert_eq!(
            e.update(
                ObjectType::Customer,
                "cus_a",
                |_| {},
                c,
                ChangeKind::Updated,
                &["customer.updated"]
            )
            .unwrap(),
            None
        );
        assert_eq!(e.log().len(), 1);
    }

    #[test]
    fn mutating_deleted_or_missing_errors() {
        let mut e = test_engine();
        let c = e.begin_action("t");
        assert!(matches!(
            e.update(
                ObjectType::Customer,
                "cus_x",
                |_| {},
                c.clone(),
                ChangeKind::Updated,
                &["customer.updated"]
            ),
            Err(MockError::NoSuchObject { .. })
        ));
        e.create(
            ObjectType::Customer,
            "cus_a".into(),
            cust("cus_a", 0),
            c.clone(),
            &["customer.created"],
        );
        e.delete(
            ObjectType::Customer,
            "cus_a",
            json!({}),
            c.clone(),
            &["customer.deleted"],
        )
        .unwrap();
        assert!(matches!(
            e.delete(
                ObjectType::Customer,
                "cus_a",
                json!({}),
                c,
                &["customer.deleted"]
            ),
            Err(MockError::Deleted { .. })
        ));
        assert_eq!(e.log().len(), 2);
    }

    #[test]
    fn list_order_breaks_ties_by_creation() {
        let mut e = test_engine();
        let c = e.begin_action("t");
        for (id, t) in [("cus_1", 5), ("cus_2", 5), ("cus_3", 4)] {
            e.create(
                ObjectType::Customer,
                id.into(),
                cust(id, t),
                c.clone(),
                &["customer.created"],
            );
        }
        let ids: Vec<_> = e
            .store()
            .list_order(ObjectType::Customer)
            .iter()
            .map(|o| o.latest.object_id.clone())
            .collect();
        assert_eq!(ids, ["cus_2", "cus_1", "cus_3"]);
    }

    #[test]
    fn begin_action_ids() {
        let mut e = test_engine();
        assert_eq!(e.begin_action("a").action_id, "act_1");
        assert_eq!(e.begin_action("b").action_id, "act_2");
    }
}
