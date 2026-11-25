//! Product and price JSON (hand-written until phase 7 generates it from OpenAPI).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::model::customer::random_metadata_value;
use crate::rng::SimRng;

const PRODUCT_NAMES: [&str; 6] = [
    "Basic plan",
    "Pro plan",
    "Enterprise",
    "Add-on seats",
    "Ünïcödé Widget",
    "配送料",
];
const CURRENCIES: [(&str, u64); 3] = [("usd", 70), ("eur", 20), ("jpy", 10)];
const AMOUNTS: [i64; 5] = [0, 99, 500, 1999, 99_999_999];
const METADATA_KEYS: [&str; 3] = ["tier", "sku", "note"];

/// Content for a new product.
#[derive(Debug, Clone)]
pub(crate) struct ProductSpec {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

/// Content for a new price, drawn when its action starts.
#[derive(Debug, Clone)]
pub(crate) struct PriceSpec {
    pub id: String,
    pub currency: &'static str,
    pub unit_amount: i64,
    /// `Some("month" | "year")` for recurring prices.
    pub interval: Option<&'static str>,
    pub nickname: Option<String>,
}

pub(crate) fn product_json(p: &ProductSpec, created: i64) -> Value {
    json!({
        "id": p.id,
        "object": "product",
        "active": true,
        "created": created,
        "default_price": null,
        "description": p.description,
        "images": [],
        "livemode": false,
        "marketing_features": [],
        "metadata": p.metadata,
        "name": p.name,
        "package_dimensions": null,
        "shippable": null,
        "statement_descriptor": null,
        "tax_code": null,
        "type": "service",
        "unit_label": null,
        "updated": created,
        "url": null
    })
}

pub(crate) fn price_json(p: &PriceSpec, product: &str, created: i64) -> Value {
    let recurring = p.interval.map(|interval| {
        json!({
            "interval": interval,
            "interval_count": 1,
            "meter": null,
            "usage_type": "licensed"
        })
    });
    json!({
        "id": p.id,
        "object": "price",
        "active": true,
        "billing_scheme": "per_unit",
        "created": created,
        "currency": p.currency,
        "custom_unit_amount": null,
        "livemode": false,
        "lookup_key": null,
        "metadata": {},
        "nickname": p.nickname,
        "product": product,
        "recurring": recurring,
        "tax_behavior": "unspecified",
        "tiers_mode": null,
        "transform_quantity": null,
        "type": if p.interval.is_some() { "recurring" } else { "one_time" },
        "unit_amount": p.unit_amount,
        "unit_amount_decimal": p.unit_amount.to_string()
    })
}

pub(crate) fn random_product(rng: &mut SimRng, id: String) -> ProductSpec {
    let name = rng.choose(&PRODUCT_NAMES).to_string();
    let description = (rng.below(2) == 0).then(|| format!("{name} by ductkit-stripe-mock"));
    let mut metadata = BTreeMap::new();
    for _ in 0..rng.below(3) {
        metadata.insert(
            rng.choose(&METADATA_KEYS).to_string(),
            random_metadata_value(rng),
        );
    }
    ProductSpec {
        id,
        name,
        description,
        metadata,
    }
}

pub(crate) fn random_currency(rng: &mut SimRng) -> &'static str {
    let mut roll = rng.below(100);
    for (c, weight) in CURRENCIES {
        if roll < weight {
            return c;
        }
        roll -= weight;
    }
    unreachable!("weights sum to 100")
}

/// `Some(interval)` with `recurring`, else a one-time price.
pub(crate) fn random_price(
    rng: &mut SimRng,
    id: String,
    currency: &'static str,
    recurring: bool,
) -> PriceSpec {
    let interval = recurring.then(|| if rng.below(10) < 8 { "month" } else { "year" });
    let unit_amount = *rng.choose(&AMOUNTS);
    let nickname = (rng.below(4) == 0).then(|| "Launch price".to_string());
    PriceSpec {
        id,
        currency,
        unit_amount,
        interval,
        nickname,
    }
}

/// A random change for a product: name, description or metadata.
pub(crate) fn random_product_change(rng: &mut SimRng) -> impl FnOnce(&mut Value) + use<> {
    let which = rng.below(3);
    let name = rng.choose(&PRODUCT_NAMES).to_string();
    let key = rng.choose(&METADATA_KEYS).to_string();
    let value = random_metadata_value(rng);
    move |v: &mut Value| match which {
        0 => v["name"] = json!(name),
        1 => v["description"] = json!(format!("{name}, updated")),
        _ => v["metadata"][key] = json!(value),
    }
}

/// A random change for a price: nickname or metadata (never amounts).
pub(crate) fn random_price_change(rng: &mut SimRng) -> impl FnOnce(&mut Value) + use<> {
    let nickname = rng.below(2) == 0;
    let key = rng.choose(&METADATA_KEYS).to_string();
    let value = random_metadata_value(rng);
    move |v: &mut Value| {
        if nickname {
            v["nickname"] = json!(format!("Nickname {value:.8}"));
        } else {
            v["metadata"][key] = json!(value);
        }
    }
}
