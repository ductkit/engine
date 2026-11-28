//! Credit notes and their line items (phase 9 ruling Q8), built on the generated
//! [`stripe::CreditNote`] and [`stripe::CreditNoteLineItem`].

use std::collections::BTreeMap;

use crate::model::embedded::embedded_list;
use crate::stripe::{self, Expandable};

/// The reasons a note is drawn from: the whole pinned enum.
pub(crate) const REASONS: [stripe::CreditNoteReason; 4] = [
    stripe::CreditNoteReason::Duplicate,
    stripe::CreditNoteReason::Fraudulent,
    stripe::CreditNoteReason::OrderChange,
    stripe::CreditNoteReason::ProductUnsatisfactory,
];

pub(crate) fn lines_url(credit_note: &str) -> String {
    format!("/v1/credit_notes/{credit_note}/lines")
}

pub(crate) fn pdf_url(credit_note: &str) -> String {
    format!("https://pay.stripe.com/credit_notes/mock/{credit_note}/pdf")
}

/// A line item crediting `amount` of the invoice line `invoice_line` (described as that
/// line is). Presence follows ruling R1: every field is present. Ruling Q8: no tax
/// breakdown, and `quantity` and `unit_amount` are `null`.
pub(crate) fn line_item(
    id: &str,
    invoice_line: &str,
    description: Option<String>,
    amount: i64,
) -> stripe::CreditNoteLineItem {
    stripe::CreditNoteLineItem {
        amount,
        description: Some(description),
        discount_amount: 0,
        discount_amounts: Vec::new(),
        id: id.to_string(),
        invoice_line_item: Some(invoice_line.to_string()),
        livemode: false,
        metadata: Some(Some(BTreeMap::new())),
        object: stripe::CreditNoteLineItemObject::CreditNoteLineItem,
        pretax_credit_amounts: Vec::new(),
        quantity: Some(None),
        tax_rates: Vec::new(),
        taxes: Some(Some(Vec::new())),
        type_: stripe::CreditNoteLineItemType::InvoiceLineItem,
        unit_amount: Some(None),
        unit_amount_decimal: Some(None),
    }
}

/// Inputs for an issued credit note.
pub(crate) struct NewCreditNote<'a> {
    pub id: &'a str,
    pub number: &'a str,
    pub invoice: &'a str,
    pub customer: &'a str,
    pub currency: &'a str,
    pub amount: i64,
    pub type_: stripe::CreditNoteType,
    pub reason: stripe::CreditNoteReason,
    /// The refund of a post-payment note.
    pub refund: Option<&'a str>,
    pub lines: &'a [stripe::CreditNoteLineItem],
    pub page_size: usize,
    pub created: i64,
}

/// An `issued` note for `amount` (ruling Q8): `subtotal = total = amount`, no discount,
/// no tax, and `amount` in `pre_payment_amount` or `post_payment_amount` by its type.
/// Presence follows ruling R1: every field is present, `null` or empty when unset.
pub(crate) fn credit_note(n: &NewCreditNote) -> stripe::CreditNote {
    let pre = n.type_ == stripe::CreditNoteType::PrePayment;
    stripe::CreditNote {
        amount: n.amount,
        amount_shipping: 0,
        created: n.created,
        currency: n.currency.to_string(),
        customer: Expandable::Id(n.customer.to_string()),
        customer_account: Some(None),
        customer_balance_transaction: Some(None),
        discount_amount: 0,
        discount_amounts: Vec::new(),
        effective_at: Some(Some(n.created)),
        id: n.id.to_string(),
        invoice: Expandable::Id(n.invoice.to_string()),
        lines: embedded_list(n.lines, n.page_size, &lines_url(n.id)),
        livemode: false,
        memo: Some(None),
        metadata: Some(Some(BTreeMap::new())),
        number: n.number.to_string(),
        object: stripe::CreditNoteObject::CreditNote,
        out_of_band_amount: Some(None),
        pdf: pdf_url(n.id),
        post_payment_amount: if pre { 0 } else { n.amount },
        pre_payment_amount: if pre { n.amount } else { 0 },
        pretax_credit_amounts: Vec::new(),
        reason: Some(Some(n.reason)),
        refunds: n
            .refund
            .map(|re| stripe::CreditNoteRefund {
                amount_refunded: n.amount,
                payment_record_refund: Some(None),
                refund: Expandable::Id(re.to_string()),
                type_: Some(Some(stripe::CreditNoteRefundType::Refund)),
            })
            .into_iter()
            .collect(),
        shipping_cost: Some(None),
        status: stripe::CreditNoteStatus::Issued,
        subtotal: n.amount,
        subtotal_excluding_tax: Some(Some(n.amount)),
        total: n.amount,
        total_excluding_tax: Some(Some(n.amount)),
        total_taxes: Some(Some(Vec::new())),
        type_: n.type_,
        voided_at: Some(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn conforms<T: serde::Serialize>(obj: &T) -> Value {
        let v = serde_json::to_value(obj).unwrap();
        stripe::check_payload(&v).unwrap_or_else(|e| panic!("{e}\n{v:#}"));
        v
    }

    #[test]
    fn notes_and_lines_conform() {
        let lines = vec![
            line_item("cnli_1", "il_1", Some("Gold".into()), 60),
            line_item("cnli_2", "il_2", None, 40),
        ];
        for l in &lines {
            conforms(l);
        }
        for (type_, refund) in [
            (stripe::CreditNoteType::PrePayment, None),
            (stripe::CreditNoteType::PostPayment, Some("re_1")),
        ] {
            let v = conforms(&credit_note(&NewCreditNote {
                id: "cn_1",
                number: "ABC-0001-CN-01",
                invoice: "in_1",
                customer: "cus_1",
                currency: "usd",
                amount: 100,
                type_,
                reason: stripe::CreditNoteReason::Duplicate,
                refund,
                lines: &lines,
                page_size: 1,
                created: 1_700_000_000,
            }));
            assert_eq!(v["lines"]["has_more"], true);
            assert_eq!(v["lines"]["url"], "/v1/credit_notes/cn_1/lines");
            let pre = refund.is_none();
            assert_eq!(v["pre_payment_amount"], if pre { 100 } else { 0 });
            assert_eq!(v["post_payment_amount"], if pre { 0 } else { 100 });
            assert_eq!(v["refunds"].as_array().unwrap().len(), usize::from(!pre));
        }
    }
}
