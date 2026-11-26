//! Checkout sessions: the `checkout` action in payment and subscription mode, completion
//! and expiry (spec sections 4, 5.5 and 11).

mod common;

use std::time::Duration;

use common::{get, mock_with, recurring_prices};
use ductkit_stripe_mock::{
    Action, Cause, CheckoutMode, EventFormat, MockError, MockStripe, NewCustomer, ObjectType,
    StateUpdate,
};
use serde_json::Value;

const MINUTE_MS: i64 = 60_000;
const DAY_SECS: i64 = 86_400;

fn caused(m: &MockStripe, cause: &Cause) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(cause))
        .collect()
}

fn ids_of(updates: &[StateUpdate], t: ObjectType) -> Vec<String> {
    let mut ids: Vec<String> = updates
        .iter()
        .filter(|u| u.object_type == t)
        .map(|u| u.object_id.clone())
        .collect();
    ids.dedup();
    ids
}

fn only_id(updates: &[StateUpdate], t: ObjectType) -> String {
    let ids = ids_of(updates, t);
    assert_eq!(ids.len(), 1, "{t:?}: {ids:?}");
    ids[0].clone()
}

async fn json(m: &MockStripe, path: &str) -> Value {
    let r = get(m, path).await;
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

/// The snapshot event types of `cause`, in order.
fn event_types(m: &MockStripe, cause: &Cause) -> Vec<String> {
    let seqs: Vec<u64> = caused(m, cause).iter().map(|u| u.seq).collect();
    m.event_timeline()
        .into_iter()
        .filter(|e| seqs.contains(&e.truth_seq) && e.format == EventFormat::Snapshot)
        .map(|e| e.event_type)
        .collect()
}

/// Fetches every snapshot event of `cause` through the API, so each payload is checked.
async fn fetch_events(m: &MockStripe, cause: &Cause) {
    let seqs: Vec<u64> = caused(m, cause).iter().map(|u| u.seq).collect();
    for e in m.event_timeline() {
        if seqs.contains(&e.truth_seq) && e.format == EventFormat::Snapshot {
            json(m, &format!("/v1/events/{}", e.id)).await;
        }
    }
}

fn checkout(
    m: &MockStripe,
    customer: &str,
    mode: CheckoutMode,
    complete: bool,
) -> Result<Cause, MockError> {
    m.run_action(Action::Checkout {
        customer: Some(customer.to_string()),
        mode: Some(mode),
        complete: Some(complete),
    })
}

/// The session's truth-log entries, oldest first.
fn history(m: &MockStripe, id: &str) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::CheckoutSession && u.object_id == id)
        .collect()
}

#[tokio::test]
async fn checkout_payment_mode_completes() {
    let m = mock_with("seed = 21").await;
    let cus = m
        .create_customer(NewCustomer {
            email: Some("pat@example.com".to_string()),
            ..NewCustomer::default()
        })
        .unwrap();
    let cause = checkout(&m, &cus, CheckoutMode::Payment, true).unwrap();
    let cs = only_id(&caused(&m, &cause), ObjectType::CheckoutSession);
    assert!(cs.starts_with("cs_test_"), "{cs}");
    assert!(event_types(&m, &cause).is_empty(), "no event on creation");
    let open = json(&m, &format!("/v1/checkout/sessions/{cs}")).await;
    assert_eq!(open["status"], "open");
    assert_eq!(open["mode"], "payment");
    assert_eq!(open["payment_status"], "unpaid");
    assert_eq!(open["customer"], cus.as_str());
    assert_eq!(open["payment_intent"], Value::Null);
    assert_eq!(
        open["expires_at"].as_i64().unwrap(),
        open["created"].as_i64().unwrap() + DAY_SECS
    );
    assert!(open["url"].as_str().unwrap().contains(&cs), "{open}");
    assert!(open["amount_total"].as_i64().unwrap() > 0);

    m.step(Duration::from_secs(31 * 60)).await.unwrap();
    let h = history(&m, &cs);
    assert_eq!(h.len(), 2, "{h:?}");
    let after = h[1].at.0 - h[0].at.0;
    assert!((MINUTE_MS..=30 * MINUTE_MS).contains(&after), "{after}");
    let types = event_types(&m, &cause);
    let payment: Vec<&str> = types
        .iter()
        .map(String::as_str)
        .filter(|t| !t.starts_with("payment_method."))
        .collect();
    assert_eq!(
        payment,
        [
            "payment_intent.created",
            "charge.succeeded",
            "payment_intent.succeeded",
            "checkout.session.completed"
        ]
    );
    fetch_events(&m, &cause).await;
    let pi = only_id(&caused(&m, &cause), ObjectType::PaymentIntent);
    let done = json(&m, &format!("/v1/checkout/sessions/{cs}")).await;
    assert_eq!(done["status"], "complete");
    assert_eq!(done["payment_status"], "paid");
    assert_eq!(done["payment_intent"], pi.as_str());
    assert_eq!(done["subscription"], Value::Null);
    assert_eq!(done["url"], Value::Null);
    assert_eq!(done["customer_details"]["email"], "pat@example.com");
    let intent = json(&m, &format!("/v1/payment_intents/{pi}")).await;
    assert_eq!(intent["status"], "succeeded");
    assert_eq!(intent["amount"], done["amount_total"]);
    assert_eq!(intent["currency"], done["currency"]);
    assert_eq!(intent["customer"], cus.as_str());
    let expanded = json(
        &m,
        &format!("/v1/checkout/sessions/{cs}?expand[]=payment_intent"),
    )
    .await;
    assert_eq!(expanded["payment_intent"]["id"], pi.as_str());
    let by_pi = json(&m, &format!("/v1/checkout/sessions?payment_intent={pi}")).await;
    assert_eq!(by_pi["data"].as_array().unwrap().len(), 1);
    // The session never expires once complete.
    m.step(Duration::from_secs(2 * DAY_SECS as u64))
        .await
        .unwrap();
    assert_eq!(history(&m, &cs).len(), 2);
}

#[tokio::test]
async fn checkout_subscription_mode_starts_subscription() {
    let m = mock_with("seed = 22").await;
    recurring_prices(&m, 1, "month");
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    let cause = checkout(&m, &cus, CheckoutMode::Subscription, true).unwrap();
    let cs = only_id(&caused(&m, &cause), ObjectType::CheckoutSession);
    let open = json(&m, &format!("/v1/checkout/sessions/{cs}")).await;
    assert_eq!(open["mode"], "subscription");
    assert_eq!(open["subscription"], Value::Null);
    assert_eq!(open["invoice"], Value::Null);
    m.step(Duration::from_secs(31 * 60)).await.unwrap();
    let mine = caused(&m, &cause);
    let sub = only_id(&mine, ObjectType::Subscription);
    let inv = only_id(&mine, ObjectType::Invoice);
    let done = json(&m, &format!("/v1/checkout/sessions/{cs}")).await;
    assert_eq!(done["status"], "complete");
    assert_eq!(done["payment_status"], "paid");
    assert_eq!(done["subscription"], sub.as_str());
    assert_eq!(done["invoice"], inv.as_str());
    assert_eq!(done["payment_intent"], Value::Null);
    let s = json(&m, &format!("/v1/subscriptions/{sub}")).await;
    assert_eq!(s["status"], "active");
    assert_eq!(s["customer"], cus.as_str());
    assert_eq!(s["latest_invoice"], inv.as_str());
    let i = json(&m, &format!("/v1/invoices/{inv}")).await;
    assert_eq!(i["status"], "paid");
    assert_eq!(i["amount_paid"], done["amount_total"]);
    let types = event_types(&m, &cause);
    let pos = |t: &str| {
        types
            .iter()
            .position(|x| x == t)
            .unwrap_or_else(|| panic!("{t} in {types:?}"))
    };
    assert!(pos("customer.subscription.created") < pos("invoice.finalized"));
    assert!(pos("invoice.finalized") < pos("invoice.paid"));
    assert_eq!(types.last().unwrap(), "checkout.session.completed");
    fetch_events(&m, &cause).await;
}

#[tokio::test]
async fn checkout_subscription_mode_falls_back_without_recurring_price() {
    let m = mock_with("seed = 23").await;
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    let cause = checkout(&m, &cus, CheckoutMode::Subscription, true).unwrap();
    let cs = only_id(&caused(&m, &cause), ObjectType::CheckoutSession);
    let open = json(&m, &format!("/v1/checkout/sessions/{cs}")).await;
    assert_eq!(open["mode"], "payment");
    m.step(Duration::from_secs(31 * 60)).await.unwrap();
    assert!(ids_of(&caused(&m, &cause), ObjectType::Subscription).is_empty());
    let done = json(&m, &format!("/v1/checkout/sessions/{cs}")).await;
    assert_eq!(done["status"], "complete");
}

#[tokio::test]
async fn checkout_expires_after_24h() {
    let m = mock_with("seed = 24").await;
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    let cause = checkout(&m, &cus, CheckoutMode::Payment, false).unwrap();
    let cs = only_id(&caused(&m, &cause), ObjectType::CheckoutSession);
    m.step(Duration::from_secs(23 * 3600)).await.unwrap();
    assert_eq!(history(&m, &cs).len(), 1, "still open");
    assert!(event_types(&m, &cause).is_empty());
    m.step(Duration::from_secs(3600)).await.unwrap();
    let h = history(&m, &cs);
    assert_eq!(h.len(), 2);
    assert_eq!(h[1].at.0 - h[0].at.0, DAY_SECS * 1000);
    assert_eq!(event_types(&m, &cause), ["checkout.session.expired"]);
    fetch_events(&m, &cause).await;
    let s = json(&m, &format!("/v1/checkout/sessions/{cs}")).await;
    assert_eq!(s["status"], "expired");
    assert_eq!(s["payment_status"], "unpaid");
    assert_eq!(s["url"], Value::Null);
    assert_eq!(s["payment_intent"], Value::Null);
    assert!(
        m.truth_log()
            .iter()
            .all(|u| u.object_type != ObjectType::PaymentIntent)
    );
    let expired = json(&m, "/v1/checkout/sessions?status=expired").await;
    assert_eq!(expired["data"][0]["id"], cs.as_str());
}

#[tokio::test]
async fn checkout_session_object_name_routes() {
    let m = mock_with("seed = 25").await;
    recurring_prices(&m, 1, "month");
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    let pay = checkout(&m, &cus, CheckoutMode::Payment, true).unwrap();
    let sub_cause = checkout(&m, &cus, CheckoutMode::Subscription, true).unwrap();
    let other = checkout(&m, &cus, CheckoutMode::Subscription, true).unwrap();
    m.step(Duration::from_secs(31 * 60)).await.unwrap();
    let cs = only_id(&caused(&m, &sub_cause), ObjectType::CheckoutSession);
    let sub = only_id(&caused(&m, &sub_cause), ObjectType::Subscription);
    let s = json(&m, &format!("/v1/checkout/sessions/{cs}")).await;
    assert_eq!(s["object"], "checkout.session");
    let listed = json(&m, &format!("/v1/checkout/sessions?subscription={sub}")).await;
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [cs.as_str()]);
    let all = json(&m, &format!("/v1/checkout/sessions?customer={cus}")).await;
    assert_eq!(all["data"].as_array().unwrap().len(), 3);
    for c in [&pay, &other] {
        only_id(&caused(&m, c), ObjectType::CheckoutSession);
    }
    let expanded = json(
        &m,
        &format!("/v1/checkout/sessions/{cs}?expand[]=subscription&expand[]=invoice"),
    )
    .await;
    assert_eq!(expanded["subscription"]["id"], sub.as_str());
    assert_eq!(expanded["invoice"]["status"], "paid");
}

#[tokio::test]
async fn named_checkout_errors() {
    let m = mock_with("seed = 26").await;
    let none = m.run_action(Action::Checkout {
        customer: None,
        mode: None,
        complete: None,
    });
    assert!(matches!(none, Err(MockError::InvalidAction(_))), "{none:?}");
    let unknown = checkout(&m, "cus_nope", CheckoutMode::Payment, true);
    assert!(
        matches!(unknown, Err(MockError::NoSuchObject { .. })),
        "{unknown:?}"
    );
    assert!(m.truth_log().is_empty());
}

#[tokio::test]
async fn generated_checkouts_conform() {
    let m = mock_with(
        "seed = 27\n[actions]\nnew_customer = 4.0\ncatalog_change = 1.0\ncheckout = 6.0\n\
         payment_failure_probability = 0.2",
    )
    .await;
    m.step(Duration::from_secs(2 * DAY_SECS as u64))
        .await
        .unwrap();
    let log = m.truth_log();
    let sessions = ids_of(&log, ObjectType::CheckoutSession);
    assert!(sessions.len() > 10, "{}", sessions.len());
    for u in &log {
        common::assert_conforms(&u.state);
    }
    let mut seen = std::collections::BTreeSet::new();
    for cs in &sessions {
        let s = json(&m, &format!("/v1/checkout/sessions/{cs}")).await;
        seen.insert((
            s["mode"].as_str().unwrap().to_string(),
            s["status"].as_str().unwrap().to_string(),
        ));
    }
    for want in [("payment", "complete"), ("payment", "expired")] {
        assert!(
            seen.contains(&(want.0.to_string(), want.1.to_string())),
            "{seen:?}"
        );
    }
    assert!(
        seen.iter().any(|(mode, _)| mode == "subscription"),
        "{seen:?}"
    );
}

#[tokio::test]
async fn writes_fire_on_checkout_lists() {
    let m = mock_with("seed = 28\n[api]\npagination_writes = { probability = 1.0 }").await;
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    for _ in 0..3 {
        checkout(&m, &cus, CheckoutMode::Payment, false).unwrap();
    }
    let first = json(&m, "/v1/checkout/sessions?limit=1").await;
    let id = first["data"][0]["id"].as_str().unwrap().to_string();
    let before = m.truth_log().len();
    json(
        &m,
        &format!("/v1/checkout/sessions?limit=1&starting_after={id}"),
    )
    .await;
    let log = m.truth_log();
    assert!(log.len() > before, "the later page follows a write");
    assert!(
        log[before..]
            .iter()
            .all(|u| u.cause.as_ref().is_some_and(|c| c.action == "checkout")),
        "{:?}",
        &log[before..]
    );
}
