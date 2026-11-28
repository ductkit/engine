//! OpenAPI-to-Rust generator for the generated Stripe types (design section 6.2).
//!
//! It lives only in the test tree and is compiled through `tests/openapi_codegen.rs`.

use std::collections::BTreeMap;

use serde_json::Value;

pub mod emit;
pub mod ir;
pub mod names;

/// Generates every file of `src/stripe/generated`, keyed by path relative to it.
pub fn generate(spec: &Value) -> Result<BTreeMap<String, String>, String> {
    let ir = ir::build_ir(spec, ROOTS)?;
    emit::emit(&ir, ROOTS)
}

/// The root schemas to generate. The generator emits each root plus the closure over
/// non-expandable `$ref`s.
pub const ROOTS: &[&str] = &[
    "customer",
    "deleted_customer",
    "product",
    "deleted_product",
    "price",
    "deleted_price",
    "subscription",
    "subscription_item",
    "deleted_subscription_item",
    "invoice",
    "deleted_invoice",
    "line_item",
    "account",
    "deleted_account",
    "application",
    "v2.core.account",
    "event",
    "v2.core.event",
    "webhook_endpoint",
    "v2.core.event_destination",
    "error",
    "v2.error",
    "payment_intent",
    "charge",
    "refund",
    "dispute",
    "balance_transaction",
    "payment_method",
    "setup_intent",
    "checkout.session",
    "invoice_payment",
    "transfer",
    "transfer_reversal",
    "application_fee",
    "fee_refund",
    "payout",
    "bank_account",
    "deleted_bank_account",
    "card",
    "deleted_card",
    "person",
    "deleted_person",
    "coupon",
    "deleted_coupon",
    "promotion_code",
    "discount",
    "deleted_discount",
    "invoiceitem",
    "deleted_invoiceitem",
    "credit_note",
    "credit_note_line_item",
    "tax_rate",
    "subscription_schedule",
];
