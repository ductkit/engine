//! Tax rates on subscriptions and invoices (phase 9, Task 6, rulings Q3 and Q5).

mod common;

use std::time::Duration;

use common::{mock_with, recurring_prices};
use ductkit_stripe_mock::stripe::check_payload;
use ductkit_stripe_mock::{
    Action, Cause, ChangeKind, CheckoutMode, InvoiceOutcome, MockStripe, ObjectType, StateUpdate,
};
use serde_json::{Value, json};

const DAY: Duration = Duration::from_secs(86_400);
/// `payments::MAX_CHARGE_AMOUNT`.
const MAX_CHARGE_AMOUNT: i64 = 99_999_999;

fn entries(m: &MockStripe, cause: &Cause) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(cause))
        .collect()
}

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
fn latest(m: &MockStripe, t: ObjectType) -> Vec<StateUpdate> {
    let mut ids: Vec<String> = Vec::new();
    for u in m.truth_log() {
        if u.object_type == t && !ids.contains(&u.object_id) {
            ids.push(u.object_id);
        }
    }
    ids.iter().map(|id| m.get(t, id).unwrap()).collect()
}

fn active_rates(m: &MockStripe) -> Vec<String> {
    latest(m, ObjectType::TaxRate)
        .into_iter()
        .filter(|u| u.state["active"] == true)
        .map(|u| u.object_id)
        .collect()
}

/// Runs `BillingCatalogChange` until an active tax rate exists.
fn ensure_rate(m: &MockStripe) {
    for _ in 0..1000 {
        if !active_rates(m).is_empty() {
            return;
        }
        m.run_action(Action::BillingCatalogChange).unwrap();
    }
    panic!("no active tax rate after 1000 billing catalog changes");
}

/// The rate's basis points (ruling Q3): `8.875` is 888.
fn bp(rate: &Value) -> i64 {
    (rate["percentage"].as_f64().unwrap() * 100.0).round() as i64
}

/// The invoices of `sub`, in creation order.
fn sub_invoices(m: &MockStripe, sub: &str) -> Vec<Value> {
    latest(m, ObjectType::Invoice)
        .into_iter()
        .filter(|u| u.kind != ChangeKind::Deleted)
        .map(|u| u.state)
        .filter(|i| i["parent"]["subscription_details"]["subscription"] == sub)
        .collect()
}

/// The succeeded charge that paid `invoice`, found through its invoice payment.
fn paying_charge(m: &MockStripe, invoice: &str) -> Option<Value> {
    let intents: Vec<String> = latest(m, ObjectType::InvoicePayment)
        .into_iter()
        .filter(|u| u.state["invoice"] == invoice && u.state["status"] == "paid")
        .filter_map(|u| {
            u.state["payment"]["payment_intent"]
                .as_str()
                .map(str::to_string)
        })
        .collect();
    latest(m, ObjectType::Charge)
        .into_iter()
        .map(|u| u.state)
        .find(|c| {
            c["status"] == "succeeded"
                && c["payment_intent"]
                    .as_str()
                    .is_some_and(|pi| intents.iter().any(|i| i == pi))
        })
}

/// Checks ruling Q3's tax arithmetic on an invoice taxed with `rate`.
fn assert_taxed(invoice: &Value, rate: &Value) {
    check_payload(invoice).unwrap();
    let bp = bp(rate);
    let mut tax = 0;
    for line in invoice["lines"]["data"].as_array().unwrap() {
        let discount: i64 = line["discount_amounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["amount"].as_i64().unwrap())
            .sum();
        let taxable = line["amount"].as_i64().unwrap() - discount;
        let entry = &line["taxes"][0];
        assert_eq!(line["taxes"].as_array().unwrap().len(), 1, "{line}");
        assert_eq!(entry["taxable_amount"], taxable, "{line}");
        assert_eq!(entry["amount"], (taxable * bp + 5_000) / 10_000, "{line}");
        assert_eq!(entry["tax_rate_details"]["tax_rate"], rate["id"]);
        assert_eq!(entry["tax_behavior"], "exclusive");
        assert_eq!(entry["taxability_reason"], "standard_rated");
        assert_eq!(entry["type"], "tax_rate_details");
        tax += entry["amount"].as_i64().unwrap();
    }
    let totals = invoice["total_taxes"].as_array().unwrap();
    assert_eq!(totals.len(), 1, "{invoice}");
    assert_eq!(totals[0]["tax_rate_details"]["tax_rate"], rate["id"]);
    if invoice["lines"]["has_more"] == false {
        assert_eq!(totals[0]["amount"], tax);
    }
    let sum: i64 = totals.iter().map(|t| t["amount"].as_i64().unwrap()).sum();
    let excl = invoice["total_excluding_tax"].as_i64().unwrap();
    assert_eq!(invoice["total"], excl + sum);
    assert_eq!(invoice["amount_due"], invoice["total"]);
}

/// Starts subscriptions until one has `default_tax_rates` (40 tries); returns it with
/// its embedded rate.
async fn taxed_subscription(m: &MockStripe) -> (String, Value) {
    let prices = recurring_prices(m, 1, "month");
    for _ in 0..40 {
        let cause = m
            .run_action(Action::StartSubscription {
                customer: None,
                prices: Some(prices.clone()),
            })
            .unwrap();
        let sub = created_by(m, &cause, ObjectType::Subscription);
        let rates = state(m, ObjectType::Subscription, &sub)["default_tax_rates"].clone();
        if let Some(rate) = rates.as_array().unwrap().first() {
            assert_eq!(rates.as_array().unwrap().len(), 1);
            return (sub, rate.clone());
        }
    }
    panic!("no subscription took a tax rate in 40 starts");
}

#[tokio::test]
async fn taxed_subscription_invoices() {
    let m = mock_with("seed = 4").await;
    ensure_rate(&m);
    let (sub, rate) = taxed_subscription(&m).await;
    assert_eq!(rate["object"], "tax_rate");
    assert_eq!(rate["active"], true);
    assert_eq!(rate["inclusive"], false);
    check_payload(&state(&m, ObjectType::Subscription, &sub)).unwrap();
    // Items carry no rate of their own (ruling Q5).
    for item in latest(&m, ObjectType::SubscriptionItem)
        .into_iter()
        .filter(|u| u.state["subscription"] == sub.as_str())
    {
        assert_eq!(item.state["tax_rates"], json!([]));
    }
    m.step(DAY).await.unwrap();
    m.step(32 * DAY).await.unwrap();
    let invoices = sub_invoices(&m, &sub);
    assert!(invoices.len() >= 2, "{invoices:?}");
    let mut charged = 0;
    for invoice in &invoices {
        assert_taxed(invoice, &rate);
        // A subscription invoice's rate comes from its subscription.
        assert_eq!(invoice["default_tax_rates"], json!([]));
        if invoice["status"] == "paid" && invoice["amount_due"].as_i64() > Some(0) {
            let charge = paying_charge(&m, invoice["id"].as_str().unwrap())
                .unwrap_or_else(|| panic!("no charge for {invoice}"));
            assert_eq!(charge["amount"], invoice["amount_due"]);
            assert_eq!(invoice["amount_paid"], invoice["amount_due"]);
            charged += 1;
        }
    }
    assert!(charged > 0, "no taxed invoice was paid");
}

#[tokio::test]
async fn archived_rate_stays_embedded_and_is_not_reused() {
    let m = mock_with("seed = 5").await;
    ensure_rate(&m);
    let (sub, rate) = taxed_subscription(&m).await;
    let id = rate["id"].as_str().unwrap().to_string();
    m.step(DAY).await.unwrap();
    // Archive every active rate, the embedded one first.
    m.run_action(Action::Archive { id: id.clone() }).unwrap();
    for other in active_rates(&m) {
        m.run_action(Action::Archive { id: other }).unwrap();
    }
    assert_eq!(state(&m, ObjectType::TaxRate, &id)["active"], false);
    // The subscription keeps its snapshot (ruling Q5).
    let s = state(&m, ObjectType::Subscription, &sub);
    assert_eq!(s["default_tax_rates"][0]["id"], id.as_str());
    assert_eq!(s["default_tax_rates"][0]["active"], true);
    // Its next invoice is still taxed.
    let before = sub_invoices(&m, &sub).len();
    m.step(32 * DAY).await.unwrap();
    let invoices = sub_invoices(&m, &sub);
    assert!(invoices.len() > before, "no renewal invoice");
    assert_taxed(invoices.last().unwrap(), &rate);
    // No new subscription or one-off invoice takes an archived rate.
    let prices = recurring_prices(&m, 1, "month");
    for _ in 0..20 {
        let cause = m
            .run_action(Action::StartSubscription {
                customer: None,
                prices: Some(prices.clone()),
            })
            .unwrap();
        let new = created_by(&m, &cause, ObjectType::Subscription);
        assert_eq!(
            state(&m, ObjectType::Subscription, &new)["default_tax_rates"],
            json!([])
        );
        let cause = m
            .run_action(Action::OneOffInvoice {
                customer: None,
                outcome: Some(InvoiceOutcome::DeleteDraft),
            })
            .unwrap();
        // The draft as created: it may be deleted by now.
        let invoice = entries(&m, &cause)
            .into_iter()
            .find(|u| u.object_type == ObjectType::Invoice && u.kind == ChangeKind::Created)
            .unwrap()
            .state;
        assert_eq!(invoice["default_tax_rates"], json!([]));
        assert_eq!(invoice["total_taxes"], json!([]));
    }
}

#[tokio::test]
async fn one_off_invoices_store_their_rate() {
    let m = mock_with("seed = 6").await;
    ensure_rate(&m);
    for _ in 0..40 {
        let cause = m
            .run_action(Action::OneOffInvoice {
                customer: None,
                outcome: Some(InvoiceOutcome::Paid),
            })
            .unwrap();
        let id = created_by(&m, &cause, ObjectType::Invoice);
        let invoice = state(&m, ObjectType::Invoice, &id);
        let Some(rate) = invoice["default_tax_rates"].as_array().unwrap().first() else {
            assert_eq!(invoice["total_taxes"], json!([]));
            continue;
        };
        let rate = rate.clone();
        assert_eq!(rate["active"], true);
        assert_taxed(&invoice, &rate);
        m.step(DAY).await.unwrap();
        let invoice = state(&m, ObjectType::Invoice, &id);
        if invoice["status"] == "paid" && invoice["amount_due"].as_i64() > Some(0) {
            let charge = paying_charge(&m, &id).expect("paid invoices have a charge");
            assert_eq!(charge["amount"], invoice["amount_due"]);
        }
        return;
    }
    panic!("no one-off invoice took a tax rate in 40 tries");
}

#[tokio::test]
async fn checkout_subscription_stores_a_rate_but_its_first_invoice_is_untaxed() {
    let m = mock_with("seed = 7").await;
    ensure_rate(&m);
    recurring_prices(&m, 1, "month");
    for _ in 0..40 {
        let cause = m
            .run_action(Action::Checkout {
                customer: Some(m.create_customer(Default::default()).unwrap()),
                mode: Some(CheckoutMode::Subscription),
                complete: Some(true),
            })
            .unwrap();
        let cs = created_by(&m, &cause, ObjectType::CheckoutSession);
        m.step(DAY).await.unwrap();
        let session = state(&m, ObjectType::CheckoutSession, &cs);
        let sub = session["subscription"].as_str().unwrap().to_string();
        let s = state(&m, ObjectType::Subscription, &sub);
        let Some(rate) = s["default_tax_rates"].as_array().unwrap().first() else {
            continue;
        };
        let rate = rate.clone();
        let first = sub_invoices(&m, &sub).remove(0);
        // The session fixed the first invoice's amount.
        assert_eq!(first["total_taxes"], json!([]));
        assert_eq!(first["amount_paid"], session["amount_total"]);
        // Its renewals are taxed.
        m.step(32 * DAY).await.unwrap();
        let invoices = sub_invoices(&m, &sub);
        assert!(invoices.len() >= 2, "no renewal invoice");
        assert_taxed(invoices.last().unwrap(), &rate);
        return;
    }
    panic!("no Checkout subscription took a tax rate in 40 tries");
}

#[tokio::test]
async fn tax_dropped_over_the_charge_limit() {
    let m = mock_with("seed = 4").await;
    ensure_rate(&m);
    // A one-time price of exactly the maximum charge, and no other active price: every
    // one-off line bills it, and only the first fits, so the subtotal is the maximum.
    let big = (0..300)
        .find_map(|_| {
            let found = latest(&m, ObjectType::Price).into_iter().find(|u| {
                u.state["active"] == true
                    && u.state["type"] == "one_time"
                    && u.state["unit_amount"] == MAX_CHARGE_AMOUNT
            });
            if found.is_none() {
                m.run_action(Action::CatalogChange).unwrap();
            }
            found.map(|u| u.object_id)
        })
        .expect("no maximum-charge one-time price after 300 catalog changes");
    for u in latest(&m, ObjectType::Price) {
        if u.state["active"] == true && u.object_id != big {
            m.run_action(Action::Archive { id: u.object_id }).unwrap();
        }
    }
    for _ in 0..40 {
        let cust = m.create_customer(Default::default()).unwrap();
        let cause = m
            .run_action(Action::OneOffInvoice {
                customer: Some(cust),
                outcome: Some(InvoiceOutcome::Paid),
            })
            .unwrap();
        let id = created_by(&m, &cause, ObjectType::Invoice);
        let invoice = state(&m, ObjectType::Invoice, &id);
        assert_eq!(invoice["subtotal"], MAX_CHARGE_AMOUNT);
        if invoice["default_tax_rates"] == json!([]) {
            continue;
        }
        check_payload(&invoice).unwrap();
        assert_eq!(invoice["total_taxes"], json!([]));
        assert_eq!(invoice["total"], MAX_CHARGE_AMOUNT);
        assert_eq!(invoice["amount_due"], MAX_CHARGE_AMOUNT);
        for line in invoice["lines"]["data"].as_array().unwrap() {
            assert_eq!(line["taxes"], json!([]));
        }
        return;
    }
    panic!("no one-off invoice took a tax rate in 40 tries");
}

#[tokio::test]
async fn deleting_an_item_from_a_taxed_draft_keeps_its_snapshot_rate() {
    // Drafts wait a minute before they are finalized.
    let m = mock_with("seed = 8\n[actions]\nstep_delay = { distribution = \"fixed\", ms = 60000 }")
        .await;
    ensure_rate(&m);
    // A monthly price and a paid one-time price in the same currency.
    let mut found = None;
    for _ in 0..300 {
        let live: Vec<StateUpdate> = latest(&m, ObjectType::Price)
            .into_iter()
            .filter(|u| u.state["active"] == true)
            .collect();
        found = live.iter().find_map(|r| {
            if r.state["recurring"]["interval"] != "month" {
                return None;
            }
            live.iter()
                .find(|p| {
                    p.state["type"] == "one_time"
                        && p.state["currency"] == r.state["currency"]
                        && p.state["unit_amount"]
                            .as_i64()
                            .is_some_and(|a| (1..=1999).contains(&a))
                })
                .map(|p| (r.object_id.clone(), p.object_id.clone()))
        });
        if found.is_some() {
            break;
        }
        m.run_action(Action::CatalogChange).unwrap();
    }
    let (recurring, one_time) = found.expect("a monthly and a paid one-time price");
    for _ in 0..40 {
        let cust = m.create_customer(Default::default()).unwrap();
        let mut items = Vec::new();
        for quantity in [3, 1] {
            let cause = m
                .run_action(Action::CreateInvoiceItem {
                    customer: Some(cust.clone()),
                    price: Some(one_time.clone()),
                    quantity: Some(quantity),
                })
                .unwrap();
            items.push(created_by(&m, &cause, ObjectType::InvoiceItem));
        }
        let start = m
            .run_action(Action::StartSubscription {
                customer: Some(cust.clone()),
                prices: Some(vec![recurring.clone()]),
            })
            .unwrap();
        let sub = created_by(&m, &start, ObjectType::Subscription);
        let Some(rate) = state(&m, ObjectType::Subscription, &sub)["default_tax_rates"]
            .as_array()
            .unwrap()
            .first()
            .cloned()
        else {
            continue;
        };
        m.step(Duration::from_secs(61)).await.unwrap();
        let invoice = created_by(&m, &start, ObjectType::Invoice);
        let before = state(&m, ObjectType::Invoice, &invoice);
        assert_eq!(before["status"], "draft");
        assert_eq!(before["lines"]["data"].as_array().unwrap().len(), 3);
        assert_taxed(&before, &rate);
        // Archiving the rate does not untax the draft.
        let id = rate["id"].as_str().unwrap().to_string();
        m.run_action(Action::Archive { id }).unwrap();
        m.run_action(Action::DeleteInvoiceItem {
            id: Some(items[0].clone()),
        })
        .unwrap();
        let after = state(&m, ObjectType::Invoice, &invoice);
        assert_eq!(after["lines"]["data"].as_array().unwrap().len(), 2);
        assert!(after["subtotal"].as_i64() < before["subtotal"].as_i64());
        assert_taxed(&after, &rate);
        // The stored lines carry the new tax too.
        for line in after["lines"]["data"].as_array().unwrap() {
            let id = line["id"].as_str().unwrap();
            assert_eq!(
                state(&m, ObjectType::InvoiceLineItem, id)["taxes"],
                line["taxes"]
            );
        }
        return;
    }
    panic!("no subscription took a tax rate in 40 starts");
}
