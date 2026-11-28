//! Discounts (phase 9 rulings Q2 and Q7): the `discount_change` action, the `AddDiscount` and
//! `RemoveDiscount` control hooks, and the discount an invoice applies.
//!
//! A discount has no endpoint: it is embedded in its customer (`discount`, the full object)
//! or its subscription (`discounts`, IDs). A parent has at most one.

use serde_json::Value;

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::billing_catalog::{active_promotion_codes, new_coupon, valid_coupons};
use crate::generate::subscriptions::{sub_status, update_sub};
use crate::generate::{Continuation, Ctx, pick};
use crate::model::amounts::AppliedDiscount;
use crate::model::discounts::{NewDiscount, deleted_discount, discount};
use crate::model::subscription::add_intervals;
use crate::rng::SimRng;
use crate::stripe::{
    Coupon, CouponDuration, Customer, Discount, Expandable, PromotionCode,
    SubscriptionStatus as Status,
};
use crate::{MockError, ObjectType, SimTime};

/// What a new discount is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Customer(String),
    Subscription(String),
}

/// What a new discount redeems.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Redeem {
    Coupon(String),
    PromotionCode(String),
}

fn is_live(engine: &Engine, t: ObjectType, id: &str) -> bool {
    engine
        .latest(t, id)
        .is_some_and(|u| u.kind != ChangeKind::Deleted)
}

/// The coupon's last state before it was deleted, or its current one: a deleted coupon's
/// discounts outlive it and still apply its terms (ruling Q6).
fn coupon_terms(engine: &Engine, id: &str) -> Option<Coupon> {
    let stored = engine.store().get(ObjectType::Coupon, id)?;
    let log = engine.log();
    let state = stored
        .versions
        .iter()
        .rev()
        .map(|&pos| &log[pos])
        .find(|u| u.kind != ChangeKind::Deleted)?;
    serde_json::from_value(state.state.clone()).ok()
}

/// The coupon a discount's state (live or tombstone) redeems.
fn coupon_of(state: &Value) -> Option<&str> {
    state["source"]["coupon"].as_str()
}

/// The terms of the discount `di` on an invoice in `currency`, or `None` when it applies
/// nothing there: an `amount_off` coupon in another currency (ruling Q3). Reads a removed
/// discount too, so a draft keeps the discount it was created with.
pub(crate) fn applied_by_id(engine: &Engine, di: &str, currency: &str) -> Option<AppliedDiscount> {
    let state = &engine.latest(ObjectType::Discount, di)?.state;
    let coupon = coupon_terms(engine, coupon_of(state)?)?;
    let percent_bp = coupon
        .percent_off
        .flatten()
        .map(|p| (p * 100.0).round() as i64);
    let amount_off = coupon
        .amount_off
        .flatten()
        .filter(|_| coupon.currency.flatten().as_deref() == Some(currency));
    if percent_bp.is_none() && amount_off.is_none() {
        return None;
    }
    Some(AppliedDiscount {
        id: di.to_string(),
        percent_bp,
        amount_off,
    })
}

/// The live discount of a customer.
fn customer_discount(engine: &Engine, customer: &str) -> Option<String> {
    let u = engine.latest(ObjectType::Customer, customer)?;
    if u.kind == ChangeKind::Deleted {
        return None;
    }
    u.state["discount"]["id"].as_str().map(str::to_string)
}

/// The live discount of a subscription.
fn subscription_discount(engine: &Engine, sub: &str) -> Option<String> {
    engine.latest(ObjectType::Subscription, sub)?.state["discounts"][0]
        .as_str()
        .map(str::to_string)
}

/// The discount a new invoice of `customer` in `currency` applies (ruling Q2): the
/// subscription's for a subscription invoice, or else the customer's. `None` when there is
/// none, or when it applies nothing in `currency` (ruling Q3).
pub(crate) fn applied_discount(
    engine: &Engine,
    customer: &str,
    subscription: Option<&str>,
    currency: &str,
) -> Option<AppliedDiscount> {
    let di = subscription
        .and_then(|s| subscription_discount(engine, s))
        .or_else(|| customer_discount(engine, customer))?;
    applied_by_id(engine, &di, currency)
}

/// Removes the live discount `di` (ruling Q2): the parent first (silent for a customer;
/// `customer.subscription.updated` for a subscription), then the discount
/// (`customer.discount.deleted`, tombstone `deleted_discount`).
fn remove(engine: &mut Engine, di: &str, cause: &Cause) {
    let d: Discount = engine
        .latest_obj(ObjectType::Discount, di)
        .expect("removing a live discount");
    match d.subscription.clone().flatten() {
        Some(sub) => {
            if subscription_discount(engine, &sub).as_deref() == Some(di) {
                update_sub(
                    engine,
                    &sub,
                    |s| s.discounts.retain(|x| x.id() != di),
                    ChangeKind::Updated,
                    &["customer.subscription.updated"],
                    cause,
                );
            }
        }
        None => {
            let customer = d
                .customer
                .as_ref()
                .and_then(|c| c.as_ref())
                .map(|c| c.id().to_string())
                .expect("a discount names its customer");
            if customer_discount(engine, &customer).as_deref() == Some(di) {
                engine
                    .update_obj::<Customer>(
                        ObjectType::Customer,
                        &customer,
                        |c| c.discount = Some(None),
                        cause.clone(),
                        ChangeKind::Updated,
                        &[],
                    )
                    .expect("live customer");
            }
        }
    }
    engine
        .delete(
            ObjectType::Discount,
            di,
            &deleted_discount(&d),
            cause.clone(),
            &["customer.discount.deleted"],
        )
        .expect("live discount");
}

/// At the finalization of `invoice`: a `once` discount it carries is removed in the same
/// instant (ruling Q2), whatever happens to the invoice later. Already removed: nothing.
pub(crate) fn end_once(engine: &mut Engine, invoice: &str, cause: &Cause) {
    let Some(di) = engine
        .latest(ObjectType::Invoice, invoice)
        .and_then(|u| u.state["discounts"][0].as_str().map(str::to_string))
    else {
        return;
    };
    if !is_live(engine, ObjectType::Discount, &di) {
        return;
    }
    let once = engine
        .latest(ObjectType::Discount, &di)
        .and_then(|u| coupon_of(&u.state).and_then(|c| coupon_terms(engine, c)))
        .is_some_and(|c| c.duration == CouponDuration::Once);
    if once {
        remove(engine, &di, cause);
    }
}

/// Removes the customer's discount, if any: the first step of deleting a customer (ruling
/// Q2). Its subscriptions' discounts stay.
pub(crate) fn remove_customer_discount(engine: &mut Engine, customer: &str, cause: &Cause) {
    if let Some(di) = customer_discount(engine, customer) {
        remove(engine, &di, cause);
    }
}

/// `DiscountEnd`: a `repeating` discount ends at its `end`, unless it was removed earlier.
pub(crate) fn run_end(engine: &mut Engine, di: &str, cause: &Cause) {
    if is_live(engine, ObjectType::Discount, di) {
        remove(engine, di, cause);
    }
}

/// Creates a discount on `target` redeeming `redeem`, all under `cause` (ruling Q2): the
/// coupon's `times_redeemed` +1 (silent; `valid: false` once `max_redemptions` is reached),
/// the promotion code's +1 (silent), the discount (`customer.discount.created`), then the
/// parent (silent for a customer; `customer.subscription.updated`). A `repeating` discount
/// queues its [`Continuation::DiscountEnd`].
fn create(
    engine: &mut Engine,
    ctx: &mut Ctx,
    id: &str,
    target: &Target,
    redeem: &Redeem,
    cause: &Cause,
) {
    let (coupon, promo) = match redeem {
        Redeem::Coupon(c) => (c.clone(), None),
        Redeem::PromotionCode(p) => {
            let code: PromotionCode = engine
                .latest_obj(ObjectType::PromotionCode, p)
                .expect("checked promotion code");
            let coupon = code
                .promotion
                .coupon
                .flatten()
                .expect("generated codes name a coupon")
                .id()
                .to_string();
            (coupon, Some(p.clone()))
        }
    };
    engine
        .update_obj::<Coupon>(
            ObjectType::Coupon,
            &coupon,
            |c| {
                c.times_redeemed += 1;
                if c.max_redemptions
                    .flatten()
                    .is_some_and(|max| c.times_redeemed >= max)
                {
                    c.valid = false;
                }
            },
            cause.clone(),
            ChangeKind::Updated,
            &[],
        )
        .expect("checked coupon");
    let terms: Coupon = engine
        .latest_obj(ObjectType::Coupon, &coupon)
        .expect("checked coupon");
    if let Some(p) = &promo {
        engine
            .update_obj::<PromotionCode>(
                ObjectType::PromotionCode,
                p,
                |c| c.times_redeemed += 1,
                cause.clone(),
                ChangeKind::Updated,
                &[],
            )
            .expect("checked promotion code");
    }
    let start = engine.now().unix_seconds();
    let end = match terms.duration {
        CouponDuration::Repeating => {
            let months = terms
                .duration_in_months
                .flatten()
                .expect("repeating coupons have a duration");
            Some(add_intervals(start, "month", months as u32))
        }
        CouponDuration::Once | CouponDuration::Forever => None,
    };
    let (customer, subscription) = match target {
        Target::Customer(c) => (c.clone(), None),
        Target::Subscription(s) => {
            let customer = engine
                .latest(ObjectType::Subscription, s)
                .expect("checked")
                .state["customer"]
                .as_str()
                .expect("a subscription names its customer")
                .to_string();
            (customer, Some(s.as_str()))
        }
    };
    let d = discount(&NewDiscount {
        id,
        coupon: &coupon,
        promotion_code: promo.as_deref(),
        customer: &customer,
        subscription,
        start,
        end,
    });
    engine.create_obj(
        ObjectType::Discount,
        id.to_string(),
        &d,
        cause.clone(),
        &["customer.discount.created"],
    );
    match target {
        Target::Customer(c) => {
            engine
                .update_obj::<Customer>(
                    ObjectType::Customer,
                    c,
                    |c| c.discount = Some(Some(d)),
                    cause.clone(),
                    ChangeKind::Updated,
                    &[],
                )
                .expect("checked customer");
        }
        Target::Subscription(s) => {
            let di = id.to_string();
            update_sub(
                engine,
                s,
                |s| s.discounts = vec![Expandable::Id(di)],
                ChangeKind::Updated,
                &["customer.subscription.updated"],
                cause,
            );
        }
    }
    if let Some(end) = end {
        ctx.schedule(
            SimTime(end * 1000),
            Continuation::DiscountEnd { id: id.to_string() },
            cause.clone(),
        );
    }
}

/// Live customers with no discount, in creation order.
fn undiscounted_customers(engine: &Engine) -> Vec<String> {
    engine
        .store()
        .live_ids(ObjectType::Customer)
        .iter()
        .filter(|c| customer_discount(engine, c).is_none())
        .cloned()
        .collect()
}

/// `active`/`past_due` subscriptions with no discount, in creation order.
fn undiscounted_subscriptions(engine: &Engine) -> Vec<String> {
    engine
        .store()
        .live_ids(ObjectType::Subscription)
        .iter()
        .filter(|s| {
            matches!(
                sub_status(engine, s),
                Some(Status::Active | Status::PastDue)
            ) && subscription_discount(engine, s).is_none()
        })
        .cloned()
        .collect()
}

/// Draws a target (ruling Q7): 50/50 a customer or a subscription with no discount, the
/// other kind when the drawn one has no candidate; `None` when neither has one.
fn draw_target(rng: &mut SimRng, engine: &Engine) -> Option<Target> {
    let customers = undiscounted_customers(engine);
    let subs = undiscounted_subscriptions(engine);
    let want_customer = rng.below(2) == 0;
    let use_customer = match (customers.is_empty(), subs.is_empty()) {
        (true, true) => return None,
        (false, true) => true,
        (true, false) => false,
        (false, false) => want_customer,
    };
    Some(if use_customer {
        Target::Customer(pick(rng, &customers).clone())
    } else {
        Target::Subscription(pick(rng, &subs).clone())
    })
}

/// Draws what to redeem (ruling Q7): with probability 0.5 an active promotion code whose
/// coupon is valid, otherwise a valid coupon; with neither, a new coupon (drawn as in Q6).
fn draw_redeem(rng: &mut SimRng, engine: &mut Engine, cause: &Cause) -> Redeem {
    let codes = active_promotion_codes(engine);
    if rng.chance(0.5) && !codes.is_empty() {
        return Redeem::PromotionCode(pick(rng, &codes).clone());
    }
    let coupons = valid_coupons(engine);
    if coupons.is_empty() {
        return Redeem::Coupon(new_coupon(rng, engine, cause));
    }
    Redeem::Coupon(pick(rng, &coupons).clone())
}

/// `discount_change` (ruling Q7): removes a random live discount with probability 0.3;
/// otherwise, and whenever none exists, adds one to a drawn target. With no target,
/// nothing happens.
pub(crate) fn discount_change(rng: &mut SimRng, engine: &mut Engine, ctx: &mut Ctx) {
    let live = engine.store().live_ids(ObjectType::Discount).to_vec();
    if !live.is_empty() && rng.chance(0.3) {
        let di = pick(rng, &live).clone();
        let cause = engine.begin_action("discount_change");
        remove(engine, &di, &cause);
        return;
    }
    let Some(target) = draw_target(rng, engine) else {
        return;
    };
    let cause = engine.begin_action("discount_change");
    let redeem = draw_redeem(rng, engine, &cause);
    let id = rng.id(ObjectType::Discount.id_prefix());
    create(engine, ctx, &id, &target, &redeem, &cause);
}

fn invalid(msg: String) -> MockError {
    MockError::InvalidAction(msg)
}

/// Checks a named target: it exists, has not ended or been deleted, and has no discount.
fn check_target(engine: &Engine, target: &Target) -> Result<(), MockError> {
    match target {
        Target::Customer(c) => {
            if !is_live(engine, ObjectType::Customer, c) {
                return Err(invalid(format!(
                    "customer {c} does not exist or is deleted"
                )));
            }
            if let Some(di) = customer_discount(engine, c) {
                return Err(invalid(format!("customer {c} already has discount {di}")));
            }
        }
        Target::Subscription(s) => {
            match sub_status(engine, s) {
                None => return Err(invalid(format!("subscription {s} does not exist"))),
                Some(Status::Canceled | Status::IncompleteExpired) => {
                    return Err(invalid(format!("subscription {s} has ended")));
                }
                Some(_) => {}
            }
            if let Some(di) = subscription_discount(engine, s) {
                return Err(invalid(format!(
                    "subscription {s} already has discount {di}"
                )));
            }
        }
    }
    Ok(())
}

/// Checks a named coupon or promotion code: a live, valid coupon, or an active code whose
/// coupon is.
fn check_redeem(engine: &Engine, redeem: &Redeem) -> Result<(), MockError> {
    match redeem {
        Redeem::Coupon(c) => {
            if !valid_coupons(engine).contains(c) {
                return Err(invalid(format!(
                    "coupon {c} does not exist, is deleted or is no longer valid"
                )));
            }
        }
        Redeem::PromotionCode(p) => {
            if !active_promotion_codes(engine).contains(p) {
                return Err(invalid(format!(
                    "promotion code {p} does not exist, is inactive or its coupon is not valid"
                )));
            }
        }
    }
    Ok(())
}

/// The `AddDiscount` control hook: a discount on the named customer or subscription (drawn
/// as in Q7 when neither is named), redeeming the named coupon or promotion code (drawn as
/// in Q7 when neither is named). Naming both targets or both a coupon and a code, a target
/// that is missing, ended or already discounted, a coupon that is not valid, or a code that
/// is not active is a 409 naming it; so is no target to draw.
pub(crate) fn add_discount(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<String>,
    subscription: Option<String>,
    coupon: Option<String>,
    promotion_code: Option<String>,
) -> Result<Cause, MockError> {
    let target = match (customer, subscription) {
        (Some(c), Some(s)) => {
            return Err(invalid(format!(
                "a discount goes on customer {c} or subscription {s}, not both"
            )));
        }
        (Some(c), None) => Some(Target::Customer(c)),
        (None, Some(s)) => Some(Target::Subscription(s)),
        (None, None) => None,
    };
    let redeem = match (coupon, promotion_code) {
        (Some(c), Some(p)) => {
            return Err(invalid(format!(
                "a discount redeems coupon {c} or promotion code {p}, not both"
            )));
        }
        (Some(c), None) => Some(Redeem::Coupon(c)),
        (None, Some(p)) => Some(Redeem::PromotionCode(p)),
        (None, None) => None,
    };
    if let Some(t) = &target {
        check_target(engine, t)?;
    }
    if let Some(r) = &redeem {
        check_redeem(engine, r)?;
    }
    let target = match target {
        Some(t) => t,
        None => draw_target(rng, engine).ok_or_else(|| {
            invalid("no customer or active subscription without a discount".to_string())
        })?,
    };
    let cause = engine.begin_action("add_discount");
    let redeem = match redeem {
        Some(r) => r,
        None => draw_redeem(rng, engine, &cause),
    };
    let id = rng.id(ObjectType::Discount.id_prefix());
    create(engine, ctx, &id, &target, &redeem, &cause);
    Ok(cause)
}

/// The `RemoveDiscount` control hook: removes the named or a random live discount. A named
/// one that is missing or already removed, or none to pick, is a 409 naming it.
pub(crate) fn remove_discount(
    rng: &mut SimRng,
    engine: &mut Engine,
    id: Option<String>,
) -> Result<Cause, MockError> {
    let di = match id {
        Some(di) => {
            if !is_live(engine, ObjectType::Discount, &di) {
                return Err(invalid(format!(
                    "discount {di} does not exist or is already removed"
                )));
            }
            di
        }
        None => {
            let live = engine.store().live_ids(ObjectType::Discount);
            if live.is_empty() {
                return Err(invalid("no discount to remove".to_string()));
            }
            pick(rng, live).clone()
        }
    };
    let cause = engine.begin_action("remove_discount");
    remove(engine, &di, &cause);
    Ok(cause)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coupon_terms_survive_deletion() {
        let config = crate::Config::default();
        let streams = crate::rng::RngStreams::new(1);
        let mut e = Engine::new(SimTime(1_700_000_000_000), &config, &streams);
        let spec = crate::model::discounts::CouponSpec {
            id: "AbCd1234".into(),
            name: "20% off".into(),
            percent_off: Some(20),
            amount_off: None,
            duration: CouponDuration::Forever,
            duration_in_months: None,
            max_redemptions: None,
        };
        let cause = e.begin_action("test");
        let c = crate::model::discounts::coupon(&spec, 1);
        e.create_obj(ObjectType::Coupon, spec.id.clone(), &c, cause.clone(), &[]);
        e.delete(
            ObjectType::Coupon,
            "AbCd1234",
            &crate::model::discounts::deleted_coupon("AbCd1234"),
            cause,
            &[],
        )
        .unwrap();
        let terms = coupon_terms(&e, "AbCd1234").unwrap();
        assert_eq!(terms.percent_off, Some(Some(20.0)));
        assert!(coupon_terms(&e, "missing0").is_none());
    }
}
