mod common;

use common::{get, mock_with};
use ductkit_stripe_mock::{Action, ChangeKind, MockStripe, NewCustomer, ObjectType};
use serde_json::Value;

const WRITES: &str = "seed = 5\n[api]\npagination_writes = { probability = 1.0 }";

async fn page(m: &MockStripe, path: &str) -> Value {
    let r = get(m, path).await;
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

/// Walks `base` (with `limit` set) page by page; returns the truth-log length
/// after each page.
async fn walk(m: &MockStripe, base: &str) -> Vec<usize> {
    let mut lens = Vec::new();
    let mut v = page(m, base).await;
    lens.push(m.truth_log().len());
    while v["has_more"] == true {
        let last = v["data"].as_array().unwrap().last().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        v = page(m, &format!("{base}&starting_after={last}")).await;
        lens.push(m.truth_log().len());
    }
    lens
}

async fn with_customers(cfg: &str) -> MockStripe {
    let m = mock_with(cfg).await;
    for _ in 0..30 {
        m.create_customer(NewCustomer::default()).unwrap();
    }
    m
}

#[tokio::test]
async fn writes_happen_between_pages() {
    let m = with_customers(WRITES).await;
    let before = m.truth_log().len();
    let lens = walk(&m, "/v1/customers?limit=5").await;
    assert!(lens.len() > 1);
    assert_eq!(lens[0], before, "the first page writes nothing");
    for w in lens.windows(2) {
        assert!(w[1] > w[0], "every later page follows a write: {lens:?}");
    }
    let log = m.truth_log();
    assert!(
        log[before..]
            .iter()
            .all(|u| u.object_type == ObjectType::Customer)
    );
}

#[tokio::test]
async fn writes_follow_the_listed_type() {
    let m = mock_with(WRITES).await;
    let products = |m: &MockStripe| {
        m.truth_log()
            .iter()
            .filter(|u| u.object_type == ObjectType::Product && u.kind == ChangeKind::Created)
            .count()
    };
    while products(&m) < 3 {
        m.run_action(Action::CatalogChange).unwrap();
    }
    let first = page(&m, "/v1/products?limit=1").await;
    let id = first["data"][0]["id"].as_str().unwrap().to_string();
    let before = m.truth_log().len();
    page(&m, &format!("/v1/products?limit=1&starting_after={id}")).await;
    let log = m.truth_log();
    assert!(log.len() > before);
    assert!(
        log[before..]
            .iter()
            .all(|u| matches!(u.object_type, ObjectType::Product | ObjectType::Price))
    );
}

#[tokio::test]
async fn no_writes_after_quiesce() {
    let m = with_customers(WRITES).await;
    m.quiesce().await.unwrap();
    let before = m.truth_log().len();
    let lens = walk(&m, "/v1/customers?limit=5").await;
    assert!(lens.iter().all(|n| *n == before), "{lens:?}");
}

#[tokio::test]
async fn pagination_writes_are_deterministic() {
    let mut logs = Vec::new();
    for _ in 0..2 {
        let m = with_customers(WRITES).await;
        walk(&m, "/v1/customers?limit=4").await;
        logs.push(serde_json::to_value(m.truth_log()).unwrap());
    }
    assert_eq!(logs[0], logs[1]);
}
