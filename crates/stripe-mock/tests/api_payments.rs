mod common;

use common::{get, mock_with};
use ductkit_stripe_mock::{NewCustomer, ObjectType};
use serde_json::Value;

async fn json(m: &ductkit_stripe_mock::MockStripe, path: &str) -> (u16, Value) {
    let r = get(m, path).await;
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

/// Every new top-level list path, with the query that makes it servable.
const LISTS: [(&str, &str); 9] = [
    ("/v1/payment_intents", ""),
    ("/v1/charges", ""),
    ("/v1/refunds", ""),
    ("/v1/disputes", ""),
    ("/v1/balance_transactions", ""),
    ("/v1/payment_methods", "?customer=cus_none"),
    ("/v1/setup_intents", ""),
    ("/v1/checkout/sessions", ""),
    ("/v1/invoice_payments", ""),
];

#[tokio::test]
async fn new_lists_are_empty_and_well_formed() {
    let m = mock_with("").await;
    for (path, query) in LISTS {
        let (s, v) = json(&m, &format!("{path}{query}")).await;
        assert_eq!(s, 200, "{path}: {v}");
        assert_eq!(v["object"], "list", "{path}");
        assert_eq!(v["data"].as_array().unwrap().len(), 0, "{path}");
        assert_eq!(v["has_more"], false, "{path}");
        assert_eq!(v["url"], path, "{path}");
    }
}

#[tokio::test]
async fn customer_payment_methods_sub_list() {
    let m = mock_with("").await;
    let id = m.create_customer(NewCustomer::default()).unwrap();
    let path = format!("/v1/customers/{id}/payment_methods");
    let (s, v) = json(&m, &path).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["object"], "list");
    assert_eq!(v["url"], path);
    assert_eq!(v["data"].as_array().unwrap().len(), 0);
    let (s, v) = json(&m, "/v1/customers/cus_nope/payment_methods").await;
    assert_eq!(s, 404, "{v}");
    assert_eq!(v["error"]["code"], "resource_missing");
}

#[tokio::test]
async fn filters_are_accepted() {
    let m = mock_with("").await;
    for q in [
        "/v1/payment_intents?customer=cus_1",
        "/v1/charges?customer=cus_1&payment_intent=pi_1",
        "/v1/refunds?charge=ch_1&payment_intent=pi_1",
        "/v1/disputes?charge=ch_1&payment_intent=pi_1",
        "/v1/balance_transactions?type=charge&source=ch_1&payout=po_1",
        "/v1/payment_methods?customer=cus_1&type=card",
        "/v1/setup_intents?customer=cus_1&payment_method=pm_1",
        "/v1/checkout/sessions?customer=cus_1&payment_intent=pi_1&subscription=sub_1&status=open",
        "/v1/invoice_payments?invoice=in_1&payment=pi_1",
    ] {
        let (s, v) = json(&m, q).await;
        assert_eq!(s, 200, "{q}: {v}");
    }
}

#[tokio::test]
async fn payment_methods_list_requires_customer() {
    let m = mock_with("").await;
    let (s, v) = json(&m, "/v1/payment_methods").await;
    assert_eq!(s, 400);
    assert_eq!(v["error"]["code"], "parameter_missing");
    assert_eq!(v["error"]["param"], "customer");
}

#[tokio::test]
async fn unknown_payment_ids_404() {
    let m = mock_with("").await;
    for (path, _) in LISTS {
        let (s, v) = json(&m, &format!("{path}/x_nope")).await;
        assert_eq!(s, 404, "{path}: {v}");
        assert_eq!(v["error"]["code"], "resource_missing", "{path}");
    }
    let (s, v) = json(&m, "/v1/charges/ch_nope").await;
    assert_eq!(s, 404);
    assert_eq!(v["error"]["code"], "resource_missing");
    assert_eq!(v["error"]["message"], "No such charge: 'ch_nope'");
}

#[tokio::test]
async fn new_endpoints_are_read_only() {
    let m = mock_with("").await;
    let mut paths: Vec<String> = LISTS.iter().map(|(p, _)| p.to_string()).collect();
    paths.extend(LISTS.iter().map(|(p, _)| format!("{p}/x_1")));
    paths.push("/v1/charges/ch_1".to_string());
    paths.push("/v1/customers/cus_1/payment_methods".to_string());
    for path in paths {
        let r = reqwest::Client::new()
            .post(format!("{}{path}", m.api_url()))
            .bearer_auth("sk_test_mock")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "{path}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["type"], "invalid_request_error", "{path}");
    }
}

#[test]
fn new_types_have_the_documented_names() {
    let expected = [
        (
            ObjectType::PaymentIntent,
            "payment_intent",
            "payment_intent",
            "pi",
        ),
        (ObjectType::Charge, "charge", "charge", "ch"),
        (ObjectType::Refund, "refund", "refund", "re"),
        (ObjectType::Dispute, "dispute", "dispute", "du"),
        (
            ObjectType::BalanceTransaction,
            "balance_transaction",
            "balance_transaction",
            "txn",
        ),
        (
            ObjectType::PaymentMethod,
            "payment_method",
            "payment_method",
            "pm",
        ),
        (
            ObjectType::SetupIntent,
            "setup_intent",
            "setup_intent",
            "seti",
        ),
        (
            ObjectType::CheckoutSession,
            "checkout.session",
            "checkout_session",
            "cs_test",
        ),
        (
            ObjectType::InvoicePayment,
            "invoice_payment",
            "invoice_payment",
            "inpay",
        ),
        (ObjectType::Transfer, "transfer", "transfer", "tr"),
        (
            ObjectType::TransferReversal,
            "transfer_reversal",
            "transfer_reversal",
            "trr",
        ),
        (
            ObjectType::ApplicationFee,
            "application_fee",
            "application_fee",
            "fee",
        ),
        (ObjectType::FeeRefund, "fee_refund", "fee_refund", "fr"),
        (ObjectType::Payout, "payout", "payout", "po"),
        (
            ObjectType::BankAccount,
            "bank_account",
            "bank_account",
            "ba",
        ),
        (ObjectType::Card, "card", "card", "card"),
        (ObjectType::Person, "person", "person", "person"),
        // Phase 9 ruling Q1.
        (ObjectType::Coupon, "coupon", "coupon", ""),
        (
            ObjectType::PromotionCode,
            "promotion_code",
            "promotion_code",
            "promo",
        ),
        (ObjectType::Discount, "discount", "discount", "di"),
        (ObjectType::InvoiceItem, "invoiceitem", "invoice_item", "ii"),
        (ObjectType::CreditNote, "credit_note", "credit_note", "cn"),
        (
            ObjectType::CreditNoteLineItem,
            "credit_note_line_item",
            "credit_note_line_item",
            "cnli",
        ),
        (ObjectType::TaxRate, "tax_rate", "tax_rate", "txr"),
        (
            ObjectType::SubscriptionSchedule,
            "subscription_schedule",
            "subscription_schedule",
            "sub_sched",
        ),
    ];
    assert_eq!(ObjectType::ALL.len(), 33);
    // The table lists the types from `PaymentIntent` on, in declaration order.
    assert_eq!(
        expected.iter().map(|e| e.0).collect::<Vec<_>>(),
        ObjectType::ALL[ObjectType::ALL.len() - expected.len()..]
    );
    for (t, name, serde_name, prefix) in expected {
        assert_eq!(t.as_str(), name);
        assert_eq!(t.serde_name(), serde_name);
        assert_eq!(t.id_prefix(), prefix);
    }
    assert_eq!(
        ObjectType::CheckoutSession.api_path("cs_test_1").unwrap(),
        "/v1/checkout/sessions/cs_test_1"
    );
    assert_eq!(
        ObjectType::Coupon.api_path("ABCDEFGH").unwrap(),
        "/v1/coupons/ABCDEFGH"
    );
    assert_eq!(
        ObjectType::InvoiceItem.api_path("ii_1").unwrap(),
        "/v1/invoiceitems/ii_1"
    );
    assert_eq!(ObjectType::Discount.api_path("di_1"), None);
    assert_eq!(ObjectType::CreditNoteLineItem.api_path("cnli_1"), None);
}
