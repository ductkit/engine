//! The `dispute` action: disputes with multi-week timelines (spec sections 4, 5.5 and 11).

mod common;

use std::time::Duration;

use common::{Receiver, get, mock_with};
use ductkit_stripe_mock::{
    AccountApi, Action, Cause, DisputeOutcome, MockError, MockStripe, ObjectType, StateUpdate,
};
use serde_json::Value;

const DAY_MS: i64 = 86_400_000;
const DISPUTE_FEE: i64 = 1500;

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

/// A succeeded one-off payment of `amount` through `run`; returns its charge.
fn paid_charge(
    m: &MockStripe,
    run: impl Fn(Action) -> Result<Cause, MockError>,
    amount: i64,
) -> String {
    let cause = run(Action::OneOffPayment {
        customer: None,
        amount: Some(amount),
        succeed: Some(true),
    })
    .unwrap();
    only_id(&caused(m, &cause), ObjectType::Charge)
}

fn dispute(m: &MockStripe, charge: &str, outcome: DisputeOutcome) -> Result<Cause, MockError> {
    m.run_action(Action::Dispute {
        charge: Some(charge.to_string()),
        outcome: Some(outcome),
    })
}

/// The dispute's truth-log entries, oldest first.
fn history(m: &MockStripe, id: &str) -> Vec<StateUpdate> {
    m.truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::Dispute && u.object_id == id)
        .collect()
}

/// The `charge.dispute.*` events about `id`, oldest first.
fn dispute_events(m: &MockStripe, id: &str) -> Vec<String> {
    m.event_timeline()
        .into_iter()
        .filter(|e| e.object_id == id && e.event_type.starts_with("charge.dispute."))
        .map(|e| e.event_type)
        .collect()
}

/// The sum of `net` over the dispute's balance transactions.
fn net_sum(m: &MockStripe, id: &str) -> i64 {
    let d = m.get(ObjectType::Dispute, id).unwrap().state;
    d["balance_transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let id = t["id"].as_str().unwrap();
            m.get(ObjectType::BalanceTransaction, id).unwrap().state["net"]
                .as_i64()
                .unwrap()
        })
        .sum()
}

#[tokio::test]
async fn dispute_lives_for_weeks() {
    let m = mock_with("seed = 3").await;
    let ch = paid_charge(&m, |a| m.run_action(a), 5000);
    let cause = dispute(&m, &ch, DisputeOutcome::Won).unwrap();
    let mine = caused(&m, &cause);
    let du = only_id(&mine, ObjectType::Dispute);
    assert!(du.starts_with("du_"), "{du}");
    assert_eq!(
        dispute_events(&m, &du),
        ["charge.dispute.created", "charge.dispute.funds_withdrawn"]
    );
    let d = json(&m, &format!("/v1/disputes/{du}")).await;
    assert_eq!(d["status"], "needs_response");
    assert_eq!(d["charge"], ch.as_str());
    assert_eq!(d["amount"], 5000);
    assert!(
        [
            "fraudulent",
            "product_not_received",
            "duplicate",
            "subscription_canceled",
            "general"
        ]
        .contains(&d["reason"].as_str().unwrap()),
        "{d}"
    );
    assert_eq!(d["is_charge_refundable"], false);
    let txns = d["balance_transactions"].as_array().unwrap();
    assert_eq!(txns.len(), 1);
    assert_eq!(txns[0]["type"], "adjustment");
    assert_eq!(txns[0]["amount"], -5000);
    assert_eq!(txns[0]["fee"], DISPUTE_FEE);
    assert_eq!(txns[0]["net"], -5000 - DISPUTE_FEE);
    assert_eq!(txns[0]["source"], du.as_str());
    let charge = json(&m, &format!("/v1/charges/{ch}")).await;
    assert_eq!(charge["disputed"], true);
    let listed = json(&m, &format!("/v1/disputes?charge={ch}")).await;
    assert_eq!(listed["data"][0]["id"], du.as_str());

    // Day by day, fetching each new event while it is inside the retention window: every
    // payload conforms (the API helper checks it).
    let mut seen = 0;
    for _ in 0..70 {
        m.step(Duration::from_secs(86_400)).await.unwrap();
        let ids: Vec<String> = m
            .event_timeline()
            .into_iter()
            .filter(|e| e.object_id == du && e.event_type.starts_with("charge.dispute."))
            .map(|e| e.id)
            .collect();
        for id in &ids[seen..] {
            json(&m, &format!("/v1/events/{id}")).await;
        }
        seen = ids.len();
    }
    let h = history(&m, &du);
    let statuses: Vec<&str> = h
        .iter()
        .map(|u| u.state["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, ["needs_response", "under_review", "won"]);
    let opened = h[0].at.0;
    let review = h[1].at.0 - opened;
    assert!((2 * DAY_MS..=7 * DAY_MS).contains(&review), "{review}");
    assert_eq!(h[1].state["evidence_details"]["has_evidence"], true);
    assert_eq!(h[1].state["evidence_details"]["submission_count"], 1);
    let closed = h[2].at.0 - opened;
    assert!((9 * DAY_MS..=67 * DAY_MS).contains(&closed), "{closed}");
    assert!(h.iter().all(|u| u.cause.as_ref() == Some(&cause)));
    assert_eq!(
        dispute_events(&m, &du),
        [
            "charge.dispute.created",
            "charge.dispute.funds_withdrawn",
            "charge.dispute.updated",
            "charge.dispute.closed",
            "charge.dispute.funds_reinstated",
        ]
    );
    // Won: the reinstatement returns the amount and the fee.
    let d = json(&m, &format!("/v1/disputes/{du}?expand[]=charge")).await;
    assert_eq!(d["status"], "won");
    assert_eq!(d["charge"]["disputed"], true);
    let txns = d["balance_transactions"].as_array().unwrap();
    assert_eq!(txns.len(), 2);
    assert_eq!(txns[1]["amount"], 5000);
    assert_eq!(txns[1]["fee"], -DISPUTE_FEE);
    assert_eq!(net_sum(&m, &du), 0);
    for t in txns {
        let id = t["id"].as_str().unwrap();
        let fetched = json(&m, &format!("/v1/balance_transactions/{id}")).await;
        assert_eq!(&fetched, t);
        assert_eq!(fetched["status"], "available");
    }
}

#[tokio::test]
async fn lost_dispute_keeps_funds_withdrawn() {
    let m = mock_with("seed = 5").await;
    let ch = paid_charge(&m, |a| m.run_action(a), 2500);
    let cause = dispute(&m, &ch, DisputeOutcome::Lost).unwrap();
    let du = only_id(&caused(&m, &cause), ObjectType::Dispute);
    m.step(Duration::from_secs(70 * 86_400)).await.unwrap();
    let d = json(&m, &format!("/v1/disputes/{du}")).await;
    assert_eq!(d["status"], "lost");
    assert_eq!(d["balance_transactions"].as_array().unwrap().len(), 1);
    assert_eq!(net_sum(&m, &du), -(2500 + DISPUTE_FEE));
    assert_eq!(
        dispute_events(&m, &du),
        [
            "charge.dispute.created",
            "charge.dispute.funds_withdrawn",
            "charge.dispute.updated",
            "charge.dispute.closed",
        ]
    );
}

#[tokio::test]
async fn warning_closed_inquiry_moves_no_funds() {
    let m = mock_with("seed = 6").await;
    let ch = paid_charge(&m, |a| m.run_action(a), 1200);
    let cause = dispute(&m, &ch, DisputeOutcome::WarningClosed).unwrap();
    let du = only_id(&caused(&m, &cause), ObjectType::Dispute);
    m.step(Duration::from_secs(70 * 86_400)).await.unwrap();
    let statuses: Vec<String> = history(&m, &du)
        .iter()
        .map(|u| u.state["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        statuses,
        [
            "warning_needs_response",
            "warning_under_review",
            "warning_closed"
        ]
    );
    let d = json(&m, &format!("/v1/disputes/{du}")).await;
    assert_eq!(d["balance_transactions"], serde_json::json!([]));
    assert_eq!(d["is_charge_refundable"], true);
    assert_eq!(d["payment_method_details"]["card"]["case_type"], "inquiry");
    assert_eq!(
        dispute_events(&m, &du),
        [
            "charge.dispute.created",
            "charge.dispute.updated",
            "charge.dispute.closed",
        ]
    );
}

#[tokio::test]
async fn dispute_needs_an_undisputed_charge() {
    let m = mock_with("seed = 7").await;
    // Nothing to dispute: a control dispute with no charge is an invalid action.
    let e = m
        .run_action(Action::Dispute {
            charge: None,
            outcome: None,
        })
        .unwrap_err();
    assert!(matches!(e, MockError::InvalidAction(_)), "{e:?}");
    let e = dispute(&m, "ch_nope", DisputeOutcome::Lost).unwrap_err();
    assert!(matches!(e, MockError::NoSuchObject { .. }), "{e:?}");

    let failed = {
        let cause = m
            .run_action(Action::OneOffPayment {
                customer: None,
                amount: Some(800),
                succeed: Some(false),
            })
            .unwrap();
        only_id(&caused(&m, &cause), ObjectType::Charge)
    };
    let refunded = paid_charge(&m, |a| m.run_action(a), 800);
    m.run_action(Action::Refund {
        charge: Some(refunded.clone()),
        amount: Some(100),
    })
    .unwrap();
    let disputed = paid_charge(&m, |a| m.run_action(a), 800);
    dispute(&m, &disputed, DisputeOutcome::Lost).unwrap();
    let before = m.truth_log().len();
    for ch in [&failed, &refunded, &disputed] {
        let e = dispute(&m, ch, DisputeOutcome::Won).unwrap_err();
        match e {
            MockError::InvalidAction(msg) => assert!(msg.contains(ch.as_str()), "{msg}"),
            e => panic!("{e:?}"),
        }
    }
    assert_eq!(
        m.truth_log().len(),
        before,
        "a rejected dispute writes nothing"
    );
    // The only undisputed, unrefunded charge left is gone; a random dispute finds none.
    let e = m
        .run_action(Action::Dispute {
            charge: None,
            outcome: None,
        })
        .unwrap_err();
    assert!(matches!(e, MockError::InvalidAction(_)), "{e:?}");
    // A charge under a chargeback cannot be refunded.
    let e = m
        .run_action(Action::Refund {
            charge: Some(disputed.clone()),
            amount: None,
        })
        .unwrap_err();
    assert!(matches!(e, MockError::InvalidAction(_)), "{e:?}");
}

#[tokio::test]
async fn generated_disputes_target_undisputed_charges() {
    let m = mock_with("seed = 11\n[actions]\none_off_payment = 20.0\nrefund = 2.0\ndispute = 4.0")
        .await;
    m.step(Duration::from_secs(12 * 3600)).await.unwrap();
    let log = m.truth_log();
    let opened: Vec<&StateUpdate> = log
        .iter()
        .filter(|u| u.object_type == ObjectType::Dispute && u.version == 1)
        .collect();
    assert!(opened.len() > 10, "{}", opened.len());
    let mut charges: Vec<&str> = opened
        .iter()
        .map(|u| u.state["charge"].as_str().unwrap())
        .collect();
    let n = charges.len();
    charges.sort_unstable();
    charges.dedup();
    assert_eq!(charges.len(), n, "a charge got two disputes");
    for u in &opened {
        let ch = u.state["charge"].as_str().unwrap();
        // The charge as it was just before the dispute opened.
        let before = log
            .iter()
            .filter(|c| c.object_type == ObjectType::Charge && c.object_id == ch)
            .rfind(|c| c.seq < u.seq)
            .unwrap();
        assert_eq!(before.state["status"], "succeeded");
        assert_eq!(before.state["amount_refunded"], 0);
        assert_eq!(before.state["disputed"], false);
    }
    let reasons: std::collections::BTreeSet<&str> = opened
        .iter()
        .map(|u| u.state["reason"].as_str().unwrap())
        .collect();
    assert!(reasons.len() > 1, "{reasons:?}");
}

fn dispute_ids(m: &MockStripe) -> Vec<(String, u64, Value)> {
    m.truth_log()
        .into_iter()
        .filter(|u| {
            matches!(
                u.object_type,
                ObjectType::Dispute | ObjectType::BalanceTransaction
            )
        })
        .map(|u| (u.object_id, u.version, u.state))
        .collect()
}

#[tokio::test]
async fn disputes_are_reproducible_and_replay() {
    let toml = "seed = 13\n[actions]\none_off_payment = 2.0\ndispute = 1.0";
    let a = mock_with(toml).await;
    let b = mock_with(toml).await;
    for m in [&a, &b] {
        m.step(Duration::from_secs(20 * 86_400)).await.unwrap();
    }
    let ids = dispute_ids(&a);
    assert!(
        ids.iter()
            .any(|(id, v, _)| id.starts_with("du_") && *v == 3)
    );
    assert_eq!(ids, dispute_ids(&b));

    let m = common::mock_with_control("seed = 4").await;
    let ch = paid_charge(&m, |a| m.run_action(a), 900);
    let r = common::ctl_post(
        &m,
        "/control/ops",
        serde_json::json!({"op": "run_action", "action": {"type": "dispute", "charge": ch, "outcome": "won"}}),
    )
    .await;
    assert_eq!(r.status(), 200);
    let r = common::ctl_post(
        &m,
        "/control/ops",
        serde_json::json!({"op": "run_action", "action": {"type": "dispute", "charge": ch}}),
    )
    .await;
    assert_eq!(r.status(), 409);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_action");
    m.step(Duration::from_secs(70 * 86_400)).await.unwrap();
    let replayed = MockStripe::replay(
        m.export_run(),
        ductkit_stripe_mock::ReplayOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(dispute_ids(&replayed), dispute_ids(&m));
}

#[tokio::test]
async fn dispute_events_reach_connect_destinations() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let toml = format!(
        "[accounts]\nconnect = true\n\
         [[destinations]]\nname = \"snap\"\nurl = \"{}\"\npayload = \"snapshot\"\n\
         enabled_events = [\"charge.dispute.*\"]\nsecret = \"whsec_test\"\nconnect = true\n",
        rx.url("/h")
    );
    let m = mock_with(&toml).await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    let a = m.account(&acct);
    let ch = paid_charge(&m, |x| a.run_action(x), 3000);
    let cause = a
        .run_action(Action::Dispute {
            charge: Some(ch.clone()),
            outcome: Some(DisputeOutcome::Won),
        })
        .unwrap();
    let mine = caused(&m, &cause);
    assert!(mine.iter().all(|u| u.account.as_deref() == Some(&*acct)));
    let du = only_id(&mine, ObjectType::Dispute);
    let r = common::get_as(&m, &format!("/v1/disputes/{du}"), &acct).await;
    assert_eq!(r.status(), 200);
    assert_eq!(get(&m, &format!("/v1/disputes/{du}")).await.status(), 404);
    m.step(Duration::from_secs(70 * 86_400)).await.unwrap();
    m.quiesce().await.unwrap();
    let got: Vec<Value> = rx
        .requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let mut types: Vec<&str> = got
        .iter()
        .filter(|b| b["data"]["object"]["id"] == du.as_str())
        .map(|b| {
            assert_eq!(b["account"], acct.as_str(), "{b}");
            b["type"].as_str().unwrap()
        })
        .collect();
    types.sort_unstable();
    assert_eq!(
        types,
        [
            "charge.dispute.closed",
            "charge.dispute.created",
            "charge.dispute.funds_reinstated",
            "charge.dispute.funds_withdrawn",
            "charge.dispute.updated",
        ]
    );
}

fn refund(m: &MockStripe, charge: &str) -> Result<Cause, MockError> {
    m.run_action(Action::Refund {
        charge: Some(charge.to_string()),
        amount: None,
    })
}

#[tokio::test]
async fn won_dispute_makes_charge_refundable_again() {
    let m = mock_with("seed = 5").await;
    let ch = paid_charge(&m, |a| m.run_action(a), 2500);
    let cause = dispute(&m, &ch, DisputeOutcome::Won).unwrap();
    let du = only_id(&caused(&m, &cause), ObjectType::Dispute);
    assert!(
        refund(&m, &ch).is_err(),
        "an open chargeback blocks refunds"
    );
    m.step(Duration::from_secs(70 * 86_400)).await.unwrap();
    let d = json(&m, &format!("/v1/disputes/{du}")).await;
    assert_eq!(d["status"], "won");
    assert_eq!(d["is_charge_refundable"], true);
    let cause = refund(&m, &ch).unwrap();
    let re = only_id(&caused(&m, &cause), ObjectType::Refund);
    assert_eq!(
        m.get(ObjectType::Refund, &re).unwrap().state["amount"],
        2500
    );
}

#[tokio::test]
async fn lost_dispute_keeps_charge_unrefundable() {
    let m = mock_with("seed = 5").await;
    let ch = paid_charge(&m, |a| m.run_action(a), 2500);
    let cause = dispute(&m, &ch, DisputeOutcome::Lost).unwrap();
    let du = only_id(&caused(&m, &cause), ObjectType::Dispute);
    m.step(Duration::from_secs(70 * 86_400)).await.unwrap();
    let d = json(&m, &format!("/v1/disputes/{du}")).await;
    assert_eq!(d["status"], "lost");
    assert_eq!(d["is_charge_refundable"], false);
    assert!(refund(&m, &ch).is_err());
}
