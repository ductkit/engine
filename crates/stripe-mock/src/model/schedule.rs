//! Subscription schedules (phase 9 ruling Q9), built on the generated
//! [`stripe::SubscriptionSchedule`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::model::subscription::self_issuer;
use crate::stripe::{self, Expandable};

/// What a schedule does after its last phase (ruling Q9): `release` leaves the subscription
/// running on its own; `cancel` cancels it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleEndBehavior {
    Release,
    Cancel,
}

impl ScheduleEndBehavior {
    pub(crate) fn stripe(self) -> stripe::SubscriptionScheduleEndBehavior {
        match self {
            ScheduleEndBehavior::Release => stripe::SubscriptionScheduleEndBehavior::Release,
            ScheduleEndBehavior::Cancel => stripe::SubscriptionScheduleEndBehavior::Cancel,
        }
    }

    /// The behavior of a stored schedule. The mock only writes `release` and `cancel`.
    pub(crate) fn of(s: &stripe::SubscriptionSchedule) -> ScheduleEndBehavior {
        match s.end_behavior {
            stripe::SubscriptionScheduleEndBehavior::Cancel => ScheduleEndBehavior::Cancel,
            _ => ScheduleEndBehavior::Release,
        }
    }
}

/// One phase: its items (`(price, quantity)`) and dates.
pub(crate) struct NewPhase {
    pub items: Vec<(String, u64)>,
    pub start: i64,
    pub end: i64,
}

/// Inputs for a new `active` schedule.
pub(crate) struct NewSchedule<'a> {
    pub id: &'a str,
    pub customer: &'a str,
    pub subscription: &'a str,
    pub currency: &'a str,
    pub end_behavior: ScheduleEndBehavior,
    pub phases: &'a [NewPhase],
    /// The subscription's `default_tax_rates` (ruling Q5), which every phase repeats.
    pub tax_rates: &'a [stripe::TaxRate],
    /// The subscription's billing mode.
    pub billing_mode: stripe::SubscriptionsResourceBillingMode,
    pub created: i64,
}

/// The `{start_date, end_date}` of a phase.
pub(crate) fn current_phase(
    p: &stripe::SubscriptionSchedulePhaseConfiguration,
) -> stripe::SubscriptionScheduleCurrentPhase {
    stripe::SubscriptionScheduleCurrentPhase {
        end_date: p.end_date,
        start_date: p.start_date,
    }
}

/// A phase's item. Presence follows ruling R1: every field is present.
fn phase_item(price: &str, quantity: u64) -> stripe::SubscriptionScheduleConfigurationItem {
    stripe::SubscriptionScheduleConfigurationItem {
        billing_thresholds: Some(None),
        discounts: Vec::new(),
        metadata: Some(Some(BTreeMap::new())),
        price: Expandable::Id(price.to_string()),
        quantity: Some(quantity as i64),
        tax_rates: Some(Some(Vec::new())),
    }
}

/// A phase. Presence follows ruling R1: every field is present, `null` or empty when unset;
/// `trial` is `false` and `automatic_tax` disabled, as on the subscription.
fn phase(
    p: &NewPhase,
    currency: &str,
    tax_rates: &[stripe::TaxRate],
) -> stripe::SubscriptionSchedulePhaseConfiguration {
    stripe::SubscriptionSchedulePhaseConfiguration {
        add_invoice_items: Vec::new(),
        application_fee_percent: Some(None),
        automatic_tax: Some(stripe::SchedulesPhaseAutomaticTax {
            disabled_reason: Some(None),
            enabled: false,
            liability: Some(None),
        }),
        billing_cycle_anchor: Some(None),
        billing_thresholds: Some(None),
        collection_method: Some(None),
        currency: currency.to_string(),
        default_payment_method: Some(None),
        default_tax_rates: Some(Some(tax_rates.to_vec())),
        description: Some(None),
        discounts: Vec::new(),
        end_date: p.end,
        invoice_settings: Some(None),
        items: p.items.iter().map(|(pr, q)| phase_item(pr, *q)).collect(),
        metadata: Some(Some(BTreeMap::new())),
        on_behalf_of: Some(None),
        proration_behavior:
            stripe::SubscriptionSchedulePhaseConfigurationProrationBehavior::CreateProrations,
        start_date: p.start,
        transfer_data: Some(None),
        trial: Some(false),
        trial_end: Some(None),
    }
}

/// A new `active` schedule in its first phase (ruling Q9). Presence follows ruling R1:
/// every field is present, `null` or empty when unset.
pub(crate) fn new_schedule(s: &NewSchedule) -> stripe::SubscriptionSchedule {
    let phases: Vec<_> = s
        .phases
        .iter()
        .map(|p| phase(p, s.currency, s.tax_rates))
        .collect();
    stripe::SubscriptionSchedule {
        application: Some(None),
        billing_mode: s.billing_mode.clone(),
        canceled_at: Some(None),
        completed_at: Some(None),
        created: s.created,
        current_phase: Some(phases.first().map(current_phase)),
        customer: Expandable::Id(s.customer.to_string()),
        customer_account: Some(None),
        default_settings: stripe::SubscriptionSchedulesResourceDefaultSettings {
            application_fee_percent: Some(None),
            automatic_tax: Some(
                stripe::SubscriptionSchedulesResourceDefaultSettingsAutomaticTax {
                    disabled_reason: Some(None),
                    enabled: false,
                    liability: Some(None),
                },
            ),
            billing_cycle_anchor:
                stripe::SubscriptionSchedulesResourceDefaultSettingsBillingCycleAnchor::Automatic,
            billing_thresholds: Some(None),
            collection_method: Some(Some(
                stripe::SubscriptionSchedulesResourceDefaultSettingsCollectionMethod::ChargeAutomatically,
            )),
            default_payment_method: Some(None),
            description: Some(None),
            invoice_settings: stripe::InvoiceSettingSubscriptionScheduleSetting {
                account_tax_ids: Some(None),
                custom_fields: Some(None),
                days_until_due: Some(None),
                description: Some(None),
                footer: Some(None),
                issuer: self_issuer(),
            },
            on_behalf_of: Some(None),
            transfer_data: Some(None),
        },
        end_behavior: s.end_behavior.stripe(),
        id: s.id.to_string(),
        livemode: false,
        metadata: Some(Some(BTreeMap::new())),
        object: stripe::SubscriptionScheduleObject::SubscriptionSchedule,
        pause_schedules: Vec::new(),
        phases,
        released_at: Some(None),
        released_subscription: Some(None),
        status: stripe::SubscriptionScheduleStatus::Active,
        subscription: Some(Some(Expandable::Id(s.subscription.to_string()))),
        test_clock: Some(None),
    }
}

/// The tombstone of a subscription item a phase change replaced.
pub(crate) fn deleted_item(id: &str) -> stripe::DeletedSubscriptionItem {
    stripe::DeletedSubscriptionItem {
        deleted: true,
        id: id.to_string(),
        object: stripe::DeletedSubscriptionItemObject::SubscriptionItem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::subscription::{NewSubscription, new_subscription};

    #[test]
    fn schedules_conform() {
        let sub = new_subscription(&NewSubscription {
            id: "sub_1",
            customer: "cus_1",
            currency: "usd",
            items: &[],
            created: 1,
            page_size: 10,
            tax_rate: None,
            schedule: Some("sub_sched_1"),
        });
        let v = serde_json::to_value(&sub).unwrap();
        crate::stripe::check_payload(&v).unwrap();
        assert_eq!(v["schedule"], "sub_sched_1");
        let phases = [
            NewPhase {
                items: vec![("price_1".into(), 2)],
                start: 1,
                end: 2,
            },
            NewPhase {
                items: vec![("price_1".into(), 1), ("price_2".into(), 3)],
                start: 2,
                end: 3,
            },
        ];
        for end_behavior in [ScheduleEndBehavior::Release, ScheduleEndBehavior::Cancel] {
            let s = new_schedule(&NewSchedule {
                id: "sub_sched_1",
                customer: "cus_1",
                subscription: "sub_1",
                currency: "usd",
                end_behavior,
                phases: &phases,
                tax_rates: &[],
                billing_mode: sub.billing_mode.clone(),
                created: 1,
            });
            assert_eq!(ScheduleEndBehavior::of(&s), end_behavior);
            let v = serde_json::to_value(&s).unwrap();
            crate::stripe::check_payload(&v).unwrap_or_else(|e| panic!("{e}\n{v:#}"));
            assert_eq!(v["status"], "active");
            assert_eq!(
                v["current_phase"],
                serde_json::json!({"start_date": 1, "end_date": 2})
            );
            assert_eq!(v["phases"][1]["items"][1]["price"], "price_2");
            assert_eq!(v["phases"][1]["items"][1]["quantity"], 3);
        }
        assert_eq!(
            serde_json::to_value(deleted_item("si_1")).unwrap(),
            serde_json::json!({"id": "si_1", "object": "subscription_item", "deleted": true})
        );
        assert_eq!(
            serde_json::to_value(ScheduleEndBehavior::Release).unwrap(),
            "release"
        );
    }
}
