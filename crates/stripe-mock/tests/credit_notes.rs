//! Credit notes (phase 9, Task 7, ruling Q8): pre-payment notes on open invoices,
//! post-payment notes refunding paid ones, their line items, voiding and numbering.

mod common;

use std::time::Duration;

use common::{get, mock_with};
use ductkit_stripe_mock::stripe::check_payload;
use ductkit_stripe_mock::{
    Action, Cause, ChangeKind, MockError, MockStripe, ObjectType, StateUpdate,
};
use serde_json::Value;

const DAY: Duration = Duration::from_secs(86_400);

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

/// The newest state of every object of `t` in the log, in first-seen order.
fn latest(m: &MockStripe, t: ObjectType) -> Vec<Value> {
    let mut ids: Vec<String> = Vec::new();
    for u in m.truth_log() {
        if u.object_type == t && !ids.contains(&u.object_id) {
            ids.push(u.object_id);
        }
    }
    ids.iter().map(|id| m.get(t, id).unwrap().state).collect()
}

/// The succeeded charges paying `invoice`, found through its paid invoice payment.
fn paying_charge(m: &MockStripe, invoice: &str) -> Value {
    let intent = latest(m, ObjectType::InvoicePayment)
        .into_iter()
        .find(|p| p["invoice"] == invoice && p["status"] == "paid")
        .unwrap_or_else(|| panic!("{invoice} has no paid invoice payment"))["payment"]
        ["payment_intent"]
        .as_str()
        .unwrap()
        .to_string();
    latest(m, ObjectType::Charge)
        .into_iter()
        .find(|c| c["status"] == "succeeded" && c["payment_intent"] == intent.as_str())
        .unwrap_or_else(|| panic!("{intent} has no succeeded charge"))
}

fn credit(invoice: &str, amount: Option<i64>) -> Action {
    Action::CreditNote {
        invoice: Some(invoice.to_string()),
        amount,
    }
}

fn void(id: &str) -> Action {
    Action::VoidCreditNote {
        id: Some(id.to_string()),
    }
}

fn expect_409(m: &MockStripe, a: Action, needle: &str) {
    match m.run_action(a) {
        Err(MockError::InvalidAction(msg)) => assert!(msg.contains(needle), "{msg}"),
        other => panic!("expected a 409 naming {needle}: {other:?}"),
    }
}

/// Runs catalog changes until `n` active monthly prices with a non-zero amount share a
/// currency; returns their IDs.
fn paying_prices(m: &MockStripe, n: usize) -> Vec<String> {
    for _ in 0..400 {
        let mut groups: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for p in latest(m, ObjectType::Price) {
            if p["active"] == true
                && p["recurring"]["interval"] == "month"
                && p["unit_amount"].as_i64().is_some_and(|a| a > 0)
            {
                groups
                    .entry(p["currency"].as_str().unwrap().to_string())
                    .or_default()
                    .push(p["id"].as_str().unwrap().to_string());
            }
        }
        if let Some(ids) = groups.values().find(|ids| ids.len() >= n) {
            return ids[..n].to_vec();
        }
        m.run_action(Action::CatalogChange).unwrap();
    }
    panic!("no {n} paying monthly prices after 400 catalog changes");
}

/// Starts a subscription on `n` monthly prices and steps a day, so its first invoice is
/// paid; returns `(subscription, first invoice)`.
async fn paid_subscription(m: &MockStripe, n: usize) -> (String, String) {
    let prices = paying_prices(m, n);
    let start = m
        .run_action(Action::StartSubscription {
            customer: None,
            prices: Some(prices),
        })
        .unwrap();
    let sub = created_by(m, &start, ObjectType::Subscription);
    m.step(DAY).await.unwrap();
    let invoice = created_by(m, &start, ObjectType::Invoice);
    assert_eq!(state(m, ObjectType::Invoice, &invoice)["status"], "paid");
    (sub, invoice)
}

/// An open invoice: a subscription whose first renewal fails
/// (`payment_failure_probability = 1.0`). The failed renewal stays `open` until its dunning
/// retry, three days later. Returns `(subscription, renewal invoice)`.
async fn open_invoice(m: &MockStripe) -> (String, String) {
    let (sub, first) = paid_subscription(m, 1).await;
    m.update_actions_config(|a| a.payment_failure_probability = 1.0)
        .unwrap();
    for _ in 0..40 {
        m.step(DAY).await.unwrap();
        let renewal = state(m, ObjectType::Subscription, &sub)["latest_invoice"]
            .as_str()
            .unwrap()
            .to_string();
        if renewal != first {
            let inv = state(m, ObjectType::Invoice, &renewal);
            assert_eq!(inv["status"], "open", "{inv}");
            assert!(inv["amount_remaining"].as_i64().unwrap() > 300, "{inv}");
            return (sub, renewal);
        }
    }
    panic!("{sub} did not renew in 40 days");
}

/// The line items of `note` from its embedded `lines` (every one on the page), checked
/// against its amount.
fn assert_lines_sum(note: &Value) {
    assert_eq!(note["lines"]["has_more"], false, "{note}");
    assert_line_items(note, note["lines"]["data"].as_array().unwrap());
}

fn assert_line_items(note: &Value, lines: &[Value]) {
    check_payload(note).unwrap();
    let sum: i64 = lines.iter().map(|l| l["amount"].as_i64().unwrap()).sum();
    assert_eq!(sum, note["amount"].as_i64().unwrap(), "{note}");
    for l in lines {
        assert_eq!(l["object"], "credit_note_line_item");
        assert_eq!(l["type"], "invoice_line_item");
        assert!(l["invoice_line_item"].as_str().unwrap().starts_with("il_"));
        assert_eq!(l["taxes"], serde_json::json!([]));
        assert_eq!(l["tax_rates"], serde_json::json!([]));
        assert!(l["quantity"].is_null() && l["unit_amount"].is_null(), "{l}");
    }
}

#[tokio::test]
async fn pre_payment_note_reduces_amount_due() {
    let m = mock_with("seed = 41").await;
    let (sub, invoice) = open_invoice(&m).await;
    let before = state(&m, ObjectType::Invoice, &invoice);
    let total = before["total"].as_i64().unwrap();
    let due = before["amount_due"].as_i64().unwrap();
    let cause = m.run_action(credit(&invoice, Some(100))).unwrap();
    let cn = created_by(&m, &cause, ObjectType::CreditNote);
    let note = state(&m, ObjectType::CreditNote, &cn);
    assert_eq!(note["type"], "pre_payment");
    assert_eq!(note["status"], "issued");
    assert_eq!(note["amount"], 100);
    assert_eq!(note["pre_payment_amount"], 100);
    assert_eq!(note["post_payment_amount"], 0);
    assert_eq!(
        (note["subtotal"].clone(), note["total"].clone()),
        (100.into(), 100.into())
    );
    assert_eq!(note["discount_amount"], 0);
    assert_eq!(note["invoice"], invoice.as_str());
    assert_eq!(note["customer"], before["customer"]);
    assert_eq!(note["refunds"], serde_json::json!([]));
    assert_eq!(
        note["pdf"],
        format!("https://pay.stripe.com/credit_notes/mock/{cn}/pdf")
    );
    assert!(note["reason"].is_string(), "{note}");
    assert_lines_sum(&note);
    let after = state(&m, ObjectType::Invoice, &invoice);
    assert_eq!(after["amount_due"], due - 100);
    assert_eq!(after["amount_remaining"], due - 100);
    assert_eq!(after["pre_payment_credit_notes_amount"], 100);
    assert_eq!(after["post_payment_credit_notes_amount"], 0);
    assert_eq!(after["total"], total);
    assert_eq!(
        events_of(&m, &cause),
        ["credit_note.created", "invoice.updated"]
    );
    // The line items come before the note, without events.
    let kinds: Vec<ObjectType> = entries(&m, &cause).iter().map(|u| u.object_type).collect();
    let note_at = kinds
        .iter()
        .position(|t| *t == ObjectType::CreditNote)
        .unwrap();
    assert!(
        kinds[..note_at]
            .iter()
            .all(|t| *t == ObjectType::CreditNoteLineItem)
    );
    assert!(note_at >= 1);
    // The dunning retry pays the reduced amount.
    m.update_actions_config(|a| a.payment_failure_probability = 0.0)
        .unwrap();
    m.step(4 * DAY).await.unwrap();
    let paid = state(&m, ObjectType::Invoice, &invoice);
    assert_eq!(paid["status"], "paid", "{paid}");
    assert_eq!(paid["amount_paid"], total - 100);
    assert_eq!(paying_charge(&m, &invoice)["amount"], total - 100);
    assert_eq!(
        state(&m, ObjectType::Subscription, &sub)["status"],
        "active"
    );
    // Its invoice is no longer open, so the note cannot be voided.
    expect_409(&m, void(&cn), &cn);
}

#[tokio::test]
async fn post_payment_note_refunds() {
    let m = mock_with("seed = 42").await;
    let (_, invoice) = paid_subscription(&m, 1).await;
    let charge = paying_charge(&m, &invoice);
    let ch = charge["id"].as_str().unwrap().to_string();
    let paid = charge["amount"].as_i64().unwrap();
    let a = paid / 2;
    assert!(a >= 1);
    let cause = m.run_action(credit(&invoice, Some(a))).unwrap();
    let cn = created_by(&m, &cause, ObjectType::CreditNote);
    let note = state(&m, ObjectType::CreditNote, &cn);
    assert_eq!(note["type"], "post_payment");
    assert_eq!(note["post_payment_amount"], a);
    assert_eq!(note["pre_payment_amount"], 0);
    assert_lines_sum(&note);
    let re = created_by(&m, &cause, ObjectType::Refund);
    let refund = state(&m, ObjectType::Refund, &re);
    assert_eq!(refund["charge"], ch.as_str());
    assert_eq!(refund["amount"], a);
    assert_eq!(note["refunds"][0]["refund"], re.as_str());
    assert_eq!(note["refunds"][0]["amount_refunded"], a);
    assert_eq!(note["refunds"][0]["type"], "refund");
    assert_eq!(note["refunds"].as_array().unwrap().len(), 1);
    // The refund comes first, so the note's `refunds` names an object that exists.
    assert_eq!(
        events_of(&m, &cause),
        [
            "refund.created",
            "charge.refunded",
            "credit_note.created",
            "invoice.updated"
        ]
    );
    let inv = state(&m, ObjectType::Invoice, &invoice);
    assert_eq!(inv["post_payment_credit_notes_amount"], a);
    assert_eq!(inv["pre_payment_credit_notes_amount"], 0);
    assert_eq!(inv["amount_paid"], paid);
    assert_eq!(state(&m, ObjectType::Charge, &ch)["amount_refunded"], a);
    // A second note refunds the rest, adding to both.
    m.run_action(credit(&invoice, Some(paid - a))).unwrap();
    let inv = state(&m, ObjectType::Invoice, &invoice);
    assert_eq!(inv["post_payment_credit_notes_amount"], paid);
    let c = state(&m, ObjectType::Charge, &ch);
    assert_eq!(
        (c["amount_refunded"].clone(), c["refunded"].clone()),
        (paid.into(), true.into())
    );
    // Fully refunded: nothing left to credit.
    expect_409(&m, credit(&invoice, None), &invoice);
}

#[tokio::test]
async fn void_restores_the_invoice() {
    let m = mock_with("seed = 43").await;
    let (_, invoice) = open_invoice(&m).await;
    let before = state(&m, ObjectType::Invoice, &invoice);
    let cause = m.run_action(credit(&invoice, Some(120))).unwrap();
    let cn = created_by(&m, &cause, ObjectType::CreditNote);
    let voided = m.run_action(void(&cn)).unwrap();
    let note = state(&m, ObjectType::CreditNote, &cn);
    check_payload(&note).unwrap();
    assert_eq!(note["status"], "void");
    assert_eq!(note["voided_at"], m.now().unix_seconds());
    assert_eq!(
        events_of(&m, &voided),
        ["credit_note.voided", "invoice.updated"]
    );
    let after = state(&m, ObjectType::Invoice, &invoice);
    for f in [
        "amount_due",
        "amount_remaining",
        "pre_payment_credit_notes_amount",
        "total",
    ] {
        assert_eq!(after[f], before[f], "{f}");
    }
    expect_409(&m, void(&cn), &cn);
    expect_409(&m, void("cn_missing"), "cn_missing");
}

#[tokio::test]
async fn only_open_invoice_notes_void() {
    let m = mock_with("seed = 44").await;
    let (_, invoice) = paid_subscription(&m, 1).await;
    let cause = m.run_action(credit(&invoice, Some(100))).unwrap();
    let cn = created_by(&m, &cause, ObjectType::CreditNote);
    expect_409(&m, void(&cn), &cn);
    assert_eq!(state(&m, ObjectType::CreditNote, &cn)["status"], "issued");
    // With no voidable note, an unnamed void is refused too.
    expect_409(&m, Action::VoidCreditNote { id: None }, "credit note");
}

#[tokio::test]
async fn over_credit_is_refused() {
    let m = mock_with("seed = 45").await;
    let (sub, open) = open_invoice(&m).await;
    let remaining = state(&m, ObjectType::Invoice, &open)["amount_remaining"]
        .as_i64()
        .unwrap();
    expect_409(&m, credit(&open, Some(remaining)), &open);
    expect_409(&m, credit(&open, Some(0)), &open);
    // The largest pre-payment credit leaves 1 to pay.
    m.run_action(credit(&open, Some(remaining - 1))).unwrap();
    assert_eq!(state(&m, ObjectType::Invoice, &open)["amount_remaining"], 1);
    // Nothing left to credit on it now.
    expect_409(&m, credit(&open, Some(1)), &open);
    expect_409(&m, credit(&open, None), &open);
    // A paid invoice: at most what is left to refund on its charge.
    let first = latest(&m, ObjectType::Invoice)
        .into_iter()
        .find(|i| i["parent"]["subscription_details"]["subscription"] == sub.as_str())
        .unwrap();
    let paid = first["id"].as_str().unwrap().to_string();
    let charged = paying_charge(&m, &paid)["amount"].as_i64().unwrap();
    expect_409(&m, credit(&paid, Some(charged + 1)), &paid);
    m.run_action(credit(&paid, Some(charged - 10))).unwrap();
    expect_409(&m, credit(&paid, Some(11)), &paid);
    m.run_action(credit(&paid, Some(10))).unwrap();
    expect_409(&m, credit(&paid, None), &paid);
    // An invoice that is neither open nor paid, and a missing one.
    expect_409(&m, credit("in_missing", None), "in_missing");
}

#[tokio::test]
async fn lines_sub_list_pages() {
    let m = mock_with("seed = 46").await;
    let (_, invoice) = paid_subscription(&m, 3).await;
    let inv = state(&m, ObjectType::Invoice, &invoice);
    let inv_lines: Vec<String> = inv["lines"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(inv_lines.len(), 3);
    let cause = m.run_action(credit(&invoice, Some(301))).unwrap();
    let cn = created_by(&m, &cause, ObjectType::CreditNote);
    let note = state(&m, ObjectType::CreditNote, &cn);
    assert_lines_sum(&note);
    let embedded = note["lines"]["data"].as_array().unwrap().clone();
    assert_eq!(embedded.len(), 3);
    assert_eq!(note["lines"]["url"], format!("/v1/credit_notes/{cn}/lines"));
    // One line item per invoice line, in the invoice's line order.
    let targets: Vec<&str> = embedded
        .iter()
        .map(|l| l["invoice_line_item"].as_str().unwrap())
        .collect();
    assert_eq!(targets, inv_lines);
    let r = get(&m, &format!("/v1/credit_notes/{cn}/lines?limit=2")).await;
    assert_eq!(r.status(), 200);
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["has_more"], true);
    let first = page["data"].as_array().unwrap().clone();
    assert_eq!(first, embedded[..2]);
    let last = first[1]["id"].as_str().unwrap();
    let r = get(
        &m,
        &format!("/v1/credit_notes/{cn}/lines?limit=2&starting_after={last}"),
    )
    .await;
    let page: Value = r.json().await.unwrap();
    assert_eq!(page["has_more"], false);
    assert_eq!(page["data"].as_array().unwrap()[..], embedded[2..]);
    // The note is served, and listed by invoice.
    let r = get(&m, &format!("/v1/credit_notes?invoice={invoice}")).await;
    let list: Value = r.json().await.unwrap();
    assert_eq!(list["data"][0]["id"], cn.as_str());
}

#[tokio::test]
async fn numbers_count_per_invoice() {
    let m = mock_with("seed = 47").await;
    let (_, invoice) = open_invoice(&m).await;
    let number = state(&m, ObjectType::Invoice, &invoice)["number"]
        .as_str()
        .unwrap()
        .to_string();
    let a = m.run_action(credit(&invoice, Some(100))).unwrap();
    let b = m.run_action(credit(&invoice, Some(100))).unwrap();
    let a = state(
        &m,
        ObjectType::CreditNote,
        &created_by(&m, &a, ObjectType::CreditNote),
    );
    let b = state(
        &m,
        ObjectType::CreditNote,
        &created_by(&m, &b, ObjectType::CreditNote),
    );
    assert_eq!(a["number"], format!("{number}-CN-01"));
    assert_eq!(b["number"], format!("{number}-CN-02"));
    assert_eq!(
        state(&m, ObjectType::Invoice, &invoice)["pre_payment_credit_notes_amount"],
        200
    );
}

#[tokio::test]
async fn generated_credit_notes_add_up() {
    let m = mock_with(
        "seed = 48
[actions]
new_customer = 2.0
start_subscription = 1.5
one_off_invoice = 2.0
credit_note = 3.0
payment_failure_probability = 0.3",
    )
    .await;
    m.step(25 * DAY).await.unwrap();
    let notes = latest(&m, ObjectType::CreditNote);
    assert!(
        notes.iter().any(|n| n["type"] == "pre_payment"),
        "no pre-payment note"
    );
    assert!(
        notes.iter().any(|n| n["type"] == "post_payment"),
        "no post-payment note"
    );
    for n in &notes {
        let id = n["id"].as_str().unwrap();
        let r = get(&m, &format!("/v1/credit_notes/{id}/lines?limit=100")).await;
        let all: Value = r.json().await.unwrap();
        assert_eq!(all["has_more"], false);
        assert_eq!(
            all["data"].as_array().unwrap().len() as i64,
            n["lines"]["total_count"].as_i64().unwrap()
        );
        assert_line_items(n, all["data"].as_array().unwrap());
        let amount = n["amount"].as_i64().unwrap();
        assert!(amount >= 1, "{n}");
        if n["type"] == "post_payment" {
            assert_eq!(n["refunds"][0]["amount_refunded"], amount);
        }
    }
    // Every invoice's credit amounts match its issued notes.
    for inv in latest(&m, ObjectType::Invoice) {
        if inv["deleted"] == true {
            continue;
        }
        let id = inv["id"].as_str().unwrap();
        let of = |kind: &str| -> i64 {
            notes
                .iter()
                .filter(|n| n["invoice"] == id && n["type"] == kind && n["status"] == "issued")
                .map(|n| n["amount"].as_i64().unwrap())
                .sum()
        };
        assert_eq!(
            inv["pre_payment_credit_notes_amount"],
            of("pre_payment"),
            "{id}"
        );
        assert_eq!(
            inv["post_payment_credit_notes_amount"],
            of("post_payment"),
            "{id}"
        );
        assert_eq!(
            inv["amount_due"].as_i64().unwrap(),
            inv["total"].as_i64().unwrap() - of("pre_payment"),
            "{id}"
        );
    }
    // The credit note list pages, filtered by customer.
    let cust = notes[0]["customer"].as_str().unwrap();
    let r = get(&m, &format!("/v1/credit_notes?customer={cust}")).await;
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn paging_credit_notes_updates_a_note() {
    let m = mock_with("seed = 49\n[api]\npagination_writes = { probability = 1.0 }").await;
    let (_, invoice) = paid_subscription(&m, 1).await;
    m.run_action(credit(&invoice, Some(10))).unwrap();
    m.run_action(credit(&invoice, Some(10))).unwrap();
    let r = get(&m, "/v1/credit_notes?limit=1").await;
    let first: Value = r.json().await.unwrap();
    assert_eq!(first["has_more"], true);
    let last = first["data"][0]["id"].as_str().unwrap().to_string();
    let before = m.truth_log().len();
    let r = get(
        &m,
        &format!("/v1/credit_notes?limit=1&starting_after={last}"),
    )
    .await;
    assert_eq!(r.status(), 200);
    let log = m.truth_log();
    assert_eq!(log.len(), before + 1, "one write between pages");
    let u = &log[before];
    assert_eq!(u.object_type, ObjectType::CreditNote);
    assert!(u.state["metadata"]["note"].is_string(), "{}", u.state);
    let timeline = m.event_timeline();
    let types: Vec<&str> = u
        .events
        .snapshot
        .iter()
        .filter_map(|id| timeline.iter().find(|e| &e.id == id))
        .map(|e| e.event_type.as_str())
        .collect();
    assert_eq!(types, ["credit_note.updated"]);
}
