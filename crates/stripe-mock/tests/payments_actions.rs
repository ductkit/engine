//! The `one_off_payment` and `refund` actions (spec sections 4, 5.5 and 11).

mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::{Action, Cause, MockError, MockStripe, ObjectType, StateUpdate};
use serde_json::Value;

fn caused(m: &MockStripe, cause: &Cause) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(cause))
        .collect()
}

fn only_id(updates: &[StateUpdate], t: ObjectType) -> String {
    let mut ids: Vec<String> = updates
        .iter()
        .filter(|u| u.object_type == t)
        .map(|u| u.object_id.clone())
        .collect();
    ids.dedup();
    assert_eq!(ids.len(), 1, "{t:?}: {ids:?}");
    ids.pop().unwrap()
}

/// The snapshot event types of `updates`, in truth order.
fn event_types(m: &MockStripe, updates: &[StateUpdate]) -> Vec<String> {
    let timeline = m.event_timeline();
    updates
        .iter()
        .flat_map(|u| u.events.snapshot.clone())
        .map(|id| {
            timeline
                .iter()
                .find(|e| e.id == id)
                .unwrap()
                .event_type
                .clone()
        })
        .collect()
}

async fn json(m: &MockStripe, path: &str) -> Value {
    let r = get(m, path).await;
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

fn pay(m: &MockStripe, customer: Option<String>, amount: i64, succeed: bool) -> Cause {
    m.run_action(Action::OneOffPayment {
        customer,
        amount: Some(amount),
        succeed: Some(succeed),
    })
    .unwrap()
}

/// A succeeded one-off payment of `amount`; returns its charge.
fn paid_charge(m: &MockStripe, amount: i64) -> String {
    let cause = pay(m, None, amount, true);
    only_id(&caused(m, &cause), ObjectType::Charge)
}

fn refund(m: &MockStripe, charge: &str, amount: Option<i64>) -> Result<Cause, MockError> {
    m.run_action(Action::Refund {
        charge: Some(charge.to_string()),
        amount,
    })
}

#[tokio::test]
async fn one_off_payment_without_invoice() {
    let m = mock_with("").await;
    m.run_action(Action::NewCustomer).unwrap();
    let cust = m.truth_log()[0].object_id.clone();
    let cause = pay(&m, Some(cust.clone()), 1234, true);
    let mine = caused(&m, &cause);
    assert!(
        !mine.iter().any(|u| matches!(
            u.object_type,
            ObjectType::Invoice | ObjectType::InvoicePayment | ObjectType::InvoiceLineItem
        )),
        "{mine:?}"
    );
    assert_eq!(
        event_types(&m, &mine),
        [
            "payment_method.attached",
            "payment_intent.created",
            "charge.succeeded",
            "payment_intent.succeeded",
        ]
    );
    let pi_id = only_id(&mine, ObjectType::PaymentIntent);
    let pi = json(&m, &format!("/v1/payment_intents/{pi_id}")).await;
    assert_eq!(pi["status"], "succeeded");
    assert_eq!(pi["amount"], 1234);
    assert_eq!(pi["amount_received"], 1234);
    assert_eq!(pi["customer"], cust.as_str());
    // The pinned API has no `invoice` on payment intents or charges; nothing links one.
    assert!(pi.get("invoice").is_none_or(Value::is_null), "{pi}");
    assert!(pi.get("payment_details").is_none_or(Value::is_null), "{pi}");
    let ch = json(
        &m,
        &format!("/v1/charges/{}", pi["latest_charge"].as_str().unwrap()),
    )
    .await;
    assert!(ch.get("invoice").is_none_or(Value::is_null), "{ch}");
    assert_eq!(ch["amount"], 1234);
    assert_eq!(ch["customer"], cust.as_str());
    let ips = json(&m, &format!("/v1/invoice_payments?payment={pi_id}")).await;
    assert_eq!(ips["data"], serde_json::json!([]));

    // A failed one-off payment: a failed charge and the intent back to
    // `requires_payment_method`, still with no invoice objects.
    let cause = pay(&m, Some(cust), 500, false);
    let mine = caused(&m, &cause);
    assert_eq!(
        event_types(&m, &mine),
        [
            "payment_intent.created",
            "charge.failed",
            "payment_intent.payment_failed",
        ]
    );
    assert!(
        !mine
            .iter()
            .any(|u| u.object_type == ObjectType::BalanceTransaction)
    );
}

#[tokio::test]
async fn one_off_payment_rejects_bad_targets() {
    let m = mock_with("").await;
    let e = m
        .run_action(Action::OneOffPayment {
            customer: Some("cus_missing".into()),
            amount: None,
            succeed: None,
        })
        .unwrap_err();
    assert!(matches!(e, MockError::NoSuchObject { .. }), "{e}");
    let e = m
        .run_action(Action::OneOffPayment {
            customer: None,
            amount: Some(0),
            succeed: None,
        })
        .unwrap_err();
    assert!(matches!(e, MockError::InvalidAction(_)), "{e}");
    assert!(m.truth_log().is_empty());
}

#[tokio::test]
async fn generated_one_off_payments_sometimes_have_no_customer() {
    let m = mock_with("seed = 3\n[actions]\nnew_customer = 5.0\none_off_payment = 30.0").await;
    m.step(Duration::from_secs(10 * 3600)).await.unwrap();
    let intents: Vec<StateUpdate> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::PaymentIntent && u.version == 1)
        .collect();
    assert!(intents.len() > 100, "{}", intents.len());
    let without = intents
        .iter()
        .filter(|u| u.state["customer"].is_null())
        .count();
    let share = without as f64 / intents.len() as f64;
    assert!((0.08..0.35).contains(&share), "{without}/{}", intents.len());
    assert!(
        intents
            .iter()
            .all(|u| u.cause.as_ref().unwrap().action == "one_off_payment")
    );
    // Payments without a customer pay with an unattached card.
    let anon = intents
        .iter()
        .find(|u| u.state["customer"].is_null())
        .unwrap();
    let pm = anon.state["payment_method"].as_str().unwrap();
    let pm = json(&m, &format!("/v1/payment_methods/{pm}")).await;
    assert_eq!(pm["customer"], Value::Null);
}

#[tokio::test]
async fn partial_then_full_refund() {
    let m = mock_with("").await;
    let ch = paid_charge(&m, 1000);
    let first = refund(&m, &ch, Some(300)).unwrap();
    let re1 = only_id(&caused(&m, &first), ObjectType::Refund);
    let after_first = json(&m, &format!("/v1/charges/{ch}")).await;
    assert_eq!(after_first["amount_refunded"], 300);
    assert_eq!(after_first["refunded"], false);
    // More than is left is an error naming the charge.
    let e = refund(&m, &ch, Some(701)).unwrap_err();
    assert!(
        matches!(&e, MockError::InvalidAction(msg) if msg.contains(&ch)),
        "{e}"
    );
    let second = refund(&m, &ch, Some(700)).unwrap();
    let re2 = only_id(&caused(&m, &second), ObjectType::Refund);
    let charge = json(&m, &format!("/v1/charges/{ch}")).await;
    assert_eq!(charge["amount_refunded"], 1000);
    assert_eq!(charge["amount"], 1000);
    assert_eq!(charge["refunded"], true);
    let refunds = &charge["refunds"];
    assert_eq!(refunds["total_count"], 2);
    assert_eq!(refunds["has_more"], false);
    // Newest first, as Stripe lists refunds.
    let ids: Vec<&str> = refunds["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [re2.as_str(), re1.as_str()]);
    let listed = json(&m, &format!("/v1/refunds?charge={ch}")).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 2);

    // A fully refunded charge cannot be refunded again, with or without an amount.
    for amount in [None, Some(1)] {
        let e = refund(&m, &ch, amount).unwrap_err();
        assert!(
            matches!(&e, MockError::InvalidAction(msg) if msg.contains(&ch)),
            "{e}"
        );
    }
}

#[tokio::test]
async fn refund_rejects_failed_and_missing_charges() {
    let m = mock_with("").await;
    let cause = pay(&m, None, 800, false);
    let failed = only_id(&caused(&m, &cause), ObjectType::Charge);
    let e = refund(&m, &failed, None).unwrap_err();
    assert!(
        matches!(&e, MockError::InvalidAction(msg) if msg.contains(&failed)),
        "{e}"
    );
    let e = refund(&m, "ch_missing", None).unwrap_err();
    assert!(matches!(e, MockError::NoSuchObject { .. }), "{e}");
    // A random target needs a refundable charge.
    let e = m
        .run_action(Action::Refund {
            charge: None,
            amount: None,
        })
        .unwrap_err();
    assert!(matches!(e, MockError::InvalidAction(_)), "{e}");
}

#[tokio::test]
async fn refund_balance_transaction_is_negative() {
    let m = mock_with("").await;
    let ch = paid_charge(&m, 2000);
    let cause = refund(&m, &ch, Some(450)).unwrap();
    let mine = caused(&m, &cause);
    let re_id = only_id(&mine, ObjectType::Refund);
    let re = json(&m, &format!("/v1/refunds/{re_id}")).await;
    assert_eq!(re["status"], "succeeded");
    assert_eq!(re["amount"], 450);
    assert_eq!(re["charge"], ch.as_str());
    let charge = json(&m, &format!("/v1/charges/{ch}")).await;
    assert_eq!(re["payment_intent"], charge["payment_intent"]);
    assert_eq!(re["currency"], charge["currency"]);
    let txn_id = re["balance_transaction"].as_str().unwrap();
    assert_eq!(only_id(&mine, ObjectType::BalanceTransaction), txn_id);
    let txn = json(&m, &format!("/v1/balance_transactions/{txn_id}")).await;
    assert_eq!(txn["type"], "refund");
    assert_eq!(txn["reporting_category"], "refund");
    assert_eq!(txn["amount"], -450);
    assert_eq!(txn["net"], -450);
    assert_eq!(txn["fee"], 0);
    assert_eq!(txn["status"], "available");
    assert_eq!(txn["available_on"], txn["created"]);
    assert_eq!(txn["source"], re_id.as_str());
    // Expanding the refund's balance transaction works, and nothing changes later.
    m.step(Duration::from_secs(3 * 86_400)).await.unwrap();
    let after = m.get(ObjectType::BalanceTransaction, txn_id).unwrap();
    assert_eq!(after.version, 1);
}

#[tokio::test]
async fn refund_events_in_order() {
    let m = mock_with("").await;
    let ch = paid_charge(&m, 1500);
    let cause = refund(&m, &ch, None).unwrap();
    let mine = caused(&m, &cause);
    assert_eq!(
        event_types(&m, &mine),
        ["refund.created", "charge.refunded"]
    );
    let re_id = only_id(&mine, ObjectType::Refund);
    let timeline = m.event_timeline();
    let refunded = timeline
        .iter()
        .find(|e| e.event_type == "charge.refunded")
        .unwrap();
    let ev = json(&m, &format!("/v1/events/{}", refunded.id)).await;
    let obj = &ev["data"]["object"];
    assert_eq!(obj["id"], ch.as_str());
    assert_eq!(obj["refunds"]["data"][0]["id"], re_id.as_str());
    assert!(obj["amount_refunded"].as_i64().unwrap() > 0);
    assert!(ev["data"]["previous_attributes"]["amount_refunded"] == 0);
    let created = timeline
        .iter()
        .find(|e| e.event_type == "refund.created")
        .unwrap();
    let ev = json(&m, &format!("/v1/events/{}", created.id)).await;
    assert_eq!(ev["data"]["object"]["id"], re_id.as_str());
    assert!(
        timeline
            .iter()
            .all(|e| e.event_type != "charge.refund.updated")
    );
}

#[tokio::test]
async fn generated_refunds_target_refundable_charges() {
    let m = mock_with("seed = 9\n[actions]\none_off_payment = 20.0\nrefund = 10.0").await;
    m.step(Duration::from_secs(10 * 3600)).await.unwrap();
    let log = m.truth_log();
    let refunds: Vec<&StateUpdate> = log
        .iter()
        .filter(|u| u.object_type == ObjectType::Refund)
        .collect();
    assert!(refunds.len() > 20, "{}", refunds.len());
    let mut full = 0;
    for r in &refunds {
        let ch = r.state["charge"].as_str().unwrap();
        let charge = m.get(ObjectType::Charge, ch).unwrap().state;
        assert_eq!(charge["status"], "succeeded");
        assert!(charge["amount_refunded"].as_i64() <= charge["amount"].as_i64());
        if r.state["amount"] == charge["amount"] {
            full += 1;
        }
    }
    assert!(full > 0 && full < refunds.len(), "{full}/{}", refunds.len());
}

fn ids_of(m: &MockStripe) -> Vec<(ObjectType, String, u64)> {
    m.truth_log()
        .into_iter()
        .filter(|u| {
            matches!(
                u.object_type,
                ObjectType::PaymentIntent
                    | ObjectType::Charge
                    | ObjectType::Refund
                    | ObjectType::BalanceTransaction
                    | ObjectType::PaymentMethod
            )
        })
        .map(|u| (u.object_type, u.object_id, u.version))
        .collect()
}

#[tokio::test]
async fn actions_are_reproducible() {
    let toml = "seed = 21\n[actions]\nnew_customer = 4.0\none_off_payment = 10.0\nrefund = 5.0\n\
                payment_failure_probability = 0.2";
    let a = mock_with(toml).await;
    let b = mock_with(toml).await;
    for m in [&a, &b] {
        m.step(Duration::from_secs(6 * 3600)).await.unwrap();
    }
    let ids = ids_of(&a);
    assert!(ids.iter().any(|(t, _, _)| *t == ObjectType::Refund));
    assert_eq!(ids, ids_of(&b));
}

#[tokio::test]
async fn control_ops_run_payment_actions_and_replay() {
    let m = common::mock_with_control("seed = 4").await;
    let r = common::ctl_post(
        &m,
        "/control/ops",
        serde_json::json!({"op": "run_action", "action": {"type": "one_off_payment", "amount": 900, "succeed": true}}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let charge = m
        .truth_log()
        .into_iter()
        .find(|u| u.object_type == ObjectType::Charge)
        .unwrap()
        .object_id;
    let op = |amount: Value| serde_json::json!({"op": "run_action", "action": {"type": "refund", "charge": charge, "amount": amount}});
    let r = common::ctl_post(&m, "/control/ops", op(Value::Null)).await;
    assert_eq!(r.status(), 200);
    // Refund whatever is left, then once more: the last one is a conflict naming the charge.
    let left = 900
        - m.get(ObjectType::Charge, &charge).unwrap().state["amount_refunded"]
            .as_i64()
            .unwrap();
    if left > 0 {
        let r = common::ctl_post(&m, "/control/ops", op(left.into())).await;
        assert_eq!(r.status(), 200);
    }
    let r = common::ctl_post(&m, "/control/ops", op(1.into())).await;
    assert_eq!(r.status(), 409);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_action");
    assert!(
        body["error"]["message"].as_str().unwrap().contains(&charge),
        "{body}"
    );
    let export = m.export_run();
    let replayed = MockStripe::replay(export, ductkit_stripe_mock::ReplayOptions::default())
        .await
        .unwrap();
    assert_eq!(ids_of(&replayed), ids_of(&m));
    assert_eq!(
        replayed.get(ObjectType::Charge, &charge).unwrap().state,
        m.get(ObjectType::Charge, &charge).unwrap().state
    );
}

#[tokio::test]
async fn connected_refund_stays_in_scope() {
    let m = mock_with("[accounts]\nconnect = true\n").await;
    let acct = m
        .connect_account(ductkit_stripe_mock::AccountApi::V1)
        .unwrap();
    let a = m.account(&acct);
    let cause = a
        .run_action(Action::OneOffPayment {
            customer: None,
            amount: Some(1000),
            succeed: Some(true),
        })
        .unwrap();
    let ch = only_id(&caused(&m, &cause), ObjectType::Charge);
    let cause = a
        .run_action(Action::Refund {
            charge: Some(ch.clone()),
            amount: Some(400),
        })
        .unwrap();
    let mine = caused(&m, &cause);
    assert!(mine.iter().all(|u| u.account.as_deref() == Some(&*acct)));
    let re = only_id(&mine, ObjectType::Refund);
    let txn = only_id(&mine, ObjectType::BalanceTransaction);
    for path in [
        format!("/v1/refunds/{re}"),
        format!("/v1/balance_transactions/{txn}"),
    ] {
        let r = common::get_as(&m, &path, &acct).await;
        assert_eq!(r.status(), 200, "{path}");
        let r = get(&m, &path).await;
        assert_eq!(r.status(), 404, "{path} leaked to the platform");
    }
    let r = common::get_as(&m, &format!("/v1/charges/{ch}"), &acct).await;
    let charge: Value = r.json().await.unwrap();
    assert_eq!(charge["amount_refunded"], 400);
    assert!(m.get(ObjectType::Refund, &re).is_none());
    let ids = [re.as_str(), ch.as_str()];
    let events: Vec<_> = m
        .event_timeline()
        .into_iter()
        .filter(|e| matches!(e.event_type.as_str(), "refund.created" | "charge.refunded"))
        .collect();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| ids.contains(&e.object_id.as_str())));
    assert!(events.iter().all(|e| e.account.as_deref() == Some(&*acct)));
}
