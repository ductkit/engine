mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::{MockStripe, NewCustomer};
use serde_json::{Value, json};

async fn list(m: &MockStripe, q: &str) -> (u16, Value) {
    let r = get(m, &format!("/v1/customers?{q}")).await;
    (r.status().as_u16(), r.json().await.unwrap())
}

fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect()
}

/// Creates `n` customers, all in the same simulated second.
fn make(m: &MockStripe, n: usize) -> Vec<String> {
    (0..n)
        .map(|_| m.create_customer(NewCustomer::default()).unwrap())
        .collect()
}

#[tokio::test]
async fn retrieve_live_deleted_missing() {
    let m = mock_with("").await;
    let id = m.create_customer(NewCustomer::default()).unwrap();
    let r: Value = get(&m, &format!("/v1/customers/{id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(r["id"], id.as_str());
    assert_eq!(r["object"], "customer");
    m.delete_customer(&id).unwrap();
    let r: Value = get(&m, &format!("/v1/customers/{id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(r, json!({"id": id, "object": "customer", "deleted": true}));
    let r = get(&m, "/v1/customers/cus_missing").await;
    assert_eq!(r.status(), 404);
    let e: Value = r.json().await.unwrap();
    assert_eq!(e["error"]["code"], "resource_missing");
    assert_eq!(e["error"]["param"], "id");
    assert_eq!(e["error"]["message"], "No such customer: 'cus_missing'");
}

#[tokio::test]
async fn default_page() {
    let m = mock_with("").await;
    let made = make(&m, 12);
    let (s, v) = list(&m, "").await;
    assert_eq!(s, 200);
    assert_eq!(v["object"], "list");
    assert_eq!(v["url"], "/v1/customers");
    assert_eq!(v["has_more"], true);
    assert_eq!(
        ids(&v),
        made.iter().rev().take(10).cloned().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn same_second_forward_walk_is_exact() {
    let m = mock_with("").await;
    let made = make(&m, 25);
    let mut seen = vec![];
    let mut q = "limit=7".to_string();
    loop {
        let (_, v) = list(&m, &q).await;
        seen.extend(ids(&v));
        if v["has_more"] == false {
            break;
        }
        q = format!("limit=7&starting_after={}", seen.last().unwrap());
    }
    assert_eq!(seen, made.iter().rev().cloned().collect::<Vec<_>>());
}

#[tokio::test]
async fn backward_walk() {
    let m = mock_with("").await;
    let made = make(&m, 5); // list order: made[4] .. made[0]
    let (_, v) = list(&m, &format!("limit=2&ending_before={}", made[0])).await;
    assert_eq!(ids(&v), vec![made[2].clone(), made[1].clone()]);
    assert_eq!(v["has_more"], true);
    let (_, v) = list(&m, &format!("limit=2&ending_before={}", made[2])).await;
    assert_eq!(ids(&v), vec![made[4].clone(), made[3].clone()]);
    assert_eq!(v["has_more"], false);
}

#[tokio::test]
async fn cursor_on_deleted_customer_continues() {
    let m = mock_with("").await;
    let made = make(&m, 6);
    let (_, v) = list(&m, "limit=3").await;
    let cursor = ids(&v)[2].clone();
    m.delete_customer(&cursor).unwrap();
    let (s, v) = list(&m, &format!("limit=3&starting_after={cursor}")).await;
    assert_eq!(s, 200);
    assert_eq!(ids(&v), made[..3].iter().rev().cloned().collect::<Vec<_>>());
}

#[tokio::test]
async fn filters() {
    let m = mock_with("").await;
    let a = m
        .create_customer(NewCustomer {
            email: Some("a@example.com".into()),
            ..Default::default()
        })
        .unwrap();
    m.step(Duration::from_secs(10)).unwrap();
    let b = m.create_customer(NewCustomer::default()).unwrap();
    let t = m.now().unix_seconds();
    assert_eq!(
        ids(&list(&m, &format!("created[gte]={t}")).await.1),
        vec![b.clone()]
    );
    assert_eq!(
        ids(&list(&m, &format!("created[lt]={t}")).await.1),
        vec![a.clone()]
    );
    assert_eq!(ids(&list(&m, &format!("created={t}")).await.1), vec![b]);
    assert_eq!(ids(&list(&m, "email=a@example.com").await.1), vec![a]);
}

#[tokio::test]
async fn bad_params() {
    let m = mock_with("").await;
    make(&m, 1);
    for (q, code, param) in [
        ("limit=0", "parameter_invalid_integer", "limit"),
        ("limit=101", "parameter_invalid_integer", "limit"),
        ("limit=abc", "parameter_invalid_integer", "limit"),
        ("created[gt]=x", "parameter_invalid_integer", "created[gt]"),
        ("colour=red", "parameter_unknown", "colour"),
        (
            "starting_after=cus_nope",
            "resource_missing",
            "starting_after",
        ),
    ] {
        let (s, v) = list(&m, q).await;
        assert_eq!(s, 400, "{q}");
        assert_eq!(v["error"]["code"], code, "{q}");
        assert_eq!(v["error"]["param"], param, "{q}");
    }
    assert_eq!(list(&m, "starting_after=a&ending_before=b").await.0, 400);
}
