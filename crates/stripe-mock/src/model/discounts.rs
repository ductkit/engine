//! Coupons, promotion codes and tax rates (phase 9 rulings Q3 and Q6), built on the
//! generated [`stripe::Coupon`], [`stripe::PromotionCode`] and [`stripe::TaxRate`].

use std::collections::BTreeMap;

use crate::model::catalog::random_currency;
use crate::rng::SimRng;
use crate::stripe::{self, Expandable};

/// Tax percentages a rate is drawn from (ruling Q3).
const TAX_PERCENTAGES: [f64; 5] = [5.0, 7.25, 8.875, 10.0, 20.0];

/// Content for a new coupon, drawn when its action starts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CouponSpec {
    pub id: String,
    pub name: String,
    /// Whole percent, 5..=50. Exactly one of this and `amount_off` is set.
    pub percent_off: Option<i64>,
    /// Minor units and currency.
    pub amount_off: Option<(i64, &'static str)>,
    pub duration: stripe::CouponDuration,
    /// Set for `repeating` coupons, 1..=6.
    pub duration_in_months: Option<i64>,
    pub max_redemptions: Option<i64>,
}

/// Draws a coupon (ruling Q6): 60% `percent_off`, else `amount_off`; `once` 40%,
/// `repeating` 30%, `forever` 30%; a redemption cap with probability 0.2.
pub(crate) fn random_coupon(rng: &mut SimRng) -> CouponSpec {
    let id = rng.code(8);
    let (percent_off, amount_off, name) = if rng.below(10) < 6 {
        let p = 5 * (1 + rng.below(10) as i64);
        (Some(p), None, format!("{p}% off"))
    } else {
        let amount = 100 * (1 + rng.below(20) as i64);
        let currency = random_currency(rng);
        let name = format!("{amount} {} off", currency.to_uppercase());
        (None, Some((amount, currency)), name)
    };
    let (duration, duration_in_months) = match rng.below(10) {
        0..=3 => (stripe::CouponDuration::Once, None),
        4..=6 => (
            stripe::CouponDuration::Repeating,
            Some(1 + rng.below(6) as i64),
        ),
        _ => (stripe::CouponDuration::Forever, None),
    };
    let max_redemptions = rng.chance(0.2).then(|| 1 + rng.below(100) as i64);
    CouponSpec {
        id,
        name,
        percent_off,
        amount_off,
        duration,
        duration_in_months,
        max_redemptions,
    }
}

/// A new valid coupon. Presence follows ruling R1: every field is present, `null` when
/// unset, except the includable-only `applies_to` and `currency_options`.
pub(crate) fn coupon(c: &CouponSpec, created: i64) -> stripe::Coupon {
    stripe::Coupon {
        amount_off: Some(c.amount_off.map(|(a, _)| a)),
        // Includable-only in Stripe: absent unless requested.
        applies_to: None,
        created,
        currency: Some(c.amount_off.map(|(_, cur)| cur.to_string())),
        // Includable-only in Stripe: absent unless requested.
        currency_options: None,
        duration: c.duration,
        duration_in_months: Some(c.duration_in_months),
        id: c.id.clone(),
        livemode: false,
        max_redemptions: Some(c.max_redemptions),
        metadata: Some(Some(BTreeMap::new())),
        name: Some(Some(c.name.clone())),
        object: stripe::CouponObject::Coupon,
        percent_off: Some(c.percent_off.map(|p| p as f64)),
        // Always `null`: the generator never sets an expiry (ruling Q6).
        redeem_by: Some(None),
        times_redeemed: 0,
        valid: true,
    }
}

/// What Stripe returns for a deleted coupon.
pub(crate) fn deleted_coupon(id: &str) -> stripe::DeletedCoupon {
    stripe::DeletedCoupon {
        deleted: true,
        id: id.to_string(),
        object: stripe::DeletedCouponObject::Coupon,
    }
}

/// A new active promotion code for `coupon`, with no customer, expiry or redemption cap.
pub(crate) fn promotion_code(
    id: &str,
    code: &str,
    coupon: &str,
    created: i64,
) -> stripe::PromotionCode {
    stripe::PromotionCode {
        active: true,
        code: code.to_string(),
        created,
        customer: Some(None),
        customer_account: Some(None),
        expires_at: Some(None),
        id: id.to_string(),
        livemode: false,
        max_redemptions: Some(None),
        metadata: Some(Some(BTreeMap::new())),
        object: stripe::PromotionCodeObject::PromotionCode,
        promotion: stripe::PromotionCodesResourcePromotion {
            coupon: Some(Some(Expandable::Id(coupon.to_string()))),
            type_: stripe::PromotionCodesResourcePromotionType::Coupon,
        },
        restrictions: stripe::PromotionCodesResourceRestrictions {
            // Includable-only in Stripe: absent unless requested.
            currency_options: None,
            first_time_transaction: false,
            minimum_amount: Some(None),
            minimum_amount_currency: Some(None),
        },
        times_redeemed: 0,
    }
}

/// Content for a new tax rate.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaxRateSpec {
    pub id: String,
    pub display_name: &'static str,
    pub country: &'static str,
    pub state: Option<&'static str>,
    pub tax_type: stripe::TaxRateTaxType,
    pub level: stripe::TaxRateJurisdictionLevel,
    pub percentage: f64,
}

/// Draws a tax rate (ruling Q6): one of three kinds, uniformly, and a percentage from
/// the Q3 list.
pub(crate) fn random_tax_rate(rng: &mut SimRng, id: String) -> TaxRateSpec {
    let (display_name, country, state, tax_type, level) = match rng.below(3) {
        0 => (
            "Sales Tax",
            "US",
            Some("CA"),
            stripe::TaxRateTaxType::SalesTax,
            stripe::TaxRateJurisdictionLevel::State,
        ),
        1 => (
            "VAT",
            "DE",
            None,
            stripe::TaxRateTaxType::Vat,
            stripe::TaxRateJurisdictionLevel::Country,
        ),
        _ => (
            "GST",
            "AU",
            None,
            stripe::TaxRateTaxType::Gst,
            stripe::TaxRateJurisdictionLevel::Country,
        ),
    };
    let percentage = *rng.choose(&TAX_PERCENTAGES);
    TaxRateSpec {
        id,
        display_name,
        country,
        state,
        tax_type,
        level,
        percentage,
    }
}

/// A new active, exclusive percentage tax rate.
pub(crate) fn tax_rate(t: &TaxRateSpec, created: i64) -> stripe::TaxRate {
    stripe::TaxRate {
        active: true,
        country: Some(Some(t.country.to_string())),
        created,
        description: Some(None),
        display_name: t.display_name.to_string(),
        effective_percentage: Some(Some(t.percentage)),
        flat_amount: Some(None),
        id: t.id.clone(),
        inclusive: false,
        jurisdiction: Some(Some(match t.state {
            Some(s) => format!("{}-{s}", t.country),
            None => t.country.to_string(),
        })),
        jurisdiction_level: Some(Some(t.level)),
        livemode: false,
        metadata: Some(Some(BTreeMap::new())),
        object: stripe::TaxRateObject::TaxRate,
        percentage: t.percentage,
        rate_type: Some(Some(stripe::TaxRateRateType::Percentage)),
        state: Some(t.state.map(str::to_string)),
        tax_type: Some(Some(t.tax_type)),
    }
}

/// Inputs for a discount (phase 9 ruling Q2).
pub(crate) struct NewDiscount<'a> {
    pub id: &'a str,
    pub coupon: &'a str,
    /// The promotion code redeemed, if any.
    pub promotion_code: Option<&'a str>,
    /// Always set: a subscription's discount names its customer too.
    pub customer: &'a str,
    /// Set when the subscription is the parent.
    pub subscription: Option<&'a str>,
    pub start: i64,
    /// A `repeating` coupon's end; `None` for `once` and `forever`.
    pub end: Option<i64>,
}

/// A discount. Presence follows ruling R1: every field is present, `null` when unset. A
/// discount always names its customer, so its thin events have a parent to point at.
pub(crate) fn discount(d: &NewDiscount) -> stripe::Discount {
    stripe::Discount {
        checkout_session: Some(None),
        customer: Some(Some(Expandable::Id(d.customer.to_string()))),
        customer_account: Some(None),
        end: Some(d.end),
        id: d.id.to_string(),
        invoice: Some(None),
        invoice_item: Some(None),
        object: stripe::DiscountObject::Discount,
        promotion_code: Some(d.promotion_code.map(|p| Expandable::Id(p.to_string()))),
        source: stripe::DiscountSource {
            coupon: Some(Some(Expandable::Id(d.coupon.to_string()))),
            type_: stripe::DiscountSourceType::Coupon,
        },
        start: d.start,
        subscription: Some(d.subscription.map(str::to_string)),
        subscription_item: Some(None),
    }
}

/// What a removed discount becomes (ruling Q2): the full discount plus `deleted: true`,
/// less `end`, which the pinned `deleted_discount` schema does not have.
pub(crate) fn deleted_discount(d: &stripe::Discount) -> stripe::DeletedDiscount {
    stripe::DeletedDiscount {
        checkout_session: d.checkout_session.clone(),
        customer: d
            .customer
            .clone()
            .map(|c| c.map(|c| Expandable::Id(c.id().to_string()))),
        customer_account: d.customer_account.clone(),
        deleted: true,
        id: d.id.clone(),
        invoice: d.invoice.clone(),
        invoice_item: d.invoice_item.clone(),
        object: stripe::DeletedDiscountObject::Discount,
        promotion_code: d.promotion_code.clone(),
        source: d.source.clone(),
        start: d.start,
        subscription: d.subscription.clone(),
        subscription_item: d.subscription_item.clone(),
    }
}

/// The rate's percentage in basis points (ruling Q3): `8.875` is `888`.
pub(crate) fn tax_bp(rate: &stripe::TaxRate) -> i64 {
    (rate.percentage * 100.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::RngStreams;
    use serde_json::to_value;

    fn rng() -> SimRng {
        RngStreams::new(7).stream("test.discounts")
    }

    #[test]
    fn drawn_coupons_conform() {
        let mut rng = rng();
        let mut seen = (false, false, false);
        for _ in 0..200 {
            let spec = random_coupon(&mut rng);
            assert_eq!(spec.id.len(), 8);
            assert_ne!(spec.percent_off.is_some(), spec.amount_off.is_some());
            let c = coupon(&spec, 1_700_000_000);
            crate::stripe::check_payload(&to_value(&c).unwrap()).unwrap();
            assert_eq!(
                c.duration_in_months.flatten().is_some(),
                c.duration == stripe::CouponDuration::Repeating
            );
            seen.0 |= spec.percent_off.is_some();
            seen.1 |= spec.amount_off.is_some();
            seen.2 |= spec.max_redemptions.is_some();
        }
        assert!(seen.0 && seen.1 && seen.2);
    }

    #[test]
    fn coupon_tombstone_conforms() {
        let v = to_value(deleted_coupon("AbCd1234")).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"id": "AbCd1234", "object": "coupon", "deleted": true})
        );
        crate::stripe::check_payload(&v).unwrap();
    }

    #[test]
    fn promotion_code_conforms() {
        let p = promotion_code("promo_1", "SAVE2026", "AbCd1234", 1_700_000_000);
        let v = to_value(&p).unwrap();
        crate::stripe::check_payload(&v).unwrap();
        assert_eq!(v["promotion"]["coupon"], "AbCd1234");
        assert_eq!(v["customer"], serde_json::Value::Null);
        assert_eq!(v["restrictions"]["first_time_transaction"], false);
    }

    #[test]
    fn discounts_and_their_tombstones_conform() {
        for (sub, promo, end) in [
            (None, None, None),
            (Some("sub_1"), Some("promo_1"), Some(1_702_592_000)),
        ] {
            let d = discount(&NewDiscount {
                id: "di_1",
                coupon: "AbCd1234",
                promotion_code: promo,
                customer: "cus_1",
                subscription: sub,
                start: 1_700_000_000,
                end,
            });
            let v = to_value(&d).unwrap();
            crate::stripe::check_payload(&v).unwrap();
            assert_eq!(v["customer"], "cus_1");
            assert_eq!(v["subscription"], serde_json::json!(sub));
            assert_eq!(v["promotion_code"], serde_json::json!(promo));
            assert_eq!(v["source"]["coupon"], "AbCd1234");
            assert_eq!(v["end"], serde_json::json!(end));
            let t = to_value(deleted_discount(&d)).unwrap();
            crate::stripe::check_payload(&t).unwrap();
            // The pinned `deleted_discount` has no `end`; everything else is the discount's.
            let mut expected = v.clone();
            expected["deleted"] = true.into();
            expected.as_object_mut().unwrap().remove("end");
            assert_eq!(t, expected);
        }
    }

    #[test]
    fn tax_rates_conform_and_have_basis_points() {
        let mut rng = rng();
        let mut bps = std::collections::BTreeSet::new();
        for i in 0..200 {
            let spec = random_tax_rate(&mut rng, format!("txr_{i}"));
            let t = tax_rate(&spec, 1_700_000_000);
            crate::stripe::check_payload(&to_value(&t).unwrap()).unwrap();
            assert!(!t.inclusive);
            bps.insert(tax_bp(&t));
        }
        assert_eq!(
            bps.into_iter().collect::<Vec<_>>(),
            [500, 725, 888, 1000, 2000]
        );
    }
}
