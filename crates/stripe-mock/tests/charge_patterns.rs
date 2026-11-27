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
payout = \"daily\"
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
    // Payouts live in the scope whose balance they pay out: the platform's and each
    // account's own.
    let mut payout_scopes = std::collections::BTreeSet::<Option<String>>::new();
    for u in &log {
        *seen.entry(u.object_type).or_default() += 1;
        if u.object_type == ObjectType::Payout {
            payout_scopes.insert(u.account.clone());
        }
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
                // A payout's balance transactions live with the payout.
                "payout" | "payout_failure" => assert!(
                    log.iter().any(|p| p.object_type == ObjectType::Payout
                        && p.object_id == source
                        && p.account == u.account),
                    "{u:?}"
                ),
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
        ObjectType::Payout,
        ObjectType::BankAccount,
        ObjectType::Card,
        ObjectType::Person,
    ] {
        assert!(seen.get(&t).is_some_and(|n| *n > 0), "no {t:?}: {seen:?}");
    }
    assert!(!py_scope.is_empty());
    assert!(
        payout_scopes.contains(&None) && payout_scopes.len() > 1,
        "{payout_scopes:?}"
    );

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
        if name.starts_with("payout.") {
            assert!(payout_scopes.contains(&e.account), "{e:?}");
        }
        if name.starts_with("person.") || name.starts_with("account.external_account.") {
            assert!(e.account.is_some(), "{e:?}");
        }
    }
}

fn reverse(transfer: &str, amount: Option<i64>) -> Action {
    Action::TransferReversal {
        transfer: Some(transfer.to_string()),
        amount,
    }
}

/// A mock with one destination payment of `amount`: the account, the transfer and its
/// `py_` payment.
async fn transfer_setup(amount: i64) -> (MockStripe, String, String, String) {
    let m = mock_with(&with_pattern("{ destination = 1.0 }")).await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    let cause = m.run_action(payment(amount)).unwrap();
    let tr = only_id(&caused(&m, &cause), ObjectType::Transfer);
    let t = json(&m, &format!("/v1/transfers/{tr}")).await;
    let py = t["destination_payment"].as_str().unwrap().to_string();
    (m, acct, tr, py)
}

#[tokio::test]
async fn partial_then_full_reversal() {
    let (m, acct, tr, py) = transfer_setup(1000).await;

    // A partial reversal.
    let cause = m.run_action(reverse(&tr, Some(300))).unwrap();
    let mine = caused(&m, &cause);
    let trr1 = only_id(&mine, ObjectType::TransferReversal);
    assert!(trr1.starts_with("trr_"), "{trr1}");
    let r1 = json(&m, &format!("/v1/transfers/{tr}/reversals/{trr1}")).await;
    assert_eq!(r1["object"], "transfer_reversal");
    assert_eq!(r1["amount"], 300);
    assert_eq!(r1["transfer"], tr.as_str());
    assert!(r1["source_refund"].is_null(), "{r1:#}");
    let pyr1 = r1["destination_payment_refund"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(pyr1.starts_with("pyr_"), "{pyr1}");
    // The platform's balance transaction: `+amount`, available at once.
    let bt = json(
        &m,
        &format!(
            "/v1/balance_transactions/{}",
            r1["balance_transaction"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(bt["type"], "transfer_refund");
    assert_eq!(bt["amount"], 300);
    assert_eq!(bt["fee"], 0);
    assert_eq!(bt["net"], 300);
    assert_eq!(bt["status"], "available");
    let t = json(&m, &format!("/v1/transfers/{tr}")).await;
    assert_eq!(t["amount_reversed"], 300);
    assert_eq!(t["reversed"], false);
    assert_eq!(t["reversals"]["data"][0]["id"], trr1.as_str());
    // The refund of the `py_` payment, in the account.
    let refund = json_as(&m, &format!("/v1/refunds/{pyr1}"), &acct).await;
    assert_eq!(refund["object"], "refund");
    assert_eq!(refund["amount"], 300);
    assert_eq!(refund["status"], "succeeded");
    assert_eq!(refund["charge"], py.as_str());
    assert_eq!(refund["source_transfer_reversal"], trr1.as_str());
    let rbt = json_as(
        &m,
        &format!(
            "/v1/balance_transactions/{}",
            refund["balance_transaction"].as_str().unwrap()
        ),
        &acct,
    )
    .await;
    assert_eq!(rbt["type"], "payment_refund");
    assert_eq!(rbt["amount"], -300);
    assert_eq!(rbt["net"], -300);
    assert_eq!(rbt["status"], "available");
    assert_eq!(rbt["source"], pyr1.as_str());
    let p = json_as(&m, "/v1/charges", &acct).await;
    assert_eq!(p["data"][0]["id"], py.as_str());
    assert_eq!(p["data"][0]["amount_refunded"], 300);
    assert_eq!(p["data"][0]["refunded"], false);
    // Everything happened in this instant.
    assert!(mine.iter().all(|u| u.at == mine[0].at));

    // Too much is refused, naming the transfer.
    let msg = invalid_action(m.run_action(reverse(&tr, Some(701))));
    assert!(msg.contains(&tr), "{msg}");
    invalid_action(m.run_action(reverse(&tr, Some(0))));

    // The rest.
    let cause = m.run_action(reverse(&tr, Some(700))).unwrap();
    let trr2 = only_id(&caused(&m, &cause), ObjectType::TransferReversal);
    let t = json(&m, &format!("/v1/transfers/{tr}")).await;
    assert_eq!(t["amount_reversed"], 1000);
    assert_eq!(t["reversed"], true);
    // The amounts across the transfer, its reversals and the `py_` payment agree.
    let embedded: Vec<&str> = t["reversals"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(embedded, [trr2.as_str(), trr1.as_str()], "newest first");
    let sum: i64 = t["reversals"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["amount"].as_i64().unwrap())
        .sum();
    assert_eq!(sum, t["amount_reversed"].as_i64().unwrap());
    // The sub-list lists newest first too, and is the embedded list's first page.
    let sub = json(&m, &format!("/v1/transfers/{tr}/reversals")).await;
    assert_eq!(sub["data"], t["reversals"]["data"]);
    assert_eq!(sub["has_more"], false);
    let p = json_as(&m, &format!("/v1/charges/{py}"), &acct).await;
    assert_eq!(p["amount_refunded"], 1000);
    assert_eq!(p["refunded"], true);
    let refunds = json_as(&m, "/v1/refunds", &acct).await;
    let total: i64 = refunds["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["amount"].as_i64().unwrap())
        .sum();
    assert_eq!(total, 1000);

    // A third reversal has nothing left to reverse.
    let msg = invalid_action(m.run_action(reverse(&tr, None)));
    assert!(msg.contains(&tr), "{msg}");
    let msg = invalid_action(m.run_action(reverse(&tr, Some(1))));
    assert!(msg.contains(&tr), "{msg}");
    // An unknown transfer is a 404; an account scope cannot reverse.
    assert!(matches!(
        m.run_action(reverse("tr_missing", None)),
        Err(MockError::NoSuchObject { .. })
    ));
    invalid_action(m.account(&acct).run_action(reverse(&tr, Some(1))));
}

#[tokio::test]
async fn reversal_events_and_scopes() {
    let (m, acct, tr, py) = transfer_setup(1000).await;
    let cause = m.run_action(reverse(&tr, Some(400))).unwrap();
    let mine = caused(&m, &cause);
    // One event, platform scope, unlabelled: the transfer's. Nothing else announces.
    let events = events_of(&m, &mine);
    assert_eq!(events, [("transfer.reversed".to_string(), None)]);
    let trr = only_id(&mine, ObjectType::TransferReversal);
    let reversal = mine
        .iter()
        .find(|u| u.object_type == ObjectType::TransferReversal)
        .unwrap();
    assert!(reversal.events.snapshot.is_empty());
    // Steps 4-6 are in the account scope, with no events.
    let in_account: Vec<(ObjectType, String)> = mine
        .iter()
        .filter(|u| u.account.is_some())
        .map(|u| (u.object_type, u.object_id.clone()))
        .collect();
    let types: Vec<ObjectType> = in_account.iter().map(|(t, _)| *t).collect();
    assert_eq!(
        types,
        [
            ObjectType::BalanceTransaction,
            ObjectType::Refund,
            ObjectType::Charge
        ]
    );
    assert_eq!(in_account[2].1, py);
    assert!(
        mine.iter()
            .filter(|u| u.account.is_some())
            .all(|u| u.account.as_deref() == Some(acct.as_str()) && u.events.snapshot.is_empty())
    );
    // The platform side: balance transaction, reversal, transfer.
    let platform: Vec<ObjectType> = mine
        .iter()
        .filter(|u| u.account.is_none())
        .map(|u| u.object_type)
        .collect();
    assert_eq!(
        platform,
        [
            ObjectType::BalanceTransaction,
            ObjectType::TransferReversal,
            ObjectType::Transfer
        ]
    );
    // The `pyr_` refund is found in the account's refunds, not the platform's.
    let r = json(&m, &format!("/v1/transfers/{tr}/reversals/{trr}")).await;
    let pyr = r["destination_payment_refund"].as_str().unwrap();
    let listed = json_as(&m, "/v1/refunds", &acct).await;
    assert_eq!(listed["data"][0]["id"], pyr);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(json(&m, "/v1/refunds").await["data"], Value::Array(vec![]));
    assert_eq!(get(&m, &format!("/v1/refunds/{pyr}")).await.status(), 404);
    assert!(m.event_timeline().iter().all(|e| e.object_id != pyr));
}

#[tokio::test]
async fn reversal_refund_expands_across_scopes() {
    let (m, acct, tr, _py) = transfer_setup(800).await;
    let cause = m.run_action(reverse(&tr, Some(250))).unwrap();
    let trr = only_id(&caused(&m, &cause), ObjectType::TransferReversal);
    // The platform expands the reversal's refund, which lives in the account.
    let e = json(
        &m,
        &format!("/v1/transfers/{tr}/reversals/{trr}?expand[]=destination_payment_refund"),
    )
    .await;
    assert_eq!(e["destination_payment_refund"]["object"], "refund");
    assert_eq!(e["destination_payment_refund"]["amount"], 250);
    let pyr = e["destination_payment_refund"]["id"].as_str().unwrap();
    assert!(pyr.starts_with("pyr_"), "{pyr}");
    // The account expands the refund's reversal, which lives on the platform.
    let e = json_as(
        &m,
        &format!("/v1/refunds/{pyr}?expand[]=source_transfer_reversal"),
        &acct,
    )
    .await;
    assert_eq!(e["source_transfer_reversal"]["object"], "transfer_reversal");
    assert_eq!(e["source_transfer_reversal"]["id"], trr.as_str());
    assert_eq!(e["source_transfer_reversal"]["amount"], 250);
    // Through the transfer's list too: the sub-list expands its items.
    let l = json(
        &m,
        &format!("/v1/transfers/{tr}/reversals?expand[]=data.destination_payment_refund"),
    )
    .await;
    assert_eq!(l["data"][0]["destination_payment_refund"]["id"], pyr);
}

#[tokio::test]
async fn transfers_to_disconnected_accounts_are_not_reversed() {
    let m = mock_with(&format!(
        "{}[actions]\ntransfer_reversal = 50.0\n",
        with_pattern("{ destination = 1.0 }")
    ))
    .await;
    let gone = m.connect_account(AccountApi::V1).unwrap();
    let kept = m.connect_account(AccountApi::V1).unwrap();
    let pay_to = |to: &str| Action::OneOffPayment {
        customer: None,
        amount: Some(1000),
        succeed: Some(true),
        pattern: Some(ChargePattern::Destination),
        recipient: Some(to.to_string()),
    };
    let tr_gone = only_id(
        &caused(&m, &m.run_action(pay_to(&gone)).unwrap()),
        ObjectType::Transfer,
    );
    let tr_kept = only_id(
        &caused(&m, &m.run_action(pay_to(&kept)).unwrap()),
        ObjectType::Transfer,
    );
    m.disconnect_account(&gone).unwrap();
    let before = m.truth_log().len();
    // A named transfer to a disconnected account is a 409 naming it.
    let msg = invalid_action(m.run_action(reverse(&tr_gone, Some(10))));
    assert!(msg.contains(&tr_gone), "{msg}");
    // The generator reverses the other one only.
    m.step(Duration::from_secs(2 * HOUR)).await.unwrap();
    let log = m.truth_log();
    let reversed: Vec<&StateUpdate> = log[before..]
        .iter()
        .filter(|u| u.object_type == ObjectType::TransferReversal)
        .collect();
    assert!(!reversed.is_empty(), "the connected transfer is reversed");
    assert!(
        reversed
            .iter()
            .all(|u| u.state["transfer"] == tr_kept.as_str()),
        "{reversed:?}"
    );
    assert!(
        log[before..]
            .iter()
            .all(|u| u.account.as_deref() != Some(&gone)),
        "nothing happens in a disconnected account's scope"
    );
    let t = json(&m, &format!("/v1/transfers/{tr_gone}")).await;
    assert_eq!(t["amount_reversed"], 0);
    // Once the connected one is fully reversed, nothing is left to draw.
    let t = json(&m, &format!("/v1/transfers/{tr_kept}")).await;
    assert_eq!(t["reversed"], true);
    let n = m.truth_log().len();
    m.step(Duration::from_secs(HOUR)).await.unwrap();
    assert!(
        m.truth_log()[n..]
            .iter()
            .all(|u| u.object_type != ObjectType::TransferReversal)
    );
}

/// Regression (phase 8, Task 9): a `py_` payment's money is returned only by reversing its
/// transfer. The account's own refunds and disputes never pick it, and a control refund or
/// dispute naming it is a 409, so `amount_refunded` never runs past the transfer's
/// `amount_reversed`.
#[tokio::test]
async fn destination_payments_are_not_refunded_or_disputed_directly() {
    let (m, acct, tr, py) = transfer_setup(1000).await;
    let a = m.account(&acct);
    let msg = invalid_action(a.run_action(Action::Refund {
        charge: Some(py.clone()),
        amount: None,
    }));
    assert!(msg.contains(&py) && msg.contains("reverse"), "{msg}");
    let msg = invalid_action(a.run_action(Action::Dispute {
        charge: Some(py.clone()),
        outcome: None,
    }));
    assert!(msg.contains(&py) && msg.contains("reverse"), "{msg}");
    // With the `py_` payment the account's only charge, random ones find nothing to do.
    let r = a.run_action(Action::Refund {
        charge: None,
        amount: None,
    });
    assert!(r.is_err(), "{r:?}");
    let r = a.run_action(Action::Dispute {
        charge: None,
        outcome: None,
    });
    assert!(r.is_err(), "{r:?}");
    let p = json_as(&m, &format!("/v1/charges/{py}"), &acct).await;
    assert_eq!(p["amount_refunded"], 0);
    assert_eq!(p["disputed"], false);
    // Reversing the transfer still refunds it.
    m.run_action(reverse(&tr, Some(400))).unwrap();
    let p = json_as(&m, &format!("/v1/charges/{py}"), &acct).await;
    assert_eq!(p["amount_refunded"], 400);
}

/// Regression (phase 8, Task 9): an account that disconnected is out of the platform's
/// reach, so a destination charge's `transfer_data.destination` and a transfer's
/// `destination` stay its ID when expanded (they are not nullable), as a cross-scope
/// expansion into it does.
#[tokio::test]
async fn disconnected_destination_expands_to_its_id() {
    let (m, acct, tr, _py) = transfer_setup(1000).await;
    let t = json(&m, &format!("/v1/transfers/{tr}")).await;
    let ch = t["source_transaction"].as_str().unwrap().to_string();
    // While connected, both expand to the account.
    let v = json(&m, &format!("/v1/transfers/{tr}?expand[]=destination")).await;
    assert_eq!(v["destination"]["id"], acct.as_str());
    let v = json(
        &m,
        &format!("/v1/charges/{ch}?expand[]=transfer_data.destination"),
    )
    .await;
    assert_eq!(v["transfer_data"]["destination"]["id"], acct.as_str());
    m.disconnect_account(&acct).unwrap();
    let v = json(&m, &format!("/v1/transfers/{tr}?expand[]=destination")).await;
    common::assert_conforms(&v);
    assert_eq!(v["destination"], acct.as_str());
    let v = json(
        &m,
        &format!("/v1/charges/{ch}?expand[]=transfer_data.destination"),
    )
    .await;
    common::assert_conforms(&v);
    assert_eq!(v["transfer_data"]["destination"], acct.as_str());
    let v = json(
        &m,
        "/v1/charges?limit=3&expand[]=data.transfer_data.destination",
    )
    .await;
    common::assert_conforms(&v);
    assert_eq!(v["data"][0]["transfer_data"]["destination"], acct.as_str());
}

/// Regression (phase 8 final review): `transfer_reversal` is an `[actions]` rate, so changing
/// it at runtime redraws its next due time from now, in both directions.
#[tokio::test]
async fn transfer_reversal_rate_changes_at_runtime() {
    let m = mock_with(&with_pattern("{ destination = 1.0 }")).await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    m.run_action(Action::OneOffPayment {
        customer: None,
        amount: Some(100_000),
        succeed: Some(true),
        pattern: Some(ChargePattern::Destination),
        recipient: Some(acct),
    })
    .unwrap();
    let reversals = |m: &MockStripe| {
        m.truth_log()
            .iter()
            .filter(|u| u.object_type == ObjectType::TransferReversal)
            .count()
    };
    // Off by default: nothing is reversed.
    m.step(Duration::from_secs(2 * HOUR)).await.unwrap();
    assert_eq!(reversals(&m), 0);

    m.update_actions_config(|a| a.transfer_reversal = 50.0)
        .unwrap();
    m.step(Duration::from_secs(2 * HOUR)).await.unwrap();
    let on = reversals(&m);
    assert!(on > 0, "the new rate takes effect");
    assert!(
        m.event_timeline()
            .iter()
            .any(|e| e.event_type == "transfer.reversed")
    );

    m.update_actions_config(|a| a.transfer_reversal = 0.0)
        .unwrap();
    m.step(Duration::from_secs(2 * HOUR)).await.unwrap();
    assert_eq!(reversals(&m), on, "rate 0 stops reversals");
}
