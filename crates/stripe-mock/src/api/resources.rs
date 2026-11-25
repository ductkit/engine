//! Retrieve and list for every object type, from per-type descriptors
//! (spec sections 7.2, 7.3).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, RawQuery, State};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::api::expand::expand;
use crate::api::list::{ListParams, array_base, decode, paginate, parse_list_params};
use crate::api::{ApiError, RequestScope, ensure_connected};
use crate::engine::store::{ObjectStore, StoredObject};
use crate::engine::{Engine, Scope, StateUpdate};
use crate::handle::{Shared, Sim};
use crate::{ChangeKind, ObjectType, SimTime};

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

pub(crate) static ACCOUNTS: Resource = Resource {
    t: ObjectType::Account,
    path: "/v1/accounts",
    filters: &[],
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

/// What one request sees: the latest state, or (a stale read) every object as
/// it was at `at` (spec section 7.4), in the request's `scope`.
#[derive(Clone)]
pub(crate) struct View<'a> {
    pub engine: &'a Engine,
    pub at: Option<SimTime>,
    pub scope: Scope,
}

impl<'a> View<'a> {
    /// The objects this view reads; empty while the scope has none.
    pub(crate) fn store(&self) -> &'a ObjectStore {
        self.engine.scope_store(&self.scope)
    }

    /// The version this view shows of `o`; `None` if it did not exist yet.
    pub(crate) fn state(&self, o: &'a StoredObject) -> Option<&'a StateUpdate> {
        match self.at {
            None => Some(&o.latest),
            Some(at) => {
                self.engine
                    .as_of_in(&self.scope, o.latest.object_type, &o.latest.object_id, at)
            }
        }
    }

    /// The version a list shows: existing in this view and not deleted.
    pub(crate) fn listed(&self, o: &'a StoredObject) -> Option<&'a StateUpdate> {
        self.state(o).filter(|u| u.kind != ChangeKind::Deleted)
    }

    /// The state retrieve shows: a deleted customer is its stub, other deleted
    /// objects (and objects not yet created in this view) are missing.
    pub(crate) fn visible(&self, t: ObjectType, id: &str) -> Option<Value> {
        let u = self.state(self.store().get(t, id)?)?;
        if u.kind == ChangeKind::Deleted && t != ObjectType::Customer {
            return None;
        }
        Some(u.state.clone())
    }
}

/// Whether `s` (a state of `o` in `view`) passes the resource's filters.
fn matches(
    view: &View,
    r: &Resource,
    o: &StoredObject,
    s: &Value,
    f: &BTreeMap<String, String>,
    active: Option<bool>,
) -> bool {
    let eq = |field: &str, key: &str| {
        f.get(key)
            .is_none_or(|want| s[field].as_str() == Some(want))
    };
    match r.t {
        ObjectType::Customer => eq("email", "email"),
        ObjectType::Product => active.is_none_or(|a| s["active"].as_bool() == Some(a)),
        ObjectType::Price => {
            active.is_none_or(|a| s["active"].as_bool() == Some(a))
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
                    view.store()
                        .children(
                            ObjectType::SubscriptionItem,
                            "subscription",
                            &o.latest.object_id,
                        )
                        .into_iter()
                        .filter_map(|i| view.listed(i))
                        .any(|i| i.state["price"]["id"].as_str() == Some(want))
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
        ObjectType::InvoiceLineItem | ObjectType::Account => true,
    }
}

fn ordered<'a>(view: &View<'a>, r: &Resource) -> Vec<&'a StoredObject> {
    match r.order {
        Order::CreatedDesc => view.store().list_order(r.t),
        Order::CreationAsc => view.store().in_creation_order(r.t),
    }
}

/// Adds `GET {path}` and `GET {path}/{id}` for `r`.
pub(crate) fn routes(router: Router<Arc<Shared>>, r: &'static Resource) -> Router<Arc<Shared>> {
    router
        .route(
            r.path,
            get(
                move |s: State<Arc<Shared>>, sc: Extension<RequestScope>, q: RawQuery| {
                    list(r, s, sc, q)
                },
            ),
        )
        .route(
            &format!("{}/{{id}}", r.path),
            get(
                move |s: State<Arc<Shared>>,
                      sc: Extension<RequestScope>,
                      id: Path<String>,
                      q: RawQuery| retrieve(r, s, sc, id, q),
            ),
        )
}

/// `GET {path}`.
async fn list(
    r: &'static Resource,
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let p = parse_list_params(decode(query), r.filters, &["expand"])?;
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
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    // A write during pagination runs before the view is taken, so this page sees it.
    if (p.starting_after.is_some() || p.ending_before.is_some()) && sim.api.pagination_write() {
        let Sim {
            engine, generators, ..
        } = &mut *sim;
        generators.pagination_write(&scope, r.t, engine);
    }
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
    };
    // A required filter is always a parent field, so its children index serves it.
    let all = match r.required {
        Some(name) => view.store().children(r.t, name, &p.extra[name]),
        None => ordered(&view, r),
    };
    let (page, has_more) = paginate(
        &all,
        |o| {
            p.created.matches(o.created)
                && view
                    .listed(o)
                    .is_some_and(|u| matches(&view, r, o, &u.state, &p.extra, active))
        },
        &p,
    )?;
    let url = match r.required {
        Some(name) => format!("{}?{name}={}", r.path, p.extra[name]),
        None => r.path.to_string(),
    };
    let mut out = json!({
        "object": "list",
        "url": url,
        "has_more": has_more,
        "data": states(&view, &page),
    });
    expand(&view, r.t, &mut out, expand_paths(&p))?;
    Ok(Json(out))
}

/// Draws whether this object read is stale, and if so the time it shows.
pub(crate) fn stale_at(sim: &mut Sim) -> Option<SimTime> {
    let lag = sim.api.stale_lag()?;
    Some(sim.engine.now().plus_ms(-lag))
}

/// The states `view` shows for a page (every page entry is listed in `view`).
fn states(view: &View, page: &[&StoredObject]) -> Vec<Value> {
    page.iter()
        .filter_map(|o| view.listed(o))
        .map(|u| u.state.clone())
        .collect()
}

fn expand_paths(p: &ListParams) -> &[String] {
    p.extra_multi.get("expand").map_or(&[], Vec::as_slice)
}

fn no_such(t: ObjectType, id: &str) -> ApiError {
    ApiError {
        param: Some("id".to_string()),
        code: Some("resource_missing"),
        ..ApiError::not_found(format!("No such {}: '{id}'", t.as_str()))
    }
}

/// `GET {path}/{id}`.
async fn retrieve(
    r: &'static Resource,
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let mut paths = Vec::new();
    for (key, value) in decode(query) {
        if array_base(&key) != Some("expand") {
            return Err(ApiError::bad_request(
                format!("Received unknown parameter: {key}"),
                Some("parameter_unknown"),
                Some(&key),
            ));
        }
        paths.push(value);
    }
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
    };
    let mut obj = view.visible(r.t, &id).ok_or_else(|| no_such(r.t, &id))?;
    expand(&view, r.t, &mut obj, &paths)?;
    Ok(Json(obj))
}

/// `GET /v1/invoices/{id}/lines`.
pub(crate) async fn invoice_lines(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let p = parse_list_params(decode(query), &[], &["expand"])?;
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
    };
    if view.visible(ObjectType::Invoice, &id).is_none() {
        return Err(no_such(ObjectType::Invoice, &id));
    }
    let lines = view
        .store()
        .children(ObjectType::InvoiceLineItem, "invoice", &id);
    let (page, has_more) = paginate(&lines, |o| view.listed(o).is_some(), &p)?;
    let mut out = json!({
        "object": "list",
        "url": format!("/v1/invoices/{id}/lines"),
        "has_more": has_more,
        "data": states(&view, &page),
    });
    expand(
        &view,
        ObjectType::InvoiceLineItem,
        &mut out,
        expand_paths(&p),
    )?;
    Ok(Json(out))
}
