//! Reference consumers against one faulty run (design:
//! `docs/superpowers/specs/2026-11-28-reference-consumer-design.md`).

mod client;
#[path = "../common/mod.rs"]
mod common;
mod hooks;
mod truth;

use std::time::Duration;

use ductkit_stripe_mock::{ChangeKind, EventRefs, ObjectType, SimTime, StateUpdate};
use serde_json::{Value, json};

use client::Client;
use hooks::HookServer;
use truth::{Divergence, Synced, Truth};

fn entry(seq: u64, id: &str, version: u64, kind: ChangeKind, state: Value) -> StateUpdate {
    StateUpdate {
        seq,
        at: SimTime(seq as i64),
        account: None,
        object_type: ObjectType::Customer,
        object_id: id.to_string(),
        version,
        kind,
        state,
        events: EventRefs::default(),
        cause: None,
    }
}

fn key(id: &str) -> truth::Key {
    (None, ObjectType::Customer, id.to_string())
}

#[test]
fn classify_counts_each_kind() {
    let a1 = json!({"id": "cus_a", "v": 1});
    let a2 = json!({"id": "cus_a", "v": 2});
    let b1 = json!({"id": "cus_b", "v": 1});
    let c1 = json!({"id": "cus_c", "v": 1});
    let log = vec![
        entry(1, "cus_a", 1, ChangeKind::Created, a1.clone()),
        entry(2, "cus_b", 1, ChangeKind::Created, b1.clone()),
        entry(3, "cus_c", 1, ChangeKind::Created, c1),
        entry(4, "cus_a", 2, ChangeKind::Updated, a2),
        entry(
            5,
            "cus_b",
            2,
            ChangeKind::Deleted,
            json!({"id": "cus_b", "deleted": true}),
        ),
    ];
    let truth = Truth::from_log(&log);
    let synced: Synced = [
        (key("cus_a"), a1),
        (key("cus_b"), b1),
        (key("cus_x"), json!({})),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        truth.classify(&synced),
        Divergence {
            equal: 0,
            stale: 1,
            missing: 1,
            resurrected: 1,
            other: vec![key("cus_x")],
        }
    );
    assert!(truth.classify(&truth.latest).converged());
}

/// The run every scenario shares: accounts, history and generation rates. Fault
/// sections are added per test, because TOML forbids a table twice.
const SCENARIO_BASE: &str = r#"
seed = 7
[accounts]
connect = true
connected_accounts = { initial = 2 }
[history]
duration = "2d"
[actions]
new_customer = 6.0
start_subscription = 3.0
cancel_subscription = 0.5
catalog_change = 0.5
[churn.customer]
update = 3.0
delete = 0.5
[churn.product]
update = 0.5
[churn.price]
update = 0.5
[churn.subscription]
update = 0.5
[churn.invoice]
update = 0.5
"#;

#[tokio::test]
async fn backfill_after_quiesce_matches_truth() {
    let m = common::mock_with(&format!(
        "{SCENARIO_BASE}[api]
errors = {{ \"500\" = 0.1, \"503\" = 0.05, reset = 0.05 }}
pagination_writes = {{ probability = 0.3 }}
"
    ))
    .await;
    for _ in 0..36 {
        m.step(Duration::from_secs(600)).await.unwrap();
    }
    m.quiesce().await.unwrap();
    let c = Client::new(&m);
    let synced = c.backfill(&client::scopes(&m)).await;
    let d = Truth::from_log(&m.truth_log()).classify(&synced);
    assert!(d.converged(), "seed {}: {d:?}", m.seed());
    for t in [
        ObjectType::Customer,
        ObjectType::Subscription,
        ObjectType::Invoice,
    ] {
        assert!(
            synced.keys().any(|(s, kt, _)| s.is_some() && *kt == t),
            "no {t:?} in a connected scope"
        );
    }
}

#[tokio::test]
async fn hook_server_fails_first_attempt_of_chosen_events() {
    use ductkit_stripe_mock::{AttemptOutcome, DeliveryRecordKind, EventFormat};
    use std::collections::{BTreeMap, BTreeSet};

    let hooks = HookServer::start();
    let m = common::mock_with(&format!(
        "[actions]
new_customer = 20.0
[webhooks]
retry_schedule = [\"30m\"]
         [[destinations]]
name = \"s\"
url = \"{}\"
payload = \"snapshot\"
         enabled_events = [\"*\"]
secret = \"{}\"
",
        hooks.url(),
        hooks::SECRET
    ))
    .await;
    m.step(Duration::from_secs(3600)).await.unwrap();
    m.quiesce().await.unwrap();

    let mut cursor = 0;
    let got = hooks.take_new(&mut cursor);
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for e in &got {
        *count
            .entry(e["id"].as_str().unwrap().to_string())
            .or_default() += 1;
    }
    let events: Vec<String> = m
        .event_timeline()
        .into_iter()
        .filter(|e| e.format == EventFormat::Snapshot)
        .map(|e| e.id)
        .collect();
    assert!(!events.is_empty());
    for id in &events {
        assert_eq!(count.get(id), Some(&1), "{id} accepted once");
    }
    assert_eq!(count.len(), events.len());

    let failed: BTreeSet<String> = m
        .delivery_log()
        .into_iter()
        .filter(|r| {
            matches!(
                r.kind,
                DeliveryRecordKind::Attempt {
                    outcome: AttemptOutcome::HttpError { status: 500 },
                    ..
                }
            )
        })
        .map(|r| r.event_id)
        .collect();
    let chosen: BTreeSet<String> = events
        .iter()
        .filter(|id| hooks::fails_first(id))
        .cloned()
        .collect();
    assert!(!chosen.is_empty());
    assert_eq!(failed, chosen);
    assert!(hooks.take_new(&mut cursor).is_empty());
}
