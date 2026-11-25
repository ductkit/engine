//! Stripe object types and their JSON models.

use serde::{Deserialize, Serialize};

pub mod customer;

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
}

impl ObjectType {
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
