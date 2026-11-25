mod common;

use std::time::Duration;

use common::{Receiver, dest, mock_with};
use ductkit_stripe_mock::{
    AbandonReason, AttemptOutcome, DeliveryRecord, DeliveryRecordKind, NewCustomer,
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
