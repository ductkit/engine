mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::{CustomerUpdate, Distribution, MockStripe, NewCustomer};
use serde_json::Value;

async fn events(m: &MockStripe, q: &str) -> (u16, Value) {
    let r = get(m, &format!("/v1/events?{q}")).await;
    (r.status().as_u16(), r.json().await.unwrap())
}

fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn lists_snapshot_events_newest_first() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(1)).await.unwrap();
    m.update_customer(&a, CustomerUpdate::SetName(Some("B".into())))
        .unwrap();
    let (s, v) = events(&m, "").await;
    assert_eq!(s, 200);
    assert_eq!(v["url"], "/v1/events");
    let types: Vec<_> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["customer.updated", "customer.created"]);
    let one: Value = get(&m, &format!("/v1/events/{}", ids(&v)[0]))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(one["type"], "customer.updated");
}

#[tokio::test]
async fn invisible_until_lag_passes() {
    let m = mock_with("[events]\nvisibility_lag = { distribution = \"fixed\", ms = 2000 }").await;
    m.create_customer(NewCustomer::default()).unwrap();
    assert!(ids(&events(&m, "").await.1).is_empty());
    m.step(Duration::from_millis(2000)).await.unwrap();
    assert_eq!(ids(&events(&m, "").await.1).len(), 1);
}

#[tokio::test]
async fn late_event_appears_behind_ending_before_cursor() {
    let m = mock_with("").await;
    m.update_events_config(|e| e.visibility_lag = Distribution::Fixed { ms: 60_000 })
        .unwrap();
    m.create_customer(NewCustomer::default()).unwrap(); // older, visible at +60 s
    m.step(Duration::from_secs(1)).await.unwrap();
    m.update_events_config(|e| e.visibility_lag = Distribution::Fixed { ms: 0 })
        .unwrap();
    m.create_customer(NewCustomer::default()).unwrap(); // newer, visible now
    let newest = ids(&events(&m, "").await.1);
    assert_eq!(newest.len(), 1);
    m.step(Duration::from_secs(60)).await.unwrap();
    // A poller asking for events newer than what it has seen gets nothing...
    assert!(ids(&events(&m, &format!("ending_before={}", newest[0])).await.1).is_empty());
    // ...although the late event is now listed, behind the cursor.
    let all = ids(&events(&m, "").await.1);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0], newest[0]);
}

#[tokio::test]
async fn expired_cursor_is_resource_missing() {
    let m = mock_with("[event_retention]\nwindow = \"1h\"").await;
    m.create_customer(NewCustomer::default()).unwrap();
    let id = ids(&events(&m, "").await.1)[0].clone();
    m.step(Duration::from_secs(3600)).await.unwrap();
    let (s, v) = events(&m, &format!("starting_after={id}")).await;
    assert_eq!(s, 400);
    assert_eq!(v["error"]["code"], "resource_missing");
    assert_eq!(v["error"]["param"], "starting_after");
    assert_eq!(get(&m, &format!("/v1/events/{id}")).await.status(), 404);
}

#[tokio::test]
async fn type_filters() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap();
    m.delete_customer(&a).unwrap();
    assert_eq!(ids(&events(&m, "type=customer.deleted").await.1).len(), 1);
    assert_eq!(ids(&events(&m, "type=customer.*").await.1).len(), 2);
    assert_eq!(
        ids(
            &events(&m, "types[]=customer.created&types[]=customer.deleted")
                .await
                .1
        )
        .len(),
        2
    );
    assert_eq!(
        events(&m, "type=customer.created&types[]=customer.deleted")
            .await
            .0,
        400
    );
    assert_eq!(events(&m, "delivery_success=maybe").await.0, 400);
}

async fn order_of_twenty(cfg: &str) -> Vec<String> {
    let m = mock_with(cfg).await;
    for _ in 0..20 {
        m.create_customer(NewCustomer::default()).unwrap();
    }
    ids(&events(&m, "limit=100").await.1)
}

#[tokio::test]
async fn same_second_shuffle_is_seeded() {
    let cfg = "seed = 5\n[events]\nsame_second_shuffle = true";
    let (a, b) = (order_of_twenty(cfg).await, order_of_twenty(cfg).await);
    assert_eq!(a, b);
    let plain = order_of_twenty("seed = 5").await;
    assert_ne!(a, plain);
}
