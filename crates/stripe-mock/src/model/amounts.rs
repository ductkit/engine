//! Invoice amounts (phase 9 ruling Q3): one pure function does every invoice's arithmetic,
//! in integers.

use crate::stripe;

/// One invoice line's amount, before discount and tax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineAmount {
    pub amount: i64,
}

/// The discount an invoice applies: exactly one of `percent_bp` and `amount_off` is set.
/// An `amount_off` is already known to be in the invoice's currency (see
/// [`crate::generate`]'s `applied_discount`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppliedDiscount {
    /// The `di_…` applied.
    pub id: String,
    /// `percent_off` in basis points (whole percents × 100, so exact).
    pub percent_bp: Option<i64>,
    /// Minor units of the invoice's currency.
    pub amount_off: Option<i64>,
}

/// The exclusive tax rate an invoice applies to every line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppliedTax {
    /// The rate's state when it was attached (ruling Q5: embedded copies are snapshots).
    pub rate: stripe::TaxRate,
    /// The rate's percentage in basis points (`model::discounts::tax_bp`).
    pub bp: i64,
}

/// One line's share of the discount and its tax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineAmounts {
    /// The line's share of the invoice discount.
    pub discount: i64,
    /// The tax on `taxable`.
    pub tax: i64,
    /// The line's amount minus its discount share.
    pub taxable: i64,
}

/// Every computed amount of an invoice. `subtotal − discount + tax = total`, and the
/// lines' discounts and taxes add up to `discount` and `tax`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InvoiceAmounts {
    /// One entry per input line, in order.
    pub lines: Vec<LineAmounts>,
    /// The sum of the line amounts.
    pub subtotal: i64,
    /// The discount total `D`.
    pub discount: i64,
    /// The tax total.
    pub tax: i64,
    /// `subtotal − discount + tax`.
    pub total: i64,
}

/// `(value × bp + 5_000) / 10_000`: basis points of a non-negative value, rounded half up.
fn apply_bp(value: i64, bp: i64) -> i64 {
    ((value as i128 * bp as i128 + 5_000) / 10_000) as i64
}

/// The Q3 sharing rule: `total` spread over the entries with a weight above 0, in order.
/// Each gets `total × weight / sum` rounded down, and the last such entry gets the
/// remainder; entries with a weight of 0 or less get 0. With no weight above 0, every
/// share is 0.
pub(crate) fn share(total: i64, weights: &[i64]) -> Vec<i64> {
    let sum: i128 = weights.iter().filter(|w| **w > 0).map(|w| *w as i128).sum();
    let mut out = vec![0; weights.len()];
    let Some(last) = weights.iter().rposition(|w| *w > 0) else {
        return out;
    };
    let mut given = 0;
    for (i, w) in weights.iter().enumerate() {
        if *w <= 0 {
            continue;
        }
        let s = if i == last {
            total - given
        } else {
            (total as i128 * *w as i128 / sum) as i64
        };
        out[i] = s;
        given += s;
    }
    out
}

/// The amounts of an invoice with `lines`, `discount` and `tax` (ruling Q3):
/// - `D` is `min(subtotal, amount_off)`, or `(subtotal × percent_bp + 5_000) / 10_000`;
/// - `D` is shared over the lines by [`share`];
/// - each line's tax is `(taxable × bp + 5_000) / 10_000`, where `taxable` is its amount
///   minus its share;
/// - `total = subtotal − D + tax`.
///
/// With neither a discount nor a tax, every share is 0 and `total = subtotal`.
pub(crate) fn compute(
    lines: &[LineAmount],
    discount: Option<&AppliedDiscount>,
    tax: Option<&AppliedTax>,
) -> InvoiceAmounts {
    let amounts: Vec<i64> = lines.iter().map(|l| l.amount).collect();
    let subtotal: i64 = amounts.iter().sum();
    let d = match discount {
        None => 0,
        Some(AppliedDiscount {
            amount_off: Some(off),
            ..
        }) => subtotal.min(*off).max(0),
        Some(AppliedDiscount {
            percent_bp: Some(bp),
            ..
        }) => apply_bp(subtotal, *bp).min(subtotal),
        Some(_) => 0,
    };
    let shares = share(d, &amounts);
    let lines: Vec<LineAmounts> = amounts
        .iter()
        .zip(shares)
        .map(|(amount, discount)| {
            let taxable = amount - discount;
            let tax = tax.map_or(0, |t| apply_bp(taxable, t.bp));
            LineAmounts {
                discount,
                tax,
                taxable,
            }
        })
        .collect();
    let tax_total: i64 = lines.iter().map(|l| l.tax).sum();
    InvoiceAmounts {
        lines,
        subtotal,
        discount: d,
        tax: tax_total,
        total: subtotal - d + tax_total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::discounts::{TaxRateSpec, tax_rate};

    fn lines(amounts: &[i64]) -> Vec<LineAmount> {
        amounts
            .iter()
            .map(|&amount| LineAmount { amount })
            .collect()
    }

    fn percent(p: i64) -> AppliedDiscount {
        AppliedDiscount {
            id: "di_1".into(),
            percent_bp: Some(p * 100),
            amount_off: None,
        }
    }

    fn tax(percentage: f64) -> AppliedTax {
        let rate = tax_rate(
            &TaxRateSpec {
                id: "txr_1".into(),
                display_name: "Sales Tax",
                country: "US",
                state: Some("CA"),
                tax_type: stripe::TaxRateTaxType::SalesTax,
                level: stripe::TaxRateJurisdictionLevel::State,
                percentage,
            },
            1,
        );
        let bp = crate::model::discounts::tax_bp(&rate);
        AppliedTax { rate, bp }
    }

    #[test]
    fn no_discount_no_tax_is_unchanged() {
        let a = compute(&lines(&[500, 1000]), None, None);
        assert_eq!((a.subtotal, a.discount, a.tax, a.total), (1500, 0, 0, 1500));
        assert_eq!(
            a.lines,
            vec![
                LineAmounts {
                    discount: 0,
                    tax: 0,
                    taxable: 500
                },
                LineAmounts {
                    discount: 0,
                    tax: 0,
                    taxable: 1000
                },
            ]
        );
    }

    #[test]
    fn percent_rounds_half_up_and_shares_to_the_last() {
        let a = compute(&lines(&[333, 333, 334]), Some(&percent(15)), None);
        // D = (1000 × 1500 + 5000) / 10000 = 150.
        assert_eq!(a.discount, 150);
        let shares: Vec<i64> = a.lines.iter().map(|l| l.discount).collect();
        assert_eq!(shares, [49, 49, 52]);
        assert_eq!(a.total, 850);
        // Half up: 10 × 25% = 2.5 → 3.
        assert_eq!(compute(&lines(&[10]), Some(&percent(25)), None).discount, 3);
    }

    #[test]
    fn amount_off_caps_at_subtotal() {
        let off = AppliedDiscount {
            id: "di_1".into(),
            percent_bp: None,
            amount_off: Some(2000),
        };
        let a = compute(&lines(&[500, 1000]), Some(&off), None);
        assert_eq!((a.discount, a.total), (1500, 0));
        let shares: Vec<i64> = a.lines.iter().map(|l| l.discount).collect();
        assert_eq!(shares, [500, 1000]);
    }

    #[test]
    fn tax_on_the_discounted_amount() {
        let a = compute(&lines(&[1000]), Some(&percent(10)), Some(&tax(7.25)));
        assert_eq!(a.lines[0].taxable, 900);
        // (900 × 725 + 5000) / 10000 = 65.75 → 65.
        assert_eq!((a.lines[0].tax, a.tax), (65, 65));
        assert_eq!(a.total, 965);
        // 8.875% uses 888 bp.
        let b = compute(&lines(&[1000]), None, Some(&tax(8.875)));
        assert_eq!(b.tax, 89);
    }

    #[test]
    fn zero_lines_get_no_share() {
        let a = compute(&lines(&[0, 1000]), Some(&percent(50)), None);
        let shares: Vec<i64> = a.lines.iter().map(|l| l.discount).collect();
        assert_eq!(shares, [0, 500]);
        assert_eq!(share(7, &[0, 0]), [0, 0]);
        assert_eq!(share(10, &[1, 0, 2, 0]), [3, 0, 7, 0]);
    }
}
