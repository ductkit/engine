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

#[tokio::test]
async fn ending_before_poller_misses_late_events() {
    use crate::client::{Client, Fetched};
    use ductkit_stripe_mock::EventFormat;
    use std::collections::BTreeSet;

    let (snapshot, thin) = (HookServer::start(), HookServer::start());
    let m = quiet_mock(
        "[actions]\nnew_customer = 30.0\n\
         [events]\nlate_tail = { probability = 0.5, min = \"1m\", max = \"10m\" }\n",
        &snapshot,
        &thin,
    )
    .await;
    let c = Client::new(&m);
    let page = async |path: String| match c.get(&path, None).await {
        Fetched::Object(v) => v,
        Fetched::Gone => panic!("{path} is gone"),
    };

    // Naive: ask only for what is newer than the newest event seen.
    let mut naive: BTreeSet<String> = BTreeSet::new();
    let mut cursor: Option<String> = None;
    // Correct: re-read a window as long as the late tail, and dedupe by ID.
    let mut overlap: BTreeSet<String> = BTreeSet::new();
    let mut newest_created: i64 = 0;
    for tick in 0..=120 {
        if tick == 120 {
            m.quiesce().await.unwrap(); // every event is visible now
        } else {
            m.step(Duration::from_secs(60)).await.unwrap();
        }
        let path = match &cursor {
            Some(id) => format!("/v1/events?limit=100&ending_before={id}"),
            None => "/v1/events?limit=100".to_string(),
        };
        let p = page(path).await;
        let data = p["data"].as_array().unwrap();
        if let Some(first) = data.first() {
            cursor = Some(first["id"].as_str().unwrap().to_string());
        }
        naive.extend(data.iter().map(|e| e["id"].as_str().unwrap().to_string()));

        let base = format!("/v1/events?limit=100&created[gte]={}", newest_created - 600);
        let mut p = page(base.clone()).await;
        loop {
            let data = p["data"].as_array().unwrap();
            for e in data {
                overlap.insert(e["id"].as_str().unwrap().to_string());
                newest_created = newest_created.max(e["created"].as_i64().unwrap());
            }
            if p["has_more"] != true {
                break;
            }
            let last = data.last().unwrap()["id"].as_str().unwrap();
            p = page(format!("{base}&starting_after={last}")).await;
        }
    }

    let all: BTreeSet<String> = m
        .event_timeline()
        .into_iter()
        .filter(|e| e.format == EventFormat::Snapshot)
        .map(|e| e.id)
        .collect();
    assert!(naive.is_subset(&all));
    assert!(
        naive.len() < all.len(),
        "the ending_before poller missed nothing ({} events)",
        all.len()
    );
    eprintln!(
        "ending_before poller missed {} of {}",
        all.len() - naive.len(),
        all.len()
    );
    assert_eq!(overlap, all, "the overlap poller missed events");
}

#[tokio::test]
async fn same_second_events_list_out_of_truth_order() {
    use crate::client::{Client, Fetched};
    use ductkit_stripe_mock::{EventFormat, ObjectType};

    let (snapshot, thin) = (HookServer::start(), HookServer::start());
    let m = quiet_mock("[events]\nsame_second_shuffle = true\n", &snapshot, &thin).await;
    let client = Client::new(&m);
    let c = m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(2)).await.unwrap();

    let mut replayed_last: Option<Value> = None;
    for burst in 0..20 {
        let second = m.now().unix_seconds();
        for i in 0..4 {
            m.update_customer(&c, CustomerUpdate::SetName(Some(format!("b{burst}-{i}"))))
                .unwrap();
        }
        // The burst's events in truth order (the timeline is in creation order).
        let burst_ids: Vec<String> = m
            .event_timeline()
            .into_iter()
            .filter(|e| {
                e.format == EventFormat::Snapshot
                    && e.object_id == c
                    && e.event_type == "customer.updated"
                    && e.created.unix_seconds() == second
            })
            .map(|e| e.id)
            .collect();
        assert_eq!(burst_ids.len(), 4);
        let Fetched::Object(page) = client
            .get(&format!("/v1/events?limit=100&created[gte]={second}"), None)
            .await
        else {
            panic!("events list is gone");
        };
        // Oldest first, as a consumer replaying the list would apply it.
        let mut replay: Vec<&Value> = page["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| burst_ids.iter().any(|id| e["id"] == id.as_str()))
            .collect();
        replay.reverse();
        assert_eq!(replay.len(), 4);
        if replay.last().unwrap()["id"] != burst_ids.last().unwrap().as_str() {
            replayed_last = Some(replay.last().unwrap()["data"]["object"].clone());
            break;
        }
        m.step(Duration::from_secs(2)).await.unwrap();
    }
    let replayed_last = replayed_last.expect("no burst listed out of truth order in 20 tries");
    let latest = m.get(ObjectType::Customer, &c).unwrap().state;
    assert_ne!(
        replayed_last, latest,
        "replaying the list ends on a stale state"
    );
}

#[tokio::test]
async fn thin_fetch_after_delete_returns_stub() {
    use crate::client::{Client, Fetched};
    use crate::truth::{Synced, Truth};
    use ductkit_stripe_mock::ObjectType;

    let (snapshot, thin) = (HookServer::start(), HookServer::start());
    let m = quiet_mock("", &snapshot, &thin).await;
    let c = m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(60)).await.unwrap();
    m.update_customer(&c, CustomerUpdate::SetName(Some("before delete".into())))
        .unwrap();
    m.step(Duration::from_secs(60)).await.unwrap();
    m.delete_customer(&c).unwrap();
    m.quiesce().await.unwrap();

    // The hint for the update arrives, but the object is gone by the time it's fetched.
    let mut cursor = 0;
    let hint = thin
        .take_new(&mut cursor)
        .into_iter()
        .find(|e| e["type"] == "v1.customer.updated" && e["related_object"]["id"] == c.as_str())
        .expect("a thin v1.customer.updated for the customer");
    let url = hint["related_object"]["url"].as_str().unwrap();
    let resp = common::get(&m, url).await;
    assert_eq!(
        resp.status(),
        200,
        "a deleted customer is a 200 stub, not a 404"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["deleted"], true, "{body}");

    // Storing the stub as the object is a state the customer never had.
    let truth = Truth::from_log(&m.truth_log());
    let key = (None, ObjectType::Customer, c.clone());
    let naive: Synced = [(key.clone(), body)].into_iter().collect();
    assert_eq!(truth.classify(&naive).other, vec![key]);
    // Correct: the stub means gone.
    assert!(matches!(
        Client::new(&m).get(url, None).await,
        Fetched::Gone
    ));
}

#[tokio::test]
async fn stale_read_regresses_after_webhook() {
    let (snapshot, thin) = (HookServer::start(), HookServer::start());
    let m = quiet_mock(
        "[api]\nstale_read = { probability = 1.0, max_lag = \"10m\" }\n",
        &snapshot,
        &thin,
    )
    .await;
    let c = m.create_customer(NewCustomer::default()).unwrap();
    // Older than `max_lag`, so a stale read lands on version 1, never before creation.
    m.step(Duration::from_secs(20 * 60)).await.unwrap();
    m.update_customer(&c, CustomerUpdate::SetName(Some("v2".into())))
        .unwrap();
    m.step(Duration::from_secs(1)).await.unwrap();
    m.quiesce().await.unwrap();

    let mut cursor = 0;
    let webhook = snapshot
        .take_new(&mut cursor)
        .into_iter()
        .find(|e| e["type"] == "customer.updated" && e["data"]["object"]["id"] == c.as_str())
        .expect("the customer.updated webhook");
    assert_eq!(webhook["data"]["object"], state_at(&m, &c, 2));

    // A refetch after the webhook can go back to version 1.
    let v1 = state_at(&m, &c, 1);
    let mut regressed = false;
    for _ in 0..10 {
        let r = common::get(&m, &format!("/v1/customers/{c}")).await;
        assert_eq!(r.status(), 200);
        if r.json::<Value>().await.unwrap() == v1 {
            regressed = true;
            break;
        }
    }
    assert!(
        regressed,
        "no retrieve returned the version before the webhook"
    );
}
