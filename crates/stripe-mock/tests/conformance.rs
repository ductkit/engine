use ductkit_stripe_mock::stripe::check_payload;
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
