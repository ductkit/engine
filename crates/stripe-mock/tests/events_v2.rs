mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::{MockStripe, NewCustomer};
use serde_json::Value;

async fn v2(m: &MockStripe, q: &str) -> (u16, Value) {
    let r = get(m, &format!("/v2/core/events?{q}")).await;
    (r.status().as_u16(), r.json().await.unwrap())
}

fn len(v: &Value) -> usize {
    v["data"].as_array().unwrap().len()
}

#[tokio::test]
async fn lists_thin_events_with_page_urls() {
    let m = mock_with("").await;
    for _ in 0..3 {
        m.create_customer(NewCustomer::default()).unwrap();
    }
    let (s, v) = v2(&m, "limit=2").await;
    assert_eq!(s, 200);
    assert_eq!(len(&v), 2);
    assert_eq!(v["data"][0]["object"], "v2.core.event");
    assert_eq!(v["data"][0]["type"], "v1.customer.created");
    assert_eq!(v["previous_page_url"], Value::Null);
    let next = v["next_page_url"].as_str().unwrap();
    let page: Value = get(&m, next).await.json().await.unwrap();
    assert_eq!(len(&page), 1);
    assert_eq!(page["next_page_url"], Value::Null);
    assert!(page["previous_page_url"].is_string());
    let back: Value = get(&m, page["previous_page_url"].as_str().unwrap())
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(back["data"], v["data"]);
}

#[tokio::test]
async fn filters_types_and_object_id() {
    let m = mock_with("").await;
    let a = m.create_customer(NewCustomer::default()).unwrap();
    m.create_customer(NewCustomer::default()).unwrap();
    m.delete_customer(&a).unwrap();
    assert_eq!(len(&v2(&m, "types=v1.customer.deleted").await.1), 1);
    assert_eq!(
        len(&v2(
            &m,
            "types[]=v1.customer.deleted&types[]=v1.customer.created"
        )
        .await
        .1),
        3
    );
    assert_eq!(len(&v2(&m, &format!("object_id={a}")).await.1), 2);
    let t = m.now().to_string();
    assert_eq!(len(&v2(&m, &format!("created[gte]={t}")).await.1), 3);
    assert_eq!(v2(&m, "page=zz&limit=2").await.0, 400);
    assert_eq!(v2(&m, "page=zz").await.0, 400);
    assert_eq!(v2(&m, "created[gte]=not-a-time").await.0, 400);
    assert_eq!(v2(&m, "colour=red").await.0, 400);
}

#[tokio::test]
async fn retrieve_full_event_and_v2_not_found() {
    let m = mock_with("").await;
    m.create_customer(NewCustomer::default()).unwrap();
    let id = v2(&m, "").await.1["data"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let full: Value = get(&m, &format!("/v2/core/events/{id}"))
        .await
        .json()
        .await
        .unwrap();
    assert!(full["snapshot_event"].as_str().unwrap().starts_with("evt_"));
    let r = get(&m, "/v2/core/events/evt_test_missing").await;
    assert_eq!(r.status(), 404);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["type"],
        "not_found"
    );
}

#[tokio::test]
async fn expired_page_token_is_resource_missing() {
    let m = mock_with("[event_retention]\nwindow = \"1h\"").await;
    for _ in 0..2 {
        m.create_customer(NewCustomer::default()).unwrap();
    }
    let next = v2(&m, "limit=1").await.1["next_page_url"]
        .as_str()
        .unwrap()
        .to_string();
    m.step(Duration::from_secs(3600)).await.unwrap();
    let r = get(&m, &next).await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["code"],
        "resource_missing"
    );
}
