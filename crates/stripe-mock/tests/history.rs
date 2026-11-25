mod common;

use std::time::Duration;

use common::{Receiver, dest, get, mock_with};
use ductkit_stripe_mock::{ChangeKind, Config, MockStripe, ObjectType, SimTime};
use serde_json::Value;

const DAY: i64 = 86_400_000;
/// The default `clock.start`, 2026-01-01T00:00:00Z.
const START: SimTime = SimTime(1_767_225_600_000);
const RATES: &str = "[actions]\nnew_customer = 2.0\nstart_subscription = 1.0\n";

async fn json(m: &MockStripe, path: &str) -> Value {
    let r = get(m, path).await;
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

/// Every object of a v1 list, following `starting_after`.
async fn walk_v1(m: &MockStripe, path: &str) -> Vec<Value> {
    let sep = if path.contains('?') { '&' } else { '?' };
    let mut all: Vec<Value> = Vec::new();
    let mut page = json(m, &format!("{path}{sep}limit=100")).await;
    loop {
        all.extend(page["data"].as_array().unwrap().iter().cloned());
        if page["has_more"] != true {
            return all;
        }
        let last = all.last().unwrap()["id"].as_str().unwrap().to_string();
        page = json(m, &format!("{path}{sep}limit=100&starting_after={last}")).await;
    }
}

/// Every event of `/v2/core/events`, following `next_page_url`.
async fn walk_v2(m: &MockStripe) -> Vec<Value> {
    let mut all: Vec<Value> = Vec::new();
    let mut page = json(m, "/v2/core/events?limit=100").await;
    loop {
        all.extend(page["data"].as_array().unwrap().iter().cloned());
        match page["next_page_url"].as_str() {
            Some(next) => page = json(m, next).await,
            None => return all,
        }
    }
}

#[tokio::test]
async fn history_runs_before_start() {
    let m = mock_with(&format!("{RATES}[history]\nduration = \"3d\"\n")).await;
    assert_eq!(m.now(), START);
    let log = m.truth_log();
    assert!(!log.is_empty());
    assert!(
        log.iter()
            .all(|u| u.at.0 >= START.0 - 3 * DAY && u.at <= START)
    );
}

#[tokio::test]
async fn old_history_only_in_lists() {
    let m = mock_with(&format!(
        "{RATES}[history]\nduration = \"10d\"\n[event_retention]\nwindow = \"2d\"\n"
    ))
    .await;
    let cutoff_s = (m.now().0 - 2 * DAY) / 1000;
    let customers = walk_v1(&m, "/v1/customers").await;
    assert!(
        customers
            .iter()
            .any(|c| c["created"].as_i64().unwrap() < cutoff_s),
        "backfill must find customers older than retention"
    );
    let v1 = walk_v1(&m, "/v1/events").await;
    assert!(!v1.is_empty());
    assert!(v1.iter().all(|e| e["created"].as_i64().unwrap() > cutoff_s));
    let v2 = walk_v2(&m).await;
    assert!(!v2.is_empty());
    let cutoff = SimTime(cutoff_s * 1000).to_string();
    assert!(
        v2.iter()
            .all(|e| e["created"].as_str().unwrap() > cutoff.as_str()),
        "thin events respect retention too"
    );
}

#[tokio::test]
async fn history_is_not_delivered() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!(
        "{RATES}[history]\nduration = \"1d\"\n{}",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    assert!(!m.truth_log().is_empty());
    m.quiesce().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(rx.requests().is_empty(), "history was delivered");
    m.resume_generation();
    m.step(Duration::from_secs(2 * 3600)).await.unwrap();
    m.quiesce().await.unwrap();
    let got = rx.wait_for(1, Duration::from_secs(5)).await;
    assert!(!got.is_empty());
    for r in got {
        let e: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(
            e["created"].as_i64().unwrap() >= START.unix_seconds(),
            "{e}"
        );
    }
}

#[tokio::test]
async fn history_subscriptions_keep_renewing() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!(
        "[actions]\nnew_customer = 0.5\nstart_subscription = 0.5\n[history]\nduration = \"20d\"\n{}",
        dest(&rx.url("/h"), "snapshot", "invoice.created")
    ))
    .await;
    let old_subs: Vec<String> = m
        .truth_log()
        .iter()
        .filter(|u| u.object_type == ObjectType::Subscription && u.kind == ChangeKind::Created)
        .map(|u| u.object_id.clone())
        .collect();
    assert!(!old_subs.is_empty());
    m.step(Duration::from_secs(35 * 86_400)).await.unwrap();
    m.quiesce().await.unwrap();
    let renewed = rx.requests().iter().any(|r| {
        let e: Value = serde_json::from_slice(&r.body).unwrap();
        let inv = &e["data"]["object"];
        inv["billing_reason"] == "subscription_cycle"
            && inv["parent"]["subscription_details"]["subscription"]
                .as_str()
                .is_some_and(|s| old_subs.iter().any(|o| o == s))
    });
    assert!(renewed, "a subscription from history renewed after start");
}

#[tokio::test]
async fn scale_multiplies_history_rates() {
    let m = mock_with(&format!(
        "{RATES}[history]\nduration = \"5d\"\nscale = 0.0\n"
    ))
    .await;
    assert!(m.truth_log().is_empty());
    let err = Config::from_toml_str("[history]\nscale = -1.0")
        .unwrap_err()
        .to_string();
    assert!(err.contains("history.scale"), "{err}");
}

#[tokio::test]
async fn history_is_deterministic() {
    let cfg = format!("seed = 3\n{RATES}[history]\nduration = \"5d\"\n");
    let a = mock_with(&cfg).await;
    let b = mock_with(&cfg).await;
    assert_eq!(
        serde_json::to_value(a.truth_log()).unwrap(),
        serde_json::to_value(b.truth_log()).unwrap()
    );
}

#[tokio::test]
async fn backfill_race_converges() {
    use std::collections::BTreeMap;
    let m = mock_with(
        "[actions]\nnew_customer = 4.0\n[churn.customer]\nupdate = 2.0\ndelete = 0.2\n[history]\nduration = \"7d\"\n[event_retention]\nwindow = \"2d\"\n[api]\npagination_writes = { probability = 0.3 }\n",
    )
    .await;
    // 1-2. Backfill while live generation (and pagination writes) continue.
    let t0 = m.now().unix_seconds();
    let mut synced: BTreeMap<String, Value> = BTreeMap::new();
    let mut page = json(&m, "/v1/customers?limit=20").await;
    loop {
        for c in page["data"].as_array().unwrap() {
            synced.insert(c["id"].as_str().unwrap().to_string(), c.clone());
        }
        if page["has_more"] != true {
            break;
        }
        m.step(Duration::from_secs(60)).await.unwrap();
        let last = page["data"].as_array().unwrap().last().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        page = json(&m, &format!("/v1/customers?limit=20&starting_after={last}")).await;
    }
    // 3-4. Then apply every event since the backfill started, oldest first.
    m.quiesce().await.unwrap();
    let mut events = walk_v1(&m, &format!("/v1/events?created[gte]={t0}")).await;
    events.reverse();
    assert!(
        events.iter().any(|e| e["type"] == "customer.created"),
        "live generation ran during the backfill"
    );
    for e in events {
        let obj = &e["data"]["object"];
        let id = obj["id"].as_str().unwrap().to_string();
        match e["type"].as_str().unwrap() {
            "customer.created" | "customer.updated" => {
                synced.insert(id, obj.clone());
            }
            "customer.deleted" => {
                synced.remove(&id);
            }
            other => panic!("unexpected event {other}"),
        }
    }
    let mut truth: BTreeMap<String, Value> = BTreeMap::new();
    for u in m.truth_log() {
        if u.kind == ChangeKind::Deleted {
            truth.remove(&u.object_id);
        } else {
            truth.insert(u.object_id.clone(), u.state.clone());
        }
    }
    assert_eq!(synced.len(), truth.len());
    assert!(synced == truth, "backfill plus events did not converge");
}
