mod common;

use std::time::Duration;

use common::{mock_with, recurring_prices};
use ductkit_stripe_mock::{Action, ChangeKind, MockStripe, ObjectType, StateUpdate};
use serde_json::json;

const DAY: u64 = 86_400;

fn start(m: &MockStripe, prices: Option<Vec<String>>) -> String {
    m.run_action(Action::StartSubscription {
        customer: None,
        prices,
    })
    .unwrap();
    m.truth_log()
        .into_iter()
        .rev()
        .find(|u| u.object_type == ObjectType::Subscription)
        .unwrap()
        .object_id
}

/// Latest state of every invoice of `sub`, in creation order.
fn sub_invoices(m: &MockStripe, sub: &str) -> Vec<StateUpdate> {
    let mut ids: Vec<String> = Vec::new();
    for u in m.truth_log() {
        if u.object_type == ObjectType::Invoice
            && u.kind == ChangeKind::Created
            && u.state["parent"]["subscription_details"]["subscription"] == sub
        {
            ids.push(u.object_id);
        }
    }
    ids.iter()
        .map(|id| m.get(ObjectType::Invoice, id).unwrap())
        .collect()
}

#[tokio::test]
async fn monthly_subscription_renews_each_period() {
    let m = mock_with("").await;
    let price = recurring_prices(&m, 1, "month");
    let sub = start(&m, Some(price));
    m.step(Duration::from_secs(95 * DAY)).await.unwrap();
    let invoices = sub_invoices(&m, &sub);
    let reasons: Vec<_> = invoices
        .iter()
        .map(|i| i.state["billing_reason"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        reasons,
        [
            "subscription_create",
            "subscription_cycle",
            "subscription_cycle",
            "subscription_cycle"
        ]
    );
    assert!(invoices.iter().all(|i| i.state["status"] == "paid"));
    for w in invoices.windows(2) {
        assert_eq!(w[1].state["period_start"], w[0].state["period_end"]);
    }
    let item = &m.get(ObjectType::Subscription, &sub).unwrap().state["items"]["data"][0];
    assert_eq!(
        item["current_period_start"],
        invoices.last().unwrap().state["period_start"]
    );
}

#[tokio::test]
async fn archived_price_is_still_billed() {
    let m = mock_with("").await;
    let p = recurring_prices(&m, 1, "month").remove(0);
    let sub = start(&m, Some(vec![p.clone()]));
    m.run_action(Action::Archive { id: p.clone() }).unwrap();
    m.step(Duration::from_secs(32 * DAY)).await.unwrap();
    let renewal = sub_invoices(&m, &sub).pop().unwrap();
    assert_eq!(renewal.state["billing_reason"], "subscription_cycle");
    assert_eq!(
        renewal.state["lines"]["data"][0]["pricing"]["price_details"]["price"],
        p.as_str()
    );
    let before = m.truth_log().len();
    for _ in 0..20 {
        start(&m, None);
    }
    let picked_p = m.truth_log()[before..].iter().any(|u| {
        u.object_type == ObjectType::SubscriptionItem && u.state["price"]["id"] == p.as_str()
    });
    assert!(!picked_p, "archived prices are never picked");
}

#[tokio::test]
async fn cancel_at_period_end_then_ends() {
    let m = mock_with("").await;
    let sub = start(&m, Some(recurring_prices(&m, 1, "month")));
    m.run_action(Action::CancelSubscription {
        id: Some(sub.clone()),
        at_period_end: Some(true),
    })
    .unwrap();
    let s = m.get(ObjectType::Subscription, &sub).unwrap();
    assert_eq!(s.state["status"], "active");
    assert_eq!(s.state["cancel_at_period_end"], true);
    assert!(s.state["cancel_at"].is_i64());
    m.step(Duration::from_secs(32 * DAY)).await.unwrap();
    let s = m.get(ObjectType::Subscription, &sub).unwrap();
    assert_eq!(s.state["status"], "canceled");
    assert!(
        sub_invoices(&m, &sub)
            .iter()
            .all(|i| i.state["billing_reason"] != "subscription_cycle")
    );
    let last = m
        .event_timeline()
        .into_iter()
        .find(|e| Some(&e.id) == s.events.snapshot.last())
        .unwrap();
    assert_eq!(last.event_type, "customer.subscription.deleted");
}

#[tokio::test]
async fn immediate_cancel_stops_renewal() {
    let m = mock_with("").await;
    let sub = start(&m, Some(recurring_prices(&m, 1, "month")));
    m.run_action(Action::CancelSubscription {
        id: Some(sub.clone()),
        at_period_end: Some(false),
    })
    .unwrap();
    m.step(Duration::from_secs(40 * DAY)).await.unwrap();
    assert_eq!(sub_invoices(&m, &sub).len(), 1);
    let s = m.get(ObjectType::Subscription, &sub).unwrap();
    assert_eq!(s.state["status"], "canceled");
    assert!(matches!(
        m.run_action(Action::CancelSubscription {
            id: Some(sub),
            at_period_end: None
        }),
        Err(ductkit_stripe_mock::MockError::InvalidAction(_))
    ));
}

#[tokio::test]
async fn past_due_then_recovers_or_ends() {
    let m = mock_with("").await;
    let sub = start(&m, Some(recurring_prices(&m, 1, "month")));
    m.update_actions_config(|a| a.payment_failure_probability = 1.0)
        .unwrap();
    m.step(Duration::from_secs(32 * DAY)).await.unwrap();
    assert_eq!(
        m.get(ObjectType::Subscription, &sub).unwrap().state["status"],
        "past_due"
    );
    let renewal = sub_invoices(&m, &sub).pop().unwrap();
    assert_eq!(
        (
            renewal.state["status"].clone(),
            renewal.state["attempt_count"].clone()
        ),
        (json!("open"), json!(1))
    );
    m.step(Duration::from_secs(3 * DAY)).await.unwrap();
    assert_eq!(
        m.get(ObjectType::Subscription, &sub).unwrap().state["status"],
        "canceled"
    );
    assert_eq!(
        m.get(ObjectType::Invoice, &renewal.object_id)
            .unwrap()
            .state["status"],
        "uncollectible"
    );
    assert!(
        m.update_actions_config(|a| a.payment_failure_probability = 2.0)
            .is_err()
    );
}

#[tokio::test]
async fn customer_delete_cancels_subscriptions_first() {
    let m = mock_with("").await;
    let sub = start(&m, Some(recurring_prices(&m, 1, "month")));
    let customer = m.get(ObjectType::Subscription, &sub).unwrap().state["customer"]
        .as_str()
        .unwrap()
        .to_string();
    let before = m.truth_log().len();
    m.delete_customer(&customer).unwrap();
    let entries = m.truth_log()[before..].to_vec();
    let canceled = entries
        .iter()
        .position(|u| u.object_id == sub && u.state["status"] == "canceled")
        .expect("subscription canceled");
    let deleted = entries
        .iter()
        .position(|u| u.object_id == customer && u.kind == ChangeKind::Deleted)
        .expect("customer deleted");
    assert!(canceled < deleted);
    assert!(
        entries.iter().all(|u| u.cause == entries[0].cause),
        "one action"
    );
    let s = m.get(ObjectType::Subscription, &sub).unwrap();
    assert_eq!(s.state["status"], "canceled");
    assert_eq!(s.state["customer"], customer.as_str());
}

fn monotonic(m: &MockStripe) {
    let log = m.truth_log();
    assert!(
        log.windows(2).all(|w| w[0].at <= w[1].at),
        "truth log time went backwards"
    );
}

#[tokio::test]
async fn long_quiesce_catches_up_renewals() {
    let m = mock_with("").await;
    let sub = start(&m, Some(recurring_prices(&m, 1, "month")));
    m.quiesce().await.unwrap();
    m.step(Duration::from_secs(100 * DAY)).await.unwrap();
    m.resume_generation();
    m.step(Duration::from_secs(3600)).await.unwrap();
    monotonic(&m);
    let invoices = sub_invoices(&m, &sub);
    assert!(invoices.len() >= 4, "{} invoices", invoices.len());
    for w in invoices.windows(2) {
        assert_eq!(w[1].state["period_start"], w[0].state["period_end"]);
    }
    let item = &m.get(ObjectType::Subscription, &sub).unwrap().state["items"]["data"][0];
    assert!(item["current_period_end"].as_i64().unwrap() * 1000 > m.now().0);
}

#[tokio::test]
async fn pending_steps_never_bill_after_customer_delete() {
    let m = mock_with("[actions]\nstep_delay = { distribution = \"fixed\", ms = 7200000 }").await;
    let sub = start(&m, Some(recurring_prices(&m, 1, "month")));
    // Run until the renewal draft exists but has not been finalized.
    let mut hours = 0;
    while sub_invoices(&m, &sub).len() < 2 {
        m.step(Duration::from_secs(3600)).await.unwrap();
        hours += 1;
        assert!(hours < 40 * 24);
    }
    let renewal = sub_invoices(&m, &sub).pop().unwrap();
    assert_eq!(renewal.state["status"], "draft");
    m.run_action(Action::OneOffInvoice {
        customer: Some(renewal.state["customer"].as_str().unwrap().to_string()),
        outcome: Some(ductkit_stripe_mock::InvoiceOutcome::Paid),
    })
    .unwrap();
    let customer = renewal.state["customer"].as_str().unwrap().to_string();
    m.delete_customer(&customer).unwrap();
    let deleted_at = m.truth_log().len();
    m.step(Duration::from_secs(2 * DAY)).await.unwrap();
    let after = &m.truth_log()[deleted_at..];
    assert!(
        !after.iter().any(|u| u.object_type == ObjectType::Invoice
            && u.state["customer"] == customer.as_str()
            && matches!(u.state["status"].as_str(), Some("open" | "paid"))),
        "an invoice of a deleted customer was finalized or paid"
    );
}

#[tokio::test]
async fn cancel_now_drops_pending_renewal_invoice() {
    let m = mock_with("[actions]\nstep_delay = { distribution = \"fixed\", ms = 7200000 }").await;
    let sub = start(&m, Some(recurring_prices(&m, 1, "month")));
    while sub_invoices(&m, &sub).len() < 2 {
        m.step(Duration::from_secs(3600)).await.unwrap();
    }
    let renewal = sub_invoices(&m, &sub).pop().unwrap().object_id;
    m.run_action(Action::CancelSubscription {
        id: Some(sub.clone()),
        at_period_end: Some(false),
    })
    .unwrap();
    m.step(Duration::from_secs(2 * DAY)).await.unwrap();
    let inv = m.get(ObjectType::Invoice, &renewal).unwrap();
    assert!(
        inv.kind == ChangeKind::Deleted || inv.state["status"] == "void",
        "{}",
        inv.state["status"]
    );
}

#[tokio::test]
async fn renewals_resume_after_renew_mode_off() {
    use ductkit_stripe_mock::RenewMode;
    let m = mock_with("").await;
    let sub = start(&m, Some(recurring_prices(&m, 1, "month")));
    m.update_actions_config(|a| a.renew_subscription_cycle = RenewMode::Off)
        .unwrap();
    m.step(Duration::from_secs(40 * DAY)).await.unwrap();
    assert_eq!(sub_invoices(&m, &sub).len(), 1);
    m.update_actions_config(|a| a.renew_subscription_cycle = RenewMode::Natural)
        .unwrap();
    m.step(Duration::from_secs(40 * DAY)).await.unwrap();
    monotonic(&m);
    assert!(sub_invoices(&m, &sub).len() >= 3);
    let item = &m.get(ObjectType::Subscription, &sub).unwrap().state["items"]["data"][0];
    assert!(item["current_period_end"].as_i64().unwrap() * 1000 > m.now().0);
}
