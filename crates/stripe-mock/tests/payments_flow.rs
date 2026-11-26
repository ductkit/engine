//! Paying an invoice creates payment objects (spec sections 5.5 and 6.1).

mod common;

use std::time::Duration;

use common::{get, get_as, mock_with, recurring_prices};
use ductkit_stripe_mock::{
    AccountApi, Action, Cause, ChangeKind, InvoiceOutcome, MockStripe, ObjectType, StateUpdate,
};
use serde_json::Value;

const DAY: u64 = 86_400;

fn paid_one_off() -> Action {
    Action::OneOffInvoice {
        customer: None,
        outcome: Some(InvoiceOutcome::Paid),
    }
}

fn caused(m: &MockStripe, cause: &Cause) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(cause))
        .collect()
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

/// The types after `invoice.finalized`.
fn after_finalize(types: &[String]) -> Vec<String> {
    let at = types
        .iter()
        .position(|t| t == "invoice.finalized")
        .unwrap_or_else(|| panic!("no invoice.finalized in {types:?}"));
    types[at + 1..].to_vec()
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

async fn json(m: &MockStripe, path: &str) -> Value {
    let r = get(m, path).await;
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

fn fee(amount: i64) -> i64 {
    if amount == 0 {
        0
    } else {
        (amount * 29 + 500) / 1000 + 30
    }
}

#[tokio::test]
async fn paid_invoice_creates_payment_objects() {
    let m = mock_with("").await;
    let cause = m.run_action(paid_one_off()).unwrap();
    let mine = caused(&m, &cause);
    assert_eq!(
        after_finalize(&event_types(&m, &mine)),
        [
            "payment_method.attached",
            "payment_intent.created",
            "charge.succeeded",
            "payment_intent.succeeded",
            "invoice_payment.paid",
            "invoice.paid",
            "invoice.payment_succeeded",
        ]
    );
    let types = event_types(&m, &mine);
    let created = types.iter().position(|t| t == "invoice.created").unwrap();
    let finalized = types.iter().position(|t| t == "invoice.finalized").unwrap();
    assert!(created < finalized);

    let inv_id = only_id(&mine, ObjectType::Invoice);
    let inv = json(&m, &format!("/v1/invoices/{inv_id}")).await;
    assert_eq!(inv["status"], "paid");
    let payments = &inv["payments"];
    assert_eq!(payments["data"].as_array().unwrap().len(), 1, "{payments}");
    assert_eq!(payments["total_count"], 1);
    let inpay = &payments["data"][0];
    assert_eq!(inpay["status"], "paid");
    assert_eq!(inpay["payment"]["type"], "payment_intent");
    assert_eq!(inpay["invoice"], inv_id.as_str());
    assert_eq!(inpay["amount_paid"], inv["amount_paid"]);
    let pi_id = inpay["payment"]["payment_intent"].as_str().unwrap();
    assert_eq!(only_id(&mine, ObjectType::PaymentIntent), pi_id);

    let pi = json(&m, &format!("/v1/payment_intents/{pi_id}")).await;
    assert_eq!(pi["status"], "succeeded");
    assert_eq!(pi["amount"], inv["total"]);
    assert_eq!(pi["amount_received"], inv["total"]);
    assert_eq!(pi["customer"], inv["customer"]);
    let ch_id = pi["latest_charge"].as_str().unwrap();
    let pm_id = pi["payment_method"].as_str().unwrap();

    let ch = json(&m, &format!("/v1/charges/{ch_id}")).await;
    assert_eq!(ch["status"], "succeeded");
    assert_eq!(
        (ch["paid"].clone(), ch["captured"].clone()),
        (true.into(), true.into())
    );
    assert_eq!(ch["payment_intent"], pi_id);
    assert_eq!(ch["amount"], pi["amount"]);
    assert_eq!(ch["amount_captured"], pi["amount"]);
    assert_eq!(ch["payment_method"], pm_id);
    let txn_id = ch["balance_transaction"].as_str().unwrap();

    let txn = json(&m, &format!("/v1/balance_transactions/{txn_id}")).await;
    assert_eq!(txn["source"], ch_id);
    assert_eq!(txn["type"], "charge");
    assert_eq!(txn["status"], "pending");
    let amount = txn["amount"].as_i64().unwrap();
    assert_eq!(amount, ch["amount"].as_i64().unwrap());
    assert_eq!(txn["fee"].as_i64().unwrap(), fee(amount));
    assert_eq!(txn["net"].as_i64().unwrap(), amount - fee(amount));
    assert_eq!(
        txn["available_on"].as_i64().unwrap(),
        txn["created"].as_i64().unwrap() + 2 * DAY as i64
    );

    let pm = json(&m, &format!("/v1/payment_methods/{pm_id}")).await;
    assert_eq!(pm["type"], "card");
    assert_eq!(pm["customer"], inv["customer"]);
    let listed = json(&m, &format!("/v1/invoice_payments?invoice={inv_id}")).await;
    assert_eq!(listed["data"][0]["id"], inpay["id"]);

    // A second paid invoice for the same customer reuses the card.
    let cust = inv["customer"].as_str().unwrap().to_string();
    let cause = m
        .run_action(Action::OneOffInvoice {
            customer: Some(cust),
            outcome: Some(InvoiceOutcome::Paid),
        })
        .unwrap();
    let again = caused(&m, &cause);
    assert!(
        !again
            .iter()
            .any(|u| u.object_type == ObjectType::PaymentMethod)
    );
    let pi2 = only_id(&again, ObjectType::PaymentIntent);
    assert_eq!(
        m.get(ObjectType::PaymentIntent, &pi2).unwrap().state["payment_method"],
        pm_id
    );
}

#[tokio::test]
async fn failed_payment_creates_failed_charge() {
    let m = mock_with("[actions]\npayment_failure_probability = 1.0").await;
    let cause = m.run_action(paid_one_off()).unwrap();
    let mine = caused(&m, &cause);
    let types = after_finalize(&event_types(&m, &mine));
    assert_eq!(
        &types[..5],
        [
            "payment_method.attached",
            "payment_intent.created",
            "charge.failed",
            "payment_intent.payment_failed",
            "invoice.payment_failed",
        ]
    );
    assert!(
        !mine
            .iter()
            .any(|u| u.object_type == ObjectType::BalanceTransaction)
    );
    assert!(
        !mine
            .iter()
            .any(|u| u.object_type == ObjectType::InvoicePayment)
    );
    let inv_id = only_id(&mine, ObjectType::Invoice);
    let inv = json(&m, &format!("/v1/invoices/{inv_id}")).await;
    assert_eq!(inv["payments"]["data"], serde_json::json!([]));
    let ch_id = only_id(&mine, ObjectType::Charge);
    let ch = json(&m, &format!("/v1/charges/{ch_id}")).await;
    assert_eq!(ch["status"], "failed");
    assert_eq!(ch["failure_code"], "card_declined");
    assert_eq!(ch["paid"], false);
    assert_eq!(ch["balance_transaction"], Value::Null);
    // The failure's payment intent state, before the void that follows.
    let pi_id = only_id(&mine, ObjectType::PaymentIntent);
    let failed = mine
        .iter()
        .find(|u| u.object_id == pi_id && u.state["status"] == "requires_payment_method")
        .unwrap();
    assert_eq!(failed.state["last_payment_error"]["code"], "card_declined");
    assert_eq!(failed.state["last_payment_error"]["charge"], ch_id.as_str());
    assert_eq!(failed.state["latest_charge"], ch_id.as_str());
    assert_eq!(failed.state["amount_received"], 0);
}

#[tokio::test]
async fn retry_reuses_payment_intent() {
    let m = mock_with("").await;
    m.run_action(Action::StartSubscription {
        customer: None,
        prices: Some(recurring_prices(&m, 1, "month")),
    })
    .unwrap();
    m.update_actions_config(|a| a.payment_failure_probability = 1.0)
        .unwrap();
    m.step(Duration::from_secs(32 * DAY)).await.unwrap();
    let renewal = m
        .truth_log()
        .into_iter()
        .rev()
        .find(|u| {
            u.object_type == ObjectType::Invoice
                && u.state["billing_reason"] == "subscription_cycle"
        })
        .unwrap()
        .object_id;
    m.update_actions_config(|a| a.payment_failure_probability = 0.0)
        .unwrap();
    m.step(Duration::from_secs(3 * DAY)).await.unwrap();
    let inv = json(&m, &format!("/v1/invoices/{renewal}")).await;
    assert_eq!(inv["status"], "paid");
    assert_eq!(inv["attempt_count"], 2);
    let pi_id = inv["payments"]["data"][0]["payment"]["payment_intent"]
        .as_str()
        .unwrap()
        .to_string();
    let pi = json(&m, &format!("/v1/payment_intents/{pi_id}")).await;
    assert_eq!(pi["status"], "succeeded");
    let charges = json(&m, &format!("/v1/charges?payment_intent={pi_id}")).await;
    let statuses: Vec<&str> = charges["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["status"].as_str().unwrap())
        .collect();
    // Newest first.
    assert_eq!(statuses, ["succeeded", "failed"]);
    assert_eq!(pi["latest_charge"], charges["data"][0]["id"]);
    // Every payment intent of the renewal's customer: one for the first invoice, one
    // for the renewal (reused by the retry).
    let cust = inv["customer"].as_str().unwrap();
    let intents = json(&m, &format!("/v1/payment_intents?customer={cust}")).await;
    assert_eq!(intents["data"].as_array().unwrap().len(), 2, "{intents}");
}

#[tokio::test]
async fn voided_after_failure_cancels_payment_intent() {
    let m = mock_with("[actions]\npayment_failure_probability = 1.0").await;
    let cause = m.run_action(paid_one_off()).unwrap();
    let mine = caused(&m, &cause);
    let types = after_finalize(&event_types(&m, &mine));
    assert_eq!(
        &types[4..],
        [
            "invoice.payment_failed",
            "invoice.voided",
            "payment_intent.canceled"
        ]
    );
    let pi_id = only_id(&mine, ObjectType::PaymentIntent);
    let pi = json(&m, &format!("/v1/payment_intents/{pi_id}")).await;
    assert_eq!(pi["status"], "canceled");
    assert_eq!(pi["cancellation_reason"], "void_invoice");
    assert!(pi["canceled_at"].is_i64());
}

#[tokio::test]
async fn uncollectible_after_failure_cancels_payment_intent() {
    let m = mock_with("").await;
    m.run_action(Action::StartSubscription {
        customer: None,
        prices: Some(recurring_prices(&m, 1, "month")),
    })
    .unwrap();
    m.update_actions_config(|a| a.payment_failure_probability = 1.0)
        .unwrap();
    m.step(Duration::from_secs(35 * DAY)).await.unwrap();
    let renewal = m
        .truth_log()
        .into_iter()
        .rev()
        .find(|u| {
            u.object_type == ObjectType::Invoice
                && u.state["billing_reason"] == "subscription_cycle"
        })
        .unwrap()
        .object_id;
    let inv = m.get(ObjectType::Invoice, &renewal).unwrap();
    assert_eq!(inv.state["status"], "uncollectible");
    let pis: Vec<StateUpdate> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::PaymentIntent && u.state["status"] == "canceled")
        .collect();
    assert_eq!(pis.len(), 1);
    assert_eq!(pis[0].state["cancellation_reason"], "void_invoice");
}

#[tokio::test]
async fn balance_transaction_becomes_available_silently() {
    let m = mock_with("").await;
    let cause = m.run_action(paid_one_off()).unwrap();
    let mine = caused(&m, &cause);
    let txn = only_id(&mine, ObjectType::BalanceTransaction);
    let created = m.get(ObjectType::BalanceTransaction, &txn).unwrap();
    assert!(created.events.snapshot.is_empty() && created.events.thin.is_empty());
    let available_on = created.state["available_on"].as_i64().unwrap();
    let now_ms = m.now().to_datetime().timestamp_millis();
    let until = (available_on - 1) * 1000 - now_ms;
    m.step(Duration::from_millis(until as u64)).await.unwrap();
    assert_eq!(
        m.get(ObjectType::BalanceTransaction, &txn).unwrap().state["status"],
        "pending"
    );
    m.step(Duration::from_secs(2)).await.unwrap();
    let after = m.get(ObjectType::BalanceTransaction, &txn).unwrap();
    assert_eq!(after.state["status"], "available");
    assert_eq!(after.kind, ChangeKind::StateChanged);
    assert!(after.events.snapshot.is_empty() && after.events.thin.is_empty());
    assert!(m.event_timeline().iter().all(|e| e.object_id != txn));
}

#[tokio::test]
async fn connected_payments_stay_in_scope() {
    let m = mock_with("[accounts]\nconnect = true\n").await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    let cause = m.account(&acct).run_action(paid_one_off()).unwrap();
    let mine: Vec<StateUpdate> = caused(&m, &cause)
        .into_iter()
        .filter(|u| u.account.as_deref() == Some(&*acct))
        .collect();
    for (t, path) in [
        (ObjectType::PaymentMethod, "payment_methods"),
        (ObjectType::PaymentIntent, "payment_intents"),
        (ObjectType::Charge, "charges"),
        (ObjectType::BalanceTransaction, "balance_transactions"),
        (ObjectType::InvoicePayment, "invoice_payments"),
    ] {
        let id = only_id(&mine, t);
        assert!(m.account(&acct).get(t, &id).unwrap().is_some(), "{t:?}");
        assert!(m.get(t, &id).is_none(), "{t:?} leaked to the platform");
        let r = get_as(&m, &format!("/v1/{path}/{id}"), &acct).await;
        assert_eq!(r.status(), 200, "{t:?}");
        let r = get(&m, &format!("/v1/{path}/{id}")).await;
        assert_eq!(r.status(), 404, "{t:?}");
    }
    let ids: Vec<&str> = mine.iter().map(|u| u.object_id.as_str()).collect();
    let events: Vec<_> = m
        .event_timeline()
        .into_iter()
        .filter(|e| ids.contains(&e.object_id.as_str()))
        .collect();
    assert!(events.iter().any(|e| e.event_type == "charge.succeeded"));
    assert!(events.iter().all(|e| e.account.as_deref() == Some(&*acct)));
}
