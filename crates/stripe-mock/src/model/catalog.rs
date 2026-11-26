//! Products and prices, built on the generated [`stripe::Product`] and [`stripe::Price`].

use std::collections::BTreeMap;

use crate::model::customer::random_metadata_value;
use crate::rng::SimRng;
use crate::stripe;

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

/// A new active product. Presence follows ruling R1: every field is present, `null` when
/// unset. The hand-written `type: "service"` is gone: the pinned schema has no `type`.
pub(crate) fn new_product(p: &ProductSpec, created: i64) -> stripe::Product {
    stripe::Product {
        active: true,
        created,
        default_price: Some(None),
        description: Some(p.description.clone()),
        id: p.id.clone(),
        images: Vec::new(),
        livemode: false,
        marketing_features: Vec::new(),
        metadata: p.metadata.clone(),
        name: p.name.clone(),
        object: stripe::ProductObject::Product,
        package_dimensions: Some(None),
        shippable: Some(None),
        statement_descriptor: Some(None),
        tax_code: Some(None),
        tax_details: Some(None),
        unit_label: Some(None),
        updated: created,
        url: Some(None),
    }
}

/// A new active per-unit price of `product`. Presence follows ruling R1; the includable-only
/// `currency_options` and `tiers` are absent.
pub(crate) fn new_price(p: &PriceSpec, product: &str, created: i64) -> stripe::Price {
    let recurring = p.interval.map(|interval| stripe::Recurring {
        interval: match interval {
            "day" => stripe::RecurringInterval::Day,
            "week" => stripe::RecurringInterval::Week,
            "month" => stripe::RecurringInterval::Month,
            "year" => stripe::RecurringInterval::Year,
            other => panic!("unknown price interval `{other}`"),
        },
        interval_count: 1,
        meter: Some(None),
        usage_type: stripe::RecurringUsageType::Licensed,
    });
    stripe::Price {
        active: true,
        billing_scheme: stripe::PriceBillingScheme::PerUnit,
        created,
        currency: p.currency.to_string(),
        currency_options: None,
        custom_unit_amount: Some(None),
        id: p.id.clone(),
        livemode: false,
        lookup_key: Some(None),
        metadata: BTreeMap::new(),
        nickname: Some(p.nickname.clone()),
        object: stripe::PriceObject::Price,
        product: stripe::Expandable::Id(product.to_string()),
        recurring: Some(recurring),
        tax_behavior: Some(Some(stripe::PriceTaxBehavior::Unspecified)),
        tiers: None,
        tiers_mode: Some(None),
        transform_quantity: Some(None),
        type_: if p.interval.is_some() {
            stripe::PriceType::Recurring
        } else {
            stripe::PriceType::OneTime
        },
        unit_amount: Some(Some(p.unit_amount)),
        unit_amount_decimal: Some(Some(p.unit_amount.to_string())),
    }
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

/// A non-zero price amount, for a payment billed without a price.
pub(crate) fn random_nonzero_amount(rng: &mut SimRng) -> i64 {
    *rng.choose(&AMOUNTS[1..])
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
pub(crate) fn random_product_change(rng: &mut SimRng) -> impl FnOnce(&mut stripe::Product) + use<> {
    let which = rng.below(3);
    let name = rng.choose(&PRODUCT_NAMES).to_string();
    let key = rng.choose(&METADATA_KEYS).to_string();
    let value = random_metadata_value(rng);
    move |p: &mut stripe::Product| match which {
        0 => p.name = name,
        1 => p.description = Some(Some(format!("{name}, updated"))),
        _ => {
            p.metadata.insert(key, value);
        }
    }
}

/// A random change for a price: nickname or metadata (never amounts).
pub(crate) fn random_price_change(rng: &mut SimRng) -> impl FnOnce(&mut stripe::Price) + use<> {
    let nickname = rng.below(2) == 0;
    let key = rng.choose(&METADATA_KEYS).to_string();
    let value = random_metadata_value(rng);
    move |p: &mut stripe::Price| {
        if nickname {
            p.nickname = Some(Some(format!("Nickname {value:.8}")));
        } else {
            p.metadata.insert(key, value);
        }
    }
}
