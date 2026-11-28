//! Invoice items (phase 9, Task 4, ruling Q4): one-off lines get real items, pending items
//! join the customer's next invoice, and items are deleted only while pending or on a draft.

mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::stripe::check_payload;
use ductkit_stripe_mock::{
    Action, Cause, ChangeKind, InvoiceOutcome, MockError, MockStripe, ObjectType, StateUpdate,
};
use serde_json::{Value, json};

const DAY: Duration = Duration::from_secs(86_400);
/// `payments::MAX_CHARGE_AMOUNT`.
const MAX_CHARGE_AMOUNT: i64 = 99_999_999;

/// The truth-log entries of `cause`, in log order.
fn entries(m: &MockStripe, cause: &Cause) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(cause))
        .collect()
}

/// The snapshot event types announced by the entries of `cause`, in log order.
fn events_of(m: &MockStripe, cause: &Cause) -> Vec<String> {
    let timeline = m.event_timeline();
    entries(m, cause)
        .into_iter()
        .flat_map(|u| u.events.snapshot)
        .filter_map(|id| timeline.iter().find(|e| e.id == id))
        .map(|e| e.event_type.clone())
        .collect()
}

/// The ID of the object of `t` the entries of `cause` create first.
fn created_by(m: &MockStripe, cause: &Cause, t: ObjectType) -> String {
    entries(m, cause)
        .into_iter()
        .find(|u| u.object_type == t && u.kind == ChangeKind::Created)
        .unwrap_or_else(|| panic!("{cause:?} creates no {t:?}"))
        .object_id
}

fn state(m: &MockStripe, t: ObjectType, id: &str) -> Value {
    m.get(t, id).unwrap().state
}

/// The lines of an invoice (every line fits one page in these tests).
fn lines(invoice: &Value) -> Vec<Value> {
    assert_eq!(invoice["lines"]["has_more"], false);
    invoice["lines"]["data"].as_array().unwrap().clone()
}

fn line_item(line: &Value) -> Option<String> {
    line["parent"]["invoice_item_details"]["invoice_item"]
        .as_str()
        .map(str::to_string)
}

fn create_item(customer: &str, price: Option<&str>, quantity: Option<u64>) -> Action {
    Action::CreateInvoiceItem {
        customer: Some(customer.to_string()),
        price: price.map(str::to_string),
        quantity,
    }
}

/// Runs catalog changes until an active one-time price with `unit_amount` (any positive
/// one when `None`) exists, in a currency that also has an active recurring monthly price.
/// Returns `(one_time, recurring)`.
fn prices(m: &MockStripe, unit_amount: Option<i64>) -> (String, String) {
    for _ in 0..300 {
        let mut ids: Vec<String> = m
            .truth_log()
            .into_iter()
            .filter(|u| u.object_type == ObjectType::Price)
            .map(|u| u.object_id)
            .collect();
        ids.dedup();
        let live: Vec<(String, Value)> = ids
            .into_iter()
            .map(|id| {
                let s = state(m, ObjectType::Price, &id);
                (id, s)
            })
            .filter(|(_, p)| p["active"] == true)
            .collect();
        for (id, p) in &live {
            let amount = p["unit_amount"].as_i64().unwrap_or(0);
            let fits = unit_amount.map_or(amount > 0, |want| amount == want);
            if p["type"] != "one_time" || !fits {
                continue;
            }
            if let Some((rec, _)) = live.iter().find(|(_, r)| {
                r["recurring"]["interval"] == "month" && r["currency"] == p["currency"]
            }) {
                return (id.clone(), rec.clone());
            }
        }
        m.run_action(Action::CatalogChange).unwrap();
    }
    panic!("no matching prices after 300 catalog changes");
}

#[tokio::test]
async fn one_off_lines_have_real_items() {
    let m = mock_with("seed = 4").await;
    let mut checked = 0;
    for _ in 0..4 {
        let cause = m
            .run_action(Action::OneOffInvoice {
                customer: None,
                outcome: Some(InvoiceOutcome::Paid),
            })
            .unwrap();
        m.step(DAY).await.unwrap();
        let id = created_by(&m, &cause, ObjectType::Invoice);
        let invoice = state(&m, ObjectType::Invoice, &id);
        for line in lines(&invoice) {
            let ii = line_item(&line).expect("one-off lines name their item");
            let r = get(&m, &format!("/v1/invoiceitems/{ii}")).await;
            assert_eq!(r.status().as_u16(), 200, "{ii}");
            let item: Value = r.json().await.unwrap();
            assert_eq!(item["object"], "invoiceitem");
            assert_eq!(item["invoice"], id.as_str());
            assert_eq!(item["amount"], line["amount"]);
            assert_eq!(item["currency"], line["currency"]);
            assert_eq!(item["period"], line["period"]);
            assert_eq!(item["customer"], invoice["customer"]);
            assert!(item["date"].as_i64() <= invoice["created"].as_i64());
            checked += 1;
        }
        // In the one instant: the items, then the lines, then the invoice.
        let events = events_of(&m, &cause);
        let created = events.iter().position(|e| e == "invoice.created").unwrap();
        let items: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| *e == "invoiceitem.created")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(items.len(), lines(&invoice).len(), "{events:?}");
        assert!(items.iter().all(|i| *i < created), "{events:?}");
        let log = entries(&m, &cause);
        let last_item = log
            .iter()
            .rposition(|u| u.object_type == ObjectType::InvoiceItem)
            .unwrap();
        let first_line = log
            .iter()
            .position(|u| u.object_type == ObjectType::InvoiceLineItem)
            .unwrap();
        assert!(last_item < first_line);
    }
    assert!(checked > 0);
}

#[tokio::test]
async fn pending_items_join_the_next_subscription_invoice() {
    let m = mock_with("seed = 4").await;
    let (one_time, recurring) = prices(&m, None);
    let cust = m.create_customer(Default::default()).unwrap();
    let cause = m
        .run_action(create_item(&cust, Some(&one_time), Some(2)))
        .unwrap();
    let ii = created_by(&m, &cause, ObjectType::InvoiceItem);
    let item = state(&m, ObjectType::InvoiceItem, &ii);
    check_payload(&item).unwrap();
    assert_eq!(item["invoice"], Value::Null);
    assert_eq!(item["quantity"], 2);
    assert_eq!(events_of(&m, &cause), ["invoiceitem.created"]);
    let pending = get(&m, "/v1/invoiceitems?pending=true").await;
    let pending: Value = pending.json().await.unwrap();
    assert_eq!(pending["data"][0]["id"], ii.as_str());

    let start = m
        .run_action(Action::StartSubscription {
            customer: Some(cust.clone()),
            prices: Some(vec![recurring]),
        })
        .unwrap();
    m.step(DAY).await.unwrap();
    let sub = created_by(&m, &start, ObjectType::Subscription);
    let first = created_by(&m, &start, ObjectType::Invoice);
    let invoice = state(&m, ObjectType::Invoice, &first);
    let ls = lines(&invoice);
    let last = ls.last().unwrap();
    assert_eq!(line_item(last).as_deref(), Some(ii.as_str()));
    assert_eq!(
        last["parent"]["invoice_item_details"]["subscription"],
        sub.as_str()
    );
    assert_eq!(last["amount"], item["amount"]);
    let sum: i64 = ls.iter().map(|l| l["amount"].as_i64().unwrap()).sum();
    assert_eq!(invoice["subtotal"], sum);
    assert!(ls.len() >= 2);
    assert_eq!(
        state(&m, ObjectType::InvoiceItem, &ii)["invoice"],
        first.as_str()
    );
    let pending = get(&m, "/v1/invoiceitems?pending=true").await;
    let pending: Value = pending.json().await.unwrap();
    assert_eq!(pending["data"], json!([]));
    let on_invoice = get(&m, &format!("/v1/invoiceitems?invoice={first}")).await;
    let on_invoice: Value = on_invoice.json().await.unwrap();
    assert_eq!(on_invoice["data"][0]["id"], ii.as_str());
}

#[tokio::test]
async fn deleted_draft_takes_its_items() {
    let m = mock_with("seed = 7").await;
    let cause = m
        .run_action(Action::OneOffInvoice {
            customer: None,
            outcome: Some(InvoiceOutcome::DeleteDraft),
        })
        .unwrap();
    m.step(DAY).await.unwrap();
    let events = events_of(&m, &cause);
    let deleted = events.iter().position(|e| e == "invoice.deleted").unwrap();
    let items: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| *e == "invoiceitem.deleted")
        .map(|(i, _)| i)
        .collect();
    let created = events
        .iter()
        .filter(|e| *e == "invoiceitem.created")
        .count();
    assert!(created > 0);
    assert_eq!(items.len(), created, "{events:?}");
    assert!(items.iter().all(|i| *i < deleted), "{events:?}");
    for u in entries(&m, &cause)
        .into_iter()
        .filter(|u| u.object_type == ObjectType::InvoiceItem)
    {
        assert_eq!(
            m.get(ObjectType::InvoiceItem, &u.object_id).unwrap().kind,
            ChangeKind::Deleted
        );
    }
}

#[tokio::test]
async fn only_pending_or_draft_items_delete() {
    let m = mock_with("seed = 4").await;
    let cust = m.create_customer(Default::default()).unwrap();
    let cause = m.run_action(create_item(&cust, None, None)).unwrap();
    let ii = created_by(&m, &cause, ObjectType::InvoiceItem);
    let deletion = m
        .run_action(Action::DeleteInvoiceItem {
            id: Some(ii.clone()),
        })
        .unwrap();
    assert_eq!(events_of(&m, &deletion), ["invoiceitem.deleted"]);
    let tomb = m.get(ObjectType::InvoiceItem, &ii).unwrap();
    assert_eq!(tomb.kind, ChangeKind::Deleted);
    assert_eq!(
        tomb.state,
        json!({"id": ii, "object": "invoiceitem", "deleted": true})
    );
    // Deleted again: a 409 naming it.
    match m.run_action(Action::DeleteInvoiceItem {
        id: Some(ii.clone()),
    }) {
        Err(MockError::InvalidAction(msg)) => assert!(msg.contains(&ii), "{msg}"),
        other => panic!("{other:?}"),
    }
    // Nothing pending: a 409.
    assert!(matches!(
        m.run_action(Action::DeleteInvoiceItem { id: None }),
        Err(MockError::InvalidAction(_))
    ));

    let paid = m
        .run_action(Action::OneOffInvoice {
            customer: Some(cust.clone()),
            outcome: Some(InvoiceOutcome::Paid),
        })
        .unwrap();
    m.step(DAY).await.unwrap();
    let invoice = created_by(&m, &paid, ObjectType::Invoice);
    assert_ne!(state(&m, ObjectType::Invoice, &invoice)["status"], "draft");
    let on_invoice = created_by(&m, &paid, ObjectType::InvoiceItem);
    match m.run_action(Action::DeleteInvoiceItem {
        id: Some(on_invoice.clone()),
    }) {
        Err(MockError::InvalidAction(msg)) => assert!(msg.contains(&on_invoice), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        m.get(ObjectType::InvoiceItem, &on_invoice).unwrap().kind,
        ChangeKind::Created
    );
    // A missing customer: a 409 naming it.
    match m.run_action(create_item("cus_missing", None, None)) {
        Err(MockError::InvalidAction(msg)) => assert!(msg.contains("cus_missing"), "{msg}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn deleting_an_item_on_a_draft_takes_its_line() {
    // The draft waits a minute before it is finalized.
    let m = mock_with("seed = 5\n[actions]\nstep_delay = { distribution = \"fixed\", ms = 60000 }")
        .await;
    let cause = m
        .run_action(Action::OneOffInvoice {
            customer: None,
            outcome: Some(InvoiceOutcome::Paid),
        })
        .unwrap();
    let invoice = created_by(&m, &cause, ObjectType::Invoice);
    let before = state(&m, ObjectType::Invoice, &invoice);
    assert_eq!(before["status"], "draft");
    let line = lines(&before)[0].clone();
    let ii = line_item(&line).unwrap();
    let deletion = m
        .run_action(Action::DeleteInvoiceItem {
            id: Some(ii.clone()),
        })
        .unwrap();
    assert_eq!(
        events_of(&m, &deletion),
        ["invoiceitem.deleted", "invoice.updated"]
    );
    let line_id = line["id"].as_str().unwrap();
    assert_eq!(
        m.get(ObjectType::InvoiceLineItem, line_id).unwrap().kind,
        ChangeKind::Deleted
    );
    let after = state(&m, ObjectType::Invoice, &invoice);
    let left = lines(&after);
    assert_eq!(left.len(), lines(&before).len() - 1);
    let want = before["total"].as_i64().unwrap() - line["amount"].as_i64().unwrap();
    for field in ["subtotal", "total", "amount_due", "amount_remaining"] {
        assert_eq!(after[field], want, "{field}");
    }
    check_payload(&after).unwrap();
}

#[tokio::test]
async fn customer_deletion_deletes_pending_items() {
    let m = mock_with("seed = 4").await;
    let cust = m.create_customer(Default::default()).unwrap();
    let items: Vec<String> = (0..2)
        .map(|_| {
            let cause = m.run_action(create_item(&cust, None, None)).unwrap();
            created_by(&m, &cause, ObjectType::InvoiceItem)
        })
        .collect();
    m.delete_customer(&cust).unwrap();
    for ii in &items {
        assert_eq!(
            m.get(ObjectType::InvoiceItem, ii).unwrap().kind,
            ChangeKind::Deleted
        );
    }
    let customer_deleted = m
        .truth_log()
        .into_iter()
        .find(|u| u.object_id == cust && u.kind == ChangeKind::Deleted)
        .unwrap();
    let events = events_of(&m, customer_deleted.cause.as_ref().unwrap());
    let last_item = events
        .iter()
        .rposition(|e| e == "invoiceitem.deleted")
        .unwrap();
    let customer = events.iter().position(|e| e == "customer.deleted").unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| *e == "invoiceitem.deleted")
            .count(),
        2
    );
    assert!(last_item < customer, "{events:?}");
    // A deleted customer: a 409 naming it.
    match m.run_action(create_item(&cust, None, None)) {
        Err(MockError::InvalidAction(msg)) => assert!(msg.contains(&cust), "{msg}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn oversized_pending_item_stays_pending() {
    let m = mock_with("seed = 4").await;
    let (big, _) = prices(&m, Some(MAX_CHARGE_AMOUNT));
    let currency = state(&m, ObjectType::Price, &big)["currency"].clone();
    let cust = m.create_customer(Default::default()).unwrap();
    let cause = m
        .run_action(create_item(&cust, Some(&big), Some(1)))
        .unwrap();
    let ii = created_by(&m, &cause, ObjectType::InvoiceItem);
    assert_eq!(
        state(&m, ObjectType::InvoiceItem, &ii)["amount"],
        MAX_CHARGE_AMOUNT
    );
    let mut same_currency = 0;
    for _ in 0..30 {
        let one_off = m
            .run_action(Action::OneOffInvoice {
                customer: Some(cust.clone()),
                outcome: Some(InvoiceOutcome::Paid),
            })
            .unwrap();
        m.step(DAY).await.unwrap();
        let invoice = state(
            &m,
            ObjectType::Invoice,
            &created_by(&m, &one_off, ObjectType::Invoice),
        );
        assert!(invoice["subtotal"].as_i64().unwrap() <= MAX_CHARGE_AMOUNT);
        assert!(
            lines(&invoice)
                .iter()
                .all(|l| line_item(l).as_deref() != Some(ii.as_str()))
        );
        assert_eq!(
            state(&m, ObjectType::InvoiceItem, &ii)["invoice"],
            Value::Null
        );
        if invoice["currency"] == currency && invoice["subtotal"].as_i64() > Some(0) {
            same_currency += 1;
            if same_currency == 2 {
                return;
            }
        }
    }
    panic!("no one-off invoice in {currency} with a positive subtotal");
}

#[tokio::test]
async fn checkout_subscriptions_leave_pending_items_pending() {
    let m = mock_with("seed = 22").await;
    common::recurring_prices(&m, 1, "month");
    let cust = m.create_customer(Default::default()).unwrap();
    // The session opens now and completes about 30 minutes later; its amount is fixed now.
    let cause = m
        .run_action(Action::Checkout {
            customer: Some(cust.clone()),
            mode: Some(ductkit_stripe_mock::CheckoutMode::Subscription),
            complete: Some(true),
        })
        .unwrap();
    let cs = created_by(&m, &cause, ObjectType::CheckoutSession);
    let currency = state(&m, ObjectType::CheckoutSession, &cs)["currency"].clone();
    // A pending item in the session's currency, created while the session is open.
    let price = (0..300)
        .find_map(|_| {
            let mut ids: Vec<String> = m
                .truth_log()
                .into_iter()
                .filter(|u| u.object_type == ObjectType::Price)
                .map(|u| u.object_id)
                .collect();
            ids.dedup();
            let found = ids.into_iter().find(|id| {
                let p = state(&m, ObjectType::Price, id);
                p["active"] == true
                    && p["type"] == "one_time"
                    && p["currency"] == currency
                    && p["unit_amount"].as_i64() > Some(0)
                    && p["unit_amount"].as_i64() < Some(MAX_CHARGE_AMOUNT)
            });
            if found.is_none() {
                m.run_action(Action::CatalogChange).unwrap();
            }
            found
        })
        .expect("a one-time price in the session currency");
    let item_cause = m
        .run_action(create_item(&cust, Some(&price), Some(1)))
        .unwrap();
    let ii = created_by(&m, &item_cause, ObjectType::InvoiceItem);
    m.step(DAY).await.unwrap();
    let session = state(&m, ObjectType::CheckoutSession, &cs);
    assert_eq!(session["status"], "complete");
    let invoice = state(
        &m,
        ObjectType::Invoice,
        session["invoice"].as_str().unwrap(),
    );
    assert_eq!(invoice["currency"], currency);
    assert_eq!(invoice["amount_paid"], session["amount_total"]);
    assert!(lines(&invoice).iter().all(|l| line_item(l).is_none()));
    assert_eq!(
        state(&m, ObjectType::InvoiceItem, &ii)["invoice"],
        Value::Null
    );
}

#[tokio::test]
async fn named_quantities_stay_within_the_maximum_charge() {
    let m = mock_with("seed = 4").await;
    let (price, _) = prices(&m, None);
    let cust = m.create_customer(Default::default()).unwrap();
    for q in [u64::MAX, i64::MAX as u64, MAX_CHARGE_AMOUNT as u64 + 1] {
        match m.run_action(create_item(&cust, Some(&price), Some(q))) {
            Err(MockError::InvalidAction(msg)) => {
                assert!(msg.contains(&q.to_string()), "{msg}")
            }
            other => panic!("{q}: {other:?}"),
        }
        // With a drawn price too.
        match m.run_action(create_item(&cust, None, Some(q))) {
            Err(MockError::InvalidAction(msg)) => {
                assert!(msg.contains(&q.to_string()), "{msg}")
            }
            other => panic!("{q} with a drawn price: {other:?}"),
        }
    }
    assert!(
        m.truth_log()
            .iter()
            .all(|u| u.object_type != ObjectType::InvoiceItem)
    );
}

#[tokio::test]
async fn generated_items_and_their_churn() {
    let m =
        mock_with("seed = 11\n[actions]\nnew_customer = 5.0\n[churn.invoiceitem]\ndelete = 2.0")
            .await;
    // Turned on at runtime: `set_actions_config` carries the rate.
    m.update_actions_config(|a| a.invoice_item = 10.0).unwrap();
    m.step(2 * DAY).await.unwrap();
    let created: Vec<StateUpdate> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::InvoiceItem && u.kind == ChangeKind::Created)
        .collect();
    assert!(created.len() > 5, "{} items", created.len());
    for u in &created {
        assert_eq!(u.cause.as_ref().unwrap().action, "invoice_item");
        assert_eq!(u.state["invoice"], Value::Null);
        assert!((1..=3).contains(&u.state["quantity"].as_i64().unwrap()));
        check_payload(&u.state).unwrap();
    }
    let deleted = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::InvoiceItem && u.kind == ChangeKind::Deleted)
        .count();
    assert!(deleted > 0);
}
