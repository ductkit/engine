mod common;

use std::time::Duration;

use common::{Receiver, dest, get, mock_with, real_now};
use ductkit_stripe_mock::{
    AttemptOutcome, CustomerUpdate, DeliveryRecordKind, NewCustomer, verify_signature,
};
use serde_json::Value;

fn ok() -> Receiver {
    Receiver::start(|_| (200, Duration::ZERO))
}

#[tokio::test]
async fn snapshot_delivery_is_signed_with_real_time() {
    let rx = ok();
    let m = mock_with(&dest(&rx.url("/hook"), "snapshot", "*")).await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(1, Duration::from_secs(5)).await;
    let body = &got[0].body;
    let ev: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(ev["type"], "customer.created");
    assert_eq!(ev["pending_webhooks"], 1);
    let sig = got[0].headers["stripe-signature"].to_str().unwrap();
    assert!(verify_signature(sig, body, "whsec_test", 300, real_now()));
    assert_eq!(
        got[0].headers["content-type"],
        "application/json; charset=utf-8"
    );
    let log = m.delivery_log();
    assert!(matches!(
        log.last().unwrap().kind,
        DeliveryRecordKind::Attempt {
            outcome: AttemptOutcome::Success { status: 200 },
            ..
        }
    ));
}

#[tokio::test]
async fn thin_delivery_is_a_notification() {
    let rx = ok();
    let m = mock_with(&dest(&rx.url("/thin"), "thin", "v1.customer.*")).await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(1, Duration::from_secs(5)).await;
    let ev: Value = serde_json::from_slice(&got[0].body).unwrap();
    assert_eq!(ev["object"], "v2.core.event");
    assert!(ev.get("snapshot_event").is_none());
    assert_eq!(ev["related_object"]["type"], "customer");
}

#[tokio::test]
async fn delay_is_simulated_and_pending_counts() {
    let rx = ok();
    let m = mock_with(&format!(
        "{}[webhooks]\ndelivery_delay = {{ distribution = \"fixed\", ms = 30000 }}",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(29)).await.unwrap();
    assert!(rx.requests().is_empty());
    let pending: Value = get(&m, "/v1/events?delivery_success=false")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(pending["data"][0]["pending_webhooks"], 1);
    m.step(Duration::from_secs(1)).await.unwrap();
    assert_eq!(rx.wait_for(1, Duration::from_secs(5)).await.len(), 1);
    let done: Value = get(&m, "/v1/events?delivery_success=true")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(done["data"].as_array().unwrap().len(), 1);
    assert_eq!(done["data"][0]["pending_webhooks"], 0);
}

#[tokio::test]
async fn filtered_events_are_not_delivered() {
    let rx = ok();
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "customer.deleted")).await;
    let id = m.create_customer(NewCustomer::default()).unwrap();
    m.delete_customer(&id).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(1, Duration::from_secs(5)).await;
    assert_eq!(got.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&got[0].body).unwrap()["type"],
        "customer.deleted"
    );
}

#[tokio::test]
async fn generated_traffic_is_delivered_in_step() {
    let rx = ok();
    let m = mock_with(&format!(
        "seed = 1\n{}[actions]\nnew_customer = 60.0",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.step(Duration::from_secs(3600)).await.unwrap();
    let created = m.truth_log().len();
    assert!(created > 30, "{created}");
    assert_eq!(
        rx.wait_for(created, Duration::from_secs(10)).await.len(),
        created
    );
}

#[tokio::test]
async fn same_instant_events_arrive_in_order() {
    // A slow receiver: if attempts to one destination raced, all three would
    // arrive at once, in any order.
    let rx = Receiver::start(|_| (200, Duration::from_millis(100)));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    let id = m.create_customer(NewCustomer::default()).unwrap();
    m.update_customer(&id, CustomerUpdate::SetName(Some("Ada".into())))
        .unwrap();
    m.delete_customer(&id).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(3, Duration::from_secs(5)).await;
    let types: Vec<String> = got
        .iter()
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        types,
        ["customer.created", "customer.updated", "customer.deleted"]
    );
    for w in got.windows(2) {
        assert!(
            w[1].at - w[0].at >= Duration::from_millis(90),
            "sent one at a time"
        );
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    use axum::http::{HeaderValue, StatusCode, header::LOCATION};
    let target = ok();
    let to = target.url("/ok");
    let app = axum::Router::new().fallback(move || {
        let to = to.clone();
        async move {
            (
                StatusCode::TEMPORARY_REDIRECT,
                [(LOCATION, HeaderValue::from_str(&to).unwrap())],
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/r", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let m = mock_with(&format!(
        "{}[webhooks]\nretry_schedule = [\"1h\"]",
        dest(&url, "snapshot", "*")
    ))
    .await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    assert!(target.requests().is_empty());
    assert!(m.delivery_log().iter().any(|r| matches!(
        r.kind,
        DeliveryRecordKind::Attempt {
            outcome: AttemptOutcome::HttpError { status: 307 },
            ..
        }
    )));
}
