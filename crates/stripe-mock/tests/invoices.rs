mod common;

use common::mock_with;
use ductkit_stripe_mock::{
    Action, ChangeKind, InvoiceOutcome, MockStripe, ObjectType, StateUpdate,
};
use serde_json::Value;

fn invoices(m: &MockStripe) -> Vec<StateUpdate> {
    let mut ids: Vec<String> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::Invoice && u.kind == ChangeKind::Created)
        .map(|u| u.object_id)
        .collect();
    ids.dedup();
    ids.iter()
        .map(|id| m.get(ObjectType::Invoice, id).unwrap())
        .collect()
}

fn one_off(customer: Option<String>, outcome: Option<InvoiceOutcome>) -> Action {
    Action::OneOffInvoice { customer, outcome }
}

#[tokio::test]
async fn one_off_invoice_reaches_an_outcome() {
    let m = mock_with("seed = 4").await;
    for _ in 0..40 {
        m.run_action(one_off(None, None)).unwrap();
    }
    let mut seen = std::collections::BTreeSet::new();
    for inv in invoices(&m) {
        let outcome = if inv.kind == ChangeKind::Deleted {
            "deleted".to_string()
        } else {
            inv.state["status"].as_str().unwrap().to_string()
        };
        assert!(
            ["deleted", "paid", "void", "uncollectible"].contains(&outcome.as_str()),
            "{outcome}"
        );
        if outcome == "paid" {
            assert_eq!(inv.state["amount_paid"], inv.state["total"]);
            let lines = inv.state["lines"]["data"].as_array().unwrap();
            if inv.state["lines"]["has_more"] == false {
                let sum: i64 = lines.iter().map(|l| l["amount"].as_i64().unwrap()).sum();
                assert_eq!(inv.state["total"], sum);
            }
        }
        seen.insert(outcome);
    }
    assert_eq!(seen.len(), 4, "{seen:?}");
}

#[tokio::test]
async fn finalize_numbers_from_customer() {
    let m = mock_with("").await;
    let cust = m.create_customer(Default::default()).unwrap();
    for _ in 0..2 {
        m.run_action(one_off(Some(cust.clone()), Some(InvoiceOutcome::Paid)))
            .unwrap();
    }
    let prefix = m.get(ObjectType::Customer, &cust).unwrap().state["invoice_prefix"]
        .as_str()
        .unwrap()
        .to_string();
    let numbers: Vec<Value> = invoices(&m)
        .into_iter()
        .map(|i| i.state["number"].clone())
        .collect();
    assert_eq!(
        numbers,
        [
            Value::from(format!("{prefix}-0001")),
            Value::from(format!("{prefix}-0002"))
        ]
    );
    let c = m.get(ObjectType::Customer, &cust).unwrap();
    assert_eq!(c.state["next_invoice_sequence"], 3);
    let bumps: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_id == cust && u.version > 1)
        .collect();
    assert_eq!(bumps.len(), 2);
    assert!(bumps.iter().all(|u| u.events.snapshot.is_empty()));
}

#[tokio::test]
async fn paid_announces_two_events() {
    let m = mock_with("").await;
    m.run_action(one_off(None, Some(InvoiceOutcome::Paid)))
        .unwrap();
    let paid = m
        .truth_log()
        .into_iter()
        .find(|u| u.object_type == ObjectType::Invoice && u.state["status"] == "paid")
        .unwrap();
    assert_eq!(paid.kind, ChangeKind::StateChanged);
    let types: Vec<String> = paid
        .events
        .snapshot
        .iter()
        .map(|id| {
            m.event_timeline()
                .into_iter()
                .find(|e| &e.id == id)
                .unwrap()
                .event_type
        })
        .collect();
    assert_eq!(types, ["invoice.paid", "invoice.payment_succeeded"]);
}

#[tokio::test]
async fn deleted_draft_takes_its_lines() {
    let m = mock_with("").await;
    m.run_action(one_off(None, Some(InvoiceOutcome::DeleteDraft)))
        .unwrap();
    let inv = invoices(&m).remove(0);
    assert_eq!(inv.kind, ChangeKind::Deleted);
    assert_eq!(inv.events.snapshot.len(), 1);
    let log = m.truth_log();
    let lines: Vec<_> = log
        .iter()
        .filter(|u| u.object_type == ObjectType::InvoiceLineItem)
        .collect();
    let created = lines
        .iter()
        .filter(|u| u.kind == ChangeKind::Created)
        .count();
    let deleted: Vec<_> = lines
        .iter()
        .filter(|u| u.kind == ChangeKind::Deleted)
        .collect();
    assert!(created > 0);
    assert_eq!(deleted.len(), created);
    assert!(deleted.iter().all(|u| u.events.snapshot.is_empty()));
    assert!(deleted.iter().all(|u| u.seq < inv.seq), "lines go first");
}

#[tokio::test]
async fn line_shape() {
    let m = mock_with("").await;
    m.run_action(one_off(None, Some(InvoiceOutcome::Paid)))
        .unwrap();
    let inv = invoices(&m).remove(0);
    let line = &inv.state["lines"]["data"][0];
    assert_eq!(line["object"], "line_item");
    assert!(
        line["pricing"]["price_details"]["price"]
            .as_str()
            .unwrap()
            .starts_with("price_")
    );
    assert_eq!(line["parent"]["type"], "invoice_item_details");
    assert_eq!(inv.state["parent"], Value::Null);
    assert_eq!(inv.state["billing_reason"], "manual");
    assert_eq!(
        inv.state["lines"]["url"],
        format!("/v1/invoices/{}/lines", inv.object_id)
    );
}
