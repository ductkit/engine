//! Stripe object types and their JSON models.

use serde::{Deserialize, Serialize};

pub mod account;
pub mod amounts;
pub mod catalog;
pub mod checkout;
pub mod connect;
pub mod credit_note;
pub mod customer;
pub mod discounts;
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
    /// A platform-to-account transfer.
    Transfer,
    /// A reversal of a transfer. Child of a transfer.
    TransferReversal,
    ApplicationFee,
    /// A refund of an application fee. Child of an application fee.
    FeeRefund,
    Payout,
    /// An external account that is a bank account. Child of an account.
    BankAccount,
    /// An external account that is a card. Child of an account.
    Card,
    /// A person of a connected account. Child of an account.
    Person,
    /// A coupon. Its ID has no prefix (phase 9 ruling Q1).
    Coupon,
    PromotionCode,
    /// A discount. Embedded in its customer or subscription; no endpoint of its own.
    Discount,
    /// An invoice item. Serializes as `invoice_item`; its Stripe `object` is `invoiceitem`.
    InvoiceItem,
    CreditNote,
    /// A line of a credit note. Served by `/v1/credit_notes/{id}/lines` only.
    CreditNoteLineItem,
    TaxRate,
    SubscriptionSchedule,
}

impl ObjectType {
    /// Every type, in declaration order.
    pub const ALL: [ObjectType; 33] = [
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
        ObjectType::Transfer,
        ObjectType::TransferReversal,
        ObjectType::ApplicationFee,
        ObjectType::FeeRefund,
        ObjectType::Payout,
        ObjectType::BankAccount,
        ObjectType::Card,
        ObjectType::Person,
        ObjectType::Coupon,
        ObjectType::PromotionCode,
        ObjectType::Discount,
        ObjectType::InvoiceItem,
        ObjectType::CreditNote,
        ObjectType::CreditNoteLineItem,
        ObjectType::TaxRate,
        ObjectType::SubscriptionSchedule,
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
            ObjectType::Transfer => "transfer",
            ObjectType::TransferReversal => "transfer_reversal",
            ObjectType::ApplicationFee => "application_fee",
            ObjectType::FeeRefund => "fee_refund",
            ObjectType::Payout => "payout",
            ObjectType::BankAccount => "bank_account",
            ObjectType::Card => "card",
            ObjectType::Person => "person",
            ObjectType::Coupon => "coupon",
            ObjectType::PromotionCode => "promotion_code",
            ObjectType::Discount => "discount",
            ObjectType::InvoiceItem => "invoiceitem",
            ObjectType::CreditNote => "credit_note",
            ObjectType::CreditNoteLineItem => "credit_note_line_item",
            ObjectType::TaxRate => "tax_rate",
            ObjectType::SubscriptionSchedule => "subscription_schedule",
        }
    }

    /// The name this type serializes to (`StateUpdate.object_type`, state dump
    /// keys). It differs from [`as_str`](Self::as_str) for invoice line items and invoice
    /// items.
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
            ObjectType::Transfer => "tr",
            ObjectType::TransferReversal => "trr",
            ObjectType::ApplicationFee => "fee",
            ObjectType::FeeRefund => "fr",
            ObjectType::Payout => "po",
            ObjectType::BankAccount => "ba",
            ObjectType::Card => "card",
            ObjectType::Person => "person",
            // Coupon IDs are 8 alphanumerics with no prefix (phase 9 ruling Q1).
            ObjectType::Coupon => "",
            ObjectType::PromotionCode => "promo",
            ObjectType::Discount => "di",
            ObjectType::InvoiceItem => "ii",
            ObjectType::CreditNote => "cn",
            ObjectType::CreditNoteLineItem => "cnli",
            ObjectType::TaxRate => "txr",
            ObjectType::SubscriptionSchedule => "sub_sched",
        }
    }

    /// The retrieve path of an object, e.g. `/v1/customers/cus_1`; `None` for
    /// line items, discounts and credit note lines, which have no retrieve endpoint, and for
    /// children, which nest under their parent (see [`sub_path`](Self::sub_path)).
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
            ObjectType::Transfer => "transfers",
            ObjectType::ApplicationFee => "application_fees",
            ObjectType::Payout => "payouts",
            ObjectType::Coupon => "coupons",
            ObjectType::PromotionCode => "promotion_codes",
            ObjectType::InvoiceItem => "invoiceitems",
            ObjectType::CreditNote => "credit_notes",
            ObjectType::TaxRate => "tax_rates",
            ObjectType::SubscriptionSchedule => "subscription_schedules",
            ObjectType::InvoiceLineItem
            | ObjectType::TransferReversal
            | ObjectType::FeeRefund
            | ObjectType::BankAccount
            | ObjectType::Card
            | ObjectType::Person
            | ObjectType::Discount
            | ObjectType::CreditNoteLineItem => return None,
        };
        Some(format!("/v1/{collection}/{id}"))
    }

    /// The retrieve path of a child object under its `parent`, e.g.
    /// `/v1/transfers/tr_1/reversals/trr_1`; `None` for types that are not such children.
    pub fn sub_path(&self, parent: &str, id: &str) -> Option<String> {
        match self {
            ObjectType::TransferReversal => Some(format!("/v1/transfers/{parent}/reversals/{id}")),
            ObjectType::FeeRefund => Some(format!("/v1/application_fees/{parent}/refunds/{id}")),
            ObjectType::BankAccount | ObjectType::Card => {
                Some(format!("/v1/accounts/{parent}/external_accounts/{id}"))
            }
            ObjectType::Person => Some(format!("/v1/accounts/{parent}/persons/{id}")),
            _ => None,
        }
    }
}

impl std::fmt::Display for ObjectType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
