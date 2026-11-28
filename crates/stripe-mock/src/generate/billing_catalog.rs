//! `billing_catalog_change` and coupon churn: coupons, promotion codes and tax rates
//! (phase 9 ruling Q6). A sibling of `catalog_change`, so that action's draws stay put.

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::pick;
use crate::model::customer::random_metadata_value;
use crate::model::discounts::{
    self, deleted_coupon, promotion_code, random_coupon, random_tax_rate,
};
use crate::rng::SimRng;
use crate::stripe;
use crate::{MockError, ObjectType};

/// IDs of the live, valid coupons.
pub(crate) fn valid_coupons(engine: &Engine) -> Vec<String> {
    engine
        .store()
        .live_ids(ObjectType::Coupon)
        .iter()
        .filter(|id| {
            engine
                .latest(ObjectType::Coupon, id)
                .is_some_and(|u| u.state["valid"] == true)
        })
        .cloned()
        .collect()
}

/// The coupon a promotion code names.
fn coupon_of(state: &serde_json::Value) -> Option<&str> {
    state["promotion"]["coupon"].as_str()
}

/// IDs of the active promotion codes whose coupon is valid and live.
pub(crate) fn active_promotion_codes(engine: &Engine) -> Vec<String> {
    let valid = valid_coupons(engine);
    engine
        .store()
        .live_ids(ObjectType::PromotionCode)
        .iter()
        .filter(|id| {
            engine
                .latest(ObjectType::PromotionCode, id)
                .is_some_and(|u| {
                    u.state["active"] == true
                        && coupon_of(&u.state).is_some_and(|c| valid.iter().any(|v| v == c))
                })
        })
        .cloned()
        .collect()
}

/// IDs of the active objects of `t`, whatever their coupon.
fn active_of(engine: &Engine, t: ObjectType) -> Vec<String> {
    super::catalog::active_ids(engine, t)
}

/// Draws and creates a coupon (`coupon.created`). Returns its ID.
pub(crate) fn new_coupon(rng: &mut SimRng, engine: &mut Engine, cause: &Cause) -> String {
    let spec = random_coupon(rng);
    let coupon = discounts::coupon(&spec, engine.now().unix_seconds());
    engine.create_obj(
        ObjectType::Coupon,
        spec.id.clone(),
        &coupon,
        cause.clone(),
        &["coupon.created"],
    );
    spec.id
}

/// Draws and creates an active promotion code for `coupon` (`promotion_code.created`).
fn new_promotion_code(rng: &mut SimRng, engine: &mut Engine, coupon: &str, cause: &Cause) {
    let id = rng.id(ObjectType::PromotionCode.id_prefix());
    let code = rng.alnum_upper(8);
    let promo = promotion_code(&id, &code, coupon, engine.now().unix_seconds());
    engine.create_obj(
        ObjectType::PromotionCode,
        id,
        &promo,
        cause.clone(),
        &["promotion_code.created"],
    );
}

/// Draws and creates an active tax rate (`tax_rate.created`).
fn new_tax_rate(rng: &mut SimRng, engine: &mut Engine, cause: &Cause) {
    let id = rng.id(ObjectType::TaxRate.id_prefix());
    let spec = random_tax_rate(rng, id.clone());
    let rate = discounts::tax_rate(&spec, engine.now().unix_seconds());
    engine.create_obj(
        ObjectType::TaxRate,
        id,
        &rate,
        cause.clone(),
        &["tax_rate.created"],
    );
}

/// Sets a promotion code or tax rate to `active: false` (`*.updated`).
pub(crate) fn archive(engine: &mut Engine, t: ObjectType, id: &str, cause: &Cause) {
    let done = match t {
        ObjectType::PromotionCode => engine.update_obj::<stripe::PromotionCode>(
            t,
            id,
            |p| p.active = false,
            cause.clone(),
            ChangeKind::Archived,
            &["promotion_code.updated"],
        ),
        ObjectType::TaxRate => engine.update_obj::<stripe::TaxRate>(
            t,
            id,
            |r| r.active = false,
            cause.clone(),
            ChangeKind::Archived,
            &["tax_rate.updated"],
        ),
        other => panic!("{other:?} cannot be archived here"),
    };
    done.expect("archiving a live object");
}

/// Deletes a coupon: its active promotion codes are deactivated first (discounts that use
/// the coupon outlive it).
fn delete_coupon_now(engine: &mut Engine, id: &str, cause: &Cause) {
    let codes: Vec<String> = active_of(engine, ObjectType::PromotionCode)
        .into_iter()
        .filter(|code| {
            engine
                .latest(ObjectType::PromotionCode, code)
                .is_some_and(|u| coupon_of(&u.state) == Some(id))
        })
        .collect();
    for code in codes {
        archive(engine, ObjectType::PromotionCode, &code, cause);
    }
    engine
        .delete(
            ObjectType::Coupon,
            id,
            &deleted_coupon(id),
            cause.clone(),
            &["coupon.deleted"],
        )
        .expect("deleting a live coupon");
}

/// `billing_catalog_change`: one step of ruling Q6, each falling back to a new coupon when
/// its target is missing.
pub(crate) fn change(rng: &mut SimRng, engine: &mut Engine) -> Cause {
    let cause = engine.begin_action("billing_catalog_change");
    step(rng, engine, &cause);
    cause
}

fn step(rng: &mut SimRng, engine: &mut Engine, cause: &Cause) {
    match rng.below(100) {
        30..=54 => {
            let coupons = valid_coupons(engine);
            if !coupons.is_empty() {
                let coupon = pick(rng, &coupons).clone();
                new_promotion_code(rng, engine, &coupon, cause);
                return;
            }
        }
        55..=69 => {
            new_tax_rate(rng, engine, cause);
            return;
        }
        70..=79 => {
            let codes = active_of(engine, ObjectType::PromotionCode);
            if !codes.is_empty() {
                let id = pick(rng, &codes).clone();
                archive(engine, ObjectType::PromotionCode, &id, cause);
                return;
            }
        }
        80..=89 => {
            let rates = active_of(engine, ObjectType::TaxRate);
            if !rates.is_empty() {
                let id = pick(rng, &rates).clone();
                archive(engine, ObjectType::TaxRate, &id, cause);
                return;
            }
        }
        90..=99 => {
            let coupons = engine.store().live_ids(ObjectType::Coupon).to_vec();
            if !coupons.is_empty() {
                let id = pick(rng, &coupons).clone();
                delete_coupon_now(engine, &id, cause);
                return;
            }
        }
        _ => {}
    }
    new_coupon(rng, engine, cause);
}

/// A write during pagination of `t` (ruling Q10): the matching creation. A promotion code
/// with no valid coupon first creates one.
pub(crate) fn pagination_create(rng: &mut SimRng, engine: &mut Engine, t: ObjectType) {
    let cause = engine.begin_action("billing_catalog_change");
    match t {
        ObjectType::Coupon => {
            new_coupon(rng, engine, &cause);
        }
        ObjectType::PromotionCode => {
            let coupons = valid_coupons(engine);
            let coupon = if coupons.is_empty() {
                new_coupon(rng, engine, &cause)
            } else {
                pick(rng, &coupons).clone()
            };
            new_promotion_code(rng, engine, &coupon, &cause);
        }
        ObjectType::TaxRate => new_tax_rate(rng, engine, &cause),
        other => panic!("{other:?} has no billing catalog creation"),
    }
}

/// The `DeleteCoupon { id }` control hook. A missing or already deleted coupon is a 409
/// naming it.
pub(crate) fn delete_coupon_by_id(engine: &mut Engine, id: &str) -> Result<Cause, MockError> {
    let live = engine
        .latest(ObjectType::Coupon, id)
        .is_some_and(|u| u.kind != ChangeKind::Deleted);
    if !live {
        return Err(MockError::InvalidAction(format!(
            "coupon {id} does not exist or is already deleted"
        )));
    }
    let cause = engine.begin_action("delete_coupon");
    delete_coupon_now(engine, id, &cause);
    Ok(cause)
}

/// The `Archive { id }` hook for `promo_…` and `txr_…`: 409 when already inactive.
pub(crate) fn archive_by_id(
    engine: &mut Engine,
    t: ObjectType,
    id: &str,
) -> Result<Cause, MockError> {
    let latest = engine
        .latest(t, id)
        .ok_or_else(|| MockError::NoSuchObject {
            object_type: t,
            id: id.to_string(),
        })?;
    if latest.state["active"] != true {
        return Err(MockError::InvalidAction(format!(
            "{t} {id} is already inactive"
        )));
    }
    let cause = engine.begin_action("archive");
    archive(engine, t, id, &cause);
    Ok(cause)
}

/// `churn.coupon.update`: a new `metadata.note` on a random live coupon.
pub(crate) fn coupon_update(rng: &mut SimRng, engine: &mut Engine) {
    let ids = engine.store().live_ids(ObjectType::Coupon).to_vec();
    if ids.is_empty() {
        return;
    }
    let id = pick(rng, &ids).clone();
    let note = random_metadata_value(rng);
    let cause = engine.begin_action("coupon_update");
    engine
        .update_obj::<stripe::Coupon>(
            ObjectType::Coupon,
            &id,
            |c| {
                c.metadata
                    .get_or_insert_with(|| Some(Default::default()))
                    .get_or_insert_with(Default::default)
                    .insert("note".to_string(), note);
            },
            cause,
            ChangeKind::Updated,
            &["coupon.updated"],
        )
        .expect("live coupon");
}
