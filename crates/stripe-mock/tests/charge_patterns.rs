//! Platform payments under the Connect destination-charge and separate-charges-and-transfers
//! patterns: transfers and `py_` payments (spec sections 4, 5.5 and 6.1; phase 8 rulings
//! P1, P2, P3 and P7).

mod common;

use std::time::Duration;

use common::{get, get_as, mock_with, recurring_prices};
use ductkit_stripe_mock::{
    AccountApi, Action, Cause, ChargePattern, MockError, MockStripe, ObjectType, StateUpdate,
};
use serde_json::Value;

const HOUR: u64 = 3600;
const DAY: u64 = 86_400;

/// Standard accounts get no setup objects, so the logs hold only the payment.
const STANDARD: &str = "[accounts]
connect = true
account_types = { standard = 1.0, express = 0.0, custom = 0.0 }
";

fn with_pattern(mix: &str) -> String {
    format!("{STANDARD}platform_payments = {mix}\n")
}

fn caused(m: &MockStripe, cause: &Cause) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(cause))
        .collect()
}

/// The one platform-scope object of type `t` in `updates`.
fn only_id(updates: &[StateUpdate], t: ObjectType) -> String {
    let mut ids: Vec<String> = updates
        .iter()
        .filter(|u| u.object_type == t && u.account.is_none())
        .map(|u| u.object_id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 1, "{t:?}: {ids:?}");
    ids.pop().unwrap()
}

/// The snapshot events of `updates`, in order: `(type, label)`.
fn events_of(m: &MockStripe, updates: &[StateUpdate]) -> Vec<(String, Option<String>)> {
    let timeline = m.event_timeline();
    updates
        .iter()
        .flat_map(|u| u.events.snapshot.clone())
        .map(|id| {
            let e = timeline.iter().find(|e| e.id == id).unwrap();
            (e.event_type.clone(), e.account.clone())
        })
        .collect()
}

async fn json(m: &MockStripe, path: &str) -> Value {
    let r = get(m, path).await;
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

async fn json_as(m: &MockStripe, path: &str, acct: &str) -> Value {
    let r = get_as(m, path, acct).await;
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

fn payment(amount: i64) -> Action {
    Action::OneOffPayment {
        customer: None,
        amount: Some(amount),
        succeed: Some(true),
        pattern: None,
        recipient: None,
    }
}

fn invalid_action(r: Result<impl std::fmt::Debug, MockError>) -> String {
    match r {
        Err(MockError::InvalidAction(m)) => m,
        other => panic!("expected invalid_action, got {other:?}"),
    }
}

fn transfers(m: &MockStripe) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::Transfer)
        .collect()
}

#[tokio::test]
async fn destination_charge_moves_money() {
    let m = mock_with(&with_pattern("{ destination = 1.0 }")).await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    let cause = m.run_action(payment(1000)).unwrap();
    let mine = caused(&m, &cause);
    let pi = only_id(&mine, ObjectType::PaymentIntent);
    let charge = only_id(&mine, ObjectType::Charge);
    let tr = only_id(&mine, ObjectType::Transfer);
    assert!(tr.starts_with("tr_"), "{tr}");
    let group = format!("group_{pi}");

    // The events, in order, all unlabelled platform events. The `py_` payment and the
    // balance transactions have none.
    let events = events_of(&m, &mine);
    assert_eq!(
        events,
        [
            ("payment_intent.created".to_string(), None),
            ("charge.succeeded".to_string(), None),
            ("payment_intent.succeeded".to_string(), None),
            ("transfer.created".to_string(), None),
        ]
    );

    // The intent and the charge carry the destination and the group.
    let intent = json(&m, &format!("/v1/payment_intents/{pi}")).await;
    assert_eq!(intent["transfer_data"]["destination"], acct.as_str());
    assert_eq!(intent["transfer_group"], group.as_str());
    let ch = json(&m, &format!("/v1/charges/{charge}")).await;
    assert_eq!(ch["transfer_data"]["destination"], acct.as_str());
    assert!(ch["transfer_data"]["amount"].is_null(), "{ch:#}");
    assert_eq!(ch["transfer_group"], group.as_str());
    assert_eq!(ch["transfer"], tr.as_str());
    assert!(ch["source_transfer"].is_null());
    // The charge's `transfer` is a silent update, in the same instant.
    let charge_updates: Vec<_> = mine
        .iter()
        .filter(|u| u.object_type == ObjectType::Charge && u.object_id == charge)
        .collect();
    assert_eq!(charge_updates.len(), 2);
    assert!(charge_updates[1].events.snapshot.is_empty());
    assert!(mine.iter().all(|u| u.at == mine[0].at));

    // The transfer.
    let t = json(&m, &format!("/v1/transfers/{tr}")).await;
    assert_eq!(t["object"], "transfer");
    assert_eq!(t["amount"], 1000);
    assert_eq!(t["amount_reversed"], 0);
    assert_eq!(t["reversed"], false);
    assert_eq!(t["currency"], ch["currency"]);
    assert_eq!(t["destination"], acct.as_str());
    assert_eq!(t["source_transaction"], charge.as_str());
    assert_eq!(t["source_type"], "card");
    assert_eq!(t["transfer_group"], group.as_str());
    assert_eq!(t["reversals"]["object"], "list");
    assert_eq!(t["reversals"]["data"], Value::Array(vec![]));
    assert_eq!(t["reversals"]["has_more"], false);
    assert_eq!(
        t["reversals"]["url"],
        format!("/v1/transfers/{tr}/reversals").as_str()
    );
    let py = t["destination_payment"].as_str().unwrap().to_string();
    assert!(py.starts_with("py_"), "{py}");
    let listed = json(&m, &format!("/v1/transfers?transfer_group={group}")).await;
    assert_eq!(listed["data"][0]["id"], tr.as_str());

    // The platform's balance transaction: `-amount`, available at once.
    let bt = json(
        &m,
        &format!(
            "/v1/balance_transactions/{}",
            t["balance_transaction"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(bt["type"], "transfer");
    assert_eq!(bt["amount"], -1000);
    assert_eq!(bt["fee"], 0);
    assert_eq!(bt["net"], -1000);
    assert_eq!(bt["status"], "available");
    assert_eq!(bt["source"], tr.as_str());

    // The `py_` payment lives in the account, found by listing its charges.
    let charges = json_as(&m, "/v1/charges", &acct).await;
    let ids: Vec<&str> = charges["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [py.as_str()]);
    let p = &charges["data"][0];
    assert_eq!(p["amount"], 1000);
    assert_eq!(p["status"], "succeeded");
    assert_eq!(p["paid"], true);
    assert_eq!(p["captured"], true);
    assert_eq!(p["source_transfer"], tr.as_str());
    assert_eq!(p["payment_method_details"]["type"], "stripe_account");
    for field in ["customer", "payment_intent", "payment_method", "outcome"] {
        assert!(p[field].is_null(), "{field}: {p:#}");
    }
    assert_eq!(
        get(&m, &format!("/v1/charges/{py}")).await.status(),
        404,
        "the py_ payment is not in the platform"
    );
    let pbt = json_as(
        &m,
        &format!(
            "/v1/balance_transactions/{}",
            p["balance_transaction"].as_str().unwrap()
        ),
        &acct,
    )
    .await;
    assert_eq!(pbt["type"], "payment");
    assert_eq!(pbt["amount"], 1000);
    assert_eq!(pbt["fee"], 0);
    assert_eq!(pbt["net"], 1000);
    assert_eq!(pbt["status"], "pending");
    assert_eq!(pbt["source"], py.as_str());
    assert_eq!(
        pbt["available_on"].as_i64().unwrap(),
        pbt["created"].as_i64().unwrap() + 2 * DAY as i64
    );
    let pbt_path = format!("/v1/balance_transactions/{}", pbt["id"].as_str().unwrap());
    assert!(m.event_timeline().iter().all(|e| e.object_id != py));
    for u in &mine {
        let in_account = u.object_id == py || u.object_id == pbt["id"].as_str().unwrap();
        assert_eq!(
            u.account.is_some(),
            in_account,
            "{:?} {}",
            u.object_type,
            u.object_id
        );
    }

    // Cross-scope expansion (P2): the platform expands the transfer's destination payment,
    // the account expands the payment's source transfer.
    let e = json(
        &m,
        &format!("/v1/transfers/{tr}?expand[]=destination_payment"),
    )
    .await;
    assert_eq!(e["destination_payment"]["object"], "charge");
    assert_eq!(e["destination_payment"]["id"], py.as_str());
    let e = json_as(
        &m,
        &format!("/v1/charges/{py}?expand[]=source_transfer"),
        &acct,
    )
    .await;
    assert_eq!(e["source_transfer"]["object"], "transfer");
    assert_eq!(e["source_transfer"]["id"], tr.as_str());
    // `destination` expanded first still finds the payment in its account.
    let e = json(
        &m,
        &format!("/v1/transfers/{tr}?expand[]=destination&expand[]=destination_payment"),
    )
    .await;
    assert_eq!(e["destination"]["id"], acct.as_str());
    assert_eq!(e["destination_payment"]["id"], py.as_str());
    let e = json(
        &m,
        &format!("/v1/charges/{charge}?expand[]=transfer&expand[]=transfer_data.destination"),
    )
    .await;
    assert_eq!(e["transfer"]["id"], tr.as_str());
    assert_eq!(e["transfer_data"]["destination"]["id"], acct.as_str());

    // The account's balance transaction settles two days later.
    m.step(Duration::from_secs(2 * DAY + 60)).await.unwrap();
    assert_eq!(json_as(&m, &pbt_path, &acct).await["status"], "available");
}

#[tokio::test]
async fn cross_scope_expansion_under_stale_reads() {
    let m = mock_with(&format!(
        "{}[api]\nstale_read = {{ probability = 1.0, max_lag = \"10s\" }}\n",
        with_pattern("{ destination = 1.0 }")
    ))
    .await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    let cause = m.run_action(payment(500)).unwrap();
    let tr = only_id(&caused(&m, &cause), ObjectType::Transfer);
    m.step(Duration::from_secs(60)).await.unwrap();
    for _ in 0..5 {
        let t = json(
            &m,
            &format!("/v1/transfers/{tr}?expand[]=destination_payment"),
        )
        .await;
        assert_eq!(t["destination_payment"]["object"], "charge");
        let py = t["destination_payment"]["id"].as_str().unwrap();
        let p = json_as(
            &m,
            &format!("/v1/charges/{py}?expand[]=source_transfer"),
            &acct,
        )
        .await;
        assert_eq!(p["source_transfer"]["id"], tr.as_str());
    }
}

#[tokio::test]
async fn separate_charge_transfers_later() {
    let m = mock_with(&with_pattern("{ separate = 1.0 }")).await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    let cause = m.run_action(payment(1000)).unwrap();
    let mine = caused(&m, &cause);
    let pi = only_id(&mine, ObjectType::PaymentIntent);
    let charge = only_id(&mine, ObjectType::Charge);
    let group = format!("group_{pi}");
    let paid_at = mine[0].at;

    let intent = json(&m, &format!("/v1/payment_intents/{pi}")).await;
    assert!(intent["transfer_data"].is_null());
    assert_eq!(intent["transfer_group"], group.as_str());
    let ch = json(&m, &format!("/v1/charges/{charge}")).await;
    assert!(ch["transfer_data"].is_null());
    assert_eq!(ch["transfer_group"], group.as_str());
    assert!(ch.get("transfer").is_none_or(Value::is_null), "{ch:#}");
    let by_group = json(&m, &format!("/v1/charges?transfer_group={group}")).await;
    assert_eq!(by_group["data"][0]["id"], charge.as_str());

    // Nothing moves for the first hour.
    assert!(transfers(&m).is_empty());
    m.step(Duration::from_secs(HOUR - 60)).await.unwrap();
    assert!(transfers(&m).is_empty());
    let path = format!("/v1/transfers?transfer_group={group}");
    assert_eq!(json(&m, &path).await["data"], Value::Array(vec![]));

    // Within 72 hours, the transfer, under the payment's cause.
    m.step(Duration::from_secs(72 * HOUR)).await.unwrap();
    let all = transfers(&m);
    assert_eq!(all.len(), 1);
    let created = &all[0];
    assert_eq!(created.cause.as_ref(), Some(&cause));
    assert!(created.account.is_none());
    let delay = created.at.0 - paid_at.0;
    assert!(
        (HOUR as i64 * 1000..=72 * HOUR as i64 * 1000).contains(&delay),
        "{delay}"
    );
    let listed = json(&m, &path).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    let t = &listed["data"][0];
    assert_eq!(t["amount"], 1000);
    assert_eq!(t["destination"], acct.as_str());
    assert_eq!(t["source_transaction"], charge.as_str());
    assert_eq!(t["transfer_group"], group.as_str());
    let py = t["destination_payment"].as_str().unwrap();
    let p = json_as(&m, &format!("/v1/charges/{py}"), &acct).await;
    assert_eq!(p["source_transfer"], t["id"]);
    assert_eq!(p["amount"], 1000);
    // The platform charge stays as it was: `transfer` is the destination pattern's.
    let ch = json(&m, &format!("/v1/charges/{charge}")).await;
    assert!(ch.get("transfer").is_none_or(Value::is_null), "{ch:#}");
    let types: Vec<String> = m
        .event_timeline()
        .into_iter()
        .filter(|e| e.event_type == "transfer.created")
        .map(|e| {
            assert_eq!(e.account, None);
            e.object_id
        })
        .collect();
    assert_eq!(types, [t["id"].as_str().unwrap()]);
}

#[tokio::test]
async fn separate_transfer_skipped_after_disconnect() {
    let m = mock_with(&with_pattern("{ separate = 1.0 }")).await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    let cause = m.run_action(payment(1000)).unwrap();
    let pi = only_id(&caused(&m, &cause), ObjectType::PaymentIntent);
    m.disconnect_account(&acct).unwrap();
    let before = m.truth_log().len();
    m.step(Duration::from_secs(73 * HOUR)).await.unwrap();
    assert!(transfers(&m).is_empty());
    let log = m.truth_log();
    assert!(
        log[before..]
            .iter()
            .all(|u| u.account.as_deref() != Some(&acct)),
        "nothing happens in a disconnected account's scope"
    );
    assert!(
        m.event_timeline()
            .iter()
            .all(|e| e.event_type != "transfer.created")
    );
    let path = format!("/v1/transfers?transfer_group=group_{pi}");
    assert_eq!(json(&m, &path).await["data"], Value::Array(vec![]));
}

#[tokio::test]
async fn no_connected_account_falls_back_to_own() {
    for mix in ["{ destination = 1.0 }", "{ separate = 1.0 }"] {
        let m = mock_with(&with_pattern(mix)).await;
        let cause = m.run_action(payment(1000)).unwrap();
        let mine = caused(&m, &cause);
        let pi = only_id(&mine, ObjectType::PaymentIntent);
        let charge = only_id(&mine, ObjectType::Charge);
        let intent = json(&m, &format!("/v1/payment_intents/{pi}")).await;
        assert!(intent["transfer_data"].is_null(), "{mix}");
        assert!(intent["transfer_group"].is_null(), "{mix}");
        let ch = json(&m, &format!("/v1/charges/{charge}")).await;
        assert!(ch["transfer_data"].is_null(), "{mix}");
        assert!(ch["transfer_group"].is_null(), "{mix}");
        m.step(Duration::from_secs(73 * HOUR)).await.unwrap();
        assert!(transfers(&m).is_empty(), "{mix}");
        // A pattern given for the payment needs a connected account.
        let msg = invalid_action(m.run_action(Action::OneOffPayment {
            customer: None,
            amount: Some(100),
            succeed: Some(true),
            pattern: Some(ChargePattern::Destination),
            recipient: None,
        }));
        assert!(msg.contains("connected account"), "{msg}");
    }
}

#[tokio::test]
async fn one_off_payment_pattern_fields() {
    // At the default mix, a given pattern skips the draw.
    let m = mock_with(STANDARD).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let b = m.connect_account(AccountApi::V1).unwrap();
    let cause = m
        .run_action(Action::OneOffPayment {
            customer: None,
            amount: Some(700),
            succeed: Some(true),
            pattern: Some(ChargePattern::Destination),
            recipient: Some(b.clone()),
        })
        .unwrap();
    let tr = only_id(&caused(&m, &cause), ObjectType::Transfer);
    let t = json(&m, &format!("/v1/transfers/{tr}")).await;
    assert_eq!(t["destination"], b.as_str());
    assert_eq!(t["amount"], 700);
    // Own is own.
    let cause = m
        .run_action(Action::OneOffPayment {
            customer: None,
            amount: Some(700),
            succeed: Some(true),
            pattern: Some(ChargePattern::Own),
            recipient: None,
        })
        .unwrap();
    assert!(
        caused(&m, &cause)
            .iter()
            .all(|u| u.object_type != ObjectType::Transfer)
    );
    // A recipient that is not connected, or either field in a connected scope, is a 409.
    m.disconnect_account(&a).unwrap();
    let msg = invalid_action(m.run_action(Action::OneOffPayment {
        customer: None,
        amount: Some(100),
        succeed: Some(true),
        pattern: Some(ChargePattern::Separate),
        recipient: Some(a.clone()),
    }));
    assert!(msg.contains(&a), "{msg}");
    invalid_action(m.run_action(Action::OneOffPayment {
        customer: None,
        amount: Some(100),
        succeed: Some(true),
        pattern: Some(ChargePattern::Destination),
        recipient: Some("acct_missing".to_string()),
    }));
    invalid_action(m.account(&b).run_action(Action::OneOffPayment {
        customer: None,
        amount: Some(100),
        succeed: Some(true),
        pattern: Some(ChargePattern::Own),
        recipient: None,
    }));
    invalid_action(m.account(&b).run_action(Action::OneOffPayment {
        customer: None,
        amount: Some(100),
        succeed: Some(true),
        pattern: None,
        recipient: Some(b.clone()),
    }));
    // Scripts written before the fields existed still parse.
    let old: Action =
        serde_json::from_str(r#"{"type":"one_off_payment","customer":null,"amount":5}"#).unwrap();
    assert_eq!(
        old,
        Action::OneOffPayment {
            customer: None,
            amount: Some(5),
            succeed: None,
            pattern: None,
            recipient: None,
        }
    );
    let new: Action = serde_json::from_str(
        r#"{"type":"one_off_payment","pattern":"separate","recipient":"acct_1"}"#,
    )
    .unwrap();
    assert_eq!(
        new,
        Action::OneOffPayment {
            customer: None,
            amount: None,
            succeed: None,
            pattern: Some(ChargePattern::Separate),
            recipient: Some("acct_1".to_string()),
        }
    );
}

#[tokio::test]
async fn invoice_payments_follow_patterns() {
    let m = mock_with(&with_pattern("{ destination = 1.0 }")).await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    m.run_action(Action::NewCustomer).unwrap();
    let cause = m
        .run_action(Action::StartSubscription {
            customer: None,
            prices: Some(recurring_prices(&m, 1, "month")),
        })
        .unwrap();
    m.step(Duration::from_secs(HOUR)).await.unwrap();
    let mine = caused(&m, &cause);
    let invoice = only_id(&mine, ObjectType::Invoice);
    let charge = only_id(&mine, ObjectType::Charge);
    let tr = only_id(&mine, ObjectType::Transfer);
    let t = json(&m, &format!("/v1/transfers/{tr}")).await;
    assert_eq!(t["destination"], acct.as_str());
    assert_eq!(t["source_transaction"], charge.as_str());
    let ch = json(&m, &format!("/v1/charges/{charge}")).await;
    assert_eq!(t["amount"], ch["amount"]);
    assert_eq!(ch["transfer"], tr.as_str());
    // After the intent's success, before the invoice payment is paid.
    let events: Vec<String> = events_of(&m, &mine).into_iter().map(|e| e.0).collect();
    let pos = |name: &str| events.iter().position(|e| e == name).unwrap();
    assert!(pos("payment_intent.succeeded") < pos("transfer.created"));
    assert!(pos("transfer.created") < pos("invoice_payment.paid"));
    // The pattern belongs to the payment, not the invoice.
    let inv = json(&m, &format!("/v1/invoices/{invoice}")).await;
    assert_eq!(inv["status"], "paid");
    assert!(inv["transfer_data"].is_null());
}

#[tokio::test]
async fn connect_objects_land_in_their_scope() {
    let m = mock_with(
        "[accounts]
connect = true
connected_accounts = { initial = 3 }
account_api = { v1 = 1.0, v2 = 0.0 }
account_types = { standard = 0.0, express = 0.5, custom = 0.5 }
platform_payments = { own = 0.2, destination = 0.4, separate = 0.4 }
application_fee = { probability = 0.8, percent = 10.0 }
[actions]
new_customer = 1.0
one_off_payment = 2.0
application_fee_refund = 0.5
[churn.external_account]
create = 0.5
delete = 0.2
[churn.person]
update = 0.5
delete = 0.1
",
    )
    .await;
    m.step(Duration::from_secs(4 * DAY)).await.unwrap();
    let log = m.truth_log();
    let platform_only = [
        ObjectType::Transfer,
        ObjectType::TransferReversal,
        ObjectType::ApplicationFee,
        ObjectType::FeeRefund,
    ];
    let account_only = [
        ObjectType::BankAccount,
        ObjectType::Card,
        ObjectType::Person,
    ];
    let mut seen = std::collections::BTreeMap::<ObjectType, usize>::new();
    let mut py_scope = std::collections::BTreeMap::<String, String>::new();
    for u in &log {
        *seen.entry(u.object_type).or_default() += 1;
        if platform_only.contains(&u.object_type) {
            assert!(u.account.is_none(), "{u:?}");
        }
        if account_only.contains(&u.object_type) {
            assert!(u.account.is_some(), "{u:?}");
        }
        if u.object_type == ObjectType::Charge && u.object_id.starts_with("py_") {
            let acct = u.account.clone().expect("a py_ payment is in its account");
            py_scope.insert(u.object_id.clone(), acct);
        }
        if u.object_type == ObjectType::BalanceTransaction {
            let source = u.state["source"].as_str().unwrap_or_default();
            match u.state["type"].as_str().unwrap() {
                "payment" => assert!(source.starts_with("py_") && u.account.is_some(), "{u:?}"),
                "transfer" | "application_fee" => assert!(u.account.is_none(), "{u:?}"),
                _ => {}
            }
        }
    }
    // Each transfer's payment lives in the transfer's destination.
    let mut patterns = (0, 0);
    for u in log.iter().filter(|u| u.object_type == ObjectType::Transfer) {
        let py = u.state["destination_payment"].as_str().unwrap();
        assert_eq!(
            py_scope.get(py).map(String::as_str),
            u.state["destination"].as_str()
        );
        let charge = u.state["source_transaction"].as_str().unwrap();
        let ch = m.get(ObjectType::Charge, charge).unwrap().state;
        if ch["transfer_data"].is_null() {
            patterns.1 += 1;
        } else {
            patterns.0 += 1;
        }
    }
    assert!(patterns.0 > 0 && patterns.1 > 0, "{patterns:?}");
    for t in [
        ObjectType::Transfer,
        ObjectType::ApplicationFee,
        ObjectType::FeeRefund,
        ObjectType::BankAccount,
        ObjectType::Card,
        ObjectType::Person,
    ] {
        assert!(seen.get(&t).is_some_and(|n| *n > 0), "no {t:?}: {seen:?}");
    }
    assert!(!py_scope.is_empty());

    // Every event's label is its object's scope (an account's own events are labelled with
    // it although the account lives on the platform).
    let by_seq: std::collections::BTreeMap<u64, &StateUpdate> =
        log.iter().map(|u| (u.seq, u)).collect();
    for e in m.event_timeline() {
        assert!(!e.object_id.starts_with("py_"), "{e:?}");
        let u = by_seq[&e.truth_seq];
        if u.object_type == ObjectType::Account {
            continue;
        }
        assert_eq!(e.account, u.account, "{e:?}");
        let name = e.event_type.trim_start_matches("v1.");
        if name.starts_with("transfer.") || name.starts_with("application_fee.") {
            assert_eq!(e.account, None, "{e:?}");
        }
        if name.starts_with("person.") || name.starts_with("account.external_account.") {
            assert!(e.account.is_some(), "{e:?}");
        }
    }
}
