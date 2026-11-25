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
