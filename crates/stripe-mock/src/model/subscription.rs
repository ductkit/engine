//! Subscription and subscription item JSON at the pinned (post-"basil") version:
//! billing periods live on the items. Hand-written until phase 7 validates it.

use chrono::{DateTime, Months};
use serde_json::{Value, json};

use crate::model::embedded::embedded_list;

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
    pub price: &'a Value,
    pub quantity: u64,
    pub created: i64,
    pub period: (i64, i64),
}

pub(crate) fn item_json(i: &NewItem) -> Value {
    json!({
        "id": i.id,
        "object": "subscription_item",
        "created": i.created,
        "current_period_end": i.period.1,
        "current_period_start": i.period.0,
        "discounts": [],
        "metadata": {},
        "price": i.price,
        "quantity": i.quantity,
        "subscription": i.subscription,
        "tax_rates": []
    })
}

/// Inputs for a new (incomplete) subscription.
pub(crate) struct NewSubscription<'a> {
    pub id: &'a str,
    pub customer: &'a str,
    pub currency: &'a Value,
    pub items: &'a [Value],
    pub created: i64,
    pub page_size: usize,
}

pub(crate) fn subscription_json(s: &NewSubscription) -> Value {
    json!({
        "id": s.id,
        "object": "subscription",
        "application": null,
        "automatic_tax": {"enabled": false, "liability": null},
        "billing_cycle_anchor": s.created,
        "cancel_at": null,
        "cancel_at_period_end": false,
        "canceled_at": null,
        "cancellation_details": {"comment": null, "feedback": null, "reason": null},
        "collection_method": "charge_automatically",
        "created": s.created,
        "currency": s.currency,
        "customer": s.customer,
        "days_until_due": null,
        "default_payment_method": null,
        "description": null,
        "discounts": [],
        "ended_at": null,
        "items": embedded_list(s.items, s.page_size, &items_url(s.id)),
        "latest_invoice": null,
        "livemode": false,
        "metadata": {},
        "pending_update": null,
        "start_date": s.created,
        "status": "incomplete",
        "test_clock": null,
        "trial_end": null,
        "trial_start": null
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
