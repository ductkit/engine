//! `catalog_change`, product/price churn and archiving (spec sections 4, 6).

use serde_json::json;

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::{Continuation, Ctx, pick};
use crate::model::catalog::{
    PriceSpec, ProductSpec, price_json, product_json, random_currency, random_price,
    random_price_change, random_product, random_product_change,
};
use crate::rng::SimRng;
use crate::{MockError, ObjectType};

/// A new product with its prices, all drawn up front.
#[derive(Debug, Clone)]
pub(crate) struct NewProduct {
    pub product: ProductSpec,
    pub prices: Vec<PriceSpec>,
}

/// Draws a product with 1-3 prices in one currency; the first price is recurring.
pub(crate) fn draw_new_product(rng: &mut SimRng) -> NewProduct {
    let id = rng_id(rng, ObjectType::Product);
    let product = random_product(rng, id);
    let currency = random_currency(rng);
    let n = 1 + rng.below(3);
    let prices = (0..n)
        .map(|i| {
            let recurring = i == 0 || rng.below(10) < 6;
            let id = rng_id(rng, ObjectType::Price);
            random_price(rng, id, currency, recurring)
        })
        .collect();
    NewProduct { product, prices }
}

fn rng_id(rng: &mut SimRng, t: ObjectType) -> String {
    rng.id(t.id_prefix())
}

pub(crate) fn create_product(engine: &mut Engine, p: &ProductSpec, cause: &Cause) {
    let state = product_json(p, engine.now().unix_seconds());
    engine.create(
        ObjectType::Product,
        p.id.clone(),
        state,
        cause.clone(),
        &["product.created"],
    );
}

pub(crate) fn create_prices(
    engine: &mut Engine,
    product: &str,
    prices: &[PriceSpec],
    cause: &Cause,
) {
    for p in prices {
        let state = price_json(p, product, engine.now().unix_seconds());
        engine.create(
            ObjectType::Price,
            p.id.clone(),
            state,
            cause.clone(),
            &["price.created"],
        );
    }
}

pub(crate) fn set_default_price(engine: &mut Engine, product: &str, price: &str, cause: &Cause) {
    let now = engine.now().unix_seconds();
    let price = price.to_string();
    // The product may have been archived meanwhile; defaults still apply.
    let _ = engine.update(
        ObjectType::Product,
        product,
        |v| {
            v["default_price"] = json!(price);
            v["updated"] = json!(now);
        },
        cause.clone(),
        ChangeKind::Updated,
        &["product.updated"],
    );
}

/// Creates a product, its prices and its default price immediately (used for
/// prerequisites inside other actions). Returns the price IDs.
pub(crate) fn create_product_now(
    engine: &mut Engine,
    np: &NewProduct,
    cause: &Cause,
) -> Vec<String> {
    create_product(engine, &np.product, cause);
    create_prices(engine, &np.product.id, &np.prices, cause);
    set_default_price(engine, &np.product.id, &np.prices[0].id, cause);
    np.prices.iter().map(|p| p.id.clone()).collect()
}

/// Objects of `t` that are live and `active`.
pub(crate) fn active_ids(engine: &Engine, t: ObjectType) -> Vec<String> {
    engine
        .store()
        .live_ids(t)
        .into_iter()
        .filter(|id| {
            engine
                .latest(t, id)
                .is_some_and(|u| u.state["active"] == json!(true))
        })
        .map(str::to_string)
        .collect()
}

/// `catalog_change`: a new product (50%), a new price for an active product (30%)
/// or an archive (20%). Steps after the first are `step_delay` apart.
pub(crate) fn change(rng: &mut SimRng, engine: &mut Engine, ctx: &mut Ctx) -> Cause {
    let cause = engine.begin_action("catalog_change");
    let roll = rng.below(10);
    let products = active_ids(engine, ObjectType::Product);
    if roll < 5 || (roll < 8 && products.is_empty()) {
        let np = draw_new_product(rng);
        create_product(engine, &np.product, &cause);
        ctx.then(
            engine,
            Continuation::CreatePrices {
                product: np.product.id.clone(),
                prices: np.prices,
                set_default: true,
            },
            cause.clone(),
        );
    } else if roll < 8 {
        let product = pick(rng, &products).to_string();
        let currency = random_currency(rng);
        let recurring = rng.below(10) < 6;
        let id = rng_id(rng, ObjectType::Price);
        let price = random_price(rng, id, currency, recurring);
        create_prices(engine, &product, &[price], &cause);
    } else {
        archive_random(rng, engine, &cause);
    }
    cause
}

/// Archives a random active price, or if none, a random active product.
fn archive_random(rng: &mut SimRng, engine: &mut Engine, cause: &Cause) {
    for t in [ObjectType::Price, ObjectType::Product] {
        let ids = active_ids(engine, t);
        if !ids.is_empty() {
            let id = pick(rng, &ids).to_string();
            archive(engine, t, &id, cause);
            return;
        }
    }
}

fn archive(engine: &mut Engine, t: ObjectType, id: &str, cause: &Cause) {
    let now = engine.now().unix_seconds();
    let event = if t == ObjectType::Product {
        "product.updated"
    } else {
        "price.updated"
    };
    engine
        .update(
            t,
            id,
            |v| {
                v["active"] = json!(false);
                if t == ObjectType::Product {
                    v["updated"] = json!(now);
                }
            },
            cause.clone(),
            ChangeKind::Archived,
            &[event],
        )
        .expect("archiving a live object");
}

/// The `Archive { id }` control hook.
pub(crate) fn archive_by_id(engine: &mut Engine, id: &str) -> Result<Cause, MockError> {
    let t = if id.starts_with("prod_") {
        ObjectType::Product
    } else if id.starts_with("price_") {
        ObjectType::Price
    } else {
        return Err(MockError::InvalidAction(format!(
            "only products and prices can be archived, not `{id}`"
        )));
    };
    let latest = engine
        .latest(t, id)
        .ok_or_else(|| MockError::NoSuchObject {
            object_type: t,
            id: id.to_string(),
        })?;
    if latest.state["active"] != json!(true) {
        return Err(MockError::InvalidAction(format!(
            "{t} {id} is already archived"
        )));
    }
    let cause = engine.begin_action("archive");
    archive(engine, t, id, &cause);
    Ok(cause)
}

/// `churn.product.update` / `churn.price.update`: one random change to a random live object.
pub(crate) fn churn_update(rng: &mut SimRng, engine: &mut Engine, t: ObjectType) {
    let ids: Vec<String> = engine
        .store()
        .live_ids(t)
        .into_iter()
        .map(str::to_string)
        .collect();
    if ids.is_empty() {
        return;
    }
    let id = pick(rng, &ids).to_string();
    let now = engine.now().unix_seconds();
    let (action, event) = if t == ObjectType::Product {
        ("product_update", "product.updated")
    } else {
        ("price_update", "price.updated")
    };
    let change: Box<dyn FnOnce(&mut serde_json::Value)> = if t == ObjectType::Product {
        let f = random_product_change(rng);
        Box::new(move |v| {
            f(v);
            v["updated"] = json!(now);
        })
    } else {
        Box::new(random_price_change(rng))
    };
    let cause = engine.begin_action(action);
    engine
        .update(t, &id, change, cause, ChangeKind::Updated, &[event])
        .expect("live object");
}

/// `churn.product.archive` / `churn.price.archive`: archive a random active object.
pub(crate) fn churn_archive(rng: &mut SimRng, engine: &mut Engine, t: ObjectType) {
    let ids = active_ids(engine, t);
    if ids.is_empty() {
        return;
    }
    let id = pick(rng, &ids).to_string();
    let action = if t == ObjectType::Product {
        "product_archive"
    } else {
        "price_archive"
    };
    let cause = engine.begin_action(action);
    archive(engine, t, &id, &cause);
}
