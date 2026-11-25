mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::{DestinationConfig, MockError, PayloadFormat};
use serde_json::{Value, json};

#[tokio::test]
async fn config_and_control_destinations_are_listed() {
    let m = mock_with(
        "[[destinations]]\nname = \"snap\"\nurl = \"http://127.0.0.1:9/a\"\npayload = \"snapshot\"\nenabled_events = [\"*\"]\nsecret = \"whsec_fixed\"",
    )
    .await;
    let thin = m
        .add_destination(DestinationConfig {
            name: "thin".into(),
            url: "http://127.0.0.1:9/b".into(),
            payload: PayloadFormat::Thin,
            enabled_events: vec!["v1.customer.*".into()],
            connect: false,
            events_from: None,
            secret: None,
        })
        .unwrap();
    assert!(thin.id.starts_with("ed_test_") && thin.secret.starts_with("whsec_"));
    assert_eq!(m.destinations()[0].secret, "whsec_fixed");
    assert!(m.destinations()[0].id.starts_with("we_"));
    let v1: Value = get(&m, "/v1/webhook_endpoints").await.json().await.unwrap();
    assert_eq!(v1["object"], "list");
    assert_eq!(v1["data"][0]["object"], "webhook_endpoint");
    assert_eq!(v1["data"][0]["url"], "http://127.0.0.1:9/a");
    assert!(!v1.to_string().contains("whsec_"));
    let v2: Value = get(&m, "/v2/core/event_destinations")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v2["data"][0]["event_payload"], "thin");
    assert_eq!(v2["data"][0]["events_from"], json!(["@self"]));
    assert!(!v2.to_string().contains("whsec_"));
    m.set_destination_enabled(&thin.id, false).unwrap();
    let v2: Value = get(&m, "/v2/core/event_destinations")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v2["data"][0]["status"], "disabled");
    m.remove_destination(&thin.id).unwrap();
    assert!(matches!(
        m.remove_destination(&thin.id),
        Err(MockError::NoSuchDestination(_))
    ));
    assert!(
        m.add_destination(DestinationConfig {
            name: "bad".into(),
            url: "https://x".into(),
            payload: PayloadFormat::Snapshot,
            enabled_events: vec!["*".into()],
            connect: false,
            events_from: None,
            secret: None,
        })
        .is_err()
    );
}

#[tokio::test]
async fn rotate_keeps_old_secret_for_overlap() {
    let m = mock_with(
        "[[destinations]]\nname = \"s\"\nurl = \"http://127.0.0.1:9/a\"\npayload = \"snapshot\"\nenabled_events = [\"*\"]\nsecret = \"whsec_old\"",
    )
    .await;
    let id = m.destinations()[0].id.clone();
    let new = m.rotate_secret(&id, Duration::from_secs(60)).unwrap();
    assert_ne!(new, "whsec_old");
    assert!(new.starts_with("whsec_"));
    assert_eq!(m.destinations()[0].secret, new);
}
