mod common;

use std::time::Duration;

use common::{Receiver, dest, mock_with, real_now};
use ductkit_stripe_mock::{
    AbandonReason, AttemptOutcome, DeliveryRecord, DeliveryRecordKind, MockStripe, NewCustomer,
    SignatureFault, verify_signature,
};
use serde_json::Value;

fn outcomes(log: Vec<DeliveryRecord>) -> Vec<AttemptOutcome> {
    log.into_iter()
        .filter_map(|r| match r.kind {
            DeliveryRecordKind::Attempt { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn failing_endpoint_retries_on_schedule_then_gives_up() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&format!(
        "{}[webhooks]\nretry_schedule = [\"1m\", \"5m\"]",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(3600)).await.unwrap();
    assert_eq!(rx.wait_for(3, Duration::from_secs(5)).await.len(), 3);
    let attempts: Vec<_> = m
        .delivery_log()
        .into_iter()
        .filter_map(|r| match r.kind {
            DeliveryRecordKind::Attempt { attempt, .. } => Some((attempt, r.at)),
            _ => None,
        })
        .collect();
    let t0 = attempts[0].1;
    assert_eq!(
        attempts,
        vec![(1, t0), (2, t0.plus_ms(60_000)), (3, t0.plus_ms(360_000))]
    );
    assert!(matches!(
        m.delivery_log().last().unwrap().kind,
        DeliveryRecordKind::Abandoned {
            reason: AbandonReason::RetriesExhausted
        }
    ));
}

#[tokio::test]
async fn slow_200_counts_as_timeout_and_is_retried() {
    let rx = Receiver::start(|i| {
        (
            200,
            if i == 0 {
                Duration::from_millis(1500)
            } else {
                Duration::ZERO
            },
        )
    });
    let m = mock_with(&format!(
        "{}[webhooks]\nresponse_timeout = \"1s\"\nretry_schedule = [\"1m\"]",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(120)).await.unwrap();
    let got = rx.wait_for(2, Duration::from_secs(5)).await;
    let id = |b: &[u8]| serde_json::from_slice::<Value>(b).unwrap()["id"].clone();
    assert_eq!(
        id(&got[0].body),
        id(&got[1].body),
        "same event delivered twice"
    );
    assert_eq!(
        outcomes(m.delivery_log()),
        vec![
            AttemptOutcome::Timeout,
            AttemptOutcome::Success { status: 200 }
        ]
    );
}

#[tokio::test]
async fn disabling_abandons_pending_and_stops_new() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    let id = m.destinations()[0].id.clone();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap(); // first attempt fails, retry queued
    m.set_destination_enabled(&id, false).unwrap();
    m.create_customer(NewCustomer::default()).unwrap(); // not queued
    m.step(Duration::from_secs(86_400)).await.unwrap();
    assert_eq!(rx.requests().len(), 1);
    let log = m.delivery_log();
    assert!(log.iter().any(|r| matches!(
        r.kind,
        DeliveryRecordKind::Abandoned {
            reason: AbandonReason::DestinationDisabled
        }
    )));
    assert_eq!(
        log.iter()
            .filter(|r| matches!(r.kind, DeliveryRecordKind::Scheduled { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn removing_abandons_pending() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    let id = m.destinations()[0].id.clone();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    m.remove_destination(&id).unwrap();
    m.step(Duration::from_secs(86_400)).await.unwrap();
    assert_eq!(rx.requests().len(), 1);
    assert!(m.delivery_log().iter().any(|r| matches!(
        r.kind,
        DeliveryRecordKind::Abandoned {
            reason: AbandonReason::DestinationRemoved
        }
    )));
}

#[tokio::test]
async fn persistent_failure_disables_destination() {
    let rx = Receiver::start(|_| (500, Duration::ZERO));
    let m = mock_with(&format!(
        "{}[webhooks]\ndisable_after_failures = \"1h\"\nretry_schedule = [\"30m\", \"30m\", \"30m\"]",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(4 * 3600)).await.unwrap();
    assert!(!m.destinations()[0].enabled);
    assert_eq!(
        m.delivery_log()
            .iter()
            .filter(|r| matches!(r.kind, DeliveryRecordKind::DestinationDisabled))
            .count(),
        1
    );
}

#[tokio::test]
async fn outage_fails_without_http_then_recovers() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!(
        "{}[webhooks]\nretry_schedule = [\"1m\"]",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    let id = m.destinations()[0].id.clone();
    m.start_outage(&id, Duration::from_secs(30)).unwrap();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(120)).await.unwrap();
    assert_eq!(rx.wait_for(1, Duration::from_secs(5)).await.len(), 1);
    assert_eq!(
        outcomes(m.delivery_log()),
        vec![
            AttemptOutcome::Outage,
            AttemptOutcome::Success { status: 200 }
        ]
    );
}

fn ok() -> Receiver {
    Receiver::start(|_| (200, Duration::ZERO))
}

fn event_id(b: &[u8]) -> Value {
    serde_json::from_slice::<Value>(b).unwrap()["id"].clone()
}

#[tokio::test]
async fn duplicates_deliver_same_event_twice() {
    let rx = ok();
    let m = mock_with(&format!(
        "{}[webhooks]\nduplicate_probability = 1.0",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(2, Duration::from_secs(5)).await;
    assert_eq!(event_id(&got[0].body), event_id(&got[1].body));
    assert_eq!(
        m.delivery_log()
            .iter()
            .filter(|r| matches!(
                r.kind,
                DeliveryRecordKind::Scheduled {
                    duplicate: true,
                    ..
                }
            ))
            .count(),
        1
    );
}

#[tokio::test]
async fn reorder_window_can_swap_deliveries() {
    let rx = ok();
    let m = mock_with(&format!(
        "seed = 11\n{}[webhooks]\nreorder_window = \"10s\"",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    let mut made = vec![];
    for _ in 0..20 {
        made.push(m.create_customer(NewCustomer::default()).unwrap());
        m.step(Duration::from_millis(500)).await.unwrap();
    }
    m.step(Duration::from_secs(20)).await.unwrap();
    let got: Vec<String> = rx
        .wait_for(20, Duration::from_secs(5))
        .await
        .iter()
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["data"]["object"]["id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(got.len(), 20);
    assert_ne!(got, made, "seed 11 should reorder at least one pair");
    let mut sorted = got.clone();
    sorted.sort();
    let mut made_sorted = made.clone();
    made_sorted.sort();
    assert_eq!(sorted, made_sorted);
}

#[tokio::test]
async fn skewed_and_invalid_signatures_fail_verification() {
    for (fault, expect_skew) in [
        ("clock_skew = { probability = 1.0, max = \"10m\" }", true),
        ("invalid_signature_probability = 1.0", false),
    ] {
        let rx = ok();
        let m = mock_with(&format!(
            "{}[webhooks]\n{fault}",
            dest(&rx.url("/h"), "snapshot", "*")
        ))
        .await;
        m.create_customer(NewCustomer::default()).unwrap();
        m.step(Duration::from_millis(1)).await.unwrap();
        let r = rx.wait_for(1, Duration::from_secs(5)).await.remove(0);
        let header = r.headers["stripe-signature"].to_str().unwrap();
        assert!(
            !verify_signature(header, &r.body, "whsec_test", 300, real_now()),
            "{fault}"
        );
        let fault_logged = m
            .delivery_log()
            .iter()
            .find_map(|r| match &r.kind {
                DeliveryRecordKind::Attempt { signature, .. } => Some(signature.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            matches!(fault_logged, SignatureFault::Skewed { .. }),
            expect_skew,
            "{fault}"
        );
    }
}

#[tokio::test]
async fn rotated_secret_signs_with_both_during_overlap() {
    let rx = ok();
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    let id = m.destinations()[0].id.clone();
    let new = m.rotate_secret(&id, Duration::from_secs(60)).unwrap();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    m.step(Duration::from_secs(61)).await.unwrap();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_millis(1)).await.unwrap();
    let got = rx.wait_for(2, Duration::from_secs(5)).await;
    let now = real_now();
    let sig = |i: usize| {
        got[i].headers["stripe-signature"]
            .to_str()
            .unwrap()
            .to_string()
    };
    assert!(verify_signature(
        &sig(0),
        &got[0].body,
        "whsec_test",
        300,
        now
    ));
    assert!(verify_signature(&sig(0), &got[0].body, &new, 300, now));
    assert!(!verify_signature(
        &sig(1),
        &got[1].body,
        "whsec_test",
        300,
        now
    ));
    assert!(verify_signature(&sig(1), &got[1].body, &new, 300, now));
}

#[tokio::test]
async fn delivery_decisions_are_deterministic() {
    let cfg = |url: &str| {
        format!(
            "seed = 9\n{}[actions]\nnew_customer = 30.0\n[webhooks]\nduplicate_probability = 0.2\nreorder_window = \"5s\"\ndelivery_delay = {{ distribution = \"lognormal\", median_ms = 300, p99_ms = 10000 }}",
            dest(url, "snapshot", "*")
        )
    };
    let scheduled = |m: &MockStripe| {
        m.delivery_log()
            .into_iter()
            .filter(|r| matches!(r.kind, DeliveryRecordKind::Scheduled { .. }))
            .map(|r| (r.event_id, r.kind))
            .collect::<Vec<_>>()
    };
    let (r1, r2) = (ok(), ok());
    let a = mock_with(&cfg(&r1.url("/h"))).await;
    let b = mock_with(&cfg(&r2.url("/h"))).await;
    a.step(Duration::from_secs(7200)).await.unwrap();
    b.step(Duration::from_secs(7200)).await.unwrap();
    assert!(!scheduled(&a).is_empty());
    assert_eq!(scheduled(&a), scheduled(&b));
}
