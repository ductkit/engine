//! Subscription schedules (phase 9 ruling Q9): the `schedule_subscription` action, the
//! phase step at each renewal, the early endings, the `CancelSubscriptionSchedule` control
//! hook and the schedule pagination write.
//!
//! A schedule manages one subscription through 2-3 phases. At the renewal that closes a
//! phase, the subscription's items are swapped for the next phase's; after the last phase
//! the schedule either releases the subscription (`release`) or cancels it (`cancel`). A
//! subscription that ends any other way cancels its schedule.

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::billing::{resolve_customer, void};
use crate::generate::subscriptions::{
    create_subscription, draw_items, draw_subscription, items, recurring_pool, set_status,
    sub_status, update_sub,
};
use crate::generate::{Continuation, Ctx, pick};
use crate::model::customer::random_metadata_value;
use crate::model::schedule::{
    NewPhase, NewSchedule, ScheduleEndBehavior, current_phase, deleted_item, new_schedule,
};
use crate::model::subscription::{NewItem, add_intervals, new_item, price_interval};
use crate::rng::SimRng;
use crate::stripe::{
    Price, Subscription, SubscriptionSchedule, SubscriptionScheduleStatus as ScheduleStatus,
    SubscriptionStatus as Status,
};
use crate::{MockError, ObjectType};

/// Chance that a drawn end behavior is `release` (else `cancel`).
const RELEASE_PROBABILITY: f64 = 0.7;

/// Whether a renewal goes on after the schedule's step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenewStep {
    /// Renew as usual: bill the (possibly new) items for the next period.
    Continue,
    /// The schedule canceled the subscription: no renewal invoice.
    Stop,
}

fn price(engine: &Engine, id: &str) -> Price {
    engine
        .latest_obj(ObjectType::Price, id)
        .expect("a drawn price exists")
}

/// The prices and iteration count of each of 2-3 phases (ruling Q9): each phase has 1-2
/// prices, distinct while the pool lasts, from the active recurring prices sharing the
/// currency and interval of one drawn from `recurring`, and lasts 1-3 of that interval.
fn draw_phases(rng: &mut SimRng, engine: &Engine, recurring: &[String]) -> Vec<(Vec<String>, u32)> {
    let n = 2 + rng.below(2);
    let first = price(engine, pick(rng, recurring));
    let pool: Vec<String> = recurring
        .iter()
        .filter(|id| {
            let p = price(engine, id);
            p.currency == first.currency && price_interval(&p) == price_interval(&first)
        })
        .cloned()
        .collect();
    (0..n)
        .map(|_| {
            let count = 1 + rng.below(2);
            let mut left = pool.clone();
            let mut prices = Vec::new();
            for _ in 0..count {
                if left.is_empty() {
                    prices.push(pick(rng, &pool).clone());
                } else {
                    let i = rng.below(left.len() as u64) as usize;
                    prices.push(left.remove(i));
                }
            }
            let iterations = 1 + rng.below(3) as u32;
            (prices, iterations)
        })
        .collect()
}

fn invalid(msg: String) -> MockError {
    MockError::InvalidAction(msg)
}

/// `schedule_subscription` (ruling Q9): a subscription for the named or a random customer,
/// managed by a new schedule ending in `end_behavior` (drawn 70% `release`, 30% `cancel`
/// when `None`). A named customer that does not exist or is deleted is a 409 naming it.
///
/// In one instant: the subscription, as `start_subscription` creates it (tax rate included)
/// with `schedule` set, then the `active` schedule in its first phase
/// (`subscription_schedule.created`). Its first invoice, payment and activation then run as
/// for any subscription. Every draw is made here: the customer, the phases, the end
/// behavior, then the subscription and each phase's item IDs and quantities, then the
/// schedule's ID.
pub(crate) fn schedule_subscription(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<String>,
    end_behavior: Option<ScheduleEndBehavior>,
) -> Result<Cause, MockError> {
    if let Some(c) = &customer {
        let live = engine
            .latest(ObjectType::Customer, c)
            .is_some_and(|u| u.kind != ChangeKind::Deleted);
        if !live {
            return Err(invalid(format!(
                "customer {c} does not exist or is deleted"
            )));
        }
    }
    let cause = engine.begin_action("schedule_subscription");
    let customer = resolve_customer(rng, engine, customer, &cause)?;
    let recurring = recurring_pool(rng, engine, &cause);
    let phases = draw_phases(rng, engine, &recurring);
    let end_behavior = end_behavior.unwrap_or_else(|| {
        if rng.chance(RELEASE_PROBABILITY) {
            ScheduleEndBehavior::Release
        } else {
            ScheduleEndBehavior::Cancel
        }
    });
    let draw = draw_subscription(rng, engine, phases[0].0.clone());
    let mut phase_items = vec![draw.items.clone()];
    for (prices, _) in &phases[1..] {
        phase_items.push(draw_items(rng, engine, prices.clone()));
    }
    let id = rng.id(ObjectType::SubscriptionSchedule.id_prefix());
    create_subscription(engine, ctx, &customer, &draw, Some(&id), &cause);
    let sub: Subscription = engine
        .latest_obj(ObjectType::Subscription, &draw.id)
        .expect("just created");
    let interval = price_interval(&price(engine, &phases[0].0[0])).expect("recurring");
    let mut start = sub.start_date;
    let new_phases: Vec<NewPhase> = phases
        .iter()
        .zip(&phase_items)
        .map(|((_, iterations), items)| {
            let end = add_intervals(start, interval, *iterations);
            let p = NewPhase {
                items: items.iter().map(|(pr, _, q)| (pr.clone(), *q)).collect(),
                start,
                end,
            };
            start = end;
            p
        })
        .collect();
    let tax_rates = sub.default_tax_rates.clone().flatten().unwrap_or_default();
    let schedule = new_schedule(&NewSchedule {
        id: &id,
        customer: &customer,
        subscription: &draw.id,
        currency: &sub.currency,
        end_behavior,
        phases: &new_phases,
        tax_rates: &tax_rates,
        billing_mode: sub.billing_mode.clone(),
        created: engine.now().unix_seconds(),
    });
    engine.create_obj(
        ObjectType::SubscriptionSchedule,
        id.clone(),
        &schedule,
        cause.clone(),
        &["subscription_schedule.created"],
    );
    ctx.schedule_items.insert(
        id,
        phase_items
            .iter()
            .map(|items| items.iter().map(|(_, item, _)| item.clone()).collect())
            .collect(),
    );
    ctx.then(
        engine,
        Continuation::SubscriptionInvoice { sub: draw.id },
        cause.clone(),
    );
    Ok(cause)
}

/// The `active` schedule managing `sub`, if any.
pub(crate) fn active_schedule(engine: &Engine, sub: &str) -> Option<SubscriptionSchedule> {
    let s: Subscription = engine.latest_obj(ObjectType::Subscription, sub)?;
    let id = s.schedule.flatten()?.id().to_string();
    let schedule: SubscriptionSchedule =
        engine.latest_obj(ObjectType::SubscriptionSchedule, &id)?;
    (schedule.status == ScheduleStatus::Active).then_some(schedule)
}

fn update_schedule(
    engine: &mut Engine,
    id: &str,
    f: impl FnOnce(&mut SubscriptionSchedule),
    kind: ChangeKind,
    event: &str,
    cause: &Cause,
) {
    engine
        .update_obj(
            ObjectType::SubscriptionSchedule,
            id,
            f,
            cause.clone(),
            kind,
            &[event],
        )
        .expect("schedules are never deleted");
}

/// Ends the schedule `id` early: `canceled`, with `canceled_at`, announced as `event`
/// (`subscription_schedule.canceled` or `.aborted`).
fn cancel_schedule(engine: &mut Engine, id: &str, event: &str, cause: &Cause) {
    let now = engine.now().unix_seconds();
    update_schedule(
        engine,
        id,
        |s| {
            s.status = ScheduleStatus::Canceled;
            s.canceled_at = Some(Some(now));
            s.current_phase = Some(None);
        },
        ChangeKind::StateChanged,
        event,
        cause,
    );
}

/// The schedule's step at a renewal of `sub` (ruling Q9), before the renewal invoice. It
/// runs only for an `active` schedule whose current phase ends at or before the period
/// being closed (the items' `current_period_end`; at an on-time renewal that is now):
///
/// - **A next phase exists:** the old items are deleted and the next phase's created (both
///   unannounced), with the closing period, which the renewal then advances as for any
///   item; `current_phase` moves on (`subscription_schedule.updated`). The renewal goes on
///   and bills the new items.
/// - **Last phase, `release`:** the schedule is `released` (`subscription_schedule.released`),
///   then the subscription's `schedule` is cleared (`customer.subscription.updated`). The
///   renewal goes on.
/// - **Last phase, `cancel`:** the subscription is canceled (`customer.subscription.deleted`),
///   then the schedule is `completed` (`subscription_schedule.completed`). No renewal.
///
/// The caller runs it only for an `active` or `past_due` subscription with natural renewals,
/// so an ended subscription, or one whose renewals are off, never moves a phase.
pub(crate) fn on_renew(engine: &mut Engine, ctx: &mut Ctx, sub: &str, cause: &Cause) -> RenewStep {
    let Some(schedule) = active_schedule(engine, sub) else {
        return RenewStep::Continue;
    };
    let Some(Some(current)) = schedule.current_phase.clone() else {
        return RenewStep::Continue;
    };
    let old = items(engine, sub);
    let Some(period) = old
        .first()
        .map(|i| (i.current_period_start, i.current_period_end))
    else {
        return RenewStep::Continue;
    };
    if period.1 < current.end_date {
        return RenewStep::Continue;
    }
    let k = schedule
        .phases
        .iter()
        .position(|p| p.start_date == current.start_date)
        .expect("the current phase is one of the phases");
    let now = engine.now().unix_seconds();
    if let Some(next) = schedule.phases.get(k + 1) {
        let ids = ctx
            .schedule_items
            .get(&schedule.id)
            .and_then(|phases| phases.get(k + 1))
            .cloned()
            .expect("item IDs are drawn with the schedule");
        for item in &old {
            engine
                .delete(
                    ObjectType::SubscriptionItem,
                    &item.id,
                    &deleted_item(&item.id),
                    cause.clone(),
                    &[],
                )
                .expect("live item");
        }
        for (id, configured) in ids.iter().zip(&next.items) {
            let price = price(engine, configured.price.id());
            let item = new_item(&NewItem {
                id,
                subscription: sub,
                price: &price,
                quantity: configured.quantity.unwrap_or(1) as u64,
                created: now,
                period,
            });
            engine.create_obj(
                ObjectType::SubscriptionItem,
                id.clone(),
                &item,
                cause.clone(),
                &[],
            );
        }
        let phase = current_phase(next);
        update_schedule(
            engine,
            &schedule.id,
            |s| s.current_phase = Some(Some(phase)),
            ChangeKind::Updated,
            "subscription_schedule.updated",
            cause,
        );
        return RenewStep::Continue;
    }
    ctx.schedule_items.remove(&schedule.id);
    match ScheduleEndBehavior::of(&schedule) {
        ScheduleEndBehavior::Release => {
            update_schedule(
                engine,
                &schedule.id,
                |s| {
                    s.status = ScheduleStatus::Released;
                    s.released_at = Some(Some(now));
                    s.released_subscription = Some(Some(sub.to_string()));
                    s.subscription = Some(None);
                    s.current_phase = Some(None);
                },
                ChangeKind::StateChanged,
                "subscription_schedule.released",
                cause,
            );
            update_sub(
                engine,
                sub,
                |s| s.schedule = Some(None),
                ChangeKind::Updated,
                &["customer.subscription.updated"],
                cause,
            );
            RenewStep::Continue
        }
        ScheduleEndBehavior::Cancel => {
            set_status(engine, sub, Status::Canceled, cause);
            update_schedule(
                engine,
                &schedule.id,
                |s| {
                    s.status = ScheduleStatus::Completed;
                    s.completed_at = Some(Some(now));
                    s.current_phase = Some(None);
                },
                ChangeKind::StateChanged,
                "subscription_schedule.completed",
                cause,
            );
            RenewStep::Stop
        }
    }
}

/// After `sub` ended some other way than its schedule's last phase (canceled, expired, its
/// customer deleted, or dunning gave up: `aborted`), its `active` schedule is `canceled`
/// (`subscription_schedule.canceled`, or `subscription_schedule.aborted` for dunning). The
/// caller announces the subscription's end first. Does nothing without an active schedule.
pub(crate) fn end_early(engine: &mut Engine, sub: &str, aborted: bool, cause: &Cause) {
    let Some(schedule) = active_schedule(engine, sub) else {
        return;
    };
    let event = if aborted {
        "subscription_schedule.aborted"
    } else {
        "subscription_schedule.canceled"
    };
    cancel_schedule(engine, &schedule.id, event, cause);
}

/// Active schedules, in creation order.
fn active_ids(engine: &Engine) -> Vec<String> {
    engine
        .store()
        .live_ids(ObjectType::SubscriptionSchedule)
        .iter()
        .filter(|id| {
            engine
                .latest(ObjectType::SubscriptionSchedule, id)
                .is_some_and(|u| u.state["status"] == "active")
        })
        .cloned()
        .collect()
}

/// The `CancelSubscriptionSchedule` control hook (ruling Q11): cancels the named or a random
/// `active` schedule (`subscription_schedule.canceled`), then its subscription now
/// (`customer.subscription.deleted`). An `incomplete` subscription is canceled too, and its
/// open first invoice voided (a draft is deleted by its own finalize step). A named schedule
/// that does not exist or is not `active`, or none active, is a 409 naming it.
pub(crate) fn cancel_subscription_schedule(
    rng: &mut SimRng,
    engine: &mut Engine,
    id: Option<String>,
) -> Result<Cause, MockError> {
    let id = match id {
        Some(id) => {
            let Some(u) = engine.latest(ObjectType::SubscriptionSchedule, &id) else {
                return Err(invalid(format!(
                    "subscription schedule {id} does not exist"
                )));
            };
            let status = u.state["status"].as_str().unwrap_or_default().to_string();
            if status != "active" {
                return Err(invalid(format!(
                    "subscription schedule {id} is {status}: only an active schedule can be canceled"
                )));
            }
            id
        }
        None => {
            let ids = active_ids(engine);
            if ids.is_empty() {
                return Err(invalid(
                    "no active subscription schedule to cancel".to_string(),
                ));
            }
            pick(rng, &ids).clone()
        }
    };
    let schedule: SubscriptionSchedule = engine
        .latest_obj(ObjectType::SubscriptionSchedule, &id)
        .expect("checked schedule");
    let sub = schedule
        .subscription
        .flatten()
        .expect("an active schedule manages a subscription")
        .id()
        .to_string();
    let cause = engine.begin_action("cancel_subscription_schedule");
    cancel_schedule(engine, &id, "subscription_schedule.canceled", &cause);
    match sub_status(engine, &sub) {
        Some(Status::Active | Status::PastDue) => {
            set_status(engine, &sub, Status::Canceled, &cause);
        }
        Some(Status::Incomplete) => {
            set_status(engine, &sub, Status::Canceled, &cause);
            let open = engine
                .latest_obj::<Subscription>(ObjectType::Subscription, &sub)
                .and_then(|s| s.latest_invoice.flatten())
                .map(|i| i.id().to_string())
                .filter(|i| {
                    engine
                        .latest(ObjectType::Invoice, i)
                        .is_some_and(|u| u.state["status"] == "open")
                });
            if let Some(invoice) = open {
                void(engine, &invoice, &cause);
            }
        }
        _ => {}
    }
    Ok(cause)
}

/// The schedule pagination write (ruling Q10): a new `metadata.note` on a random `active`
/// schedule (`subscription_schedule.updated`). With none, nothing happens.
pub(crate) fn pagination_update(rng: &mut SimRng, engine: &mut Engine) {
    let ids = active_ids(engine);
    if ids.is_empty() {
        return;
    }
    let id = pick(rng, &ids).clone();
    let note = random_metadata_value(rng);
    let cause = engine.begin_action("subscription_schedule_update");
    update_schedule(
        engine,
        &id,
        |s| {
            s.metadata
                .get_or_insert_with(|| Some(Default::default()))
                .get_or_insert_with(Default::default)
                .insert("note".to_string(), note);
        },
        ChangeKind::Updated,
        "subscription_schedule.updated",
        &cause,
    );
}
