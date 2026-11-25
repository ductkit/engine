//! Retrieve and list for every object type, from per-type descriptors
//! (spec sections 7.2, 7.3).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::api::ApiError;
use crate::api::list::{ListParams, decode, paginate, parse_list_params};
use crate::engine::Engine;
use crate::engine::store::StoredObject;
use crate::handle::Shared;
use crate::{ChangeKind, ObjectType};

/// List order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// Newest `created` first (top-level lists).
    CreatedDesc,
    /// Creation order, oldest first (sub-lists, matching the embedded list).
    CreationAsc,
}

/// One listable type.
#[derive(Debug)]
pub(crate) struct Resource {
    pub t: ObjectType,
    pub path: &'static str,
    pub filters: &'static [&'static str],
    pub order: Order,
    /// A filter the list cannot do without.
    pub required: Option<&'static str>,
}

pub(crate) static CUSTOMERS: Resource = Resource {
    t: ObjectType::Customer,
    path: "/v1/customers",
    filters: &["email"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static PRODUCTS: Resource = Resource {
    t: ObjectType::Product,
    path: "/v1/products",
    filters: &["active"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static PRICES: Resource = Resource {
    t: ObjectType::Price,
    path: "/v1/prices",
    filters: &["active", "product", "currency", "type"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static SUBSCRIPTIONS: Resource = Resource {
    t: ObjectType::Subscription,
    path: "/v1/subscriptions",
    filters: &["customer", "price", "status"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static SUBSCRIPTION_ITEMS: Resource = Resource {
    t: ObjectType::SubscriptionItem,
    path: "/v1/subscription_items",
    filters: &["subscription"],
    order: Order::CreationAsc,
    required: Some("subscription"),
};
pub(crate) static INVOICES: Resource = Resource {
    t: ObjectType::Invoice,
    path: "/v1/invoices",
    filters: &["customer", "subscription", "status"],
    order: Order::CreatedDesc,
    required: None,
};

fn bool_param(p: &ListParams, name: &str) -> Result<Option<bool>, ApiError> {
    match p.extra.get(name).map(String::as_str) {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(_) => Err(ApiError::bad_request(
            format!("Invalid boolean: {name}"),
            Some("parameter_invalid_boolean"),
            Some(name),
        )),
    }
}

/// Whether `o` passes the resource's filters.
fn matches(
    engine: &Engine,
    r: &Resource,
    o: &StoredObject,
    f: &BTreeMap<String, String>,
    active: Option<bool>,
) -> bool {
    let s = &o.latest.state;
    let eq = |field: &str, key: &str| {
        f.get(key)
            .is_none_or(|want| s[field].as_str() == Some(want))
    };
    match r.t {
        ObjectType::Customer => eq("email", "email"),
        ObjectType::Product => active.is_none_or(|a| s["active"] == json!(a)),
        ObjectType::Price => {
            active.is_none_or(|a| s["active"] == json!(a))
                && eq("product", "product")
                && eq("currency", "currency")
                && eq("type", "type")
        }
        ObjectType::Subscription => {
            let status_ok = match f.get("status").map(String::as_str) {
                Some("all") => true,
                Some(want) => s["status"].as_str() == Some(want),
                None => !matches!(
                    s["status"].as_str(),
                    Some("canceled" | "incomplete_expired")
                ),
            };
            status_ok
                && eq("customer", "customer")
                && f.get("price").is_none_or(|want| {
                    engine
                        .store()
                        .children(
                            ObjectType::SubscriptionItem,
                            "subscription",
                            &o.latest.object_id,
                        )
                        .iter()
                        .any(|i| !i.deleted && i.latest.state["price"]["id"].as_str() == Some(want))
                })
        }
        ObjectType::SubscriptionItem => eq("subscription", "subscription"),
        ObjectType::Invoice => {
            eq("customer", "customer")
                && eq("status", "status")
                && f.get("subscription").is_none_or(|want| {
                    s["parent"]["subscription_details"]["subscription"].as_str() == Some(want)
                })
        }
        ObjectType::InvoiceLineItem => true,
    }
}

fn ordered<'a>(engine: &'a Engine, r: &Resource) -> Vec<&'a StoredObject> {
    match r.order {
        Order::CreatedDesc => engine.store().list_order(r.t),
        Order::CreationAsc => engine.store().in_creation_order(r.t),
    }
}

/// Adds `GET {path}` and `GET {path}/{id}` for `r`.
pub(crate) fn routes(router: Router<Arc<Shared>>, r: &'static Resource) -> Router<Arc<Shared>> {
    router
        .route(
            r.path,
            get(move |s: State<Arc<Shared>>, q: RawQuery| list(r, s, q)),
        )
        .route(
            &format!("{}/{{id}}", r.path),
            get(move |s: State<Arc<Shared>>, id: Path<String>, q: RawQuery| retrieve(r, s, id, q)),
        )
}

/// `GET {path}`.
async fn list(
    r: &'static Resource,
    State(shared): State<Arc<Shared>>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let p = parse_list_params(decode(query), r.filters, &[])?;
    if let Some(name) = r.required
        && !p.extra.contains_key(name)
    {
        return Err(ApiError::bad_request(
            format!("Missing required param: {name}."),
            Some("parameter_missing"),
            Some(name),
        ));
    }
    let active = if r.filters.contains(&"active") {
        bool_param(&p, "active")?
    } else {
        None
    };
    let sim = shared.lock();
    let engine = &sim.engine;
    let all = ordered(engine, r);
    let (page, has_more) = paginate(
        &all,
        |o| !o.deleted && p.created.matches(o.created) && matches(engine, r, o, &p.extra, active),
        &p,
    )?;
    let url = match r.required {
        Some(name) => format!("{}?{name}={}", r.path, p.extra[name]),
        None => r.path.to_string(),
    };
    Ok(Json(json!({
        "object": "list",
        "url": url,
        "has_more": has_more,
        "data": page.iter().map(|o| o.latest.state.clone()).collect::<Vec<_>>(),
    })))
}

fn no_such(t: ObjectType, id: &str) -> ApiError {
    ApiError {
        param: Some("id".to_string()),
        code: Some("resource_missing"),
        ..ApiError::not_found(format!("No such {}: '{id}'", t.as_str()))
    }
}

/// The latest state as retrieve shows it: deleted customers are a stub, other
/// deleted objects are missing.
pub(crate) fn visible(engine: &Engine, t: ObjectType, id: &str) -> Option<Value> {
    let u = engine.latest(t, id)?;
    if u.kind == ChangeKind::Deleted && t != ObjectType::Customer {
        return None;
    }
    Some(u.state.clone())
}

/// `GET {path}/{id}`.
async fn retrieve(
    r: &'static Resource,
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    if let Some((key, _)) = decode(query).into_iter().next() {
        return Err(ApiError::bad_request(
            format!("Received unknown parameter: {key}"),
            Some("parameter_unknown"),
            Some(&key),
        ));
    }
    let sim = shared.lock();
    visible(&sim.engine, r.t, &id)
        .map(Json)
        .ok_or_else(|| no_such(r.t, &id))
}

/// `GET /v1/invoices/{id}/lines`.
pub(crate) async fn invoice_lines(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let p = parse_list_params(decode(query), &[], &[])?;
    let sim = shared.lock();
    let engine = &sim.engine;
    if visible(engine, ObjectType::Invoice, &id).is_none() {
        return Err(no_such(ObjectType::Invoice, &id));
    }
    let lines = engine
        .store()
        .children(ObjectType::InvoiceLineItem, "invoice", &id);
    let (page, has_more) = paginate(&lines, |o| !o.deleted, &p)?;
    Ok(Json(json!({
        "object": "list",
        "url": format!("/v1/invoices/{id}/lines"),
        "has_more": has_more,
        "data": page.iter().map(|o| o.latest.state.clone()).collect::<Vec<_>>(),
    })))
}
