//! Saved payment methods: the `save_payment_method` action (setup intents), detaching, and
//! invoice payments reusing a saved card (spec sections 4, 5.5 and 11).

mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::{
    Action, Cause, EventFormat, InvoiceOutcome, MockError, MockStripe, NewCustomer, ObjectType,
    StateUpdate,
};
use serde_json::Value;

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

fn listed_ids(list: &Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

/// The event types the truth-log entries of `cause` announced, in order.
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

fn save(m: &MockStripe, customer: &str) -> Result<Cause, MockError> {
    m.run_action(Action::SavePaymentMethod {
        customer: Some(customer.to_string()),
    })
}

#[tokio::test]
async fn saved_payment_method_lists_under_customer() {
    let m = mock_with("seed = 7").await;
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    let cause = save(&m, &cus).unwrap();
    let mine = caused(&m, &cause);
    let seti = only_id(&mine, ObjectType::SetupIntent);
    let pm = only_id(&mine, ObjectType::PaymentMethod);
    assert!(seti.starts_with("seti_"), "{seti}");
    assert_eq!(
        event_types(&m, &cause),
        [
            "setup_intent.created",
            "payment_method.attached",
            "setup_intent.succeeded"
        ]
    );
    fetch_events(&m, &cause).await;

    let s = json(&m, &format!("/v1/setup_intents/{seti}")).await;
    assert_eq!(s["object"], "setup_intent");
    assert_eq!(s["status"], "succeeded");
    assert_eq!(s["customer"], cus.as_str());
    assert_eq!(s["payment_method"], pm.as_str());
    assert_eq!(s["usage"], "off_session");
    assert_eq!(s["last_setup_error"], Value::Null);

    let p = json(&m, &format!("/v1/payment_methods/{pm}")).await;
    assert_eq!(p["type"], "card");
    assert_eq!(p["customer"], cus.as_str());
    let card = &p["card"];
    assert!(
        ["visa", "mastercard", "amex"].contains(&card["brand"].as_str().unwrap()),
        "{card}"
    );
    let last4 = card["last4"].as_str().unwrap();
    assert!(
        last4.len() == 4 && last4.chars().all(|c| c.is_ascii_digit()),
        "{last4}"
    );
    // The clock starts on 2026-01-01: the card expires 1-5 years ahead.
    let year = card["exp_year"].as_i64().unwrap();
    assert!((2027..=2031).contains(&year), "{year}");
    assert!((1..=12).contains(&card["exp_month"].as_i64().unwrap()));

    let top = json(&m, &format!("/v1/payment_methods?customer={cus}")).await;
    assert_eq!(listed_ids(&top), std::slice::from_ref(&pm));
    let sub = json(&m, &format!("/v1/customers/{cus}/payment_methods")).await;
    assert_eq!(listed_ids(&sub), std::slice::from_ref(&pm));
    let setis = json(&m, &format!("/v1/setup_intents?customer={cus}")).await;
    assert_eq!(listed_ids(&setis), std::slice::from_ref(&seti));
    let by_pm = json(&m, &format!("/v1/setup_intents?payment_method={pm}")).await;
    assert_eq!(listed_ids(&by_pm), [seti]);
}

#[tokio::test]
async fn detached_payment_method_is_retrievable_but_unlisted() {
    let m = mock_with("seed = 8").await;
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    let cause = save(&m, &cus).unwrap();
    let pm = only_id(&caused(&m, &cause), ObjectType::PaymentMethod);

    let detach = m
        .run_action(Action::DetachPaymentMethod {
            id: Some(pm.clone()),
        })
        .unwrap();
    assert_eq!(event_types(&m, &detach), ["payment_method.detached"]);
    fetch_events(&m, &detach).await;
    let p = json(&m, &format!("/v1/payment_methods/{pm}")).await;
    assert_eq!(p["customer"], Value::Null);
    assert_eq!(p["card"]["last4"].as_str().unwrap().len(), 4);
    let top = json(&m, &format!("/v1/payment_methods?customer={cus}")).await;
    assert!(listed_ids(&top).is_empty(), "{top}");
    let sub = json(&m, &format!("/v1/customers/{cus}/payment_methods")).await;
    assert!(listed_ids(&sub).is_empty(), "{sub}");

    // Detaching again, or an unknown ID, is rejected and writes nothing.
    let before = m.truth_log().len();
    let again = m.run_action(Action::DetachPaymentMethod { id: Some(pm) });
    assert!(
        matches!(again, Err(MockError::InvalidAction(_))),
        "{again:?}"
    );
    let unknown = m.run_action(Action::DetachPaymentMethod {
        id: Some("pm_nope".to_string()),
    });
    assert!(
        matches!(unknown, Err(MockError::NoSuchObject { .. })),
        "{unknown:?}"
    );
    // No attached payment method is left to pick.
    let random = m.run_action(Action::DetachPaymentMethod { id: None });
    assert!(
        matches!(random, Err(MockError::InvalidAction(_))),
        "{random:?}"
    );
    assert_eq!(m.truth_log().len(), before);

    // The next payment does not use the detached card: it attaches a new one.
    let pay = m
        .run_action(Action::OneOffPayment {
            customer: Some(cus.clone()),
            amount: Some(1200),
            succeed: Some(true),
        })
        .unwrap();
    let fresh = only_id(&caused(&m, &pay), ObjectType::PaymentMethod);
    let sub = json(&m, &format!("/v1/customers/{cus}/payment_methods")).await;
    assert_eq!(listed_ids(&sub), [fresh]);
}

#[tokio::test]
async fn failed_setup_intent_creates_no_payment_method() {
    let m = mock_with("seed = 9\n[actions]\npayment_failure_probability = 1.0").await;
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    let cause = save(&m, &cus).unwrap();
    let mine = caused(&m, &cause);
    assert!(ids_of(&mine, ObjectType::PaymentMethod).is_empty());
    assert!(
        m.truth_log()
            .iter()
            .all(|u| u.object_type != ObjectType::PaymentMethod)
    );
    let seti = only_id(&mine, ObjectType::SetupIntent);
    assert_eq!(
        event_types(&m, &cause),
        ["setup_intent.created", "setup_intent.setup_failed"]
    );
    fetch_events(&m, &cause).await;
    let s = json(&m, &format!("/v1/setup_intents/{seti}")).await;
    assert_eq!(s["status"], "requires_payment_method");
    assert_eq!(s["payment_method"], Value::Null);
    assert_eq!(s["last_setup_error"]["code"], "card_declined");
    assert_eq!(s["last_setup_error"]["type"], "card_error");
    let sub = json(&m, &format!("/v1/customers/{cus}/payment_methods")).await;
    assert!(listed_ids(&sub).is_empty());
}

#[tokio::test]
async fn invoice_payment_reuses_saved_payment_method() {
    let m = mock_with("seed = 10").await;
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    let cause = save(&m, &cus).unwrap();
    let pm = only_id(&caused(&m, &cause), ObjectType::PaymentMethod);
    let inv = m
        .run_action(Action::OneOffInvoice {
            customer: Some(cus.clone()),
            outcome: Some(InvoiceOutcome::Paid),
        })
        .unwrap();
    m.step(Duration::from_secs(60)).await.unwrap();
    let mine = caused(&m, &inv);
    assert!(
        ids_of(&mine, ObjectType::PaymentMethod).is_empty(),
        "no new card"
    );
    let pi = only_id(&mine, ObjectType::PaymentIntent);
    let intent = json(&m, &format!("/v1/payment_intents/{pi}")).await;
    assert_eq!(intent["status"], "succeeded");
    assert_eq!(intent["payment_method"], pm.as_str());
    let ch = only_id(&mine, ObjectType::Charge);
    let charge = json(&m, &format!("/v1/charges/{ch}")).await;
    let brand = json(&m, &format!("/v1/payment_methods/{pm}")).await["card"]["brand"].clone();
    assert_eq!(charge["payment_method_details"]["card"]["brand"], brand);
}

#[tokio::test]
async fn named_save_errors_and_random_save_without_customers() {
    let m = mock_with("seed = 11").await;
    let none = m.run_action(Action::SavePaymentMethod { customer: None });
    assert!(matches!(none, Err(MockError::InvalidAction(_))), "{none:?}");
    let unknown = save(&m, "cus_nope");
    assert!(
        matches!(unknown, Err(MockError::NoSuchObject { .. })),
        "{unknown:?}"
    );
    assert!(m.truth_log().is_empty());
}

#[tokio::test]
async fn generated_saves_and_detaches_conform() {
    let m = mock_with(
        "seed = 12\n[actions]\nnew_customer = 4.0\nsave_payment_method = 6.0\n\
         payment_failure_probability = 0.3\n[churn.payment_method]\ndetach = 3.0",
    )
    .await;
    m.step(Duration::from_secs(6 * 3600)).await.unwrap();
    let log = m.truth_log();
    for t in [ObjectType::SetupIntent, ObjectType::PaymentMethod] {
        assert!(log.iter().any(|u| u.object_type == t), "{t:?}");
    }
    let mut actions: Vec<&str> = log
        .iter()
        .filter_map(|u| u.cause.as_ref().map(|c| c.action.as_str()))
        .collect();
    actions.dedup();
    assert!(actions.contains(&"save_payment_method"), "{actions:?}");
    assert!(actions.contains(&"payment_method_detach"), "{actions:?}");
    for u in &log {
        common::assert_conforms(&u.state);
    }
    let types: Vec<String> = m
        .event_timeline()
        .into_iter()
        .map(|e| e.event_type)
        .collect();
    for want in [
        "setup_intent.created",
        "setup_intent.succeeded",
        "setup_intent.setup_failed",
        "payment_method.attached",
        "payment_method.detached",
    ] {
        assert!(types.iter().any(|t| t == want), "{want}");
    }
    // Every listed payment method is attached to the customer it is listed under.
    let mut customers = ids_of(&log, ObjectType::Customer);
    customers.sort();
    customers.dedup();
    for cus in customers {
        let sub = json(&m, &format!("/v1/customers/{cus}/payment_methods")).await;
        for p in sub["data"].as_array().unwrap() {
            assert_eq!(p["customer"], cus.as_str());
        }
    }
}

#[tokio::test]
async fn writes_fire_on_payment_method_lists() {
    let m = mock_with("seed = 13\n[api]\npagination_writes = { probability = 1.0 }").await;
    let cus = m.create_customer(NewCustomer::default()).unwrap();
    for _ in 0..3 {
        save(&m, &cus).unwrap();
    }
    let first = json(&m, &format!("/v1/payment_methods?customer={cus}&limit=1")).await;
    let id = first["data"][0]["id"].as_str().unwrap().to_string();
    let before = m.truth_log().len();
    json(
        &m,
        &format!("/v1/payment_methods?customer={cus}&limit=1&starting_after={id}"),
    )
    .await;
    let log = m.truth_log();
    assert!(log.len() > before, "the later page follows a write");
    assert!(
        log[before..].iter().all(|u| matches!(
            u.cause.as_ref().map(|c| c.action.as_str()),
            Some("save_payment_method" | "payment_method_detach")
        )),
        "{:?}",
        &log[before..]
    );
}
