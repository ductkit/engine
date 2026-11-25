mod common;

use std::time::Duration;

use common::{Receiver, dest, get, mock_with};
use ductkit_stripe_mock::{AbandonReason, DeliveryRecordKind, Distribution, NewCustomer};
use serde_json::Value;

#[tokio::test]
async fn quiesce_flushes_lag_and_deliveries() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!(
        "{}[events]\nvisibility_lag = {{ distribution = \"fixed\", ms = 600000 }}\n[webhooks]\ndelivery_delay = {{ distribution = \"fixed\", ms = 600000 }}\n[actions]\nnew_customer = 60.0",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.step(Duration::from_secs(600)).await.unwrap();
    let t = m.now();
    m.quiesce().await.unwrap();
    assert_eq!(m.now(), t);
    assert!(!m.generation_enabled());
    let n = m.truth_log().len();
    assert!(n > 0);
    assert_eq!(rx.wait_for(n, Duration::from_secs(10)).await.len(), n);
    let listed: Value = get(&m, "/v1/events?limit=100").await.json().await.unwrap();
    assert_eq!(listed["data"].as_array().unwrap().len(), n.min(100));
    m.step(Duration::from_secs(3600)).await.unwrap();
    assert_eq!(m.truth_log().len(), n);
    m.resume_generation();
    assert!(m.generation_enabled());
    m.step(Duration::from_secs(3600)).await.unwrap();
    assert!(m.truth_log().len() > n);
}

#[tokio::test]
async fn quiesce_exhausts_failing_retries_without_waiting() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await; // stripe_like, 3d
    m.create_customer(NewCustomer::default()).unwrap();
    let started = std::time::Instant::now();
    m.quiesce().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(rx.requests().len(), 12); // 1 + 5 + 6 retries within 3 days
    assert!(matches!(
        m.delivery_log().last().unwrap().kind,
        DeliveryRecordKind::Abandoned {
            reason: AbandonReason::RetriesExhausted
        }
    ));
}

#[tokio::test]
async fn runtime_webhook_settings_apply_to_new_events() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    m.update_webhooks_config(|w| w.delivery_delay = Distribution::Fixed { ms: 60_000 })
        .unwrap();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(59)).await.unwrap();
    assert!(rx.requests().is_empty());
    m.step(Duration::from_secs(1)).await.unwrap();
    assert_eq!(rx.wait_for(1, Duration::from_secs(5)).await.len(), 1);
    assert!(
        m.update_webhooks_config(|w| w.duplicate_probability = 2.0)
            .is_err()
    );
}

#[tokio::test]
async fn wall_mode_delivers() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!(
        "clock = {{ mode = \"wall\", speed = 1.0 }}\n{}",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.create_customer(NewCustomer::default()).unwrap();
    assert_eq!(rx.wait_for(1, Duration::from_secs(5)).await.len(), 1);
}

#[tokio::test]
async fn cancelled_step_does_not_lose_the_attempt() {
    let rx = Receiver::start(|_| (200, Duration::from_millis(1500)));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    m.create_customer(NewCustomer::default()).unwrap();
    let cancelled =
        tokio::time::timeout(Duration::from_millis(200), m.step(Duration::from_millis(1))).await;
    assert!(cancelled.is_err(), "step should still be sending");
    tokio::time::timeout(Duration::from_secs(10), m.quiesce())
        .await
        .expect("quiesce finishes")
        .unwrap();
    assert!(m.delivery_log().iter().any(|r| matches!(
        r.kind,
        DeliveryRecordKind::Attempt {
            outcome: ductkit_stripe_mock::AttemptOutcome::Success { .. },
            ..
        }
    )));
}

#[tokio::test]
async fn cancelled_quiesce_ends_quiesce_mode() {
    let rx = Receiver::start(|_| (500, Duration::from_millis(800)));
    let m = mock_with(&format!(
        "{}[webhooks]\nretry_schedule = [\"1m\"]",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.create_customer(NewCustomer::default()).unwrap();
    let cancelled = tokio::time::timeout(Duration::from_millis(200), m.quiesce()).await;
    assert!(cancelled.is_err(), "quiesce should still be sending");
    tokio::time::sleep(Duration::from_millis(1500)).await; // the in-flight attempt finishes
    m.step(Duration::from_millis(1)).await.unwrap();
    // Out of quiesce mode the retry waits its minute.
    assert_eq!(rx.requests().len(), 1);
}

#[tokio::test]
async fn quiesce_keeps_the_nominal_retry_window() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&format!(
        "{}[webhooks]\ndelivery_delay = {{ distribution = \"fixed\", ms = 600000 }}\nmax_retry_duration = \"10m\"\nretry_schedule = [\"1m\"]",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.quiesce().await.unwrap();
    // As under `step`: the first attempt is at +10m and the retry at +11m is within 10m of it.
    assert_eq!(rx.requests().len(), 2);
}
