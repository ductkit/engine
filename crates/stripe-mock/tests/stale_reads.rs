mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::{CustomerUpdate, MockStripe, NewCustomer};
use serde_json::Value;

const STALE: &str = "[api]\nstale_read = { probability = 1.0, max_lag = \"10s\" }";

async fn fetch(m: &MockStripe, path: &str) -> (u16, Value) {
    let r = get(m, path).await;
    let s = r.status().as_u16();
    (s, r.json().await.unwrap())
}

fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn stale_retrieve_never_shows_the_newest_version() {
    let m = mock_with(STALE).await;
    let id = m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(5)).await.unwrap();
    m.update_customer(&id, CustomerUpdate::SetName(Some("v2".into())))
        .unwrap();
    let path = format!("/v1/customers/{id}");
    let mut saw_v1 = false;
    for _ in 0..20 {
        let (s, v) = fetch(&m, &path).await;
        match s {
            200 => {
                assert_ne!(v["name"], "v2", "a stale read showed the newest version");
                saw_v1 = true;
            }
            404 => assert_eq!(v["error"]["code"], "resource_missing"),
            other => panic!("unexpected {other}: {v}"),
        }
    }
    assert!(saw_v1);
    m.step(Duration::from_secs(11)).await.unwrap();
    for _ in 0..20 {
        let (s, v) = fetch(&m, &path).await;
        assert_eq!((s, v["name"].clone()), (200, Value::from("v2")));
    }
}

#[tokio::test]
async fn stale_list_omits_just_created() {
    let m = mock_with(STALE).await;
    let id = m.create_customer(NewCustomer::default()).unwrap();
    let (s, v) = fetch(&m, "/v1/customers").await;
    assert_eq!(s, 200);
    assert!(!ids(&v).contains(&id));
    m.step(Duration::from_secs(11)).await.unwrap();
    assert!(ids(&fetch(&m, "/v1/customers").await.1).contains(&id));
}

#[tokio::test]
async fn stale_page_with_new_cursor_still_pages() {
    let m = mock_with(STALE).await;
    let mut older = Vec::new();
    for _ in 0..5 {
        older.push(m.create_customer(NewCustomer::default()).unwrap());
    }
    m.step(Duration::from_secs(20)).await.unwrap();
    let x = m.create_customer(NewCustomer::default()).unwrap();
    let (s, v) = fetch(&m, &format!("/v1/customers?starting_after={x}")).await;
    assert_eq!(s, 200, "{v}");
    older.reverse();
    assert_eq!(ids(&v), older);
}

#[tokio::test]
async fn stale_reads_off_by_default() {
    let m = mock_with("").await;
    let id = m.create_customer(NewCustomer::default()).unwrap();
    m.update_customer(&id, CustomerUpdate::SetName(Some("v2".into())))
        .unwrap();
    for _ in 0..20 {
        let (s, v) = fetch(&m, &format!("/v1/customers/{id}")).await;
        assert_eq!((s, v["name"].clone()), (200, Value::from("v2")));
    }
}
