//! Subscription lifecycle (spec sections 5.2, 6.1): start, first invoice,
//! activation and expiry of unpaid starts.

use crate::engine::store::StoredObject;
use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::billing::{
    attempt_payment, create_draft_invoice, delete_draft, finalize, mark_uncollectible,
    resolve_customer, void,
};
use crate::generate::catalog::{active_ids, create_product_now, draw_new_product};
use crate::generate::{Continuation, Ctx, payments, pick};
use crate::model::customer::{deleted_stub, random_metadata_value};
use crate::model::embedded::embedded_list;
use crate::model::invoice::{BillingReason, LineSpec};
use crate::model::subscription::{
    NewItem, NewSubscription, add_intervals, items_url, new_item, new_subscription, period_end,
    price_interval,
};
use crate::rng::SimRng;
use crate::stripe::{
    Expandable, List, Price, Subscription, SubscriptionItem, SubscriptionStatus as Status,
};
use crate::{MockError, ObjectType, RenewMode, SimTime};

/// How long an unpaid first invoice keeps a subscription `incomplete`.
const INCOMPLETE_EXPIRY_MS: i64 = 23 * 3_600_000;

/// A subscription's status, read without deserializing the whole subscription.
pub(crate) fn sub_status(engine: &Engine, id: &str) -> Option<Status> {
    engine
        .latest(ObjectType::Subscription, id)
        .and_then(|u| serde_json::from_value(u.state["status"].clone()).ok())
}

fn latest_sub(engine: &Engine, id: &str) -> Subscription {
    engine
        .latest_obj(ObjectType::Subscription, id)
        .expect("subscriptions are never hard-deleted")
}

/// Applies `f` to a subscription.
pub(crate) fn update_sub(
    engine: &mut Engine,
    id: &str,
    f: impl FnOnce(&mut Subscription),
    kind: ChangeKind,
    events: &[&str],
    cause: &Cause,
) {
    engine
        .update_obj(ObjectType::Subscription, id, f, cause.clone(), kind, events)
        .expect("subscriptions are never hard-deleted");
}

/// Live items of a subscription, in creation order.
pub(crate) fn items(engine: &Engine, sub: &str) -> Vec<SubscriptionItem> {
    engine
        .store()
        .children(ObjectType::SubscriptionItem, "subscription", sub)
        .into_iter()
        .filter(|o| !o.deleted)
        .map(|o| {
            engine
                .latest_obj(ObjectType::SubscriptionItem, &o.latest.object_id)
                .expect("listed item exists")
        })
        .collect()
}

/// The subscription's `items` list rendered from its item objects.
pub(crate) fn rendered_items(
    engine: &Engine,
    sub: &str,
    page_size: usize,
) -> List<SubscriptionItem> {
    embedded_list(&items(engine, sub), page_size, &items_url(sub))
}

fn price_state(engine: &Engine, id: &str) -> Option<Price> {
    engine.latest_obj(ObjectType::Price, id)
}

fn interval(price: &Price) -> Option<&'static str> {
    price_interval(price)
}

/// Validates named prices: active, recurring, one currency and interval.
fn check_prices(engine: &Engine, ids: &[String]) -> Result<(), MockError> {
    let mut key = None;
    for id in ids {
        let p = price_state(engine, id).ok_or_else(|| MockError::NoSuchObject {
            object_type: ObjectType::Price,
            id: id.clone(),
        })?;
        if !p.active || interval(&p).is_none() {
            return Err(MockError::InvalidAction(format!(
                "price {id} must be active and recurring"
            )));
        }
        let k = (p.currency.clone(), interval(&p));
        if key.get_or_insert(k.clone()) != &k {
            return Err(MockError::InvalidAction(
                "all prices of a subscription need one currency and interval".to_string(),
            ));
        }
    }
    if ids.is_empty() {
        return Err(MockError::InvalidAction(
            "a subscription needs at least one price".to_string(),
        ));
    }
    Ok(())
}

/// Active recurring prices, in creation order.
pub(crate) fn active_recurring_prices(engine: &Engine) -> Vec<String> {
    active_ids(engine, ObjectType::Price)
        .into_iter()
        .filter(|id| price_state(engine, id).is_some_and(|p| interval(&p).is_some()))
        .collect()
}

/// 1-3 prices (or 11-15 with probability 0.05) sharing one currency and interval,
/// distinct while the pool lasts.
fn choose_prices(rng: &mut SimRng, engine: &mut Engine, cause: &Cause) -> Vec<String> {
    let recurring = active_recurring_prices(engine);
    let recurring = if recurring.is_empty() {
        let np = draw_new_product(rng);
        create_product_now(engine, &np, cause)
            .into_iter()
            .filter(|id| price_state(engine, id).is_some_and(|p| interval(&p).is_some()))
            .collect()
    } else {
        recurring
    };
    pick_prices(rng, engine, recurring)
}

/// Draws the prices of a subscription from `recurring` (active recurring prices, not
/// empty), as [`choose_prices`] describes.
pub(crate) fn pick_prices(
    rng: &mut SimRng,
    engine: &Engine,
    recurring: Vec<String>,
) -> Vec<String> {
    let first = price_state(engine, pick(rng, &recurring)).expect("price");
    let mut pool: Vec<String> = recurring
        .into_iter()
        .filter(|id| {
            price_state(engine, id)
                .is_some_and(|p| p.currency == first.currency && interval(&p) == interval(&first))
        })
        .collect();
    let n = if rng.chance(0.05) {
        11 + rng.below(5)
    } else {
        1 + rng.below(3)
    } as usize;
    let mut chosen = Vec::with_capacity(n);
    let all = pool.clone();
    for _ in 0..n {
        if pool.is_empty() {
            chosen.push(pick(rng, &all).clone());
        } else {
            let i = rng.below(pool.len() as u64) as usize;
            chosen.push(pool.remove(i));
        }
    }
    chosen
}

/// The drawn parts of a new subscription: its ID, and each item's price, ID and quantity.
#[derive(Debug, Clone)]
pub(crate) struct SubscriptionDraw {
    pub id: String,
    pub items: Vec<(String, String, u64)>,
}

/// Draws the subscription and item IDs and the item quantities (1-5) for `prices`. The
/// quantities are then capped so one period's charge stays within Stripe's maximum
/// (see [`payments::fit_quantities`]).
pub(crate) fn draw_subscription(
    rng: &mut SimRng,
    engine: &Engine,
    prices: Vec<String>,
) -> SubscriptionDraw {
    let id = rng.id(ObjectType::Subscription.id_prefix());
    let mut items: Vec<(String, String, u64)> = prices
        .into_iter()
        .map(|price| {
            let item = rng.id(ObjectType::SubscriptionItem.id_prefix());
            let quantity = 1 + rng.below(5);
            (price, item, quantity)
        })
        .collect();
    let units: Vec<i64> = items
        .iter()
        .map(|(p, _, _)| payments::unit_amount(engine, p))
        .collect();
    let mut quantities: Vec<u64> = items.iter().map(|(_, _, q)| *q).collect();
    payments::fit_quantities(&units, &mut quantities);
    for (item, q) in items.iter_mut().zip(quantities) {
        item.2 = q;
    }
    SubscriptionDraw { id, items }
}

/// Creates the items (unannounced) and the `incomplete` subscription
/// (`customer.subscription.created`) of `draw` for `customer`, starting now.
pub(crate) fn create_subscription(
    engine: &mut Engine,
    ctx: &Ctx,
    customer: &str,
    draw: &SubscriptionDraw,
    cause: &Cause,
) {
    let sub_id = &draw.id;
    let now = engine.now().unix_seconds();
    let mut item_states = Vec::new();
    let mut currency = String::new();
    for (price_id, id, quantity) in &draw.items {
        let price = price_state(engine, price_id).expect("checked price");
        currency = price.currency.clone();
        let end = period_end(now, interval(&price).expect("recurring"));
        let item = new_item(&NewItem {
            id,
            subscription: sub_id,
            price: &price,
            quantity: *quantity,
            created: now,
            period: (now, end),
        });
        engine.create_obj(
            ObjectType::SubscriptionItem,
            id.clone(),
            &item,
            cause.clone(),
            &[],
        );
        item_states.push(item);
    }
    let sub = new_subscription(&NewSubscription {
        id: sub_id,
        customer,
        currency: &currency,
        items: &item_states,
        created: now,
        page_size: ctx.page_size,
    });
    engine.create_obj(
        ObjectType::Subscription,
        sub_id.clone(),
        &sub,
        cause.clone(),
        &["customer.subscription.created"],
    );
}

/// Starts a subscription for `customer` in one instant, as Checkout does: the subscription
/// of `draw`, its first invoice, finalized and paid (the payment succeeds), and the
/// subscription `active` with its renewal queued. Returns the first invoice.
pub(crate) fn start_paid_now(
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: &str,
    draw: &SubscriptionDraw,
    cause: &Cause,
) -> String {
    create_subscription(engine, ctx, customer, draw, cause);
    let sub = &draw.id;
    let invoice = invoice_subscription(engine, ctx, sub, BillingReason::SubscriptionCreate, cause);
    finalize(engine, &invoice, cause);
    payments::pay_invoice(engine, ctx, &invoice, true, cause);
    set_status(engine, sub, Status::Active, cause);
    schedule_renewal(engine, ctx, sub, cause);
    invoice
}

/// `start_subscription`: items and an `incomplete` subscription now, then the first
/// invoice, its finalization and the payment attempt, `step_delay` apart.
pub(crate) fn start_subscription(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<String>,
    prices: Option<Vec<String>>,
) -> Result<Cause, MockError> {
    if let Some(ids) = &prices {
        check_prices(engine, ids)?;
    }
    let cause = engine.begin_action("start_subscription");
    let customer = resolve_customer(rng, engine, customer, &cause)?;
    let prices = match prices {
        Some(p) => p,
        None => choose_prices(rng, engine, &cause),
    };
    let draw = draw_subscription(rng, engine, prices);
    create_subscription(engine, ctx, &customer, &draw, &cause);
    ctx.then(
        engine,
        Continuation::SubscriptionInvoice { sub: draw.id },
        cause.clone(),
    );
    Ok(cause)
}

/// One invoice line per live item, for the items' current period.
pub(crate) fn item_lines(engine: &Engine, sub: &str) -> (Vec<LineSpec>, (i64, i64)) {
    let items = items(engine, sub);
    let lines: Vec<LineSpec> = items
        .iter()
        .map(|i| LineSpec {
            price: i.price.id.clone(),
            quantity: i.quantity.unwrap_or(1) as u64,
            subscription_item: Some(i.id.clone()),
            period: (i.current_period_start, i.current_period_end),
        })
        .collect();
    let period = lines.first().map_or((0, 0), |l| l.period);
    (lines, period)
}

/// Creates a subscription invoice and points `latest_invoice` at it.
pub(crate) fn invoice_subscription(
    engine: &mut Engine,
    ctx: &mut Ctx,
    sub: &str,
    reason: BillingReason,
    cause: &Cause,
) -> String {
    let customer = latest_sub(engine, sub).customer.id().to_string();
    let (lines, period) = item_lines(engine, sub);
    let invoice = create_draft_invoice(
        engine,
        ctx,
        &customer,
        &lines,
        reason,
        Some(sub),
        period,
        cause,
    );
    let id = invoice.clone();
    update_sub(
        engine,
        sub,
        |s| s.latest_invoice = Some(Some(Expandable::Id(id))),
        ChangeKind::Updated,
        &["customer.subscription.updated"],
        cause,
    );
    invoice
}

/// Sets a subscription's status (`customer.subscription.deleted` for terminal ones).
pub(crate) fn set_status(engine: &mut Engine, sub: &str, status: Status, cause: &Cause) {
    let now = engine.now().unix_seconds();
    let terminal = matches!(status, Status::Canceled | Status::IncompleteExpired);
    let event = if terminal {
        "customer.subscription.deleted"
    } else {
        "customer.subscription.updated"
    };
    update_sub(
        engine,
        sub,
        |s| {
            if terminal {
                s.ended_at = Some(Some(now));
                if status == Status::Canceled {
                    s.canceled_at = Some(Some(now));
                }
            }
            s.status = status;
        },
        ChangeKind::StateChanged,
        &[event],
        cause,
    );
}

/// Runs one subscription continuation.
pub(crate) fn run(c: Continuation, cause: Cause, engine: &mut Engine, ctx: &mut Ctx) {
    match c {
        Continuation::SubscriptionInvoice { sub } => {
            if sub_status(engine, &sub) != Some(Status::Incomplete) {
                return;
            }
            let invoice =
                invoice_subscription(engine, ctx, &sub, BillingReason::SubscriptionCreate, &cause);
            ctx.then(
                engine,
                Continuation::FinalizeFirstInvoice { sub, invoice },
                cause,
            );
        }
        Continuation::FinalizeFirstInvoice { sub, invoice } => {
            if sub_status(engine, &sub) != Some(Status::Incomplete) {
                delete_draft(engine, &invoice, &cause);
                return;
            }
            if finalize(engine, &invoice, &cause) {
                ctx.then(
                    engine,
                    Continuation::PayFirstInvoice { sub, invoice },
                    cause,
                );
            }
        }
        Continuation::PayFirstInvoice { sub, invoice } => {
            if sub_status(engine, &sub) != Some(Status::Incomplete) {
                return;
            }
            if attempt_payment(engine, ctx, &invoice, &cause) {
                set_status(engine, &sub, Status::Active, &cause);
                schedule_renewal(engine, ctx, &sub, &cause);
            } else {
                let at = engine.now().plus_ms(INCOMPLETE_EXPIRY_MS);
                ctx.schedule(at, Continuation::ExpireIncomplete { sub, invoice }, cause);
            }
        }
        Continuation::ExpireIncomplete { sub, invoice } => {
            if sub_status(engine, &sub) != Some(Status::Incomplete) {
                return;
            }
            void(engine, &invoice, &cause);
            set_status(engine, &sub, Status::IncompleteExpired, &cause);
        }
        Continuation::Renew { sub } => {
            if !renewable(engine, &sub) || ctx.config.renew_subscription_cycle != RenewMode::Natural
            {
                return;
            }
            let cancel = latest_sub(engine, &sub).cancel_at_period_end;
            if cancel {
                set_status(engine, &sub, Status::Canceled, &cause);
                return;
            }
            advance_periods(engine, &sub, &cause);
            let rendered = rendered_items(engine, &sub, ctx.page_size);
            update_sub(
                engine,
                &sub,
                |s| s.items = rendered,
                ChangeKind::Updated,
                &["customer.subscription.updated"],
                &cause,
            );
            let invoice =
                invoice_subscription(engine, ctx, &sub, BillingReason::SubscriptionCycle, &cause);
            schedule_renewal(engine, ctx, &sub, &cause);
            ctx.then(
                engine,
                Continuation::FinalizeRenewal { sub, invoice },
                cause,
            );
        }
        Continuation::FinalizeRenewal { sub, invoice } => {
            if !renewable(engine, &sub) {
                delete_draft(engine, &invoice, &cause);
                return;
            }
            if finalize(engine, &invoice, &cause) {
                ctx.then(engine, Continuation::PayRenewal { sub, invoice }, cause);
            }
        }
        Continuation::PayRenewal { sub, invoice } => {
            if !renewable(engine, &sub) {
                void(engine, &invoice, &cause);
                return;
            }
            if attempt_payment(engine, ctx, &invoice, &cause) {
                if sub_status(engine, &sub) == Some(Status::PastDue) {
                    set_status(engine, &sub, Status::Active, &cause);
                }
            } else {
                if sub_status(engine, &sub) == Some(Status::Active) {
                    set_status(engine, &sub, Status::PastDue, &cause);
                }
                let at = engine.now().plus_ms(DUNNING_RETRY_MS);
                ctx.schedule(at, Continuation::DunningRetry { sub, invoice }, cause);
            }
        }
        Continuation::DunningRetry { sub, invoice } => {
            let open = engine
                .latest(ObjectType::Invoice, &invoice)
                .is_some_and(|u| u.state["status"] == "open");
            if !open {
                return;
            }
            if !renewable(engine, &sub) {
                void(engine, &invoice, &cause);
                return;
            }
            if attempt_payment(engine, ctx, &invoice, &cause) {
                if sub_status(engine, &sub) == Some(Status::PastDue) {
                    set_status(engine, &sub, Status::Active, &cause);
                }
            } else {
                mark_uncollectible(engine, &invoice, &cause);
                if renewable(engine, &sub) {
                    set_status(engine, &sub, Status::Canceled, &cause);
                }
            }
        }
        other => unreachable!("not a subscription continuation: {other:?}"),
    }
}

/// How long a failed renewal waits for its one retry.
const DUNNING_RETRY_MS: i64 = 3 * 86_400_000;

/// Queues `Renew` at the end of the subscription's current period, or now when
/// that end has passed (after a quiesce or with renewals off), so missed periods
/// renew back to back (natural renewals only).
pub(super) fn schedule_renewal(engine: &Engine, ctx: &mut Ctx, sub: &str, cause: &Cause) {
    if ctx.config.renew_subscription_cycle != RenewMode::Natural {
        return;
    }
    let Some(end) = items(engine, sub).first().map(|i| i.current_period_end) else {
        return;
    };
    ctx.schedule(
        SimTime(end * 1000).max(engine.now()),
        Continuation::Renew {
            sub: sub.to_string(),
        },
        cause.clone(),
    );
}

pub(super) fn renewable(engine: &Engine, sub: &str) -> bool {
    matches!(
        sub_status(engine, sub),
        Some(Status::Active | Status::PastDue)
    )
}

/// Moves every item to its next period (unannounced), counted from the anchor.
fn advance_periods(engine: &mut Engine, sub: &str, cause: &Cause) {
    let anchor = latest_sub(engine, sub).billing_cycle_anchor;
    for item in items(engine, sub) {
        let old_end = item.current_period_end;
        let interval = interval(&item.price).unwrap_or("month");
        let mut n = 1;
        let mut new_end = add_intervals(anchor, interval, n);
        while new_end <= old_end {
            n += 1;
            new_end = add_intervals(anchor, interval, n);
        }
        engine
            .update_obj::<SubscriptionItem>(
                ObjectType::SubscriptionItem,
                &item.id,
                |i| {
                    i.current_period_start = old_end;
                    i.current_period_end = new_end;
                },
                cause.clone(),
                ChangeKind::Updated,
                &[],
            )
            .expect("live item");
    }
}

/// `cancel_subscription`: cancel the named or a random `active`/`past_due`
/// subscription, now or at period end (drawn 50/50 when `None`).
pub(crate) fn cancel_subscription(
    rng: &mut SimRng,
    engine: &mut Engine,
    id: Option<String>,
    at_period_end: Option<bool>,
) -> Result<Option<Cause>, MockError> {
    let sub = match id {
        Some(id) => {
            if engine.latest(ObjectType::Subscription, &id).is_none() {
                return Err(MockError::NoSuchObject {
                    object_type: ObjectType::Subscription,
                    id,
                });
            }
            if !renewable(engine, &id) {
                return Err(MockError::InvalidAction(format!(
                    "subscription {id} is not active or past_due"
                )));
            }
            id
        }
        None => {
            let live: Vec<&String> = engine
                .store()
                .live_ids(ObjectType::Subscription)
                .iter()
                .filter(|id| renewable(engine, id))
                .collect();
            if live.is_empty() {
                return Ok(None);
            }
            (*pick(rng, &live)).clone()
        }
    };
    let at_period_end = at_period_end.unwrap_or_else(|| rng.below(2) == 0);
    let cause = engine.begin_action("cancel_subscription");
    if at_period_end {
        let end = items(engine, &sub).first().map(|i| i.current_period_end);
        update_sub(
            engine,
            &sub,
            |s| {
                s.cancel_at_period_end = true;
                s.cancel_at = Some(end);
            },
            ChangeKind::Updated,
            &["customer.subscription.updated"],
            &cause,
        );
    } else {
        set_status(engine, &sub, Status::Canceled, &cause);
    }
    Ok(Some(cause))
}

/// Deletes a customer after canceling its live subscriptions (incomplete ones
/// expire), deleting its drafts and voiding its open invoices, deleting its pending invoice
/// items and detaching its payment methods, all under `cause`.
pub(crate) fn delete_customer_cascade(
    engine: &mut Engine,
    id: &str,
    cause: &Cause,
) -> Result<(), MockError> {
    match engine.latest(ObjectType::Customer, id) {
        None => {
            return Err(MockError::NoSuchObject {
                object_type: ObjectType::Customer,
                id: id.to_string(),
            });
        }
        Some(u) if u.kind == ChangeKind::Deleted => {
            return Err(MockError::Deleted {
                object_type: ObjectType::Customer,
                id: id.to_string(),
            });
        }
        Some(_) => {}
    }
    let subs: Vec<String> = engine
        .store()
        .children(ObjectType::Subscription, "customer", id)
        .into_iter()
        .map(|o| o.latest.object_id.clone())
        .collect();
    for sub in subs {
        match sub_status(engine, &sub) {
            Some(Status::Active | Status::PastDue) => {
                set_status(engine, &sub, Status::Canceled, cause)
            }
            Some(Status::Incomplete) => set_status(engine, &sub, Status::IncompleteExpired, cause),
            _ => {}
        }
    }
    // Pending drafts are deleted and open invoices voided, so no later step
    // finalizes or pays an invoice of the deleted customer.
    let unsettled: Vec<(String, bool)> = engine
        .store()
        .children(ObjectType::Invoice, "customer", id)
        .into_iter()
        .filter(|o| !o.deleted)
        .filter_map(|o| match o.latest.state["status"].as_str() {
            Some("draft") => Some((o.latest.object_id.clone(), true)),
            Some("open") => Some((o.latest.object_id.clone(), false)),
            _ => None,
        })
        .collect();
    for (invoice, draft) in unsettled {
        if draft {
            delete_draft(engine, &invoice, cause);
        } else {
            void(engine, &invoice, cause);
        }
    }
    // Its pending invoice items go too (phase 9 ruling Q4); items on its drafts went with
    // them above.
    super::billing::delete_pending_items(engine, id, cause);
    // Its cards are detached, as the schema's `payment_method.customer` cannot name a
    // deleted customer.
    super::payments::detach_all(engine, id, cause);
    engine.delete(
        ObjectType::Customer,
        id,
        &deleted_stub(id),
        cause.clone(),
        &["customer.deleted"],
    )?;
    Ok(())
}

/// `churn.subscription.update`: a metadata change on a random subscription that has not ended.
pub(crate) fn churn_update(rng: &mut SimRng, engine: &mut Engine) {
    let live: Vec<&StoredObject> = engine
        .store()
        .in_creation_order(ObjectType::Subscription)
        .into_iter()
        .filter(|o| {
            !matches!(
                o.latest.state["status"].as_str(),
                Some("canceled" | "incomplete_expired")
            )
        })
        .collect();
    if live.is_empty() {
        return;
    }
    let sub = pick(rng, &live).latest.object_id.clone();
    let value = random_metadata_value(rng);
    let cause = engine.begin_action("subscription_update");
    update_sub(
        engine,
        &sub,
        |s| {
            s.metadata.insert("note".to_string(), value);
        },
        ChangeKind::Updated,
        &["customer.subscription.updated"],
        &cause,
    );
}
