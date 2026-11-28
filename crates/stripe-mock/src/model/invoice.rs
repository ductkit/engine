//! Invoices and invoice line items, built on the generated [`stripe::Invoice`] and
//! [`stripe::LineItem`].

use std::collections::BTreeMap;

use serde::Serialize;

use crate::model::amounts::{AppliedTax, InvoiceAmounts, LineAmounts};
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
    /// The `ii_…` billed: every line of a one-off invoice, and the line of a pending item
    /// that joined an invoice of either kind.
    pub invoice_item: Option<&'a str>,
    /// The line's share of the invoice's discount and its tax (ruling Q3).
    pub amounts: &'a LineAmounts,
    /// The `di_…` the invoice applies, if any.
    pub discount: Option<&'a str>,
    /// The tax rate the invoice applies, if any.
    pub tax: Option<&'a AppliedTax>,
}

type ProrationDetails = stripe::BillingBillResourceInvoicingLinesCommonProrationDetails;
type LineParent = stripe::BillingBillResourceInvoicingLinesParentsInvoiceLineItemParent;
type LineParentType = stripe::BillingBillResourceInvoicingLinesParentsInvoiceLineItemParentType;

fn no_proration() -> Option<Option<ProrationDetails>> {
    Some(Some(ProrationDetails {
        credited_items: Some(None),
    }))
}

/// The `pricing` of a line or invoice item billing `price`.
fn price_pricing(price: &stripe::Price) -> stripe::BillingBillResourceInvoicingPricingPricing {
    stripe::BillingBillResourceInvoicingPricingPricing {
        price_details: Some(
            stripe::BillingBillResourceInvoicingPricingPricingPriceDetails {
                price: stripe::Expandable::Id(price.id.clone()),
                product: price.product.id().to_string(),
            },
        ),
        type_: stripe::BillingBillResourceInvoicingPricingPricingType::PriceDetails,
        unit_amount_decimal: Some(price.unit_amount_decimal.clone().flatten()),
    }
}

type TaxEntry = stripe::BillingBillResourceInvoicingTaxesTax;

/// One exclusive tax entry of `rate` (ruling Q3).
fn tax_entry(rate: &AppliedTax, amount: i64, taxable: i64) -> TaxEntry {
    TaxEntry {
        amount,
        tax_behavior: stripe::BillingBillResourceInvoicingTaxesTaxTaxBehavior::Exclusive,
        tax_rate_details: Some(Some(
            stripe::BillingBillResourceInvoicingTaxesTaxRateDetails {
                tax_rate: stripe::Expandable::Id(rate.rate.id.clone()),
            },
        )),
        taxability_reason:
            stripe::BillingBillResourceInvoicingTaxesTaxTaxabilityReason::StandardRated,
        taxable_amount: Some(Some(taxable)),
        type_: stripe::BillingBillResourceInvoicingTaxesTaxType::TaxRateDetails,
    }
}

fn discount_amount(amount: i64, di: &str) -> stripe::DiscountsResourceDiscountAmount {
    stripe::DiscountsResourceDiscountAmount {
        amount,
        discount: stripe::Expandable::Id(di.to_string()),
    }
}

/// Sets a line's discount and tax fields from its computed amounts (ruling Q3). Its
/// `amount` stays before discount and tax, as in Stripe.
pub(crate) fn set_line_amounts(
    line: &mut stripe::LineItem,
    amounts: &LineAmounts,
    discount: Option<&str>,
    tax: Option<&AppliedTax>,
) {
    line.discount_amounts = Some(Some(
        discount
            .map(|di| vec![discount_amount(amounts.discount, di)])
            .unwrap_or_default(),
    ));
    line.discounts = discount
        .map(|di| vec![stripe::Expandable::Id(di.to_string())])
        .unwrap_or_default();
    line.taxes = Some(Some(
        tax.map(|t| vec![tax_entry(t, amounts.tax, amounts.taxable)])
            .unwrap_or_default(),
    ));
}

/// Sets an invoice's computed amounts (ruling Q3): `subtotal`, the discount and tax totals,
/// `total`, and `amount_due = amount_remaining = total`.
pub(crate) fn set_invoice_amounts(
    invoice: &mut stripe::Invoice,
    amounts: &InvoiceAmounts,
    discount: Option<&str>,
    tax: Option<&AppliedTax>,
) {
    invoice.subtotal = amounts.subtotal;
    invoice.subtotal_excluding_tax = Some(Some(amounts.subtotal));
    invoice.discounts = discount
        .map(|di| vec![stripe::Expandable::Id(di.to_string())])
        .unwrap_or_default();
    invoice.total_discount_amounts = Some(Some(
        discount
            .map(|di| vec![discount_amount(amounts.discount, di)])
            .unwrap_or_default(),
    ));
    invoice.total_excluding_tax = Some(Some(amounts.subtotal - amounts.discount));
    let taxable: i64 = amounts.lines.iter().map(|l| l.taxable).sum();
    invoice.total_taxes = Some(Some(
        tax.map(|t| vec![tax_entry(t, amounts.tax, taxable)])
            .unwrap_or_default(),
    ));
    invoice.total = amounts.total;
    invoice.amount_due = amounts.total;
    invoice.amount_remaining = amounts.total;
}

/// The line's amount: its price's unit amount times its quantity, before discount and tax.
pub(crate) fn line_amount(price: &stripe::Price, quantity: u64) -> i64 {
    price.unit_amount.flatten().unwrap_or(0) * quantity as i64
}

/// An invoice line item. Presence follows ruling R1: every field is present, `null` or empty
/// when unset.
pub(crate) fn new_line(l: &NewLine) -> stripe::LineItem {
    let mut line = line_before_amounts(l);
    set_line_amounts(&mut line, l.amounts, l.discount, l.tax);
    line
}

/// The line with no discount or tax, until [`set_line_amounts`] sets them.
fn line_before_amounts(l: &NewLine) -> stripe::LineItem {
    let amount = line_amount(l.price, l.spec.quantity);
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
                    subscription: Some(l.subscription.map(str::to_string)),
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
        pricing: Some(Some(price_pricing(l.price))),
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

/// Inputs for an invoice item.
pub(crate) struct NewInvoiceItem<'a> {
    pub id: &'a str,
    pub customer: &'a str,
    /// The invoice it is on; `None` while pending.
    pub invoice: Option<&'a str>,
    /// The price's state when billed.
    pub price: &'a stripe::Price,
    pub product_name: &'a str,
    pub quantity: u64,
    pub period: (i64, i64),
    pub date: i64,
}

/// An invoice item (phase 9 ruling Q4). Presence follows ruling R1: every field is present,
/// `null` or empty when unset, except `net_amount` (see below).
pub(crate) fn new_invoice_item(i: &NewInvoiceItem) -> stripe::Invoiceitem {
    let unit_amount = i.price.unit_amount.flatten().unwrap_or(0);
    stripe::Invoiceitem {
        amount: unit_amount * i.quantity as i64,
        currency: i.price.currency.clone(),
        customer: stripe::Expandable::Id(i.customer.to_string()),
        customer_account: Some(None),
        date: i.date,
        description: Some(Some(i.product_name.to_string())),
        discountable: true,
        discounts: Some(Some(Vec::new())),
        frozen_fields: Some(Vec::new()),
        id: i.id.to_string(),
        invoice: Some(i.invoice.map(|inv| stripe::Expandable::Id(inv.to_string()))),
        invoicing_rules: Some(Vec::new()),
        livemode: false,
        metadata: Some(Some(BTreeMap::new())),
        // Not nullable, and documented as `null` for `discountable: true` items, which every
        // generated item is: so absent.
        net_amount: None,
        object: stripe::InvoiceitemObject::Invoiceitem,
        // No subscription or proration generates an item: the `invoice_item` action and
        // one-off invoices do.
        parent: Some(None),
        period: stripe::InvoiceLineItemPeriod {
            end: i.period.1,
            start: i.period.0,
        },
        pricing: Some(Some(price_pricing(i.price))),
        proration: false,
        proration_details: Some(stripe::ProrationDetails {
            credited_items: Some(None),
            discount_amounts: Vec::new(),
        }),
        quantity: i.quantity as i64,
        quantity_decimal: i.quantity.to_string(),
        tax_rates: Some(Some(Vec::new())),
        test_clock: Some(None),
    }
}

pub(crate) fn deleted_invoice_item(id: &str) -> stripe::DeletedInvoiceitem {
    stripe::DeletedInvoiceitem {
        deleted: true,
        id: id.to_string(),
        object: stripe::DeletedInvoiceitemObject::Invoiceitem,
    }
}

/// The tombstone of a deleted line item. The pinned spec has no `deleted_line_item` schema
/// (line items have no retrieve endpoint, so Stripe never serves one); this is the
/// `{deleted, id, object}` shape every `deleted_*` schema has.
#[derive(Serialize)]
pub(crate) struct DeletedLineItem {
    deleted: bool,
    id: String,
    object: stripe::LineItemObject,
}

pub(crate) fn deleted_line(id: &str) -> DeletedLineItem {
    DeletedLineItem {
        deleted: true,
        id: id.to_string(),
        object: stripe::LineItemObject::LineItem,
    }
}

pub(crate) fn deleted_invoice(id: &str) -> stripe::DeletedInvoice {
    stripe::DeletedInvoice {
        deleted: true,
        id: id.to_string(),
        object: stripe::DeletedInvoiceObject::Invoice,
    }
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
    /// Computed from `lines`, `discount` and `tax` (ruling Q3).
    pub amounts: &'a InvoiceAmounts,
    /// The `di_…` the invoice applies, if any.
    pub discount: Option<&'a str>,
    /// The tax rate the invoice applies, if any.
    pub tax: Option<&'a AppliedTax>,
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
    let mut invoice = invoice_before_amounts(i);
    set_invoice_amounts(&mut invoice, i.amounts, i.discount, i.tax);
    invoice
}

/// The draft with every amount 0, until [`set_invoice_amounts`] sets them.
fn invoice_before_amounts(i: &NewInvoice) -> stripe::Invoice {
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
        amount_due: 0,
        amount_overpaid: 0,
        amount_paid: 0,
        amount_paid_off_stripe: 0,
        amount_remaining: 0,
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
        // Includable, so absent: `expand[]` builds it from the store.
        payments: None,
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
        subtotal: 0,
        subtotal_excluding_tax: Some(None),
        test_clock: Some(None),
        threshold_reason: None,
        total: 0,
        total_discount_amounts: Some(Some(Vec::new())),
        total_excluding_tax: Some(None),
        total_pretax_credit_amounts: Some(Some(Vec::new())),
        total_taxes: Some(Some(Vec::new())),
        webhooks_delivered_at: Some(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::amounts::{AppliedDiscount, LineAmount, compute};
    use crate::model::catalog::{PriceSpec, new_price};
    use crate::model::customer::{NewCustomer, new_customer};
    use crate::model::discounts::{TaxRateSpec, tax_bp, tax_rate};

    #[test]
    fn discounted_and_taxed_invoices_conform() {
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
        let rate = tax_rate(
            &TaxRateSpec {
                id: "txr_1".into(),
                display_name: "VAT",
                country: "DE",
                state: None,
                tax_type: stripe::TaxRateTaxType::Vat,
                level: stripe::TaxRateJurisdictionLevel::Country,
                percentage: 20.0,
            },
            1,
        );
        let tax = AppliedTax {
            bp: tax_bp(&rate),
            rate,
        };
        let discount = AppliedDiscount {
            id: "di_1".into(),
            percent_bp: Some(1000),
            amount_off: None,
        };
        let specs: Vec<LineSpec> = [1, 3]
            .into_iter()
            .map(|quantity| LineSpec {
                price: "price_1".into(),
                quantity,
                subscription_item: None,
                period: (1, 1),
            })
            .collect();
        let inputs: Vec<LineAmount> = specs
            .iter()
            .map(|s| LineAmount {
                amount: line_amount(&price, s.quantity),
            })
            .collect();
        let amounts = compute(&inputs, Some(&discount), Some(&tax));
        let ids = ["il_0", "il_1"];
        let lines: Vec<stripe::LineItem> = specs
            .iter()
            .zip(&amounts.lines)
            .zip(ids)
            .map(|((spec, la), id)| {
                new_line(&NewLine {
                    id,
                    invoice: "in_1",
                    spec,
                    price: &price,
                    product_name: "Widget",
                    subscription: None,
                    invoice_item: Some("ii_1"),
                    amounts: la,
                    discount: Some("di_1"),
                    tax: Some(&tax),
                })
            })
            .collect();
        let customer = new_customer("cus_1", 1, "ABC", &NewCustomer::default());
        let invoice = new_invoice(&NewInvoice {
            id: "in_1",
            customer: &customer,
            currency: "usd",
            lines: &lines,
            reason: BillingReason::Manual,
            subscription: None,
            period: (1, 1),
            created: 1,
            page_size: 10,
            amounts: &amounts,
            discount: Some("di_1"),
            tax: Some(&tax),
        });
        let v = serde_json::to_value(&invoice).unwrap();
        stripe::check_payload(&v).unwrap();
        // 2000 less a 200 discount, plus 20% of 1800.
        assert_eq!(
            (&v["subtotal"], &v["total_excluding_tax"], &v["total"]),
            (&2000.into(), &1800.into(), &2160.into())
        );
        assert_eq!(v["amount_due"], 2160);
        assert_eq!(v["discounts"], serde_json::json!(["di_1"]));
        assert_eq!(
            v["total_discount_amounts"],
            serde_json::json!([{"amount": 200, "discount": "di_1"}])
        );
        assert_eq!(
            v["total_taxes"],
            serde_json::json!([{
                "amount": 360,
                "tax_behavior": "exclusive",
                "tax_rate_details": {"tax_rate": "txr_1"},
                "taxability_reason": "standard_rated",
                "taxable_amount": 1800,
                "type": "tax_rate_details",
            }])
        );
        let first = &v["lines"]["data"][0];
        stripe::check_payload(first).unwrap();
        assert_eq!(first["amount"], 500);
        assert_eq!(
            first["discount_amounts"],
            serde_json::json!([{"amount": 50, "discount": "di_1"}])
        );
        assert_eq!(first["discounts"], serde_json::json!(["di_1"]));
        assert_eq!(first["taxes"][0]["amount"], 90);
        assert_eq!(first["taxes"][0]["taxable_amount"], 450);
    }

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
            amounts: &LineAmounts {
                discount: 0,
                tax: 0,
                taxable: 1000,
            },
            discount: None,
            tax: None,
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
            amounts: &compute(&[LineAmount { amount: 1000 }], None, None),
            discount: None,
            tax: None,
        });
        let v = serde_json::to_value(&invoice).unwrap();
        stripe::check_payload(&v).unwrap();
        assert_eq!(v["total"], 1000);
        assert_eq!(v["customer_email"], "a@example.com");
        // Includable: absent unless expanded.
        assert!(v.get("payments").is_none());
        assert!(v.get("confirmation_secret").is_none());
    }

    #[test]
    fn new_invoice_items_conform() {
        let price = new_price(
            &PriceSpec {
                id: "price_1".into(),
                currency: "eur",
                unit_amount: 500,
                interval: None,
                nickname: None,
            },
            "prod_1",
            1,
        );
        for invoice in [None, Some("in_1")] {
            let item = new_invoice_item(&NewInvoiceItem {
                id: "ii_1",
                customer: "cus_1",
                invoice,
                price: &price,
                product_name: "Widget",
                quantity: 3,
                period: (5, 5),
                date: 5,
            });
            let v = serde_json::to_value(&item).unwrap();
            stripe::check_payload(&v).unwrap();
            assert_eq!(
                (v["amount"].clone(), v["currency"].clone()),
                (1500.into(), "eur".into())
            );
            assert_eq!(
                v["invoice"],
                invoice.map_or(serde_json::Value::Null, Into::into)
            );
            assert!(v.get("net_amount").is_none());
        }
        assert_eq!(
            serde_json::to_value(deleted_invoice_item("ii_1")).unwrap(),
            serde_json::json!({"id": "ii_1", "object": "invoiceitem", "deleted": true})
        );
    }

    #[test]
    fn tombstones_have_the_deleted_shape() {
        assert_eq!(
            serde_json::to_value(deleted_line("il_1")).unwrap(),
            serde_json::json!({"id": "il_1", "object": "line_item", "deleted": true})
        );
        assert_eq!(
            serde_json::to_value(deleted_invoice("in_1")).unwrap(),
            serde_json::json!({"id": "in_1", "object": "invoice", "deleted": true})
        );
    }
}
