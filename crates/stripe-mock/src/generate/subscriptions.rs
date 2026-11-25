//! Subscription lifecycle (spec sections 5.2, 6.1): start, first invoice,
//! activation and expiry of unpaid starts.

use serde_json::{Value, json};

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::billing::{
    attempt_payment, create_draft_invoice, finalize, resolve_customer, void,
};
use crate::generate::catalog::{active_ids, create_product_now, draw_new_product};
use crate::generate::{Continuation, Ctx, pick};
use crate::model::embedded::embedded_list;
use crate::model::invoice::{BillingReason, LineSpec};
use crate::model::subscription::{
    NewItem, NewSubscription, item_json, items_url, period_end, subscription_json,
};
use crate::rng::SimRng;
use crate::{MockError, ObjectType};

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
#[allow(dead_code)] // TEMP: used from Task 6 (renewals)
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
        other => unreachable!("not a subscription continuation: {other:?}"),
    }
}
