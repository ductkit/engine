//! Checkout sessions (spec sections 4 and 5.5): the `checkout` action, which opens a
//! session that later completes (a payment or a new subscription) or expires.

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::payments::{charge_once, draw_amount, named_or_random_customer};
use crate::generate::subscriptions::{
    SubscriptionDraw, active_recurring_prices, draw_subscription, pick_prices, start_paid_now,
};
use crate::generate::{Continuation, Ctx};
use crate::model::checkout::{Completion, NewSession, complete, expire, new_checkout_session};
use crate::rng::SimRng;
use crate::stripe::{CheckoutSession, Customer};
use crate::{MockError, ObjectType, SimTime};

/// What a Checkout session sells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutMode {
    /// A one-time payment, through `charge_once`.
    Payment,
    /// A new subscription, started and paid at completion. Needs an active recurring price;
    /// without one the session falls back to `payment`.
    Subscription,
}

/// Chance that a session with no given outcome completes rather than expires.
const COMPLETE_PROBABILITY: f64 = 0.7;
/// Chance that a session with no given mode is in subscription mode.
const SUBSCRIPTION_PROBABILITY: f64 = 0.3;
const MINUTE_MS: i64 = 60_000;
/// Opening to completion, for a session that completes.
const COMPLETE_AFTER_MS: (i64, i64) = (MINUTE_MS, 30 * MINUTE_MS);

fn amount_of(engine: &Engine, draw: &SubscriptionDraw) -> (i64, String) {
    let mut currency = String::new();
    let mut amount = 0;
    for (price, _, quantity) in &draw.items {
        let p = &engine
            .latest(ObjectType::Price, price)
            .expect("price")
            .state;
        currency = p["currency"].as_str().unwrap_or_default().to_string();
        amount += p["unit_amount"].as_i64().unwrap_or(0) * *quantity as i64;
    }
    (amount, currency)
}

/// `checkout`: opens a session for the named or a random live customer, `open` with
/// `expires_at` 24 hours ahead (no event), then completes it after 1-30 minutes or lets it
/// expire. Every draw happens now, from `rng`: the target, whether it completes (0.7 when
/// `None`), the mode (subscription 0.3 when `None`; without an active recurring price it
/// falls back to payment), the amount (payment mode, drawn like a one-off payment's) or the
/// subscription's prices, IDs and quantities, and the completion delay. Returns `None` when
/// no customer was named and none is live.
pub(crate) fn checkout_action(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<String>,
    mode: Option<CheckoutMode>,
    complete: Option<bool>,
) -> Result<Option<Cause>, MockError> {
    let Some(customer) = named_or_random_customer(rng, engine, customer)? else {
        return Ok(None);
    };
    let completes = complete.unwrap_or_else(|| rng.chance(COMPLETE_PROBABILITY));
    let mode = mode.unwrap_or_else(|| {
        if rng.chance(SUBSCRIPTION_PROBABILITY) {
            CheckoutMode::Subscription
        } else {
            CheckoutMode::Payment
        }
    });
    let plan = match mode {
        CheckoutMode::Subscription => {
            let recurring = active_recurring_prices(engine);
            (!recurring.is_empty()).then(|| {
                let prices = pick_prices(rng, engine, recurring);
                draw_subscription(rng, prices)
            })
        }
        CheckoutMode::Payment => None,
    };
    let (amount, currency) = match &plan {
        Some(draw) => amount_of(engine, draw),
        None => draw_amount(rng, engine),
    };
    let delay = rng.uniform_ms(COMPLETE_AFTER_MS.0, COMPLETE_AFTER_MS.1);
    let cause = engine.begin_action("checkout");
    let id = ctx.payments.id(ObjectType::CheckoutSession.id_prefix());
    let session = new_checkout_session(&NewSession {
        id: &id,
        created: engine.now().unix_seconds(),
        customer: &customer,
        subscription: plan.is_some(),
        amount,
        currency: &currency,
    });
    engine.create_obj(
        ObjectType::CheckoutSession,
        id.clone(),
        &session,
        cause.clone(),
        &[],
    );
    if completes {
        let at = engine.now().plus_ms(delay);
        ctx.schedule(
            at,
            Continuation::CheckoutComplete { id, plan },
            cause.clone(),
        );
    } else {
        let at = SimTime(session.expires_at * 1000);
        ctx.schedule(at, Continuation::CheckoutExpire { id }, cause.clone());
    }
    Ok(Some(cause))
}

fn open_session(engine: &Engine, id: &str) -> Option<CheckoutSession> {
    let s: CheckoutSession = engine.latest_obj(ObjectType::CheckoutSession, id)?;
    (s.status == Some(Some(crate::stripe::CheckoutSessionStatus::Open))).then_some(s)
}

/// Completes the open session `id`: in payment mode a succeeding payment for its amount
/// (see [`charge_once`]); in subscription mode the subscription of `plan`, started and its
/// first invoice paid. Then the session is `complete` and `paid` with its links
/// (`checkout.session.completed`). If its customer was deleted meanwhile, nothing is
/// bought and the session waits to expire.
pub(crate) fn run_complete(
    engine: &mut Engine,
    ctx: &mut Ctx,
    id: &str,
    plan: Option<SubscriptionDraw>,
    cause: &Cause,
) {
    let Some(session) = open_session(engine, id) else {
        return;
    };
    let customer_id = session
        .customer
        .clone()
        .flatten()
        .map(|c| c.id().to_string())
        .expect("sessions have a customer");
    let customer: Option<Customer> = engine
        .latest(ObjectType::Customer, &customer_id)
        .filter(|u| u.kind != ChangeKind::Deleted)
        .and_then(|_| engine.latest_obj(ObjectType::Customer, &customer_id));
    let Some(customer) = customer else {
        let at = SimTime(session.expires_at * 1000).max(engine.now());
        ctx.schedule(
            at,
            Continuation::CheckoutExpire { id: id.to_string() },
            cause.clone(),
        );
        return;
    };
    let (sub, invoice, pi);
    let done = match &plan {
        Some(draw) => {
            invoice = start_paid_now(engine, ctx, &customer_id, draw, cause);
            sub = draw.id.clone();
            Completion::Subscription {
                subscription: &sub,
                invoice: &invoice,
            }
        }
        None => {
            let amount = session.amount_total.flatten().unwrap_or(0);
            let currency = session.currency.clone().flatten().unwrap_or_default();
            pi = charge_once(
                engine,
                ctx,
                Some(&customer_id),
                amount,
                &currency,
                true,
                cause,
            )
            .payment_intent;
            Completion::Payment {
                payment_intent: &pi,
            }
        }
    };
    engine
        .update_obj::<CheckoutSession>(
            ObjectType::CheckoutSession,
            id,
            |s| complete(s, &customer, &done),
            cause.clone(),
            ChangeKind::StateChanged,
            &["checkout.session.completed"],
        )
        .expect("live session");
}

/// Expires the session `id` if it is still open (`checkout.session.expired`).
pub(crate) fn run_expire(engine: &mut Engine, id: &str, cause: &Cause) {
    if open_session(engine, id).is_none() {
        return;
    }
    engine
        .update_obj::<CheckoutSession>(
            ObjectType::CheckoutSession,
            id,
            expire,
            cause.clone(),
            ChangeKind::StateChanged,
            &["checkout.session.expired"],
        )
        .expect("live session");
}
