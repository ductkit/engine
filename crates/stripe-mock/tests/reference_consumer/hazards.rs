//! One small scripted run per sync-breaking hazard (design:
//! `docs/superpowers/specs/2026-11-28-sync-hazards-design.md`). Each test shows the
//! hazard is observable, that naive handling gets it wrong and, where one exists, that
//! a simple correct handling gets it right.

use std::time::Duration;

use ductkit_stripe_mock::{
    AttemptOutcome, CustomerUpdate, DeliveryRecordKind, MockStripe, NewCustomer,
};
use serde_json::Value;

use crate::common;
use crate::hooks::{HookServer, fails_first};
use crate::scenario::{self, last_snapshot_event};

/// A mock with no generated actions: `seed = 3`, then `extra`, then a snapshot and a
/// thin destination pointing at the two hook servers.
async fn quiet_mock(extra: &str, snapshot: &HookServer, thin: &HookServer) -> MockStripe {
    common::mock_with(&format!(
        "seed = 3
{extra}{}",
        scenario::destinations(&snapshot.url(), &thin.url())
    ))
    .await
}

/// The truth-log state of `id` at `version`.
fn state_at(m: &MockStripe, id: &str, version: u64) -> Value {
    m.truth_log()
        .into_iter()
        .find(|u| u.object_id == id && u.version == version)
        .unwrap_or_else(|| panic!("{id} has no version {version}"))
        .state
}

/// Updates `id` with `update(i)` until the newest `customer.updated` satisfies `want`
/// (at most 30 tries) and returns that event's ID. Tries are 2 s apart, so every
/// update has its own `created` second and "newest created wins" has one answer.
async fn update_until(
    m: &MockStripe,
    id: &str,
    update: impl Fn(usize) -> CustomerUpdate,
    want: impl Fn(&str) -> bool,
) -> String {
    for i in 0..30 {
        m.update_customer(id, update(i)).unwrap();
        let ev = last_snapshot_event(m, id, "customer.updated");
        if want(&ev) {
            return ev;
        }
        m.step(Duration::from_secs(2)).await.unwrap();
    }
    panic!("no matching customer.updated for {id} in 30 tries");
}

#[tokio::test]
async fn stale_retry_carries_older_version() {
    let (snapshot, thin) = (HookServer::start(), HookServer::start());
    let m = quiet_mock("[webhooks]\nretry_schedule = [\"30m\"]\n", &snapshot, &thin).await;
    let c = m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(1)).await.unwrap();
    let stale = update_until(
        &m,
        &c,
        |i| CustomerUpdate::SetName(Some(format!("n{i}"))),
        fails_first,
    )
    .await;
    // Different `created` seconds, so "newest created wins" has one answer.
    m.step(Duration::from_secs(2)).await.unwrap();
    update_until(
        &m,
        &c,
        |i| CustomerUpdate::SetEmail(Some(format!("e{i}@example.com"))),
        |ev| !fails_first(ev),
    )
    .await;
    m.step(Duration::from_secs(3600)).await.unwrap();
    m.quiesce().await.unwrap();
    let latest = m
        .get(ductkit_stripe_mock::ObjectType::Customer, &c)
        .unwrap()
        .state;

    // The retry is recorded as carrying an older version than the object's latest.
    let retried_version = m
        .delivery_log()
        .into_iter()
        .find_map(|r| match r.kind {
            DeliveryRecordKind::Attempt {
                attempt: 2,
                outcome: AttemptOutcome::Success { .. },
                object_version,
                latest_version,
                ..
            } if r.event_id == stale => {
                assert!(
                    object_version < latest_version,
                    "{object_version} vs {latest_version}"
                );
                Some(object_version)
            }
            _ => None,
        })
        .expect("a successful retry of the stale event");

    let mut cursor = 0;
    let bodies: Vec<Value> = snapshot
        .take_new(&mut cursor)
        .into_iter()
        .filter(|e| e["data"]["object"]["id"] == c.as_str())
        .collect();
    let retried = bodies.iter().find(|e| e["id"] == stale.as_str()).unwrap();
    assert_eq!(retried["data"]["object"], state_at(&m, &c, retried_version));
    assert_ne!(retried["data"]["object"], latest);

    // Naive: the last body to arrive wins, and it is the late retry.
    assert_ne!(bodies.last().unwrap()["data"]["object"], latest);
    // Correct: the newest `created` wins (ties by arrival).
    let newest = bodies
        .iter()
        .enumerate()
        .max_by_key(|(i, e)| (e["created"].as_i64().unwrap(), *i))
        .unwrap()
        .1;
    assert_eq!(newest["data"]["object"], latest);
}

#[tokio::test]
async fn delete_before_create_under_reorder() {
    use ductkit_stripe_mock::{EventFormat, SimTime};
    use std::collections::{BTreeMap, BTreeSet};

    let (snapshot, thin) = (HookServer::start(), HookServer::start());
    let m = quiet_mock("[webhooks]\nreorder_window = \"10s\"\n", &snapshot, &thin).await;
    for _ in 0..20 {
        let c = m.create_customer(NewCustomer::default()).unwrap();
        m.step(Duration::from_secs(1)).await.unwrap();
        m.delete_customer(&c).unwrap();
        m.step(Duration::from_secs(60)).await.unwrap();
    }
    m.quiesce().await.unwrap();

    // When each snapshot create/delete was first scheduled. Real HTTP doesn't order
    // arrivals within a step, so the mock's schedule is the order that counts.
    let mut first_attempt: BTreeMap<String, SimTime> = BTreeMap::new();
    for r in m.delivery_log() {
        if let DeliveryRecordKind::Scheduled { attempt_at, .. } = r.kind {
            first_attempt.entry(r.event_id).or_insert(attempt_at);
        }
    }
    let mut scheduled: Vec<(SimTime, String, bool)> = m
        .event_timeline()
        .into_iter()
        .filter(|e| e.format == EventFormat::Snapshot)
        .filter_map(|e| {
            let deleted = match e.event_type.as_str() {
                "customer.created" => false,
                "customer.deleted" => true,
                _ => return None,
            };
            Some((first_attempt[&e.id], e.object_id, deleted))
        })
        .collect();
    scheduled.sort();

    let mut created_seen = BTreeSet::new();
    let inverted = scheduled
        .iter()
        .filter(|(_, id, deleted)| {
            if *deleted {
                !created_seen.contains(id)
            } else {
                created_seen.insert(id.clone());
                false
            }
        })
        .count();
    assert!(inverted >= 1, "no delete was scheduled before its create");

    // Naive upsert in delivery order: a delete before its create leaves a ghost.
    let mut naive = BTreeSet::new();
    // Correct: remember deleted IDs and ignore a later create of one.
    let (mut correct, mut tombstones) = (BTreeSet::new(), BTreeSet::new());
    for (_, id, deleted) in &scheduled {
        if *deleted {
            naive.remove(id);
            correct.remove(id);
            tombstones.insert(id.clone());
        } else {
            naive.insert(id.clone());
            if !tombstones.contains(id) {
                correct.insert(id.clone());
            }
        }
    }
    assert_eq!(naive.len(), inverted, "every inversion leaves a ghost");
    assert!(correct.is_empty(), "{correct:?}");
}
