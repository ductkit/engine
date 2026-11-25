//! Invoices and invoice line items, built on the generated [`stripe::Invoice`] and
//! [`stripe::LineItem`].

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::model::embedded::embedded_list;
use crate::model::subscription::self_issuer;
use crate::stripe;

/// Why an invoice exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BillingReason {
    SubscriptionCreate,
    SubscriptionCycle,
    Manual,
}

impl BillingReason {
    pub(crate) fn to_stripe(self) -> stripe::InvoiceBillingReason {
        match self {
            BillingReason::SubscriptionCreate => stripe::InvoiceBillingReason::SubscriptionCreate,
            BillingReason::SubscriptionCycle => stripe::InvoiceBillingReason::SubscriptionCycle,
            BillingReason::Manual => stripe::InvoiceBillingReason::Manual,
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

/// Inputs for a line.
pub(crate) struct NewLine<'a> {
    pub id: &'a str,
    pub invoice: &'a str,
    pub spec: &'a LineSpec,
    /// The price's state when billed.
    pub price: &'a stripe::Price,
    pub product_name: &'a str,
    pub subscription: Option<&'a str>,
    /// `ii_…` for one-off lines.
    pub invoice_item: Option<&'a str>,
}

type ProrationDetails = stripe::BillingBillResourceInvoicingLinesCommonProrationDetails;
type LineParent = stripe::BillingBillResourceInvoicingLinesParentsInvoiceLineItemParent;
type LineParentType = stripe::BillingBillResourceInvoicingLinesParentsInvoiceLineItemParentType;

fn no_proration() -> Option<Option<ProrationDetails>> {
    Some(Some(ProrationDetails {
        credited_items: Some(None),
    }))
}

/// An invoice line item. Presence follows ruling R1: every field is present, `null` or empty
/// when unset.
pub(crate) fn new_line(l: &NewLine) -> stripe::LineItem {
    let unit_amount = l.price.unit_amount.flatten().unwrap_or(0);
    let amount = unit_amount * l.spec.quantity as i64;
    let parent = match (l.subscription, &l.spec.subscription_item) {
        (Some(sub), Some(si)) => LineParent {
            invoice_item_details: Some(None),
            subscription_item_details: Some(Some(
                stripe::BillingBillResourceInvoicingLinesParentsInvoiceLineItemSubscriptionItemParent {
                    invoice_item: Some(None),
                    proration: false,
                    proration_details: no_proration(),
                    subscription: Some(Some(sub.to_string())),
                    subscription_item: si.clone(),
                },
            )),
            type_: LineParentType::SubscriptionItemDetails,
        },
        _ => LineParent {
            invoice_item_details: Some(Some(
                stripe::BillingBillResourceInvoicingLinesParentsInvoiceLineItemInvoiceItemParent {
                    invoice_item: l
                        .invoice_item
                        .expect("one-off lines have an invoice item")
                        .to_string(),
                    proration: false,
                    proration_details: no_proration(),
                    subscription: Some(None),
                },
            )),
            subscription_item_details: Some(None),
            type_: LineParentType::InvoiceItemDetails,
        },
    };
    stripe::LineItem {
        amount,
        currency: l.price.currency.clone(),
        description: Some(Some(format!("{} × {}", l.spec.quantity, l.product_name))),
        discount_amounts: Some(Some(Vec::new())),
        discountable: true,
        discounts: Vec::new(),
        id: l.id.to_string(),
        invoice: Some(Some(l.invoice.to_string())),
        livemode: false,
        metadata: BTreeMap::new(),
        object: stripe::LineItemObject::LineItem,
        parent: Some(Some(parent)),
        period: stripe::InvoiceLineItemPeriod {
            end: l.spec.period.1,
            start: l.spec.period.0,
        },
        pretax_credit_amounts: Some(Some(Vec::new())),
        pricing: Some(Some(stripe::BillingBillResourceInvoicingPricingPricing {
            price_details: Some(
                stripe::BillingBillResourceInvoicingPricingPricingPriceDetails {
                    price: stripe::Expandable::Id(l.price.id.clone()),
                    product: l.price.product.id().to_string(),
                },
            ),
            type_: stripe::BillingBillResourceInvoicingPricingPricingType::PriceDetails,
            unit_amount_decimal: Some(l.price.unit_amount_decimal.clone().flatten()),
        })),
        quantity: Some(Some(l.spec.quantity as i64)),
        quantity_decimal: Some(Some(l.spec.quantity.to_string())),
        subscription: Some(
            l.subscription
                .map(|s| stripe::Expandable::Id(s.to_string())),
        ),
        subtotal: amount,
        taxes: Some(Some(Vec::new())),
    }
}

/// The tombstone of a deleted line item. The pinned spec has no `deleted_line_item` schema
/// (line items have no retrieve endpoint, so Stripe never serves one); this is the
/// `{deleted, id, object}` shape every `deleted_*` schema has.
#[derive(Serialize)]
struct DeletedLineItem<'a> {
    deleted: bool,
    id: &'a str,
    object: stripe::LineItemObject,
}

pub(crate) fn deleted_line(id: &str) -> Value {
    serde_json::to_value(DeletedLineItem {
        deleted: true,
        id,
        object: stripe::LineItemObject::LineItem,
    })
    .expect("a deleted line item serializes")
}

pub(crate) fn deleted_invoice(id: &str) -> Value {
    serde_json::to_value(stripe::DeletedInvoice {
        deleted: true,
        id: id.to_string(),
        object: stripe::DeletedInvoiceObject::Invoice,
    })
    .expect("a deleted invoice serializes")
}

/// Inputs for a draft invoice.
pub(crate) struct NewInvoice<'a> {
    pub id: &'a str,
    pub customer: &'a stripe::Customer,
    pub currency: &'a str,
    pub lines: &'a [stripe::LineItem],
    pub reason: BillingReason,
    pub subscription: Option<&'a str>,
    pub period: (i64, i64),
    pub created: i64,
    pub page_size: usize,
}

pub(crate) fn lines_url(invoice: &str) -> String {
    format!("/v1/invoices/{invoice}/lines")
}

pub(crate) fn payments_url(invoice: &str) -> String {
    format!("/v1/invoice_payments?invoice={invoice}")
}

/// A draft invoice. Presence follows ruling R1: every field is present, `null` or empty when
/// unset, except the expandable-only `confirmation_secret` and the optional non-nullable
/// `status_details` and `threshold_reason`. `payments` is an empty list until the invoice
/// has payments.
pub(crate) fn new_invoice(i: &NewInvoice) -> stripe::Invoice {
    let total: i64 = i.lines.iter().map(|l| l.amount).sum();
    let parent =
        i.subscription.map(
            |sub| {
                stripe::BillingBillResourceInvoicingParentsInvoiceParent {
            quote_details: Some(None),
            subscription_details: Some(Some(
                stripe::BillingBillResourceInvoicingParentsInvoiceSubscriptionParent {
                    metadata: Some(Some(BTreeMap::new())),
                    subscription: stripe::Expandable::Id(sub.to_string()),
                    subscription_proration_date: None,
                },
            )),
            type_:
                stripe::BillingBillResourceInvoicingParentsInvoiceParentType::SubscriptionDetails,
        }
            },
        );
    let c = i.customer;
    let tax_exempt = c.tax_exempt.flatten().map(|t| match t {
        stripe::CustomerTaxExempt::Exempt => stripe::InvoiceCustomerTaxExempt::Exempt,
        stripe::CustomerTaxExempt::None => stripe::InvoiceCustomerTaxExempt::None,
        stripe::CustomerTaxExempt::Reverse => stripe::InvoiceCustomerTaxExempt::Reverse,
    });
    stripe::Invoice {
        account_country: Some(Some("US".to_string())),
        account_name: Some(Some("ductkit mock".to_string())),
        account_tax_ids: Some(None),
        amount_due: total,
        amount_overpaid: 0,
        amount_paid: 0,
        amount_paid_off_stripe: 0,
        amount_remaining: total,
        amount_shipping: 0,
        application: Some(None),
        attempt_count: 0,
        attempted: false,
        auto_advance: true,
        automatic_tax: stripe::AutomaticTax {
            disabled_reason: Some(None),
            enabled: false,
            liability: Some(None),
            provider: Some(None),
            status: Some(None),
        },
        automatically_finalizes_at: Some(None),
        billing_reason: Some(Some(i.reason.to_stripe())),
        collection_method: stripe::InvoiceCollectionMethod::ChargeAutomatically,
        confirmation_secret: None,
        created: i.created,
        currency: i.currency.to_string(),
        custom_fields: Some(None),
        customer: stripe::Expandable::Id(c.id.clone()),
        customer_account: Some(None),
        customer_address: Some(c.address.clone().flatten()),
        customer_email: Some(c.email.clone().flatten()),
        customer_name: Some(c.name.clone().flatten()),
        customer_phone: Some(c.phone.clone().flatten()),
        customer_shipping: Some(c.shipping.clone().flatten()),
        customer_tax_exempt: Some(tax_exempt),
        customer_tax_ids: Some(Some(Vec::new())),
        default_payment_method: Some(None),
        default_source: Some(None),
        default_tax_rates: Vec::new(),
        description: Some(None),
        discounts: Vec::new(),
        due_date: Some(None),
        effective_at: Some(None),
        ending_balance: Some(None),
        footer: Some(None),
        from_invoice: Some(None),
        hosted_invoice_url: Some(None),
        id: i.id.to_string(),
        invoice_pdf: Some(None),
        issuer: self_issuer(),
        last_finalization_error: Some(None),
        latest_revision: Some(None),
        lines: embedded_list(i.lines, i.page_size, &lines_url(i.id)),
        livemode: false,
        metadata: Some(Some(BTreeMap::new())),
        next_payment_attempt: Some(None),
        number: Some(None),
        object: stripe::InvoiceObject::Invoice,
        on_behalf_of: Some(None),
        parent: Some(parent),
        payment_settings: stripe::InvoicesPaymentSettings {
            default_mandate: Some(None),
            payment_method_options: Some(None),
            payment_method_types: Some(None),
        },
        payments: Some(embedded_list::<stripe::InvoicePayment>(
            &[],
            i.page_size,
            &payments_url(i.id),
        )),
        period_end: i.period.1,
        period_start: i.period.0,
        post_payment_credit_notes_amount: 0,
        pre_payment_credit_notes_amount: 0,
        receipt_number: Some(None),
        rendering: Some(None),
        shipping_cost: Some(None),
        shipping_details: Some(None),
        starting_balance: 0,
        statement_descriptor: Some(None),
        status: Some(Some(stripe::InvoiceStatus::Draft)),
        status_details: None,
        status_transitions: stripe::InvoicesResourceStatusTransitions {
            finalized_at: Some(None),
            marked_uncollectible_at: Some(None),
            paid_at: Some(None),
            voided_at: Some(None),
        },
        subtotal: total,
        subtotal_excluding_tax: Some(Some(total)),
        test_clock: Some(None),
        threshold_reason: None,
        total,
        total_discount_amounts: Some(Some(Vec::new())),
        total_excluding_tax: Some(Some(total)),
        total_pretax_credit_amounts: Some(Some(Vec::new())),
        total_taxes: Some(Some(Vec::new())),
        webhooks_delivered_at: Some(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::catalog::{PriceSpec, new_price};
    use crate::model::customer::{NewCustomer, new_customer};

    #[test]
    fn new_lines_and_invoices_conform() {
        let price = new_price(
            &PriceSpec {
                id: "price_1".into(),
                currency: "usd",
                unit_amount: 500,
                interval: None,
                nickname: None,
            },
            "prod_1",
            1,
        );
        let spec = LineSpec {
            price: "price_1".into(),
            quantity: 2,
            subscription_item: None,
            period: (1, 1),
        };
        let line = new_line(&NewLine {
            id: "il_1",
            invoice: "in_1",
            spec: &spec,
            price: &price,
            product_name: "Widget",
            subscription: None,
            invoice_item: Some("ii_1"),
        });
        assert_eq!((line.amount, line.subtotal), (1000, 1000));
        let customer = new_customer(
            "cus_1",
            1,
            "ABC",
            &NewCustomer {
                email: Some("a@example.com".into()),
                name: None,
                description: None,
                metadata: BTreeMap::new(),
            },
        );
        let invoice = new_invoice(&NewInvoice {
            id: "in_1",
            customer: &customer,
            currency: "usd",
            lines: std::slice::from_ref(&line),
            reason: BillingReason::Manual,
            subscription: None,
            period: (1, 1),
            created: 1,
            page_size: 10,
        });
        let v = serde_json::to_value(&invoice).unwrap();
        stripe::check_payload(&v).unwrap();
        assert_eq!(v["total"], 1000);
        assert_eq!(v["customer_email"], "a@example.com");
        assert_eq!(v["payments"]["data"], serde_json::json!([]));
        assert_eq!(v["payments"]["url"], "/v1/invoice_payments?invoice=in_1");
        assert!(v.get("confirmation_secret").is_none());
    }

    #[test]
    fn tombstones_have_the_deleted_shape() {
        assert_eq!(
            deleted_line("il_1"),
            serde_json::json!({"id": "il_1", "object": "line_item", "deleted": true})
        );
        assert_eq!(
            deleted_invoice("in_1"),
            serde_json::json!({"id": "in_1", "object": "invoice", "deleted": true})
        );
    }
}
