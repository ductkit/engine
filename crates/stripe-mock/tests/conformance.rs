mod common;

use std::time::Duration;

use common::{Receiver, get, get_as, get_with, mock_with};
use ductkit_stripe_mock::stripe::check_payload;
use ductkit_stripe_mock::{AccountApi, MockStripe};
use serde_json::{Value, json};

fn customer() -> Value {
    json!({
        "id": "cus_1", "object": "customer", "created": 1, "livemode": false,
        "email": "a@example.com", "name": null, "tax_exempt": "none",
        "invoice_settings": { "footer": null }
    })
}

#[test]
fn minimal_valid_customer_conforms() {
    check_payload(&customer()).unwrap();
}

#[test]
fn misspelled_field_is_rejected_with_its_path() {
    let mut v = customer();
    v.as_object_mut().unwrap().remove("email");
    v["emial"] = json!("a@example.com");
    let err = check_payload(&v).unwrap_err();
    assert!(err.path.contains("emial"), "{err}");
    assert_eq!(err.object, "customer");
}

#[test]
fn wrong_enum_value_is_rejected() {
    let mut v = customer();
    v["tax_exempt"] = json!("maybe");
    let err = check_payload(&v).unwrap_err();
    assert!(err.path.contains("tax_exempt"), "{err}");
}

#[test]
fn null_in_non_nullable_field_is_rejected() {
    let mut v = customer();
    v["livemode"] = Value::Null;
    let err = check_payload(&v).unwrap_err();
    assert!(err.path.contains("livemode"), "{err}");
}

#[test]
fn null_in_optional_non_nullable_field_is_rejected() {
    // `balance` is optional and not nullable: absent is fine, `null` is not.
    let mut v = customer();
    v["balance"] = Value::Null;
    let err = check_payload(&v).unwrap_err();
    assert!(err.path.contains("balance"), "{err}");
    let mut event = json!({ "id": "evt_test_1", "object": "v2.core.event", "type": "v1.customer.created",
        "created": "2026-01-01T00:00:00.000Z", "livemode": false });
    check_payload(&event).unwrap();
    event["context"] = Value::Null;
    let err = check_payload(&event).unwrap_err();
    assert!(err.path.contains("context"), "{err}");
}

#[test]
fn list_items_are_checked() {
    let mut bad = customer();
    bad["emial"] = json!("x");
    let list = json!({ "object": "list", "url": "/v1/customers", "has_more": false,
        "data": [customer(), bad] });
    let err = check_payload(&list).unwrap_err();
    assert!(err.path.starts_with("data[1]"), "{err}");
    let ok = json!({ "object": "list", "url": "/v1/customers", "has_more": false,
        "data": [customer()] });
    check_payload(&ok).unwrap();
    let v2 = json!({ "data": [customer()], "next_page_url": null, "previous_page_url": null });
    check_payload(&v2).unwrap();
}

#[test]
fn event_previous_attributes_keys_must_be_fields() {
    let event = |prev: Value| {
        json!({
            "id": "evt_1", "object": "event", "api_version": "2026-11-18.endive",
            "created": 1, "livemode": false, "pending_webhooks": 0, "type": "customer.updated",
            "request": { "id": null, "idempotency_key": null },
            "data": { "object": customer(), "previous_attributes": prev }
        })
    };
    check_payload(&event(json!({ "name": "old" }))).unwrap();
    let err = check_payload(&event(json!({ "nme": "old" }))).unwrap_err();
    assert!(err.path.contains("nme"), "{err}");
}

#[test]
fn deleted_customer_conforms() {
    check_payload(&json!({ "id": "cus_1", "object": "customer", "deleted": true })).unwrap();
}

#[test]
fn error_envelope_conforms() {
    check_payload(&json!({ "error": { "type": "invalid_request_error", "message": "no" } }))
        .unwrap();
    assert!(check_payload(&json!({ "error": { "bogus": 1 } })).is_err());
}

#[test]
fn unknown_object_is_rejected() {
    let err = check_payload(&json!({ "id": "x_1", "object": "no_such_thing" })).unwrap_err();
    assert!(err.message.contains("no_such_thing"), "{err}");
}

/// The status and body of an error response, after checking the body against the schema.
async fn error_body(r: reqwest::Response, status: u16) -> Value {
    assert_eq!(r.status().as_u16(), status);
    let v: Value = r.json().await.expect("JSON error body");
    if let Err(e) = check_payload(&v) {
        panic!(
            "{status} body does not conform: {e}
{v:#}"
        );
    }
    v
}

#[tokio::test]
async fn error_bodies_conform() {
    let m = mock_with(
        "[accounts]
connect = true
",
    )
    .await;
    let bad_version = get_with(&m, "/v1/customers", &[("Stripe-Version", "2020-08-27")]).await;
    error_body(bad_version, 400).await;
    let unauthorized = reqwest::Client::new()
        .get(format!("{}/v1/customers", m.api_url()))
        .bearer_auth("sk_test_wrong")
        .send()
        .await
        .unwrap();
    error_body(unauthorized, 401).await;
    let forbidden = get_as(&m, "/v1/customers", "acct_nope").await;
    let v = error_body(forbidden, 403).await;
    assert_eq!(v["error"]["code"], "account_invalid");
    error_body(get(&m, "/v1/customers/cus_nope").await, 404).await;
    m.force_rate_limit(Duration::from_secs(30));
    error_body(get(&m, "/v1/customers").await, 429).await;

    for (errors, status) in [("\"500\"", 500), ("\"503\"", 503)] {
        let m = mock_with(&format!(
            "[api]
errors = {{ {errors} = 1.0 }}"
        ))
        .await;
        error_body(get(&m, "/v1/customers").await, status).await;
    }
}

fn bodies(rx: &Receiver) -> Vec<Value> {
    rx.requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn ids(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

/// Every account, event and delivery-target view the mock serves, and every webhook body it
/// delivers, conforms (`get` checks each 2xx body).
async fn check_connect_views(m: &MockStripe) {
    let v1: Value = get(m, "/v1/accounts?limit=100").await.json().await.unwrap();
    for id in ids(&v1) {
        get(m, &format!("/v1/accounts/{id}")).await;
    }
    let mut v2 = ids(&get(m, "/v2/core/accounts?limit=100")
        .await
        .json()
        .await
        .unwrap());
    let closed: Value = get(m, "/v2/core/accounts?limit=100&closed=true")
        .await
        .json()
        .await
        .unwrap();
    v2.extend(ids(&closed));
    for id in v2 {
        get(m, &format!("/v2/core/accounts/{id}")).await;
    }
    let events: Value = get(m, "/v1/events?limit=100").await.json().await.unwrap();
    for id in ids(&events) {
        get(m, &format!("/v1/events/{id}")).await;
    }
    let thin: Value = get(m, "/v2/core/events?limit=100")
        .await
        .json()
        .await
        .unwrap();
    for id in ids(&thin) {
        get(m, &format!("/v2/core/events/{id}")).await;
    }
    get(m, "/v1/webhook_endpoints").await;
    get(m, "/v2/core/event_destinations").await;
}

#[tokio::test]
async fn accounts_events_destinations_and_deliveries_conform() {
    let snap = Receiver::start(|_| (200, Duration::ZERO));
    let thin = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!(
        "seed = 7
[actions]
new_customer = 30
[churn.customer]
update = 30
         [churn.account]
update = 30
         [accounts]
connect = true
connected_accounts = {{ initial = 6 }}
         [[destinations]]
name = \"snap\"
url = \"{}\"
payload = \"snapshot\"
         enabled_events = [\"*\"]
connect = true
         [[destinations]]
name = \"thin\"
url = \"{}\"
payload = \"thin\"
         enabled_events = [\"*\"]
events_from = [\"@self\", \"@accounts\"]
",
        snap.url("/s"),
        thin.url("/t"),
    ))
    .await;
    let v1 = m.connect_account(AccountApi::V1).unwrap();
    let v2 = m.connect_account(AccountApi::V2).unwrap();
    m.step(Duration::from_secs(600)).await.unwrap();
    m.disconnect_account(&v1).unwrap();
    m.disconnect_account(&v2).unwrap();
    m.step(Duration::from_secs(600)).await.unwrap();
    m.quiesce().await.unwrap();
    check_connect_views(&m).await;

    let snap_bodies = bodies(&snap);
    let thin_bodies = bodies(&thin);
    for (format, bodies) in [("snapshot", &snap_bodies), ("thin", &thin_bodies)] {
        assert!(!bodies.is_empty(), "no {format} deliveries");
        for b in bodies {
            if let Err(e) = check_payload(b) {
                panic!(
                    "delivered {format} body does not conform: {e}
{b:#}"
                );
            }
        }
    }
    let types = |bodies: &[Value]| -> Vec<String> {
        bodies
            .iter()
            .map(|b| b["type"].as_str().unwrap().to_string())
            .collect()
    };
    let (snap_types, thin_types) = (types(&snap_bodies), types(&thin_bodies));
    for t in [
        "account.application.authorized",
        "account.application.deauthorized",
        "account.updated",
    ] {
        assert!(snap_types.iter().any(|x| x == t), "no {t} delivered");
    }
    for t in [
        "v2.core.account.created",
        "v2.core.account.closed",
        "v1.account.updated",
    ] {
        assert!(thin_types.iter().any(|x| x == t), "no {t} delivered");
    }
}
