//! Daily payouts per scope, their lifecycle and failures, and the index of the balance
//! transactions each payout swept (spec sections 4, 5.5, 7.2 and 11; phase 8 rulings P4, P5
//! and P6).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use common::{expected_sweeps, get, get_as, mock_with};
use ductkit_stripe_mock::{
    AccountApi, AccountKind, Action, ChargePattern, EventFormat, MockError, MockStripe, ObjectType,
    PayoutMode, StateUpdate,
};
use serde_json::Value;

const HOUR: u64 = 3600;
const DAY: u64 = 86_400;

const DAILY: &str = "[actions]
payout = \"daily\"
";

async fn json(m: &MockStripe, path: &str, acct: Option<&str>) -> Value {
    let r = match acct {
        Some(a) => get_as(m, path, a).await,
        None => get(m, path).await,
    };
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

/// The IDs of the payouts in `scope`, in creation order.
fn payouts_in(m: &MockStripe, scope: Option<&str>) -> Vec<String> {
    let mut ids = Vec::new();
    for u in m.truth_log() {
        if u.object_type == ObjectType::Payout
            && u.account.as_deref() == scope
            && !ids.contains(&u.object_id)
        {
            ids.push(u.object_id.clone());
        }
    }
    ids
}

/// The snapshot events of an object, in order: `(type, created in seconds)`.
fn events_of(m: &MockStripe, id: &str) -> Vec<(String, i64)> {
    m.event_timeline()
        .into_iter()
        .filter(|e| e.object_id == id && e.format == EventFormat::Snapshot)
        .map(|e| (e.event_type, e.created.unix_seconds()))
        .collect()
}

/// Every balance transaction ID that `GET /v1/balance_transactions?payout=` lists, page by
/// page (`limit=2`), failing on a repeat.
async fn listed_members(m: &MockStripe, payout: &str, acct: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let mut path = format!("/v1/balance_transactions?payout={payout}&limit=2");
        if let Some(a) = &after {
            path.push_str(&format!("&starting_after={a}"));
        }
        let page = json(m, &path, acct).await;
        for item in page["data"].as_array().unwrap() {
            let id = item["id"].as_str().unwrap().to_string();
            assert!(!out.contains(&id), "{id} listed twice for {payout}");
            out.push(id);
        }
        if page["has_more"] != true {
            return out;
        }
        after = out.last().cloned();
    }
}

fn next_midnight_after(secs: i64) -> i64 {
    (secs.div_euclid(DAY as i64) + 1) * DAY as i64
}

#[tokio::test]
async fn daily_payout_lifecycle() {
    let m = mock_with(DAILY).await;
    m.step(Duration::from_secs(HOUR)).await.unwrap();
    let cause = m.run_action(payment(10_000)).unwrap();
    let bt: StateUpdate = m
        .truth_log()
        .into_iter()
        .find(|u| {
            u.cause.as_ref() == Some(&cause) && u.object_type == ObjectType::BalanceTransaction
        })
        .unwrap();
    let available_on = bt.state["available_on"].as_i64().unwrap();
    let net = bt.state["net"].as_i64().unwrap();

    m.step(Duration::from_secs(3 * DAY)).await.unwrap();
    let ids = payouts_in(&m, None);
    assert_eq!(ids.len(), 1, "{ids:?}");
    let po = &ids[0];
    assert!(po.starts_with("po_"), "{po}");
    let midnight = next_midnight_after(available_on);
    let p = json(&m, &format!("/v1/payouts/{po}"), None).await;
    common::assert_conforms(&p);
    assert_eq!(p["amount"], net);
    assert_eq!(p["currency"], bt.state["currency"]);
    assert_eq!(p["created"], midnight);
    assert_eq!(p["arrival_date"], midnight + 2 * DAY as i64);
    assert_eq!(p["status"], "in_transit");
    assert_eq!(p["automatic"], true);
    assert_eq!(p["method"], "standard");
    assert_eq!(p["type"], "bank_account");
    assert_eq!(p["source_type"], "card");
    assert_eq!(p["reconciliation_status"], "in_progress");
    assert!(p["failure_code"].is_null(), "{p:#}");
    assert!(
        p["destination"].as_str().unwrap().starts_with("ba_"),
        "{p:#}"
    );

    // The payout's own balance transaction: `-amount`, available at once, never a member.
    let pbt_id = p["balance_transaction"].as_str().unwrap();
    let pbt = json(&m, &format!("/v1/balance_transactions/{pbt_id}"), None).await;
    assert_eq!(pbt["type"], "payout");
    assert_eq!(pbt["amount"], -net);
    assert_eq!(pbt["net"], -net);
    assert_eq!(pbt["fee"], 0);
    assert_eq!(pbt["status"], "available");
    assert_eq!(pbt["source"], po.as_str());
    assert_eq!(
        listed_members(&m, po, None).await,
        std::slice::from_ref(&bt.object_id)
    );

    m.step(Duration::from_secs(2 * DAY)).await.unwrap();
    let p = json(&m, &format!("/v1/payouts/{po}"), None).await;
    assert_eq!(p["status"], "paid");
    assert_eq!(p["reconciliation_status"], "completed");
    let arrival = midnight + 2 * DAY as i64;
    assert_eq!(
        events_of(&m, po),
        [
            ("payout.created".to_string(), midnight),
            ("payout.updated".to_string(), midnight + HOUR as i64),
            ("payout.paid".to_string(), arrival),
            ("payout.updated".to_string(), arrival),
            ("payout.reconciliation_completed".to_string(), arrival),
        ]
    );
    // Statuses in order, one per truth-log entry.
    let statuses: Vec<String> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_id == *po)
        .map(|u| u.state["status"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(statuses, ["pending", "in_transit", "paid"]);
    // The balance was paid out, so no further payout follows.
    assert_eq!(payouts_in(&m, None).len(), 1);
}

#[tokio::test]
async fn failed_payout_returns_funds() {
    let m = mock_with(
        "[actions]
payout = \"daily\"
payout_failure_probability = 1.0
",
    )
    .await;
    m.step(Duration::from_secs(HOUR)).await.unwrap();
    m.run_action(payment(10_000)).unwrap();
    m.step(Duration::from_secs(6 * DAY)).await.unwrap();
    let ids = payouts_in(&m, None);
    assert_eq!(ids.len(), 2, "{ids:?}");
    let first = json(&m, &format!("/v1/payouts/{}", ids[0]), None).await;
    common::assert_conforms(&first);
    assert_eq!(first["status"], "failed");
    assert_eq!(first["failure_code"], "account_closed");
    assert_eq!(
        first["failure_message"],
        "The bank account has been closed."
    );
    let amount = first["amount"].as_i64().unwrap();
    let arrival = first["arrival_date"].as_i64().unwrap();
    let fbt_id = first["failure_balance_transaction"].as_str().unwrap();
    let fbt = json(&m, &format!("/v1/balance_transactions/{fbt_id}"), None).await;
    assert_eq!(fbt["type"], "payout_failure");
    assert_eq!(fbt["amount"], amount);
    assert_eq!(fbt["net"], amount);
    assert_eq!(fbt["status"], "available");
    assert_eq!(fbt["created"], arrival);
    assert_eq!(fbt["source"], ids[0].as_str());
    let events = events_of(&m, &ids[0]);
    assert_eq!(
        events[2..],
        [
            ("payout.failed".to_string(), arrival),
            ("payout.updated".to_string(), arrival),
        ]
    );

    // The next midnight pays the returned amount out again.
    let second = json(&m, &format!("/v1/payouts/{}", ids[1]), None).await;
    assert_eq!(second["amount"], amount);
    assert_eq!(second["created"], arrival + DAY as i64);
    assert_eq!(
        listed_members(&m, &ids[1], None).await,
        [fbt_id.to_string()]
    );
}

#[tokio::test]
async fn payout_sweeps_each_transaction_once() {
    let m = mock_with(
        "[accounts]
connect = true
connected_accounts = { initial = 2 }
account_types = { standard = 0.5, express = 0.0, custom = 0.5 }
[actions]
new_customer = 1.0
one_off_payment = 3.0
refund = 0.5
payout = \"daily\"
payout_failure_probability = 0.5
[api]
pagination_writes = { probability = 1.0 }
",
    )
    .await;
    m.step(Duration::from_secs(7 * DAY)).await.unwrap();
    let log = m.truth_log();
    let expected = expected_sweeps(&log);
    assert!(expected.len() >= 6, "{}", expected.len());
    let net: BTreeMap<(Option<String>, String), i64> = log
        .iter()
        .filter(|u| u.object_type == ObjectType::BalanceTransaction)
        .map(|u| {
            (
                (u.account.clone(), u.object_id.clone()),
                u.state["net"].as_i64().unwrap(),
            )
        })
        .collect();
    let mut seen: BTreeSet<(Option<String>, String)> = BTreeSet::new();
    let mut scopes = BTreeSet::new();
    let mut failures = 0;
    for (po, (scope, members)) in &expected {
        scopes.insert(scope.clone());
        let p = json(&m, &format!("/v1/payouts/{po}"), scope.as_deref()).await;
        if p["status"] == "failed" {
            failures += 1;
        }
        let mut listed = listed_members(&m, po, scope.as_deref()).await;
        listed.sort();
        let mut want = members.clone();
        want.sort();
        assert_eq!(listed, want, "{po}");
        let sum: i64 = members
            .iter()
            .map(|id| net[&(scope.clone(), id.clone())])
            .sum();
        assert!(sum > 0, "{po}: {sum}");
        assert_eq!(p["amount"], sum, "{po}");
        for id in members {
            assert!(
                seen.insert((scope.clone(), id.clone())),
                "{id} in two payouts"
            );
        }
    }
    assert!(scopes.len() >= 2, "{scopes:?}");
    assert!(failures > 0);

    // Per scope and currency, everything adds up: the sum of every balance transaction's
    // `net` equals the sum over those neither swept nor `type: payout`.
    let mut latest: BTreeMap<(Option<String>, String), &StateUpdate> = BTreeMap::new();
    for u in log
        .iter()
        .filter(|u| u.object_type == ObjectType::BalanceTransaction)
    {
        latest.insert((u.account.clone(), u.object_id.clone()), u);
    }
    let mut all: BTreeMap<(Option<String>, String), i64> = BTreeMap::new();
    let mut open: BTreeMap<(Option<String>, String), i64> = BTreeMap::new();
    for ((scope, id), u) in &latest {
        let key = (
            scope.clone(),
            u.state["currency"].as_str().unwrap().to_string(),
        );
        let n = u.state["net"].as_i64().unwrap();
        *all.entry(key.clone()).or_default() += n;
        if u.state["type"] != "payout" && !seen.contains(&(scope.clone(), id.clone())) {
            *open.entry(key).or_default() += n;
        } else {
            open.entry(key).or_default();
        }
    }
    assert_eq!(all, open);
}

#[tokio::test]
async fn connected_account_payouts_use_default_bank_account() {
    let m = mock_with(
        "[accounts]
connect = true
account_types = { standard = 0.5, express = 0.0, custom = 0.5 }
[actions]
payout = \"daily\"
",
    )
    .await;
    let mut custom = None;
    let mut standard = None;
    for _ in 0..30 {
        let id = m.connect_account(AccountApi::V1).unwrap();
        let kind = m.accounts().into_iter().find(|a| a.id == id).unwrap().kind;
        match kind {
            AccountKind::Custom if custom.is_none() => custom = Some(id),
            AccountKind::Standard if standard.is_none() => standard = Some(id),
            _ => {}
        }
        if custom.is_some() && standard.is_some() {
            break;
        }
    }
    let (custom, standard) = (custom.unwrap(), standard.unwrap());
    m.step(Duration::from_secs(HOUR)).await.unwrap();
    for acct in [&custom, &standard] {
        m.account(acct).run_action(payment(5000)).unwrap();
    }
    m.step(Duration::from_secs(3 * DAY)).await.unwrap();

    // The Custom account's payout goes to its default bank account, which expands.
    let ids = payouts_in(&m, Some(&custom));
    assert_eq!(ids.len(), 1, "{ids:?}");
    let p = json(
        &m,
        &format!("/v1/payouts/{}?expand[]=destination", ids[0]),
        Some(&custom),
    )
    .await;
    common::assert_conforms(&p);
    assert_eq!(p["destination"]["object"], "bank_account", "{p:#}");
    assert_eq!(p["destination"]["default_for_currency"], true);
    let ba = p["destination"]["id"].as_str().unwrap();
    let list = json(&m, &format!("/v1/payouts?destination={ba}"), Some(&custom)).await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);

    // The Standard account's names a bank account that is in no store: it stays an ID.
    let ids = payouts_in(&m, Some(&standard));
    assert_eq!(ids.len(), 1, "{ids:?}");
    let p = json(
        &m,
        &format!("/v1/payouts/{}?expand[]=destination", ids[0]),
        Some(&standard),
    )
    .await;
    common::assert_conforms(&p);
    let dest = p["destination"].as_str().expect("an unexpanded ID");
    assert!(dest.starts_with("ba_"), "{dest}");
    let ext = json(
        &m,
        &format!("/v1/accounts/{standard}/external_accounts"),
        None,
    )
    .await;
    assert!(ext["data"].as_array().unwrap().is_empty());

    // One destination per scope: a later payout names the same one.
    m.account(&standard).run_action(payment(5000)).unwrap();
    m.step(Duration::from_secs(3 * DAY)).await.unwrap();
    let ids = payouts_in(&m, Some(&standard));
    assert_eq!(ids.len(), 2, "{ids:?}");
    let later = json(&m, &format!("/v1/payouts/{}", ids[1]), Some(&standard)).await;
    assert_eq!(later["destination"], dest);
}

#[tokio::test]
async fn disconnect_mid_money_flow() {
    let m = mock_with(
        "[accounts]
connect = true
account_types = { standard = 0.0, express = 0.0, custom = 1.0 }
platform_payments = { own = 0.0, destination = 0.0, separate = 1.0 }
application_fee = { probability = 1.0, percent = 10.0 }
[actions]
payout = \"daily\"
",
    )
    .await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    m.step(Duration::from_secs(HOUR)).await.unwrap();
    m.account(&acct).run_action(payment(10_000)).unwrap();
    // Jan 1 01:00 + 3 days 1 hour: the payout made at Jan 4 00:00 went in transit at 01:00.
    m.step(Duration::from_secs(3 * DAY + HOUR)).await.unwrap();
    let ids = payouts_in(&m, Some(&acct));
    assert_eq!(ids.len(), 1, "{ids:?}");
    let po = ids[0].clone();
    // From the truth log: a disconnected account's handle refuses reads.
    let status = |m: &MockStripe| {
        m.truth_log()
            .into_iter()
            .rfind(|u| u.object_id == po)
            .unwrap()
            .state["status"]
            .clone()
    };
    assert_eq!(status(&m), "in_transit");

    // A fee whose balance transaction is still pending, and a separate transfer queued.
    let fee_cause = m.account(&acct).run_action(payment(4000)).unwrap();
    let fee_bt = m
        .truth_log()
        .into_iter()
        .find(|u| {
            u.cause.as_ref() == Some(&fee_cause)
                && u.account.is_none()
                && u.object_type == ObjectType::BalanceTransaction
        })
        .expect("the fee's balance transaction");
    assert_eq!(fee_bt.state["status"], "pending");
    let separate = Action::OneOffPayment {
        customer: None,
        amount: Some(2000),
        succeed: Some(true),
        pattern: Some(ChargePattern::Separate),
        recipient: Some(acct.clone()),
    };
    m.run_action(separate).unwrap();
    let transfers = |m: &MockStripe| {
        m.truth_log()
            .into_iter()
            .filter(|u| u.object_type == ObjectType::Transfer)
            .count()
    };
    assert_eq!(transfers(&m), 0, "the transfer is queued");

    m.disconnect_account(&acct).unwrap();
    let cut = m.truth_log().last().unwrap().seq;
    m.step(Duration::from_secs(4 * DAY)).await.unwrap();

    for e in m.event_timeline() {
        if e.truth_seq > cut {
            assert_ne!(e.account.as_deref(), Some(acct.as_str()), "{e:?}");
        }
    }
    assert!(
        m.truth_log()
            .iter()
            .filter(|u| u.seq > cut && u.account.as_deref() == Some(acct.as_str()))
            .all(|u| u.object_type == ObjectType::BalanceTransaction
                && u.state["status"] == "available"),
        "only settlements happen in the account's scope"
    );
    let fee_bt = m
        .get(ObjectType::BalanceTransaction, &fee_bt.object_id)
        .unwrap();
    assert_eq!(fee_bt.state["status"], "available");
    assert_eq!(status(&m), "in_transit");
    assert_eq!(transfers(&m), 0, "no transfer to a disconnected account");
    // The platform's own payouts go on.
    assert!(!payouts_in(&m, None).is_empty());
}

#[tokio::test]
async fn payouts_off_by_default() {
    let m = mock_with("").await;
    let actions = m.effective_config().actions;
    assert_eq!(actions.payout, PayoutMode::Off);
    assert_eq!(actions.payout_failure_probability, 0.0);
    m.run_action(payment(10_000)).unwrap();
    m.step(Duration::from_secs(5 * DAY)).await.unwrap();
    assert!(payouts_in(&m, None).is_empty());
    let r = json(&m, "/v1/payouts", None).await;
    assert!(r["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn payout_mode_switches_at_runtime() {
    let m = mock_with("").await;
    m.run_action(payment(10_000)).unwrap();
    m.step(Duration::from_secs(3 * DAY + HOUR)).await.unwrap();
    assert!(payouts_in(&m, None).is_empty());

    // On: the next midnight pays out.
    m.update_actions_config(|a| a.payout = PayoutMode::Daily)
        .unwrap();
    m.step(Duration::from_secs(DAY)).await.unwrap();
    let ids = payouts_in(&m, None);
    assert_eq!(ids.len(), 1, "{ids:?}");
    let p = json(&m, &format!("/v1/payouts/{}", ids[0]), None).await;
    assert_eq!(p["created"], 4 * DAY as i64 + 1_767_225_600);

    // Off: no later midnight pays out, though the started payout still arrives.
    m.update_actions_config(|a| a.payout = PayoutMode::Off)
        .unwrap();
    m.run_action(payment(3000)).unwrap();
    m.step(Duration::from_secs(5 * DAY)).await.unwrap();
    assert_eq!(payouts_in(&m, None).len(), 1);
    let p = json(&m, &format!("/v1/payouts/{}", ids[0]), None).await;
    assert_eq!(p["status"], "paid");
}

#[tokio::test]
async fn payout_action_pays_out_now() {
    let m = mock_with("").await;
    let err = m.run_action(Action::Payout).unwrap_err();
    assert!(matches!(err, MockError::InvalidAction(_)), "{err:?}");
    m.run_action(payment(10_000)).unwrap();
    m.step(Duration::from_secs(2 * DAY + HOUR)).await.unwrap();
    let cause = m.run_action(Action::Payout).unwrap();
    assert_eq!(cause.action, "payout");
    let ids = payouts_in(&m, None);
    assert_eq!(ids.len(), 1, "{ids:?}");
    let p = json(&m, &format!("/v1/payouts/{}", ids[0]), None).await;
    assert_eq!(p["created"], m.now().unix_seconds());
    assert_eq!(p["status"], "pending");
    // Nothing left to pay out.
    assert!(m.run_action(Action::Payout).is_err());
}
