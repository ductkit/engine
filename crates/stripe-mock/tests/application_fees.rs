//! Application fees on connected accounts' direct charges, and their refunds (spec
//! sections 4, 5.5 and 6.1; phase 8 rulings P1, P2 and P6).

mod common;

use std::time::Duration;

use common::{get, get_as, mock_with};
use ductkit_stripe_mock::{
    AccountApi, Action, Cause, MockError, MockStripe, ObjectType, StateUpdate,
};
use serde_json::Value;

const FEES: &str = "[accounts]
connect = true
account_types = { standard = 0.0, express = 0.0, custom = 1.0 }
application_fee = { probability = 1.0, percent = 10.0 }
";

/// Stripe's processing fee of a 1000-unit card charge (2.9% + 30).
const CARD_FEE_1000: i64 = 59;

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

fn pay(m: &MockStripe, acct: Option<&str>, amount: i64) -> Cause {
    let a = Action::OneOffPayment {
        customer: None,
        amount: Some(amount),
        succeed: Some(true),
        pattern: None,
        recipient: None,
    };
    match acct {
        Some(acct) => m.account(acct).run_action(a),
        None => m.run_action(a),
    }
    .unwrap()
}

fn invalid_action(r: Result<impl std::fmt::Debug, MockError>) -> String {
    match r {
        Err(MockError::InvalidAction(m)) => m,
        other => panic!("expected invalid_action, got {other:?}"),
    }
}

/// A mock with one connected account and one 1000-unit payment in its scope: the account,
/// the charge, the fee.
async fn fee_setup() -> (MockStripe, String, String, String) {
    let m = mock_with(FEES).await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    let cause = pay(&m, Some(&acct), 1000);
    let charge = only_id(&caused(&m, &cause), ObjectType::Charge);
    let fees = json(&m, &format!("/v1/application_fees?charge={charge}")).await;
    let fee = fees["data"][0]["id"].as_str().unwrap().to_string();
    (m, acct, charge, fee)
}

#[tokio::test]
async fn direct_charge_carries_application_fee() {
    let m = mock_with(FEES).await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    let cause = pay(&m, Some(&acct), 1000);
    let mine = caused(&m, &cause);
    let charge = only_id(&mine, ObjectType::Charge);
    let pi = only_id(&mine, ObjectType::PaymentIntent);

    // The account's payment objects carry the fee.
    let ch = json_as(&m, &format!("/v1/charges/{charge}"), &acct).await;
    assert_eq!(ch["application_fee_amount"], 100);
    let intent = json_as(&m, &format!("/v1/payment_intents/{pi}"), &acct).await;
    assert_eq!(intent["application_fee_amount"], 100);
    let bt = json_as(
        &m,
        &format!(
            "/v1/balance_transactions/{}",
            ch["balance_transaction"].as_str().unwrap()
        ),
        &acct,
    )
    .await;
    assert_eq!(bt["amount"], 1000);
    assert_eq!(bt["fee"], CARD_FEE_1000 + 100);
    assert_eq!(bt["net"], 1000 - CARD_FEE_1000 - 100);
    let details = bt["fee_details"].as_array().unwrap();
    assert_eq!(details.len(), 2);
    assert_eq!(details[0]["type"], "stripe_fee");
    assert_eq!(details[0]["amount"], CARD_FEE_1000);
    assert_eq!(details[1]["type"], "application_fee");
    assert_eq!(details[1]["amount"], 100);
    assert!(
        details[1]["application"]
            .as_str()
            .unwrap()
            .starts_with("ca_"),
        "{details:?}"
    );

    // The platform has the fee, found by its charge.
    let fees = json(&m, &format!("/v1/application_fees?charge={charge}")).await;
    assert_eq!(fees["data"].as_array().unwrap().len(), 1);
    let fee = &fees["data"][0];
    let fee_id = fee["id"].as_str().unwrap();
    assert!(fee_id.starts_with("fee_"));
    assert_eq!(ch["application_fee"], fee_id);
    assert_eq!(fee["account"], acct.as_str());
    assert_eq!(fee["application"], details[1]["application"]);
    assert_eq!(fee["charge"], charge.as_str());
    assert_eq!(fee["amount"], 100);
    assert_eq!(fee["amount_refunded"], 0);
    assert_eq!(fee["currency"], ch["currency"]);
    assert_eq!(fee["refunded"], false);
    assert_eq!(fee["refunds"]["data"].as_array().unwrap().len(), 0);
    assert_eq!(fee["refunds"]["has_more"], false);
    assert_eq!(
        fee["refunds"]["url"],
        format!("/v1/application_fees/{fee_id}/refunds").as_str()
    );
    assert_eq!(fee["fee_source"]["type"], "charge");
    assert_eq!(fee["fee_source"]["charge"], charge.as_str());
    assert!(fee["originating_transaction"].is_null());

    // Its balance transaction is pending in the platform scope, with the charge's
    // `available_on`.
    let fee_bt = json(
        &m,
        &format!(
            "/v1/balance_transactions/{}",
            fee["balance_transaction"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(fee_bt["type"], "application_fee");
    assert_eq!(fee_bt["amount"], 100);
    assert_eq!(fee_bt["fee"], 0);
    assert_eq!(fee_bt["net"], 100);
    assert_eq!(fee_bt["status"], "pending");
    assert_eq!(fee_bt["source"], fee_id);
    assert_eq!(fee_bt["available_on"], bt["available_on"]);
    assert_eq!(fee_bt["currency"], ch["currency"]);
    let r = get_as(
        &m,
        &format!(
            "/v1/balance_transactions/{}",
            fee_bt["id"].as_str().unwrap()
        ),
        &acct,
    )
    .await;
    assert_eq!(
        r.status(),
        404,
        "the fee's balance transaction is platform scope"
    );

    // `application_fee.created` is an unlabelled platform event; the fee comes after the
    // payment intent's success, in the same cause.
    let created: Vec<_> = m
        .event_timeline()
        .into_iter()
        .filter(|e| e.event_type == "application_fee.created")
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].account, None);
    assert_eq!(created[0].object_id, fee_id);
    let log = caused(&m, &cause);
    let pos = |t: ObjectType| log.iter().position(|u| u.object_type == t).unwrap();
    assert!(pos(ObjectType::ApplicationFee) > pos(ObjectType::PaymentIntent));
    assert!(
        log.iter()
            .filter(|u| u.object_type == ObjectType::ApplicationFee)
            .all(|u| u.account.is_none())
    );
}

#[tokio::test]
async fn application_fee_charge_expands_across_scopes() {
    let (m, acct, charge, fee) = fee_setup().await;
    // From the platform, the fee's charge lives in the account's scope.
    let f = json(&m, &format!("/v1/application_fees/{fee}?expand[]=charge")).await;
    assert_eq!(f["charge"]["object"], "charge");
    assert_eq!(f["charge"]["id"], charge.as_str());
    assert_eq!(f["charge"]["amount"], 1000);
    // And the other way: from the account's scope, the charge's fee lives in the platform.
    let c = json_as(
        &m,
        &format!("/v1/charges/{charge}?expand[]=application_fee"),
        &acct,
    )
    .await;
    assert_eq!(c["application_fee"]["object"], "application_fee");
    assert_eq!(c["application_fee"]["id"], fee.as_str());
    assert_eq!(c["application_fee"]["amount"], 100);
    // Without `expand`, both stay IDs.
    let f = json(&m, &format!("/v1/application_fees/{fee}")).await;
    assert_eq!(f["charge"], charge.as_str());
}

#[tokio::test]
async fn platform_charges_never_carry_fees() {
    let m = mock_with(FEES).await;
    let _acct = m.connect_account(AccountApi::V1).unwrap();
    let cause = pay(&m, None, 1000);
    let mine = caused(&m, &cause);
    let charge = only_id(&mine, ObjectType::Charge);
    let pi = only_id(&mine, ObjectType::PaymentIntent);
    let ch = json(&m, &format!("/v1/charges/{charge}")).await;
    assert!(ch["application_fee"].is_null());
    assert!(ch["application_fee_amount"].is_null());
    let intent = json(&m, &format!("/v1/payment_intents/{pi}")).await;
    assert!(intent["application_fee_amount"].is_null());
    let bt = json(
        &m,
        &format!(
            "/v1/balance_transactions/{}",
            ch["balance_transaction"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(bt["fee"], CARD_FEE_1000);
    assert_eq!(bt["fee_details"].as_array().unwrap().len(), 1);
    assert_eq!(
        json(&m, "/v1/application_fees").await["data"],
        Value::Array(vec![])
    );
}

#[tokio::test]
async fn fee_refund_partial_then_full() {
    let (m, acct, _charge, fee) = fee_setup().await;

    // A partial refund.
    let cause = m
        .run_action(Action::ApplicationFeeRefund {
            fee: Some(fee.clone()),
            amount: Some(30),
        })
        .unwrap();
    let mine = caused(&m, &cause);
    let fr1 = only_id(&mine, ObjectType::FeeRefund);
    assert!(fr1.starts_with("fr_"));
    let timeline = m.event_timeline();
    let events: Vec<_> = mine
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
        .collect();
    assert_eq!(
        events,
        ["application_fee.refunded"],
        "the refund has no event"
    );
    let refund = json(&m, &format!("/v1/application_fees/{fee}/refunds/{fr1}")).await;
    assert_eq!(refund["object"], "fee_refund");
    assert_eq!(refund["amount"], 30);
    assert_eq!(refund["fee"], fee.as_str());
    let f = json(&m, &format!("/v1/application_fees/{fee}")).await;
    assert_eq!(f["amount_refunded"], 30);
    assert_eq!(f["refunded"], false);
    assert_eq!(f["refunds"]["data"][0]["id"], fr1.as_str());

    // The platform's balance transaction, available at once.
    let bt = json(
        &m,
        &format!(
            "/v1/balance_transactions/{}",
            refund["balance_transaction"].as_str().unwrap()
        ),
    )
    .await;
    assert_eq!(bt["type"], "application_fee_refund");
    assert_eq!(bt["amount"], -30);
    assert_eq!(bt["net"], -30);
    assert_eq!(bt["fee"], 0);
    assert_eq!(bt["status"], "available");
    // And the account's, in its own scope.
    let acct_bts: Vec<_> = mine
        .iter()
        .filter(|u| u.object_type == ObjectType::BalanceTransaction && u.account.is_some())
        .collect();
    assert_eq!(acct_bts.len(), 1);
    assert_eq!(acct_bts[0].account.as_deref(), Some(acct.as_str()));
    let abt = json_as(
        &m,
        &format!("/v1/balance_transactions/{}", acct_bts[0].object_id),
        &acct,
    )
    .await;
    assert_eq!(abt["type"], "application_fee_refund");
    assert_eq!(abt["amount"], 30);
    assert_eq!(abt["status"], "available");

    // Too much is refused; so is a refund in an account's scope.
    let msg = invalid_action(m.run_action(Action::ApplicationFeeRefund {
        fee: Some(fee.clone()),
        amount: Some(71),
    }));
    assert!(msg.contains(&fee) && msg.contains("70"), "{msg}");
    invalid_action(m.account(&acct).run_action(Action::ApplicationFeeRefund {
        fee: None,
        amount: None,
    }));

    // The rest.
    let cause = m
        .run_action(Action::ApplicationFeeRefund {
            fee: Some(fee.clone()),
            amount: Some(70),
        })
        .unwrap();
    let fr2 = only_id(&caused(&m, &cause), ObjectType::FeeRefund);
    let f = json(&m, &format!("/v1/application_fees/{fee}")).await;
    assert_eq!(f["amount_refunded"], 100);
    assert_eq!(f["refunded"], true);
    let embedded: Vec<_> = f["refunds"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(embedded, [fr2.clone(), fr1.clone()], "newest first");
    let listed = json(&m, &format!("/v1/application_fees/{fee}/refunds")).await;
    assert_eq!(listed["data"][0]["id"], fr2.as_str());
    assert_eq!(listed["data"][1]["id"], fr1.as_str());

    // Nothing is left: a named fee is a 409 naming it, an unnamed one finds none.
    let msg = invalid_action(m.run_action(Action::ApplicationFeeRefund {
        fee: Some(fee.clone()),
        amount: None,
    }));
    assert!(msg.contains(&fee), "{msg}");
    invalid_action(m.run_action(Action::ApplicationFeeRefund {
        fee: None,
        amount: None,
    }));
}

#[tokio::test]
async fn unnamed_fee_refund_draws_a_fee_and_amount() {
    let (m, _acct, _charge, fee) = fee_setup().await;
    let cause = m
        .run_action(Action::ApplicationFeeRefund {
            fee: None,
            amount: None,
        })
        .unwrap();
    let fr = only_id(&caused(&m, &cause), ObjectType::FeeRefund);
    let refund = json(&m, &format!("/v1/application_fees/{fee}/refunds/{fr}")).await;
    let amount = refund["amount"].as_i64().unwrap();
    assert!((1..=100).contains(&amount), "{amount}");
}

#[tokio::test]
async fn fee_balance_transaction_settles_after_disconnect() {
    let (m, acct, charge, fee) = fee_setup().await;
    let f = json(&m, &format!("/v1/application_fees/{fee}")).await;
    let fee_bt = f["balance_transaction"].as_str().unwrap().to_string();
    let path = format!("/v1/balance_transactions/{fee_bt}");
    assert_eq!(json(&m, &path).await["status"], "pending");

    m.disconnect_account(&acct).unwrap();
    // The platform's fee and its balance transaction are still there, in its scope.
    assert_eq!(json(&m, &path).await["status"], "pending");
    m.step(Duration::from_secs(3 * 86_400)).await.unwrap();
    assert_eq!(
        json(&m, &path).await["status"],
        "available",
        "the platform's pending balance transaction settles after the account is gone"
    );
    let f = json(&m, &format!("/v1/application_fees?charge={charge}")).await;
    assert_eq!(f["data"][0]["id"], fee.as_str());

    // A disconnected account's fees are not refundable.
    let msg = invalid_action(m.run_action(Action::ApplicationFeeRefund {
        fee: Some(fee.clone()),
        amount: None,
    }));
    assert!(msg.contains(&fee), "{msg}");
    invalid_action(m.run_action(Action::ApplicationFeeRefund {
        fee: None,
        amount: None,
    }));
}
