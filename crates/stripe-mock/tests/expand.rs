mod common;

use common::{get, mock_with, recurring_prices};
use ductkit_stripe_mock::{Action, MockStripe, ObjectType};
use serde_json::{Value, json};

async fn json(m: &MockStripe, path: &str) -> (u16, Value) {
    let r = get(m, path).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
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

#[tokio::test]
async fn retrieve_expands_customer_and_invoice() {
    let m = mock_with("").await;
    let sub = start(&m, None);
    let (s, plain) = json(&m, &format!("/v1/subscriptions/{sub}")).await;
    assert_eq!(s, 200, "{plain}");
    assert!(plain["customer"].is_string());
    assert!(plain["latest_invoice"].is_string());
    let (s, v) = json(
        &m,
        &format!("/v1/subscriptions/{sub}?expand[]=customer&expand[]=latest_invoice"),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["customer"]["object"], "customer");
    assert_eq!(v["customer"]["id"], plain["customer"]);
    assert_eq!(v["latest_invoice"]["object"], "invoice");
    assert_eq!(v["latest_invoice"]["id"], plain["latest_invoice"]);
}

#[tokio::test]
async fn list_expands_with_data_prefix() {
    let m = mock_with("").await;
    recurring_prices(&m, 3, "month");
    let (s, v) = json(&m, "/v1/prices?expand[]=data.product").await;
    assert_eq!(s, 200, "{v}");
    let data = v["data"].as_array().unwrap();
    assert!(!data.is_empty());
    assert!(data.iter().all(|p| p["product"]["object"] == "product"));
    let (s, v) = json(&m, "/v1/prices?expand[]=product").await;
    assert_eq!(s, 400, "{v}");
    assert_eq!(v["error"]["param"], "expand");
}

#[tokio::test]
async fn nested_items_price_product() {
    let m = mock_with("").await;
    let sub = start(&m, None);
    let (s, v) = json(
        &m,
        &format!("/v1/subscriptions/{sub}?expand[]=items.data.price.product"),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v["items"]["data"][0]["price"]["product"]["object"],
        "product"
    );
}

#[tokio::test]
async fn deleted_customer_expands_to_stub() {
    let m = mock_with("").await;
    let sub = start(&m, None);
    let customer = m.get(ObjectType::Subscription, &sub).unwrap().state["customer"]
        .as_str()
        .unwrap()
        .to_string();
    m.delete_customer(&customer).unwrap();
    let (s, v) = json(&m, &format!("/v1/subscriptions/{sub}?expand[]=customer")).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["status"], "canceled");
    assert_eq!(
        v["customer"],
        json!({"id": customer, "object": "customer", "deleted": true})
    );
}

#[tokio::test]
async fn bad_expand_is_400() {
    let m = mock_with("").await;
    let sub = start(&m, None);
    let (s, v) = json(&m, &format!("/v1/subscriptions/{sub}?expand[]=status")).await;
    assert_eq!(s, 400);
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(
        v["error"]["message"],
        "This property cannot be expanded (status)."
    );
    assert_eq!(v["error"]["param"], "expand");
    let (s, v) = json(
        &m,
        &format!("/v1/subscriptions/{sub}?expand[]=items.data.price.product.a.b"),
    )
    .await;
    assert_eq!(s, 400);
    assert_eq!(
        v["error"]["message"],
        "You cannot expand more than 4 levels of a property."
    );
}

#[tokio::test]
async fn connect_money_expand_paths_are_validated() {
    let m = mock_with("[accounts]\nconnect = true\n").await;
    for ok in [
        "/v1/transfers?expand[]=data.destination_payment&expand[]=data.balance_transaction",
        "/v1/application_fees?expand[]=data.charge",
        "/v1/payouts?expand[]=data.destination",
        "/v1/charges?expand[]=data.transfer&expand[]=data.application_fee&expand[]=data.source_transfer&expand[]=data.transfer_data.destination",
        "/v1/refunds?expand[]=data.source_transfer_reversal&expand[]=data.transfer_reversal",
    ] {
        let (s, v) = json(&m, ok).await;
        assert_eq!((s, v["object"].as_str()), (200, Some("list")), "{ok}");
    }
    for bad in [
        "/v1/transfers?expand[]=data.reversals",
        "/v1/payouts?expand[]=data.nope",
    ] {
        let (s, v) = json(&m, bad).await;
        assert_eq!(s, 400, "{bad}: {v}");
    }
}
