mod common;

use std::time::Duration;

use common::{Receiver, dest, mock_with, recurring_prices};
use ductkit_stripe_mock::{Action, ChangeKind, ObjectType};
use serde_json::{Value, json};

fn start(customer: Option<String>, prices: Option<Vec<String>>) -> Action {
    Action::StartSubscription { customer, prices }
}

#[tokio::test]
async fn start_subscription_fans_out_in_order() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&dest(&rx.url("/h"), "snapshot", "*")).await;
    let cause = m.run_action(start(None, None)).unwrap();
    let mine: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.cause.as_ref() == Some(&cause))
        .collect();
    assert!(mine.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
    let timeline = m.event_timeline();
    let types: Vec<String> = mine
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
    let prerequisites = [
        "customer.created",
        "product.created",
        "price.created",
        "product.updated",
    ];
    let core = [
        "customer.subscription.created",
        "invoice.created",
        "customer.subscription.updated",
        "invoice.finalized",
        "payment_method.attached",
        "payment_intent.created",
        "charge.succeeded",
        "payment_intent.succeeded",
        "invoice_payment.paid",
        "invoice.paid",
        "invoice.payment_succeeded",
        "customer.subscription.updated",
    ];
    let start_at = types
        .iter()
        .position(|t| t == "customer.subscription.created")
        .unwrap();
    assert_eq!(&types[start_at..], &core);
    assert!(
        types[..start_at]
            .iter()
            .all(|t| prerequisites.contains(&t.as_str())),
        "{types:?}"
    );
    m.step(Duration::from_millis(1)).await.unwrap();
    let got: Vec<String> = rx
        .wait_for(types.len(), Duration::from_secs(5))
        .await
        .iter()
        .map(|r| {
            serde_json::from_slice::<Value>(&r.body).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        got, types,
        "one destination receives the fan-out in truth order"
    );
    let sub = mine
        .iter()
        .rev()
        .find(|u| u.object_type == ObjectType::Subscription)
        .unwrap();
    assert_eq!(sub.state["status"], "active");
    assert!(
        sub.state["latest_invoice"]
            .as_str()
            .unwrap()
            .starts_with("in_")
    );
}

#[tokio::test]
async fn many_items_are_truncated() {
    let m = mock_with("[payloads]\nnested_list_page_size = 2").await;
    let prices = recurring_prices(&m, 3, "month");
    m.run_action(start(None, Some(prices))).unwrap();
    let sub = m
        .truth_log()
        .into_iter()
        .rev()
        .find(|u| u.object_type == ObjectType::Subscription)
        .unwrap();
    let items = &sub.state["items"];
    assert_eq!(
        (
            items["data"].as_array().unwrap().len(),
            items["has_more"].clone(),
            items["total_count"].clone()
        ),
        (2, json!(true), json!(3))
    );
    assert_eq!(
        items["url"],
        format!("/v1/subscription_items?subscription={}", sub.object_id)
    );
    let inv = m
        .get(
            ObjectType::Invoice,
            sub.state["latest_invoice"].as_str().unwrap(),
        )
        .unwrap();
    assert_eq!(inv.state["lines"]["data"].as_array().unwrap().len(), 2);
    assert_eq!(inv.state["lines"]["total_count"], 3);
    assert_eq!(inv.state["billing_reason"], "subscription_create");
    assert_eq!(
        inv.state["parent"]["subscription_details"]["subscription"],
        sub.object_id.as_str()
    );
}

#[tokio::test]
async fn failed_first_payment_expires_after_23h() {
    let m = mock_with("[actions]\npayment_failure_probability = 1.0").await;
    let cause = m.run_action(start(None, None)).unwrap();
    let sub_id = m
        .truth_log()
        .into_iter()
        .find(|u| u.object_type == ObjectType::Subscription)
        .unwrap()
        .object_id;
    assert_eq!(
        m.get(ObjectType::Subscription, &sub_id).unwrap().state["status"],
        "incomplete"
    );
    m.step(Duration::from_secs(23 * 3600)).await.unwrap();
    let s = m.get(ObjectType::Subscription, &sub_id).unwrap();
    assert_eq!(
        (s.state["status"].clone(), s.kind),
        (json!("incomplete_expired"), ChangeKind::StateChanged)
    );
    let inv = m
        .get(
            ObjectType::Invoice,
            s.state["latest_invoice"].as_str().unwrap(),
        )
        .unwrap();
    assert_eq!(inv.state["status"], "void");
    assert_eq!(s.cause.unwrap(), cause);
}

#[tokio::test]
async fn billing_run_is_deterministic() {
    // Rates are per simulated hour: a tenth of the spec example keeps 40 days fast.
    let cfg = "seed = 12\n[actions]\nnew_customer = 1.0\nstart_subscription = 0.4\ncancel_subscription = 0.05\ncatalog_change = 0.02\none_off_invoice = 0.2\nstep_delay = { distribution = \"lognormal\", median_ms = 200, p99_ms = 2000 }\npayment_failure_probability = 0.1\n[churn.product]\nupdate = 0.05\narchive = 0.05\n[churn.price]\nupdate = 0.05\narchive = 0.05\n[churn.subscription]\nupdate = 0.05\n[churn.invoice]\nupdate = 0.05\n";
    let a = mock_with(cfg).await;
    let b = mock_with(cfg).await;
    a.step(Duration::from_secs(40 * 86_400)).await.unwrap();
    b.step(Duration::from_secs(40 * 86_400)).await.unwrap();
    let log = a.truth_log();
    assert_eq!(
        serde_json::to_value(&log).unwrap(),
        serde_json::to_value(b.truth_log()).unwrap()
    );
    for t in [
        ObjectType::Customer,
        ObjectType::Product,
        ObjectType::Price,
        ObjectType::Subscription,
        ObjectType::SubscriptionItem,
        ObjectType::Invoice,
        ObjectType::InvoiceLineItem,
    ] {
        assert!(log.iter().any(|u| u.object_type == t), "{t:?} missing");
    }
    for k in [ChangeKind::Archived, ChangeKind::StateChanged] {
        assert!(log.iter().any(|u| u.kind == k), "{k:?} missing");
    }
}
