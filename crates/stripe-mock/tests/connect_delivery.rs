//! Connect-aware destinations (spec sections 4, 5.4 and 9).

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::{Receiver, mock_with, real_now};
use ductkit_stripe_mock::{
    AccountApi, AttemptOutcome, DeliveryRecordKind, MockStripe, NewCustomer, verify_signature,
};
use serde_json::Value;

const BASE: &str = "[actions]\nnew_customer = 20\n[accounts]\nconnect = true\nconnected_accounts = { initial = 1 }\n";

fn ok() -> Receiver {
    Receiver::start(|_| (200, Duration::ZERO))
}

fn block(name: &str, rx: &Receiver, payload: &str, extra: &str) -> String {
    format!(
        "[[destinations]]\nname = \"{name}\"\nurl = \"{}\"\npayload = \"{payload}\"\nenabled_events = [\"*\"]\nsecret = \"whsec_test\"\n{extra}\n",
        rx.url("/h")
    )
}

fn bodies(rx: &Receiver) -> Vec<Value> {
    rx.requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn types(bodies: &[Value]) -> Vec<&str> {
    bodies.iter().map(|b| b["type"].as_str().unwrap()).collect()
}

/// Event ID to the event's Connect label.
fn labels(m: &MockStripe) -> HashMap<String, Option<String>> {
    m.event_timeline()
        .into_iter()
        .map(|e| (e.id, e.account))
        .collect()
}

#[tokio::test]
async fn deliveries_route_by_account() {
    let (plain, conn, thin_self, thin_both, thin_accts) = (ok(), ok(), ok(), ok(), ok());
    let toml = format!(
        "{BASE}{}{}{}{}{}",
        block("snap_plain", &plain, "snapshot", ""),
        block("snap_connect", &conn, "snapshot", "connect = true"),
        block("thin_self", &thin_self, "thin", "events_from = [\"@self\"]"),
        block(
            "thin_both",
            &thin_both,
            "thin",
            "events_from = [\"@self\", \"@accounts\"]"
        ),
        block(
            "thin_accts",
            &thin_accts,
            "thin",
            "events_from = [\"@accounts\"]"
        ),
    );
    let m = mock_with(&toml).await;
    m.connect_account(AccountApi::V2).unwrap();
    m.connect_account(AccountApi::V1).unwrap();
    m.step(Duration::from_secs(2 * 3600)).await.unwrap();
    m.quiesce().await.unwrap();
    let labels = labels(&m);

    let plain = bodies(&plain);
    assert!(!plain.is_empty());
    for b in &plain {
        assert!(b.get("account").is_none(), "{b}");
        assert_eq!(labels[b["id"].as_str().unwrap()], None, "{b}");
    }

    let conn = bodies(&conn);
    assert!(conn.iter().any(|b| b.get("account").is_none()));
    assert!(conn.iter().any(|b| b.get("account").is_some()));
    for b in &conn {
        let want = labels[b["id"].as_str().unwrap()].as_deref();
        assert_eq!(b.get("account").and_then(Value::as_str), want, "{b}");
    }
    assert!(conn.len() > plain.len());

    let only_self = bodies(&thin_self);
    assert!(!only_self.is_empty());
    assert!(only_self.iter().all(|b| b["context"].is_null()), "self");
    assert!(types(&only_self).contains(&"v2.core.account.created"));

    let both = bodies(&thin_both);
    assert!(both.iter().any(|b| b["context"].is_null()));
    assert!(both.iter().any(|b| !b["context"].is_null()));
    for b in &both {
        let want = labels[b["id"].as_str().unwrap()].as_deref();
        assert_eq!(b["context"].as_str(), want, "{b}");
    }
    assert!(types(&both).contains(&"v2.core.account.created"));

    let accts = bodies(&thin_accts);
    assert!(!accts.is_empty());
    assert!(accts.iter().all(|b| !b["context"].is_null()), "accounts");
    assert!(
        !types(&accts)
            .iter()
            .any(|t| t.starts_with("v2.core.account")),
        "v2.core.account.* has no account context"
    );
}

#[tokio::test]
async fn account_lifecycle_delivered() {
    let (conn, thin) = (ok(), ok());
    let toml = format!(
        "{BASE}{}{}",
        block("snap_connect", &conn, "snapshot", "connect = true"),
        block("thin", &thin, "thin", "events_from = [\"@accounts\"]"),
    );
    let m = mock_with(&toml).await;
    let id = m.connect_account(AccountApi::V1).unwrap();
    m.step(Duration::from_secs(60)).await.unwrap();
    m.disconnect_account(&id).unwrap();
    m.step(Duration::from_secs(60)).await.unwrap();
    m.quiesce().await.unwrap();

    let snaps = bodies(&conn);
    let find = |t: &str| {
        snaps
            .iter()
            .find(|b| b["type"] == t && b["account"] == id.as_str())
            .unwrap_or_else(|| panic!("no {t} for {id}"))
    };
    find("account.application.authorized");
    let deauth = find("account.application.deauthorized");
    assert_eq!(deauth["data"]["object"]["object"], "application");

    let thins = bodies(&thin);
    for t in [
        "v1.account.application.authorized",
        "v1.account.application.deauthorized",
    ] {
        assert!(
            thins
                .iter()
                .any(|b| b["type"] == t && b["context"] == id.as_str()),
            "no {t} for {id}"
        );
    }
}

#[tokio::test]
async fn connected_deliveries_signed_and_retried() {
    let rx = ok();
    let toml = format!(
        "{BASE}{}[webhooks]\nretry_schedule = [\"1m\"]\n",
        block("snap_connect", &rx, "snapshot", "connect = true"),
    );
    let m = mock_with(&toml).await;
    let dest_id = m.destinations()[0].id.clone();
    m.start_outage(&dest_id, Duration::from_secs(30)).unwrap();
    let acct = m.connect_account(AccountApi::V1).unwrap();
    m.account(&acct)
        .create_customer(NewCustomer::default())
        .unwrap();
    m.step(Duration::from_secs(300)).await.unwrap();
    m.quiesce().await.unwrap();
    let labels = labels(&m);
    let log = m.delivery_log();
    let account_events: Vec<&String> = log
        .iter()
        .filter(|r| {
            labels
                .get(&r.event_id)
                .is_some_and(|a| a.as_deref() == Some(&*acct))
        })
        .map(|r| &r.event_id)
        .collect();
    assert!(!account_events.is_empty());
    let outage_then_ok = account_events.iter().any(|e| {
        let outcomes: Vec<&AttemptOutcome> = log
            .iter()
            .filter(|r| &r.event_id == *e)
            .filter_map(|r| match &r.kind {
                DeliveryRecordKind::Attempt { outcome, .. } => Some(outcome),
                _ => None,
            })
            .collect();
        outcomes.first() == Some(&&AttemptOutcome::Outage)
            && outcomes.last() == Some(&&AttemptOutcome::Success { status: 200 })
    });
    assert!(
        outage_then_ok,
        "no connected event was retried after the outage"
    );
    let got = rx.requests();
    assert!(!got.is_empty());
    for r in &got {
        let sig = r.headers["stripe-signature"].to_str().unwrap();
        assert!(verify_signature(
            sig,
            &r.body,
            "whsec_test",
            300,
            real_now()
        ));
    }
    let accounts_seen = bodies(&rx)
        .iter()
        .filter(|b| b["account"] == acct.as_str())
        .count();
    assert!(accounts_seen > 0);
}
