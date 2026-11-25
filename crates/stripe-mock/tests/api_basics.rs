mod common;

use common::{get, mock_with};
use ductkit_stripe_mock::{SubscribeFilter, SubscriptionItem};
use futures_util::StreamExt;
use serde_json::Value;

#[tokio::test]
async fn auth_required() {
    let m = mock_with("").await;
    let c = reqwest::Client::new();
    let r = c
        .get(format!("{}/v1/customers", m.api_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert!(
        r.headers()["request-id"]
            .to_str()
            .unwrap()
            .starts_with("req_")
    );
    let r = c
        .get(format!("{}/v1/customers", m.api_url()))
        .bearer_auth("sk_test_wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["type"],
        "invalid_request_error"
    );
}

#[tokio::test]
async fn unknown_route_404() {
    let m = mock_with("").await;
    let r = get(&m, "/v1/nope").await;
    assert_eq!(r.status(), 404);
    assert!(
        r.headers()["request-id"]
            .to_str()
            .unwrap()
            .starts_with("req_")
    );
    let body: Value = r.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/v1/nope")
    );
}

#[tokio::test]
async fn writes_rejected() {
    let m = mock_with("").await;
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let r = reqwest::Client::new()
            .request(method, format!("{}/v1/customers", m.api_url()))
            .bearer_auth("sk_test_mock")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
        let body: Value = r.json().await.unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("read-only")
        );
    }
}

#[tokio::test]
async fn version_header() {
    let m = mock_with("").await;
    let c = reqwest::Client::new();
    let send = |v: &'static str| {
        c.get(format!("{}/v1/nope", m.api_url()))
            .bearer_auth("sk_test_mock")
            .header("Stripe-Version", v)
            .send()
    };
    assert_eq!(send("2026-11-18.endive").await.unwrap().status(), 404);
    let r = send("2024-06-20").await.unwrap();
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("2026-11-18.endive"));
}

#[tokio::test]
async fn stripe_account_rejected() {
    let m = mock_with("").await;
    let r = reqwest::Client::new()
        .get(format!("{}/v1/customers", m.api_url()))
        .bearer_auth("sk_test_mock")
        .header("Stripe-Account", "acct_1")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn shutdown_stops_server_and_subscriptions() {
    let m = mock_with("").await;
    let url = m.api_url();
    let mut s = m.subscribe(SubscribeFilter::default());
    assert_eq!(s.next().await, Some(SubscriptionItem::Synced));
    m.shutdown().await;
    assert_eq!(s.next().await, None);
    assert!(reqwest::get(format!("{url}/v1/customers")).await.is_err());
}
