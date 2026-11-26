//! Stripe object types and their JSON models.

use serde::{Deserialize, Serialize};

pub mod account;
pub mod catalog;
pub mod customer;
pub mod embedded;
pub mod invoice;
pub mod payments;
pub mod subscription;

/// A simulated Stripe object type.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    Customer,
    Product,
    Price,
    Subscription,
    SubscriptionItem,
    Invoice,
    InvoiceLineItem,
    /// A connected account. Lives in the platform scope.
    Account,
    PaymentIntent,
    Charge,
    Refund,
    Dispute,
    BalanceTransaction,
    PaymentMethod,
    SetupIntent,
    CheckoutSession,
    InvoicePayment,
}

impl ObjectType {
    /// Every type, in declaration order.
    pub const ALL: [ObjectType; 17] = [
        ObjectType::Customer,
        ObjectType::Product,
        ObjectType::Price,
        ObjectType::Subscription,
        ObjectType::SubscriptionItem,
        ObjectType::Invoice,
        ObjectType::InvoiceLineItem,
        ObjectType::Account,
        ObjectType::PaymentIntent,
        ObjectType::Charge,
        ObjectType::Refund,
        ObjectType::Dispute,
        ObjectType::BalanceTransaction,
        ObjectType::PaymentMethod,
        ObjectType::SetupIntent,
        ObjectType::CheckoutSession,
        ObjectType::InvoicePayment,
    ];

    /// The Stripe `object` name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ObjectType::Customer => "customer",
            ObjectType::Product => "product",
            ObjectType::Price => "price",
            ObjectType::Subscription => "subscription",
            ObjectType::SubscriptionItem => "subscription_item",
            ObjectType::Invoice => "invoice",
            ObjectType::InvoiceLineItem => "line_item",
            ObjectType::Account => "account",
            ObjectType::PaymentIntent => "payment_intent",
            ObjectType::Charge => "charge",
            ObjectType::Refund => "refund",
            ObjectType::Dispute => "dispute",
            ObjectType::BalanceTransaction => "balance_transaction",
            ObjectType::PaymentMethod => "payment_method",
            ObjectType::SetupIntent => "setup_intent",
            ObjectType::CheckoutSession => "checkout.session",
            ObjectType::InvoicePayment => "invoice_payment",
        }
    }

    /// The name this type serializes to (`StateUpdate.object_type`, state dump
    /// keys). It differs from [`as_str`](Self::as_str) for line items.
    pub fn serde_name(&self) -> String {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(s)) => s,
            _ => self.as_str().to_string(),
        }
    }

    /// The ID prefix, without the underscore.
    pub fn id_prefix(&self) -> &'static str {
        match self {
            ObjectType::Customer => "cus",
            ObjectType::Product => "prod",
            ObjectType::Price => "price",
            ObjectType::Subscription => "sub",
            ObjectType::SubscriptionItem => "si",
            ObjectType::Invoice => "in",
            ObjectType::InvoiceLineItem => "il",
            ObjectType::Account => "acct",
            ObjectType::PaymentIntent => "pi",
            ObjectType::Charge => "ch",
            ObjectType::Refund => "re",
            ObjectType::Dispute => "du",
            ObjectType::BalanceTransaction => "txn",
            ObjectType::PaymentMethod => "pm",
            ObjectType::SetupIntent => "seti",
            // Includes the mode: real session IDs read `cs_test_...`.
            ObjectType::CheckoutSession => "cs_test",
            ObjectType::InvoicePayment => "inpay",
        }
    }

    /// The retrieve path of an object, e.g. `/v1/customers/cus_1`; `None` for
    /// line items, which have no retrieve endpoint.
    pub fn api_path(&self, id: &str) -> Option<String> {
        let collection = match self {
            ObjectType::Customer => "customers",
            ObjectType::Product => "products",
            ObjectType::Price => "prices",
            ObjectType::Subscription => "subscriptions",
            ObjectType::SubscriptionItem => "subscription_items",
            ObjectType::Invoice => "invoices",
            ObjectType::Account => "accounts",
            ObjectType::PaymentIntent => "payment_intents",
            ObjectType::Charge => "charges",
            ObjectType::Refund => "refunds",
            ObjectType::Dispute => "disputes",
            ObjectType::BalanceTransaction => "balance_transactions",
            ObjectType::PaymentMethod => "payment_methods",
            ObjectType::SetupIntent => "setup_intents",
            ObjectType::CheckoutSession => "checkout/sessions",
            ObjectType::InvoicePayment => "invoice_payments",
            ObjectType::InvoiceLineItem => return None,
        };
        Some(format!("/v1/{collection}/{id}"))
    }
}

impl std::fmt::Display for ObjectType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
