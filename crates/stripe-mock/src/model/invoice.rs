//! Invoice and invoice line item JSON at the pinned (post-"basil") version.
//! Hand-written until phase 7 validates it against OpenAPI.

use serde_json::{Value, json};

use crate::model::embedded::embedded_list;

/// Why an invoice exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BillingReason {
    #[allow(dead_code)] // TEMP: used from Task 5
    SubscriptionCreate,
    #[allow(dead_code)] // TEMP: used from Task 6
    SubscriptionCycle,
    Manual,
}

impl BillingReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BillingReason::SubscriptionCreate => "subscription_create",
            BillingReason::SubscriptionCycle => "subscription_cycle",
            BillingReason::Manual => "manual",
        }
    }
}

/// One line to bill.
#[derive(Debug, Clone)]
pub(crate) struct LineSpec {
    pub price: String,
    pub quantity: u64,
    /// The subscription item billed, for subscription invoices.
    pub subscription_item: Option<String>,
    pub period: (i64, i64),
}

/// Inputs for a line's JSON.
pub(crate) struct NewLine<'a> {
    pub id: &'a str,
    pub invoice: &'a str,
    pub spec: &'a LineSpec,
    /// The price's state when billed.
    pub price: &'a Value,
    pub product_name: &'a str,
    pub subscription: Option<&'a str>,
    /// `ii_…` for one-off lines.
    pub invoice_item: Option<&'a str>,
}

pub(crate) fn line_json(l: &NewLine) -> Value {
    let unit_amount = l.price["unit_amount"].as_i64().unwrap_or(0);
    let amount = unit_amount * l.spec.quantity as i64;
    let parent = match (l.subscription, &l.spec.subscription_item) {
        (Some(sub), Some(si)) => json!({
            "type": "subscription_item_details",
            "subscription_item_details": {
                "invoice_item": null,
                "proration": false,
                "proration_details": {"credited_items": null},
                "subscription": sub,
                "subscription_item": si
            },
            "invoice_item_details": null
        }),
        _ => json!({
            "type": "invoice_item_details",
            "invoice_item_details": {
                "invoice_item": l.invoice_item,
                "proration": false,
                "proration_details": {"credited_items": null},
                "subscription": null
            },
            "subscription_item_details": null
        }),
    };
    json!({
        "id": l.id,
        "object": "line_item",
        "amount": amount,
        "currency": l.price["currency"],
        "description": format!("{} × {}", l.spec.quantity, l.product_name),
        "discountable": true,
        "discounts": [],
        "invoice": l.invoice,
        "livemode": false,
        "metadata": {},
        "parent": parent,
        "period": {"start": l.spec.period.0, "end": l.spec.period.1},
        "pricing": {
            "type": "price_details",
            "price_details": {"price": l.price["id"], "product": l.price["product"]},
            "unit_amount_decimal": l.price["unit_amount_decimal"]
        },
        "quantity": l.spec.quantity
    })
}

/// Inputs for a draft invoice's JSON.
pub(crate) struct NewInvoice<'a> {
    pub id: &'a str,
    pub customer: &'a Value,
    pub currency: &'a Value,
    pub lines: &'a [Value],
    pub reason: BillingReason,
    pub subscription: Option<&'a str>,
    pub period: (i64, i64),
    pub created: i64,
    pub page_size: usize,
}

pub(crate) fn invoice_json(i: &NewInvoice) -> Value {
    let total: i64 = i.lines.iter().filter_map(|l| l["amount"].as_i64()).sum();
    let parent = i.subscription.map(|sub| {
        json!({
            "type": "subscription_details",
            "subscription_details": {"metadata": {}, "subscription": sub},
            "quote_details": null
        })
    });
    json!({
        "id": i.id,
        "object": "invoice",
        "account_country": "US",
        "account_name": "ductkit mock",
        "amount_due": total,
        "amount_paid": 0,
        "amount_remaining": total,
        "attempt_count": 0,
        "attempted": false,
        "auto_advance": true,
        "billing_reason": i.reason.as_str(),
        "collection_method": "charge_automatically",
        "created": i.created,
        "currency": i.currency,
        "customer": i.customer["id"],
        "customer_email": i.customer["email"],
        "customer_name": i.customer["name"],
        "default_payment_method": null,
        "description": null,
        "discounts": [],
        "due_date": null,
        "effective_at": null,
        "hosted_invoice_url": null,
        "invoice_pdf": null,
        "lines": embedded_list(i.lines, i.page_size, &format!("/v1/invoices/{}/lines", i.id)),
        "livemode": false,
        "metadata": {},
        "next_payment_attempt": null,
        "number": null,
        "parent": parent,
        "period_end": i.period.1,
        "period_start": i.period.0,
        "status": "draft",
        "status_transitions": {
            "finalized_at": null,
            "marked_uncollectible_at": null,
            "paid_at": null,
            "voided_at": null
        },
        "subtotal": total,
        "total": total
    })
}
