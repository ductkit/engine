//! Subscriptions and subscription items, built on the generated [`stripe::Subscription`] and
//! [`stripe::SubscriptionItem`]. Billing periods live on the items.

use std::collections::BTreeMap;

use chrono::{DateTime, Months};

use crate::model::embedded::embedded_list;
use crate::stripe;

/// `start` plus one billing interval (`"month"` or `"year"`) in calendar months,
/// clamped to the end of a shorter month.
pub(crate) fn period_end(start: i64, interval: &str) -> i64 {
    add_intervals(start, interval, 1)
}

/// `anchor` plus `n` billing intervals. Periods are computed from the anchor so
/// that a 31st anchor returns to the 31st after a short month.
pub(crate) fn add_intervals(anchor: i64, interval: &str, n: u32) -> i64 {
    let months = if interval == "year" { 12 * n } else { n };
    DateTime::from_timestamp(anchor, 0)
        .and_then(|d| d.checked_add_months(Months::new(months)))
        .map_or(anchor, |d| d.timestamp())
}

pub(crate) fn items_url(subscription: &str) -> String {
    format!("/v1/subscription_items?subscription={subscription}")
}

/// Inputs for a subscription item.
pub(crate) struct NewItem<'a> {
    pub id: &'a str,
    pub subscription: &'a str,
    pub price: &'a stripe::Price,
    pub quantity: u64,
    pub created: i64,
    pub period: (i64, i64),
}

/// A subscription item. Presence follows ruling R1; `billed_until` (optional, not nullable)
/// is absent, like other unset non-nullable optionals.
pub(crate) fn new_item(i: &NewItem) -> stripe::SubscriptionItem {
    stripe::SubscriptionItem {
        billed_until: None,
        billing_thresholds: Some(None),
        created: i.created,
        current_period_end: i.period.1,
        current_period_start: i.period.0,
        current_trial: Some(None),
        discounts: Vec::new(),
        id: i.id.to_string(),
        metadata: BTreeMap::new(),
        object: stripe::SubscriptionItemObject::SubscriptionItem,
        price: i.price.clone(),
        quantity: Some(i.quantity as i64),
        subscription: i.subscription.to_string(),
        tax_rates: Some(Some(Vec::new())),
    }
}

/// Inputs for a new (incomplete) subscription.
pub(crate) struct NewSubscription<'a> {
    pub id: &'a str,
    pub customer: &'a str,
    pub currency: &'a str,
    pub items: &'a [stripe::SubscriptionItem],
    pub created: i64,
    pub page_size: usize,
    /// The tax rate its invoices apply (phase 9 ruling Q5): a snapshot taken now, kept in
    /// `default_tax_rates` whatever happens to the rate later.
    pub tax_rate: Option<&'a stripe::TaxRate>,
}

/// A new `incomplete` subscription. Presence follows ruling R1: every field is present,
/// `null` when unset, except the optional non-nullable `presentment_details` and
/// `status_details`, which Stripe only sends for adaptive pricing and paused subscriptions.
pub(crate) fn new_subscription(s: &NewSubscription) -> stripe::Subscription {
    stripe::Subscription {
        application: Some(None),
        application_fee_percent: Some(None),
        automatic_tax: stripe::SubscriptionAutomaticTax {
            disabled_reason: Some(None),
            enabled: false,
            liability: Some(None),
        },
        billing_cycle_anchor: s.created,
        billing_cycle_anchor_config: Some(None),
        // Flexible is the default billing mode for new subscriptions at this API version.
        billing_mode: stripe::SubscriptionsResourceBillingMode {
            flexible: Some(Some(stripe::SubscriptionsResourceBillingModeFlexible {
                proration_discounts: Some(
                    stripe::SubscriptionsResourceBillingModeFlexibleProrationDiscounts::Included,
                ),
            })),
            type_: stripe::SubscriptionsResourceBillingModeType::Flexible,
            updated_at: None,
        },
        billing_schedules: Vec::new(),
        billing_thresholds: Some(None),
        cancel_at: Some(None),
        cancel_at_period_end: false,
        canceled_at: Some(None),
        cancellation_details: Some(Some(stripe::CancellationDetails {
            comment: Some(None),
            feedback: Some(None),
            feedback_option: Some(None),
            reason: Some(None),
        })),
        collection_method: stripe::SubscriptionCollectionMethod::ChargeAutomatically,
        created: s.created,
        currency: s.currency.to_string(),
        customer: stripe::Expandable::Id(s.customer.to_string()),
        customer_account: Some(None),
        days_until_due: Some(None),
        default_payment_method: Some(None),
        default_source: Some(None),
        default_tax_rates: Some(Some(s.tax_rate.into_iter().cloned().collect())),
        description: Some(None),
        discounts: Vec::new(),
        ended_at: Some(None),
        id: s.id.to_string(),
        invoice_settings: stripe::SubscriptionsResourceSubscriptionInvoiceSettings {
            account_tax_ids: Some(None),
            custom_fields: Some(None),
            description: Some(None),
            footer: Some(None),
            issuer: self_issuer(),
        },
        items: embedded_list(s.items, s.page_size, &items_url(s.id)),
        latest_invoice: Some(None),
        livemode: false,
        managed_payments: Some(None),
        metadata: BTreeMap::new(),
        next_pending_invoice_item_invoice: Some(None),
        object: stripe::SubscriptionObject::Subscription,
        on_behalf_of: Some(None),
        pause_collection: Some(None),
        payment_settings: Some(Some(stripe::SubscriptionsResourcePaymentSettings {
            payment_method_options: Some(None),
            payment_method_types: Some(None),
            save_default_payment_method: Some(Some(
                stripe::SubscriptionsResourcePaymentSettingsSaveDefaultPaymentMethod::Off,
            )),
        })),
        pending_invoice_item_interval: Some(None),
        pending_setup_intent: Some(None),
        pending_update: Some(None),
        presentment_details: None,
        schedule: Some(None),
        start_date: s.created,
        status: stripe::SubscriptionStatus::Incomplete,
        status_details: None,
        test_clock: Some(None),
        transfer_data: Some(None),
        trial_end: Some(None),
        trial_settings: Some(Some(stripe::SubscriptionsResourceTrialSettingsTrialSettings {
            end_behavior: stripe::SubscriptionsResourceTrialSettingsEndBehavior {
                billing_cycle_anchor: Some(None),
                missing_payment_method:
                    stripe::SubscriptionsResourceTrialSettingsEndBehaviorMissingPaymentMethod::CreateInvoice,
            },
        })),
        trial_start: Some(None),
    }
}

/// The platform itself as the invoice issuer: `{type: "self"}`.
pub(crate) fn self_issuer() -> stripe::ConnectAccountReference {
    stripe::ConnectAccountReference {
        account: None,
        type_: stripe::ConnectAccountReferenceType::Self_,
    }
}

/// The interval of a recurring price (`"day"`, `"week"`, `"month"` or `"year"`).
pub(crate) fn price_interval(price: &stripe::Price) -> Option<&'static str> {
    price
        .recurring
        .as_ref()
        .and_then(|r| r.as_ref())
        .map(|r| match r.interval {
            stripe::RecurringInterval::Day => "day",
            stripe::RecurringInterval::Week => "week",
            stripe::RecurringInterval::Month => "month",
            stripe::RecurringInterval::Year => "year",
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::catalog::{PriceSpec, new_price};

    #[test]
    fn new_items_and_subscriptions_conform() {
        let price = new_price(
            &PriceSpec {
                id: "price_1".into(),
                currency: "usd",
                unit_amount: 500,
                interval: Some("month"),
                nickname: None,
            },
            "prod_1",
            1,
        );
        assert_eq!(price_interval(&price), Some("month"));
        let item = new_item(&NewItem {
            id: "si_1",
            subscription: "sub_1",
            price: &price,
            quantity: 3,
            created: 1,
            period: (1, 2),
        });
        let sub = new_subscription(&NewSubscription {
            id: "sub_1",
            customer: "cus_1",
            currency: "usd",
            items: &[item],
            created: 1,
            page_size: 10,
            tax_rate: None,
        });
        let v = serde_json::to_value(&sub).unwrap();
        crate::stripe::check_payload(&v).unwrap();
        assert_eq!(v["status"], "incomplete");
        assert_eq!(
            v["items"]["url"],
            "/v1/subscription_items?subscription=sub_1"
        );
        assert_eq!(v["items"]["data"][0]["quantity"], 3);
        assert!(v.get("status_details").is_none());
    }

    #[test]
    fn month_end_clamps() {
        // 2026-01-31 -> 2026-02-28
        assert_eq!(period_end(1_769_817_600, "month"), 1_772_236_800);
        // ...and back to the 31st from the anchor: 2026-03-31
        assert_eq!(add_intervals(1_769_817_600, "month", 2), 1_774_915_200);
        // 2026-01-01 + 1 year
        assert_eq!(period_end(1_767_225_600, "year"), 1_798_761_600);
    }
}
