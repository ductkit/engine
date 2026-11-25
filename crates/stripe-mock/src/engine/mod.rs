//! The truth log: every mutation, in order (spec section 2).

pub(crate) mod events;
pub(crate) mod store;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Serialize;
use serde_json::{Value, json};

use crate::delivery::{
    AbandonReason, AttemptOutcome, Deliveries, DeliveryRecord, Destinations, PreparedAttempt,
};
use crate::model::account::{self, AccountContent, AccountMeta};
use crate::rng::{RngStreams, SimRng};
use crate::stream::{self, SubscribeFilter, SubscribeOptions, Subscriber, Subscription};
use crate::{AccountApi, AccountKind, Config, Configuration, MockError, ObjectType, SimTime};

pub(crate) use events::{EventStore, Projector};

pub(crate) use store::ObjectStore;

/// Which account an object belongs to: `None` is the platform, `Some(acct_...)` a
/// connected account.
pub type Scope = Option<String>;

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
    /// One object store per account scope; the platform's is keyed `None`.
    stores: BTreeMap<Scope, ObjectStore>,
    /// The scope mutations and reads currently apply to.
    scope: Scope,
    subscribers: Vec<Subscriber>,
    projector: Projector,
    events: EventStore,
    destinations: Destinations,
    deliveries: Deliveries,
    /// Every account ever connected, in connection order.
    accounts: Vec<AccountMeta>,
    /// Draws account IDs.
    account_ids: SimRng,
}

impl Engine {
    pub(crate) fn new(start: SimTime, config: &Config, streams: &RngStreams) -> Self {
        // The platform application, present when `accounts.connect` is on.
        let application = config.accounts.connect.then(|| {
            json!({
                "id": streams.stream("platform.application").id("ca"),
                "object": "application",
                "name": "Ductkit Mock Platform",
            })
        });
        let mut projector = Projector::new(config, streams);
        projector.application = application;
        Engine {
            now: start,
            next_seq: 1,
            next_action: 1,
            log: Vec::new(),
            stores: BTreeMap::new(),
            scope: None,
            subscribers: Vec::new(),
            projector,
            events: EventStore::default(),
            destinations: Destinations::new(streams),
            deliveries: Deliveries::new(&config.webhooks, streams),
            accounts: Vec::new(),
            account_ids: streams.stream("accounts.ids"),
        }
    }

    /// What the mock knows about an account ever connected.
    pub(crate) fn account_meta(&self, id: &str) -> Option<&AccountMeta> {
        self.accounts.iter().find(|a| a.id == id)
    }

    /// Every account ever connected, in connection order.
    pub(crate) fn account_metas(&self) -> &[AccountMeta] {
        &self.accounts
    }

    /// IDs of accounts that are still connected, in connection order.
    pub(crate) fn connected_ids(&self) -> Vec<String> {
        self.accounts
            .iter()
            .filter(|a| a.connected)
            .map(|a| a.id.clone())
            .collect()
    }

    /// Connects a new account to the platform now; returns its ID. Accounts live in
    /// the platform scope.
    pub(crate) fn connect_account(
        &mut self,
        api: AccountApi,
        kind: AccountKind,
        configurations: Vec<Configuration>,
        content: AccountContent,
    ) -> String {
        let id = self.account_ids.id(ObjectType::Account.id_prefix());
        let state = account::v1_state(&id, kind, self.now.unix_seconds(), &content);
        let events: &[&str] = match api {
            AccountApi::V1 => &["account.application.authorized"],
            AccountApi::V2 => &["account.application.authorized", "v2.core.account.created"],
        };
        let cause = self.begin_action("connect_account");
        self.in_scope(None, |e| {
            e.create(ObjectType::Account, id.clone(), state, cause, events);
        });
        self.accounts.push(AccountMeta {
            id: id.clone(),
            api,
            kind,
            configurations,
            connected: true,
        });
        id
    }

    /// Disconnects an account now: a delete in the store. An unknown or already
    /// disconnected ID is `UnknownAccount`.
    pub(crate) fn disconnect_account(&mut self, id: &str) -> Result<(), MockError> {
        let api = self.connected_api(id)?;
        let events: &[&str] = match api {
            AccountApi::V1 => &["account.application.deauthorized"],
            AccountApi::V2 => &["account.application.deauthorized", "v2.core.account.closed"],
        };
        let tombstone = json!({"id": id, "object": "account", "deleted": true});
        let cause = self.begin_action("disconnect_account");
        self.in_scope(None, |e| {
            e.delete(ObjectType::Account, id, tombstone, cause, events)
        })?;
        if let Some(meta) = self.accounts.iter_mut().find(|a| a.id == id) {
            meta.connected = false;
        }
        Ok(())
    }

    /// Changes a connected account's fields now.
    pub(crate) fn update_account(
        &mut self,
        id: &str,
        content: AccountContent,
    ) -> Result<(), MockError> {
        let api = self.connected_api(id)?;
        let events: &[&str] = match api {
            AccountApi::V1 => &["account.updated"],
            AccountApi::V2 => &["account.updated", "v2.core.account.updated"],
        };
        let cause = self.begin_action("account_update");
        self.in_scope(None, |e| {
            e.update(
                ObjectType::Account,
                id,
                |s| account::apply_content(s, &content),
                cause,
                ChangeKind::Updated,
                events,
            )
        })?;
        Ok(())
    }

    fn connected_api(&self, id: &str) -> Result<AccountApi, MockError> {
        self.accounts
            .iter()
            .find(|a| a.id == id && a.connected)
            .map(|a| a.api)
            .ok_or_else(|| MockError::UnknownAccount(id.to_string()))
    }

    /// Adds the configured destinations at the current time (after history).
    pub(crate) fn add_config_destinations(&mut self, config: &Config) {
        for c in &config.destinations {
            self.destinations
                .add(c.clone(), self.now)
                .expect("config destinations are validated");
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
            .and_then(|ev| self.store_in(&ev.scope)?.get(ev.object_type, &ev.object_id))
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
        let pos = self.log.len() - 1;
        self.store_mut().insert(update.clone(), created, pos);
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
        let pos = self.log.len() - 1;
        self.store_mut().replace(update.clone(), pos);
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
        let pos = self.log.len() - 1;
        self.store_mut().replace(update.clone(), pos);
        self.notify(&update);
        Ok(update)
    }

    /// The object's latest version with `at <= at`; `None` if it did not exist yet.
    #[cfg(test)]
    pub(crate) fn as_of(&self, t: ObjectType, id: &str, at: SimTime) -> Option<&StateUpdate> {
        self.as_of_in(&self.scope, t, id, at)
    }

    /// Like [`as_of`](Self::as_of), reading the object in `scope`.
    pub(crate) fn as_of_in(
        &self,
        scope: &Scope,
        t: ObjectType,
        id: &str,
        at: SimTime,
    ) -> Option<&StateUpdate> {
        self.store_in(scope)?
            .get(t, id)?
            .versions
            .iter()
            .rev()
            .map(|&pos| &self.log[pos])
            .find(|u| u.at <= at)
    }

    pub(crate) fn latest(&self, t: ObjectType, id: &str) -> Option<&StateUpdate> {
        self.store().get(t, id).map(|o| &o.latest)
    }

    pub(crate) fn log(&self) -> &[StateUpdate] {
        &self.log
    }

    /// The current scope's store (an empty one if the scope has no objects yet).
    pub(crate) fn store(&self) -> &ObjectStore {
        self.scope_store(&self.scope)
    }

    /// `scope`'s store, or an empty one if it has no objects yet.
    pub(crate) fn scope_store(&self, scope: &Scope) -> &ObjectStore {
        static EMPTY: OnceLock<ObjectStore> = OnceLock::new();
        self.store_in(scope)
            .unwrap_or_else(|| EMPTY.get_or_init(ObjectStore::default))
    }

    pub(crate) fn store_in(&self, scope: &Scope) -> Option<&ObjectStore> {
        self.stores.get(scope)
    }

    fn store_mut(&mut self) -> &mut ObjectStore {
        self.stores.entry(self.scope.clone()).or_default()
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Runs `f` with `scope` as the current scope, then restores the previous one.
    pub(crate) fn in_scope<R>(&mut self, scope: Scope, f: impl FnOnce(&mut Engine) -> R) -> R {
        let previous = std::mem::replace(&mut self.scope, scope);
        let result = f(self);
        self.scope = previous;
        result
    }

    fn live(&self, t: ObjectType, id: &str) -> Result<&StateUpdate, MockError> {
        match self.store().get(t, id) {
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
            account: self.scope.clone(),
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
            if let Some(snapshot) = snapshot {
                update.events.snapshot.push(snapshot.id.clone());
                self.deliveries
                    .schedule_for_event(&snapshot, &self.destinations);
                self.events.push(snapshot);
            }
            update.events.thin.push(thin.id.clone());
            self.deliveries
                .schedule_for_event(&thin, &self.destinations);
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
        let snapshot = self
            .stores
            .iter()
            .filter(|(scope, _)| filter.matches_scope(scope))
            .flat_map(|(_, store)| {
                let mut types: Vec<ObjectType> = store
                    .all_types()
                    .into_iter()
                    .filter(|t| filter.matches(*t))
                    .collect();
                types.sort();
                types.into_iter().flat_map(|t| store.in_creation_order(t))
            })
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
    fn as_of_picks_version_at_time() {
        let mut e = test_engine();
        let c = e.begin_action("t");
        e.set_now(SimTime(1_000));
        e.create(
            ObjectType::Customer,
            "cus_a".into(),
            cust("cus_a", 1),
            c.clone(),
            &[],
        );
        for (t, name) in [(11_000, "two"), (21_000, "three")] {
            e.set_now(SimTime(t));
            e.update(
                ObjectType::Customer,
                "cus_a",
                |v| v["name"] = json!(name),
                c.clone(),
                ChangeKind::Updated,
                &[],
            )
            .unwrap();
        }
        let version = |at: i64| {
            e.as_of(ObjectType::Customer, "cus_a", SimTime(at))
                .map(|u| u.version)
        };
        assert_eq!(version(6_000), Some(1));
        assert_eq!(version(11_000), Some(2));
        assert_eq!(version(26_000), Some(3));
        assert_eq!(version(500), None);
        assert_eq!(
            e.as_of(ObjectType::Customer, "cus_zz", SimTime(26_000))
                .map(|u| u.version),
            None
        );
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
        // `created` never decreases in creation order (it is always "now").
        for (id, t) in [("cus_1", 5), ("cus_2", 5), ("cus_3", 6)] {
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
        assert_eq!(ids, ["cus_3", "cus_2", "cus_1"]);
    }

    fn make_customer(e: &mut Engine, id: &str) {
        let c = e.begin_action("t");
        e.create(
            ObjectType::Customer,
            id.into(),
            cust(id, 0),
            c,
            &["customer.created"],
        );
    }

    #[test]
    fn scoped_writes_are_isolated() {
        let mut e = test_engine();
        make_customer(&mut e, "cus_p");
        e.in_scope(Some("acct_1".into()), |e| make_customer(e, "cus_c"));
        assert!(e.latest(ObjectType::Customer, "cus_p").is_some());
        assert!(e.latest(ObjectType::Customer, "cus_c").is_none());
        e.in_scope(Some("acct_1".into()), |e| {
            assert!(e.latest(ObjectType::Customer, "cus_c").is_some());
            assert!(e.latest(ObjectType::Customer, "cus_p").is_none());
        });
        let accounts: Vec<_> = e.log().iter().map(|u| u.account.clone()).collect();
        assert_eq!(accounts, [None, Some("acct_1".to_string())]);
    }

    #[test]
    fn in_scope_restores() {
        let mut e = test_engine();
        e.in_scope(Some("a".into()), |e| {
            assert_eq!(e.scope(), &Some("a".to_string()));
            e.in_scope(Some("b".into()), |e| {
                assert_eq!(e.scope(), &Some("b".to_string()));
            });
            assert_eq!(e.scope(), &Some("a".to_string()));
        });
        assert_eq!(e.scope(), &None);
    }

    #[test]
    fn connected_event_payloads_carry_account() {
        let mut e = test_engine();
        make_customer(&mut e, "cus_p");
        e.in_scope(Some("acct_1".into()), |e| make_customer(e, "cus_c"));
        let payload = |id: &str, format: events::EventFormat| {
            let r = e
                .events()
                .all()
                .iter()
                .find(|r| r.object_id == id && r.format == format)
                .unwrap();
            r.payload.clone()
        };
        let snap = payload("cus_c", events::EventFormat::Snapshot);
        assert_eq!(snap["account"], "acct_1");
        let thin = payload("cus_c", events::EventFormat::Thin);
        assert_eq!(thin["context"], "acct_1");
        let snap = payload("cus_p", events::EventFormat::Snapshot);
        assert!(snap.get("account").is_none());
        let thin = payload("cus_p", events::EventFormat::Thin);
        assert_eq!(thin["context"], Value::Null);
        let rec = e
            .events()
            .all()
            .iter()
            .find(|r| r.object_id == "cus_c")
            .unwrap();
        assert_eq!(rec.scope, Some("acct_1".to_string()));
        assert_eq!(rec.account, Some("acct_1".to_string()));
    }

    #[test]
    fn as_of_is_scoped() {
        let mut e = test_engine();
        e.in_scope(Some("acct_1".into()), |e| make_customer(e, "cus_c"));
        let at = SimTime(1_000);
        assert!(e.as_of(ObjectType::Customer, "cus_c", at).is_none());
        e.in_scope(Some("acct_1".into()), |e| {
            assert!(e.as_of(ObjectType::Customer, "cus_c", at).is_some());
        });
    }

    #[test]
    fn begin_action_ids() {
        let mut e = test_engine();
        assert_eq!(e.begin_action("a").action_id, "act_1");
        assert_eq!(e.begin_action("b").action_id, "act_2");
    }
}
