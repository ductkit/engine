//! Phase 9 billing extras: the API plumbing of the new types (ruling Q10).

mod common;

use common::{get, mock_with};
use ductkit_stripe_mock::MockStripe;
use serde_json::{Value, json};

async fn json_at(m: &MockStripe, path: &str) -> (u16, Value) {
    let r = get(m, path).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

const LISTS: [&str; 6] = [
    "/v1/coupons",
    "/v1/promotion_codes",
    "/v1/invoiceitems",
    "/v1/credit_notes",
    "/v1/tax_rates",
    "/v1/subscription_schedules",
];

#[tokio::test]
async fn new_lists_are_empty_and_well_formed() {
    let m = mock_with("").await;
    for path in LISTS {
        let (s, v) = json_at(&m, path).await;
        assert_eq!(s, 200, "{path}: {v}");
        assert_eq!(
            v,
            json!({"object": "list", "data": [], "has_more": false, "url": path}),
            "{path}"
        );
    }
    // Every Q10 filter is accepted.
    for path in [
        "/v1/promotion_codes?active=true&code=ABC&coupon=X&customer=cus_x",
        "/v1/invoiceitems?customer=cus_x&invoice=in_x&pending=true",
        "/v1/credit_notes?customer=cus_x&invoice=in_x",
        "/v1/tax_rates?active=false&inclusive=false",
        "/v1/subscription_schedules?customer=cus_x",
    ] {
        let (s, v) = json_at(&m, path).await;
        assert_eq!(s, 200, "{path}: {v}");
        assert_eq!(v["data"], json!([]), "{path}");
    }
    for path in [
        "/v1/invoiceitems?pending=maybe",
        "/v1/tax_rates?inclusive=1",
    ] {
        let (s, v) = json_at(&m, path).await;
        assert_eq!(s, 400, "{path}: {v}");
        assert_eq!(v["error"]["code"], "parameter_invalid_boolean", "{path}");
    }
    // A filter the type lacks is unknown.
    let (s, v) = json_at(&m, "/v1/coupons?customer=cus_x").await;
    assert_eq!((s, &v["error"]["code"]), (400, &json!("parameter_unknown")));
    let (s, v) = json_at(&m, "/v1/credit_notes/cn_x/lines").await;
    assert_eq!(s, 404, "{v}");
    assert_eq!(v["error"]["code"], "resource_missing");
    assert_eq!(v["error"]["message"], "No such credit_note: 'cn_x'");
}

#[tokio::test]
async fn unknown_ids_are_404() {
    let m = mock_with("").await;
    for (path, object, id) in [
        ("/v1/coupons/ABCDEFGH", "coupon", "ABCDEFGH"),
        ("/v1/promotion_codes/promo_x", "promotion_code", "promo_x"),
        ("/v1/invoiceitems/ii_x", "invoiceitem", "ii_x"),
        ("/v1/credit_notes/cn_x", "credit_note", "cn_x"),
        ("/v1/tax_rates/txr_x", "tax_rate", "txr_x"),
        (
            "/v1/subscription_schedules/sub_sched_x",
            "subscription_schedule",
            "sub_sched_x",
        ),
    ] {
        let (s, v) = json_at(&m, path).await;
        assert_eq!(s, 404, "{path}: {v}");
        assert_eq!(v["error"]["code"], "resource_missing", "{path}");
        assert_eq!(
            v["error"]["message"],
            format!("No such {object}: '{id}'"),
            "{path}"
        );
    }
}

#[tokio::test]
async fn new_expand_paths_validate() {
    let m = mock_with("").await;
    for (list, paths) in [
        (
            "/v1/customers",
            &["discount.promotion_code", "discount.source.coupon"][..],
        ),
        ("/v1/subscriptions", &["schedule", "discounts"][..]),
        ("/v1/invoices", &["discounts"][..]),
        ("/v1/promotion_codes", &["customer", "promotion.coupon"][..]),
        (
            "/v1/invoiceitems",
            &["customer", "invoice", "discounts"][..],
        ),
        ("/v1/credit_notes", &["customer", "invoice"][..]),
        (
            "/v1/subscription_schedules",
            &["customer", "subscription"][..],
        ),
    ] {
        for p in paths {
            let path = format!("{list}?expand[]=data.{p}");
            let (s, v) = json_at(&m, &path).await;
            assert_eq!(s, 200, "{path}: {v}");
        }
    }
    for path in [
        "/v1/promotion_codes?expand[]=data.source",
        "/v1/coupons?expand[]=data.customer",
        "/v1/tax_rates?expand[]=data.customer",
    ] {
        let (s, v) = json_at(&m, path).await;
        assert_eq!(s, 400, "{path}: {v}");
        assert_eq!(v["error"]["param"], "expand", "{path}");
    }
}
