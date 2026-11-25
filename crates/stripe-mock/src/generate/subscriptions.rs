//! Subscription lifecycle (spec sections 5.2, 6.1): start, first invoice,
//! activation and expiry of unpaid starts.

use serde_json::{Value, json};

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::billing::{
    attempt_payment, create_draft_invoice, finalize, mark_uncollectible, resolve_customer, void,
};
use crate::generate::catalog::{active_ids, create_product_now, draw_new_product};
use crate::generate::{Continuation, Ctx, pick};
use crate::model::customer::{deleted_stub, random_metadata_value};
use crate::model::embedded::embedded_list;
use crate::model::invoice::{BillingReason, LineSpec};
use crate::model::subscription::{
    NewItem, NewSubscription, add_intervals, item_json, items_url, period_end, subscription_json,
};
use crate::rng::SimRng;
use crate::{MockError, ObjectType, RenewMode, SimTime};

/// How long an unpaid first invoice keeps a subscription `incomplete`.
const INCOMPLETE_EXPIRY_MS: i64 = 23 * 3_600_000;

pub(crate) fn sub_status(engine: &Engine, id: &str) -> Option<String> {
    engine
        .latest(ObjectType::Subscription, id)
        .and_then(|u| u.state["status"].as_str().map(str::to_string))
}

/// Applies `f` to a subscription.
pub(crate) fn update_sub(
    engine: &mut Engine,
    id: &str,
    f: impl FnOnce(&mut Value),
    kind: ChangeKind,
    events: &[&str],
    cause: &Cause,
) {
    engine
        .update(ObjectType::Subscription, id, f, cause.clone(), kind, events)
        .expect("subscriptions are never hard-deleted");
}

/// Live items of a subscription, in creation order.
pub(crate) fn items(engine: &Engine, sub: &str) -> Vec<Value> {
    engine
        .store()
        .in_creation_order(ObjectType::SubscriptionItem)
        .into_iter()
        .filter(|o| !o.deleted && o.latest.state["subscription"] == json!(sub))
        .map(|o| o.latest.state.clone())
        .collect()
}

/// The subscription's `items` list rendered from its item objects.
pub(crate) fn rendered_items(engine: &Engine, sub: &str, page_size: usize) -> Value {
    embedded_list(&items(engine, sub), page_size, &items_url(sub))
}

fn price_state(engine: &Engine, id: &str) -> Option<Value> {
    engine
        .latest(ObjectType::Price, id)
        .map(|u| u.state.clone())
}

fn interval(price: &Value) -> Option<&str> {
    price["recurring"]["interval"].as_str()
}

/// Validates named prices: active, recurring, one currency and interval.
fn check_prices(engine: &Engine, ids: &[String]) -> Result<(), MockError> {
    let mut key = None;
    for id in ids {
        let p = price_state(engine, id).ok_or_else(|| MockError::NoSuchObject {
            object_type: ObjectType::Price,
            id: id.clone(),
        })?;
        if p["active"] != json!(true) || interval(&p).is_none() {
            return Err(MockError::InvalidAction(format!(
                "price {id} must be active and recurring"
            )));
        }
        let k = (p["currency"].clone(), p["recurring"]["interval"].clone());
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

/// 1-3 prices (or 11-15 with probability 0.05) sharing one currency and interval,
/// distinct while the pool lasts.
fn choose_prices(rng: &mut SimRng, engine: &mut Engine, cause: &Cause) -> Vec<String> {
    let recurring: Vec<String> = active_ids(engine, ObjectType::Price)
        .into_iter()
        .filter(|id| price_state(engine, id).is_some_and(|p| interval(&p).is_some()))
        .collect();
    let recurring = if recurring.is_empty() {
        let np = draw_new_product(rng);
        create_product_now(engine, &np, cause)
            .into_iter()
            .filter(|id| price_state(engine, id).is_some_and(|p| interval(&p).is_some()))
            .collect()
    } else {
        recurring
    };
    let first = price_state(engine, pick(rng, &recurring)).expect("price");
    let mut pool: Vec<String> = recurring
        .into_iter()
        .filter(|id| {
            price_state(engine, id).is_some_and(|p| {
                p["currency"] == first["currency"] && interval(&p) == interval(&first)
            })
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
    let sub_id = rng.id(ObjectType::Subscription.id_prefix());
    let now = engine.now().unix_seconds();
    let mut item_states = Vec::new();
    let mut currency = Value::Null;
    for price_id in &prices {
        let price = price_state(engine, price_id).expect("checked price");
        currency = price["currency"].clone();
        let id = rng.id(ObjectType::SubscriptionItem.id_prefix());
        let quantity = 1 + rng.below(5);
        let end = period_end(now, interval(&price).expect("recurring"));
        let state = item_json(&NewItem {
            id: &id,
            subscription: &sub_id,
            price: &price,
            quantity,
            created: now,
            period: (now, end),
        });
        engine.create(
            ObjectType::SubscriptionItem,
            id,
            state.clone(),
            cause.clone(),
            &[],
        );
        item_states.push(state);
    }
    let state = subscription_json(&NewSubscription {
        id: &sub_id,
        customer: &customer,
        currency: &currency,
        items: &item_states,
        created: now,
        page_size: ctx.page_size,
    });
    engine.create(
        ObjectType::Subscription,
        sub_id.clone(),
        state,
        cause.clone(),
        &["customer.subscription.created"],
    );
    ctx.then(
        engine,
        Continuation::SubscriptionInvoice { sub: sub_id },
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
            price: i["price"]["id"].as_str().expect("item price").to_string(),
            quantity: i["quantity"].as_u64().unwrap_or(1),
            subscription_item: i["id"].as_str().map(str::to_string),
            period: (
                i["current_period_start"].as_i64().unwrap_or(0),
                i["current_period_end"].as_i64().unwrap_or(0),
            ),
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
    let customer = engine
        .latest(ObjectType::Subscription, sub)
        .expect("sub")
        .state["customer"]
        .as_str()
        .expect("customer id")
        .to_string();
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
        |v| v["latest_invoice"] = json!(id),
        ChangeKind::Updated,
        &["customer.subscription.updated"],
        cause,
    );
    invoice
}

/// Sets a subscription's status (`customer.subscription.deleted` for terminal ones).
pub(crate) fn set_status(engine: &mut Engine, sub: &str, status: &str, cause: &Cause) {
    let now = engine.now().unix_seconds();
    let terminal = matches!(status, "canceled" | "incomplete_expired");
    let event = if terminal {
        "customer.subscription.deleted"
    } else {
        "customer.subscription.updated"
    };
    let status = status.to_string();
    update_sub(
        engine,
        sub,
        |v| {
            if terminal {
                v["ended_at"] = json!(now);
                if status == "canceled" {
                    v["canceled_at"] = json!(now);
                }
            }
            v["status"] = json!(status);
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
            if sub_status(engine, &sub).as_deref() != Some("incomplete") {
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
            if finalize(engine, &invoice, &cause) {
                ctx.then(
                    engine,
                    Continuation::PayFirstInvoice { sub, invoice },
                    cause,
                );
            }
        }
        Continuation::PayFirstInvoice { sub, invoice } => {
            if sub_status(engine, &sub).as_deref() != Some("incomplete") {
                return;
            }
            if attempt_payment(engine, ctx, &invoice, &cause) {
                set_status(engine, &sub, "active", &cause);
                schedule_renewal(engine, ctx, &sub, &cause);
            } else {
                let at = engine.now().plus_ms(INCOMPLETE_EXPIRY_MS);
                ctx.schedule(at, Continuation::ExpireIncomplete { sub, invoice }, cause);
            }
        }
        Continuation::ExpireIncomplete { sub, invoice } => {
            if sub_status(engine, &sub).as_deref() != Some("incomplete") {
                return;
            }
            void(engine, &invoice, &cause);
            set_status(engine, &sub, "incomplete_expired", &cause);
        }
        Continuation::Renew { sub } => {
            if !renewable(engine, &sub) || ctx.config.renew_subscription_cycle != RenewMode::Natural
            {
                return;
            }
            let cancel = engine
                .latest(ObjectType::Subscription, &sub)
                .expect("sub")
                .state["cancel_at_period_end"]
                == json!(true);
            if cancel {
                set_status(engine, &sub, "canceled", &cause);
                return;
            }
            advance_periods(engine, &sub, &cause);
            let rendered = rendered_items(engine, &sub, ctx.page_size);
            update_sub(
                engine,
                &sub,
                |v| v["items"] = rendered,
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
            if finalize(engine, &invoice, &cause) {
                ctx.then(engine, Continuation::PayRenewal { sub, invoice }, cause);
            }
        }
        Continuation::PayRenewal { sub, invoice } => {
            if attempt_payment(engine, ctx, &invoice, &cause) {
                if sub_status(engine, &sub).as_deref() == Some("past_due") {
                    set_status(engine, &sub, "active", &cause);
                }
            } else {
                if sub_status(engine, &sub).as_deref() == Some("active") {
                    set_status(engine, &sub, "past_due", &cause);
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
            if attempt_payment(engine, ctx, &invoice, &cause) {
                if sub_status(engine, &sub).as_deref() == Some("past_due") {
                    set_status(engine, &sub, "active", &cause);
                }
            } else {
                mark_uncollectible(engine, &invoice, &cause);
                if renewable(engine, &sub) {
                    set_status(engine, &sub, "canceled", &cause);
                }
            }
        }
        other => unreachable!("not a subscription continuation: {other:?}"),
    }
}

/// How long a failed renewal waits for its one retry.
const DUNNING_RETRY_MS: i64 = 3 * 86_400_000;

/// Queues `Renew` at the end of the subscription's current period (natural renewals only).
fn schedule_renewal(engine: &Engine, ctx: &mut Ctx, sub: &str, cause: &Cause) {
    if ctx.config.renew_subscription_cycle != RenewMode::Natural {
        return;
    }
    let Some(end) = items(engine, sub)
        .first()
        .and_then(|i| i["current_period_end"].as_i64())
    else {
        return;
    };
    ctx.schedule(
        SimTime(end * 1000),
        Continuation::Renew {
            sub: sub.to_string(),
        },
        cause.clone(),
    );
}

fn renewable(engine: &Engine, sub: &str) -> bool {
    matches!(
        sub_status(engine, sub).as_deref(),
        Some("active" | "past_due")
    )
}

/// Moves every item to its next period (unannounced), counted from the anchor.
fn advance_periods(engine: &mut Engine, sub: &str, cause: &Cause) {
    let anchor = engine
        .latest(ObjectType::Subscription, sub)
        .expect("sub")
        .state["billing_cycle_anchor"]
        .as_i64()
        .unwrap_or(0);
    for item in items(engine, sub) {
        let old_end = item["current_period_end"].as_i64().unwrap_or(anchor);
        let interval = item["price"]["recurring"]["interval"]
            .as_str()
            .unwrap_or("month")
            .to_string();
        let mut n = 1;
        let mut new_end = add_intervals(anchor, &interval, n);
        while new_end <= old_end {
            n += 1;
            new_end = add_intervals(anchor, &interval, n);
        }
        let id = item["id"].as_str().expect("item id").to_string();
        engine
            .update(
                ObjectType::SubscriptionItem,
                &id,
                |v| {
                    v["current_period_start"] = json!(old_end);
                    v["current_period_end"] = json!(new_end);
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
            let live: Vec<String> = engine
                .store()
                .in_creation_order(ObjectType::Subscription)
                .into_iter()
                .map(|o| o.latest.object_id.clone())
                .filter(|id| renewable(engine, id))
                .collect();
            if live.is_empty() {
                return Ok(None);
            }
            pick(rng, &live).clone()
        }
    };
    let at_period_end = at_period_end.unwrap_or_else(|| rng.below(2) == 0);
    let cause = engine.begin_action("cancel_subscription");
    if at_period_end {
        let end = items(engine, &sub)
            .first()
            .and_then(|i| i["current_period_end"].as_i64());
        update_sub(
            engine,
            &sub,
            |v| {
                v["cancel_at_period_end"] = json!(true);
                v["cancel_at"] = json!(end);
            },
            ChangeKind::Updated,
            &["customer.subscription.updated"],
            &cause,
        );
    } else {
        set_status(engine, &sub, "canceled", &cause);
    }
    Ok(Some(cause))
}

/// Deletes a customer after canceling its live subscriptions (incomplete ones
/// expire) and voiding its open invoices, all under `cause`.
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
        .in_creation_order(ObjectType::Subscription)
        .into_iter()
        .filter(|o| o.latest.state["customer"] == json!(id))
        .map(|o| o.latest.object_id.clone())
        .collect();
    for sub in subs {
        match sub_status(engine, &sub).as_deref() {
            Some("active" | "past_due") => set_status(engine, &sub, "canceled", cause),
            Some("incomplete") => set_status(engine, &sub, "incomplete_expired", cause),
            _ => {}
        }
    }
    let open: Vec<String> = engine
        .store()
        .in_creation_order(ObjectType::Invoice)
        .into_iter()
        .filter(|o| {
            !o.deleted
                && o.latest.state["customer"] == json!(id)
                && o.latest.state["status"] == "open"
        })
        .map(|o| o.latest.object_id.clone())
        .collect();
    for invoice in open {
        void(engine, &invoice, cause);
    }
    engine.delete(
        ObjectType::Customer,
        id,
        deleted_stub(id),
        cause.clone(),
        &["customer.deleted"],
    )?;
    Ok(())
}

/// `churn.subscription.update`: a metadata change on a random subscription that has not ended.
pub(crate) fn churn_update(rng: &mut SimRng, engine: &mut Engine) {
    let live: Vec<String> = engine
        .store()
        .in_creation_order(ObjectType::Subscription)
        .into_iter()
        .filter(|o| {
            !matches!(
                o.latest.state["status"].as_str(),
                Some("canceled" | "incomplete_expired")
            )
        })
        .map(|o| o.latest.object_id.clone())
        .collect();
    if live.is_empty() {
        return;
    }
    let sub = pick(rng, &live).clone();
    let value = random_metadata_value(rng);
    let cause = engine.begin_action("subscription_update");
    update_sub(
        engine,
        &sub,
        |v| v["metadata"]["note"] = json!(value),
        ChangeKind::Updated,
        &["customer.subscription.updated"],
        &cause,
    );
}
