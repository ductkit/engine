mod common;

use std::time::Duration;

use common::{Receiver, get, mock_with, recurring_prices};
use ductkit_stripe_mock::{Action, InvoiceOutcome, MockStripe, ObjectType};
use serde_json::Value;

async fn json(m: &MockStripe, path: &str) -> (u16, Value) {
    let r = get(m, path).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

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

/// Every ID of a sub-list, following `starting_after` with `limit=2`.
async fn walk(m: &MockStripe, base: &str) -> (Vec<String>, Vec<String>) {
    let sep = if base.contains('?') { '&' } else { '?' };
    let (s, first) = json(m, &format!("{base}{sep}limit=2")).await;
    assert_eq!(s, 200, "{first}");
    let first_ids = ids(&first);
    let mut all = first_ids.clone();
    let mut page = first;
    while page["has_more"] == true {
        let last = all.last().unwrap().clone();
        page = json(m, &format!("{base}{sep}limit=2&starting_after={last}"))
            .await
            .1;
        all.extend(ids(&page));
    }
    (first_ids, all)
}

#[tokio::test]
async fn every_type_lists_and_retrieves() {
    let m = mock_with("seed = 6").await;
    start(&m, None);
    m.run_action(Action::OneOffInvoice {
        customer: None,
        outcome: Some(InvoiceOutcome::Paid),
    })
    .unwrap();
    for path in [
        "/v1/products",
        "/v1/prices",
        "/v1/subscriptions",
        "/v1/invoices",
        "/v1/customers",
    ] {
        let (s, list) = json(&m, path).await;
        assert_eq!(
            (s, list["object"].clone()),
            (200, Value::from("list")),
            "{path}"
        );
        assert_eq!(list["url"], path);
        let first = &list["data"][0];
        let (s, one) = json(&m, &format!("{path}/{}", first["id"].as_str().unwrap())).await;
        assert_eq!(s, 200, "{path}");
        assert_eq!(&one, first, "{path}");
    }
}

#[tokio::test]
async fn sub_list_pages_through_all_items() {
    let m = mock_with("[payloads]\nnested_list_page_size = 2").await;
    let prices = recurring_prices(&m, 5, "month");
    let sub = start(&m, Some(prices));
    let state = m.get(ObjectType::Subscription, &sub).unwrap().state;
    let embedded = ids(&state["items"]);
    let (first, all) = walk(&m, &format!("/v1/subscription_items?subscription={sub}")).await;
    assert_eq!(first, embedded);
    assert_eq!(all.len(), 5);
    let mut unique = all.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 5);

    let invoice = state["latest_invoice"].as_str().unwrap();
    let inv = m.get(ObjectType::Invoice, invoice).unwrap().state;
    let (first, all) = walk(&m, &format!("/v1/invoices/{invoice}/lines")).await;
    assert_eq!(first, ids(&inv["lines"]));
    assert_eq!(all.len(), 5);
}

#[tokio::test]
async fn subscription_items_requires_subscription() {
    let m = mock_with("").await;
    let (s, v) = json(&m, "/v1/subscription_items").await;
    assert_eq!(s, 400);
    assert_eq!(v["error"]["code"], "parameter_missing");
    assert_eq!(v["error"]["param"], "subscription");
}

#[tokio::test]
async fn canceled_hidden_by_default() {
    let m = mock_with("").await;
    let a = start(&m, None);
    start(&m, None);
    m.run_action(Action::CancelSubscription {
        id: Some(a),
        at_period_end: Some(false),
    })
    .unwrap();
    assert_eq!(ids(&json(&m, "/v1/subscriptions").await.1).len(), 1);
    assert_eq!(
        ids(&json(&m, "/v1/subscriptions?status=all").await.1).len(),
        2
    );
    assert_eq!(
        ids(&json(&m, "/v1/subscriptions?status=canceled").await.1).len(),
        1
    );
}

#[tokio::test]
async fn active_filter_hides_archived_prices() {
    let m = mock_with("").await;
    let p = recurring_prices(&m, 1, "month").remove(0);
    m.run_action(Action::Archive { id: p.clone() }).unwrap();
    assert!(ids(&json(&m, "/v1/prices").await.1).contains(&p));
    assert!(!ids(&json(&m, "/v1/prices?active=true").await.1).contains(&p));
    assert!(ids(&json(&m, "/v1/prices?active=false").await.1).contains(&p));
    let (s, v) = json(&m, "/v1/prices?active=yes").await;
    assert_eq!(
        (s, v["error"]["code"].clone()),
        (400, Value::from("parameter_invalid_boolean"))
    );
}

#[tokio::test]
async fn thin_related_object_fetch() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!(
        "[[destinations]]\nname = \"thin\"\nurl = \"{}\"\npayload = \"thin\"\nenabled_events = [\"v1.invoice.*\", \"v1.customer.subscription.*\"]\nsecret = \"whsec_test\"\n",
        rx.url("/thin")
    ))
    .await;
    m.run_action(Action::OneOffInvoice {
        customer: None,
        outcome: Some(InvoiceOutcome::DeleteDraft),
    })
    .unwrap();
    let sub = start(&m, None);
    m.step(Duration::from_millis(1)).await.unwrap();
    let got: Vec<Value> = rx
        .wait_for(3, Duration::from_secs(5))
        .await
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let created = got
        .iter()
        .find(|n| n["type"] == "v1.invoice.created")
        .unwrap();
    let (s, v) = json(&m, created["related_object"]["url"].as_str().unwrap()).await;
    assert_eq!(
        (s, v["error"]["code"].clone()),
        (404, Value::from("resource_missing"))
    );
    let updated = got
        .iter()
        .find(|n| n["type"] == "v1.customer.subscription.updated")
        .unwrap();
    assert_eq!(updated["related_object"]["id"], sub.as_str());
    let (s, v) = json(&m, updated["related_object"]["url"].as_str().unwrap()).await;
    assert_eq!(s, 200);
    assert_eq!(v, m.get(ObjectType::Subscription, &sub).unwrap().state);
}
