//! Retrieve and list for every object type, from per-type descriptors
//! (spec sections 7.2, 7.3).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Extension;
use axum::extract::{Path, RawQuery, State};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;

use crate::api::expand::expand;
use crate::api::list::{ListParams, array_base, decode, paginate, parse_list_params};
use crate::api::{ApiError, RequestScope, ensure_connected};
use crate::control::ControlOp;
use crate::engine::store::{ObjectStore, StoredObject};
use crate::engine::{Engine, Scope, StateUpdate};
use crate::handle::{Shared, Sim};
use crate::{ChangeKind, ObjectType, SimTime, stripe};

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

pub(crate) static PAYMENT_INTENTS: Resource = Resource {
    t: ObjectType::PaymentIntent,
    path: "/v1/payment_intents",
    filters: &["customer"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static CHARGES: Resource = Resource {
    t: ObjectType::Charge,
    path: "/v1/charges",
    filters: &["customer", "payment_intent", "transfer_group"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static REFUNDS: Resource = Resource {
    t: ObjectType::Refund,
    path: "/v1/refunds",
    filters: &["charge", "payment_intent"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static DISPUTES: Resource = Resource {
    t: ObjectType::Dispute,
    path: "/v1/disputes",
    filters: &["charge", "payment_intent"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static BALANCE_TRANSACTIONS: Resource = Resource {
    t: ObjectType::BalanceTransaction,
    path: "/v1/balance_transactions",
    filters: &["type", "source", "payout"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static PAYMENT_METHODS: Resource = Resource {
    t: ObjectType::PaymentMethod,
    path: "/v1/payment_methods",
    filters: &["customer", "type"],
    order: Order::CreatedDesc,
    required: Some("customer"),
};
pub(crate) static SETUP_INTENTS: Resource = Resource {
    t: ObjectType::SetupIntent,
    path: "/v1/setup_intents",
    filters: &["customer", "payment_method"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static CHECKOUT_SESSIONS: Resource = Resource {
    t: ObjectType::CheckoutSession,
    path: "/v1/checkout/sessions",
    filters: &["customer", "payment_intent", "subscription", "status"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static INVOICE_PAYMENTS: Resource = Resource {
    t: ObjectType::InvoicePayment,
    path: "/v1/invoice_payments",
    filters: &["invoice", "payment"],
    order: Order::CreatedDesc,
    required: None,
};

pub(crate) static TRANSFERS: Resource = Resource {
    t: ObjectType::Transfer,
    path: "/v1/transfers",
    filters: &["destination", "transfer_group"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static APPLICATION_FEES: Resource = Resource {
    t: ObjectType::ApplicationFee,
    path: "/v1/application_fees",
    filters: &["charge"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static PAYOUTS: Resource = Resource {
    t: ObjectType::Payout,
    path: "/v1/payouts",
    filters: &["status", "destination"],
    order: Order::CreatedDesc,
    required: None,
};

// Phase 9 ruling Q10.
pub(crate) static COUPONS: Resource = Resource {
    t: ObjectType::Coupon,
    path: "/v1/coupons",
    filters: &[],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static PROMOTION_CODES: Resource = Resource {
    t: ObjectType::PromotionCode,
    path: "/v1/promotion_codes",
    filters: &["active", "code", "coupon", "customer"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static INVOICE_ITEMS: Resource = Resource {
    t: ObjectType::InvoiceItem,
    path: "/v1/invoiceitems",
    filters: &["customer", "invoice", "pending"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static CREDIT_NOTES: Resource = Resource {
    t: ObjectType::CreditNote,
    path: "/v1/credit_notes",
    filters: &["customer", "invoice"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static TAX_RATES: Resource = Resource {
    t: ObjectType::TaxRate,
    path: "/v1/tax_rates",
    filters: &["active", "inclusive"],
    order: Order::CreatedDesc,
    required: None,
};
pub(crate) static SUBSCRIPTION_SCHEDULES: Resource = Resource {
    t: ObjectType::SubscriptionSchedule,
    path: "/v1/subscription_schedules",
    filters: &["customer"],
    order: Order::CreatedDesc,
    required: None,
};

/// Boolean filters other than `active`: checked when the list is parsed, compared as
/// `"true"`/`"false"` by [`matches`].
const BOOL_FILTERS: [&str; 2] = ["inclusive", "pending"];

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
    /// `payloads.nested_list_page_size`, for lists `expand[]` builds.
    pub page_size: usize,
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

    /// The state retrieve shows: a deleted customer is its stub, other deleted objects
    /// (and objects not yet created in this view) are missing. A deleted coupon is missing
    /// too: the pinned schema allows only `coupon` on its retrieve.
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
    // Stripe documents the promotion code `code` filter as case-insensitive.
    let eq_ci = |field: &str, key: &str| {
        f.get(key).is_none_or(|want| {
            s[field]
                .as_str()
                .is_some_and(|v| v.eq_ignore_ascii_case(want))
        })
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
        ObjectType::PaymentIntent => eq("customer", "customer"),
        ObjectType::SetupIntent => {
            eq("customer", "customer") && eq("payment_method", "payment_method")
        }
        ObjectType::Charge => {
            eq("customer", "customer")
                && eq("payment_intent", "payment_intent")
                && eq("transfer_group", "transfer_group")
        }
        ObjectType::Transfer => {
            eq("destination", "destination") && eq("transfer_group", "transfer_group")
        }
        ObjectType::ApplicationFee => eq("charge", "charge"),
        ObjectType::Payout => eq("status", "status") && eq("destination", "destination"),
        // Children are listed under their parent, whose handler applies any filter.
        ObjectType::TransferReversal
        | ObjectType::FeeRefund
        | ObjectType::BankAccount
        | ObjectType::Card
        | ObjectType::Person => true,
        ObjectType::Refund | ObjectType::Dispute => {
            eq("charge", "charge") && eq("payment_intent", "payment_intent")
        }
        // `payout` lists what that payout swept (phase 8 ruling P5), once the payout
        // exists in this view.
        ObjectType::BalanceTransaction => {
            eq("type", "type")
                && eq("source", "source")
                && f.get("payout").is_none_or(|po| {
                    view.store()
                        .get(ObjectType::Payout, po)
                        .and_then(|p| view.state(p))
                        .is_some()
                        && view
                            .engine
                            .payout_members(&view.scope, po)
                            .contains(&o.latest.object_id)
                })
        }
        // The children index keeps a detached payment method under the customer it was
        // created for, so the state shown must still name that customer.
        ObjectType::PaymentMethod => eq("customer", "customer") && eq("type", "type"),
        ObjectType::CheckoutSession => {
            eq("customer", "customer")
                && eq("payment_intent", "payment_intent")
                && eq("subscription", "subscription")
                && eq("status", "status")
        }
        ObjectType::Coupon => true,
        // `coupon` names the coupon the code promotes.
        ObjectType::PromotionCode => {
            active.is_none_or(|a| s["active"].as_bool() == Some(a))
                && eq_ci("code", "code")
                && eq("customer", "customer")
                && f.get("coupon")
                    .is_none_or(|want| s["promotion"]["coupon"].as_str() == Some(want))
        }
        // `pending=true` lists the items on no invoice yet, `false` the others.
        ObjectType::InvoiceItem => {
            eq("customer", "customer")
                && eq("invoice", "invoice")
                && f.get("pending")
                    .is_none_or(|want| (want == "true") == s["invoice"].is_null())
        }
        ObjectType::CreditNote => eq("customer", "customer") && eq("invoice", "invoice"),
        ObjectType::TaxRate => {
            active.is_none_or(|a| s["active"].as_bool() == Some(a))
                && f.get("inclusive")
                    .is_none_or(|want| s["inclusive"].as_bool() == Some(want == "true"))
        }
        ObjectType::SubscriptionSchedule => eq("customer", "customer"),
        // No list of their own: a discount is embedded, and credit note lines are listed
        // under their note from the engine's index.
        ObjectType::Discount | ObjectType::CreditNoteLineItem => true,
        // `payment` names the paid object: a payment intent, charge or payment record.
        ObjectType::InvoicePayment => {
            eq("invoice", "invoice")
                && f.get("payment").is_none_or(|want| {
                    ["payment_intent", "charge", "payment_record"]
                        .iter()
                        .any(|k| s["payment"][k].as_str() == Some(want))
                })
        }
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
    for name in BOOL_FILTERS {
        if r.filters.contains(&name) {
            bool_param(&p, name)?;
        }
    }
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    pagination_write(&mut sim, &scope, &p, r.t);
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
        page_size: sim.generators.nested_list_page_size(),
    };
    // A required filter is always a parent field, so its children index serves it.
    let all = match r.required {
        Some(name) => {
            let mut children = view.store().children(r.t, name, &p.extra[name]);
            if r.order == Order::CreatedDesc {
                children.reverse();
            }
            children
        }
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
    let mut out = v1_list(r.path, has_more, states(&view, &page));
    expand(&view, r.t, &mut out, expand_paths(&p))?;
    Ok(Json(out))
}

/// Whether `id` of type `t` exists in `scope` now and is not deleted. Unlike a view, it
/// draws nothing (no stale read), so a handler can validate before it writes.
fn exists_now(sim: &Sim, scope: &Scope, t: ObjectType, id: &str) -> bool {
    sim.engine
        .scope_store(scope)
        .get(t, id)
        .is_some_and(|o| !o.deleted)
}

/// A write during pagination of a `t` list: on a page after the first, when drawn. It
/// runs before the view is taken, so this page sees it.
fn pagination_write(sim: &mut Sim, scope: &Scope, p: &ListParams, t: ObjectType) {
    if (p.starting_after.is_some() || p.ending_before.is_some()) && sim.api.pagination_write() {
        // Journalled first, so a replay runs the same write at the same time.
        sim.record(ControlOp::PaginationWrite {
            account: scope.clone(),
            object_type: t,
        });
        let Sim {
            engine, generators, ..
        } = sim;
        generators.pagination_write(scope, t, engine);
    }
}

/// A v1 list page, serialized from the typed envelope.
pub(crate) fn v1_list(url: impl Into<String>, has_more: bool, data: Vec<Value>) -> Value {
    serde_json::to_value(stripe::List {
        object: stripe::ListObject::List,
        data,
        has_more,
        url: url.into(),
        total_count: None,
    })
    .expect("a list serializes")
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

/// The `expand[]` paths of a retrieve, whose only parameter is `expand`.
fn expand_only(query: Option<String>) -> Result<Vec<String>, ApiError> {
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
    Ok(paths)
}

/// `GET {path}/{id}`.
async fn retrieve(
    r: &'static Resource,
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let paths = expand_only(query)?;
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
        page_size: sim.generators.nested_list_page_size(),
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
        page_size: sim.generators.nested_list_page_size(),
    };
    if view.visible(ObjectType::Invoice, &id).is_none() {
        return Err(no_such(ObjectType::Invoice, &id));
    }
    let lines = view
        .store()
        .children(ObjectType::InvoiceLineItem, "invoice", &id);
    let (page, has_more) = paginate(&lines, |o| view.listed(o).is_some(), &p)?;
    let mut out = v1_list(
        format!("/v1/invoices/{id}/lines"),
        has_more,
        states(&view, &page),
    );
    expand(
        &view,
        ObjectType::InvoiceLineItem,
        &mut out,
        expand_paths(&p),
    )?;
    Ok(Json(out))
}

/// `GET /v1/credit_notes/{id}/lines`: the note's line items, in creation order, from the
/// engine's index (phase 9 ruling Q8).
pub(crate) async fn credit_note_lines(
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
        page_size: sim.generators.nested_list_page_size(),
    };
    if view.visible(ObjectType::CreditNote, &id).is_none() {
        return Err(no_such(ObjectType::CreditNote, &id));
    }
    let lines: Vec<&StoredObject> = view
        .engine
        .credit_note_lines(&view.scope, &id)
        .iter()
        .filter_map(|l| view.store().get(ObjectType::CreditNoteLineItem, l))
        .collect();
    let (page, has_more) = paginate(&lines, |o| view.listed(o).is_some(), &p)?;
    let mut out = v1_list(
        format!("/v1/credit_notes/{id}/lines"),
        has_more,
        states(&view, &page),
    );
    expand(
        &view,
        ObjectType::CreditNoteLineItem,
        &mut out,
        expand_paths(&p),
    )?;
    Ok(Json(out))
}

/// `GET /v1/customers/{id}/payment_methods`.
pub(crate) async fn customer_payment_methods(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let r = &PAYMENT_METHODS;
    let p = parse_list_params(decode(query), &["type"], &["expand"])?;
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    pagination_write(&mut sim, &scope, &p, r.t);
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
        page_size: sim.generators.nested_list_page_size(),
    };
    if view.visible(ObjectType::Customer, &id).is_none() {
        return Err(no_such(ObjectType::Customer, &id));
    }
    let mut all = view.store().children(r.t, "customer", &id);
    all.reverse();
    let mut filters = p.extra.clone();
    filters.insert("customer".to_string(), id.clone());
    let (page, has_more) = paginate(
        &all,
        |o| {
            p.created.matches(o.created)
                && view
                    .listed(o)
                    .is_some_and(|u| matches(&view, r, o, &u.state, &filters, None))
        },
        &p,
    )?;
    let mut out = v1_list(
        format!("/v1/customers/{id}/payment_methods"),
        has_more,
        states(&view, &page),
    );
    expand(&view, r.t, &mut out, expand_paths(&p))?;
    Ok(Json(out))
}

/// `GET /v1/transfers/{id}/reversals`.
pub(crate) async fn transfer_reversals(
    state: State<Arc<Shared>>,
    scope: Extension<RequestScope>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    sub_list(state, scope, &id, query, &Sub::TRANSFER_REVERSALS).await
}

/// `GET /v1/transfers/{id}/reversals/{rid}`.
pub(crate) async fn transfer_reversal(
    state: State<Arc<Shared>>,
    scope: Extension<RequestScope>,
    Path((id, rid)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    sub_retrieve(state, scope, &id, &rid, query, &Sub::TRANSFER_REVERSALS).await
}

/// `GET /v1/application_fees/{id}/refunds`.
pub(crate) async fn fee_refunds(
    state: State<Arc<Shared>>,
    scope: Extension<RequestScope>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    sub_list(state, scope, &id, query, &Sub::FEE_REFUNDS).await
}

/// `GET /v1/application_fees/{id}/refunds/{rid}`.
pub(crate) async fn fee_refund(
    state: State<Arc<Shared>>,
    scope: Extension<RequestScope>,
    Path((id, rid)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    sub_retrieve(state, scope, &id, &rid, query, &Sub::FEE_REFUNDS).await
}

/// A child collection under a parent that lives in the request's scope.
struct Sub {
    parent: ObjectType,
    child: ObjectType,
    /// The child's parent field.
    field: &'static str,
    /// The collection segment after the parent's path.
    segment: &'static str,
    base: &'static str,
}

impl Sub {
    const TRANSFER_REVERSALS: Sub = Sub {
        parent: ObjectType::Transfer,
        child: ObjectType::TransferReversal,
        field: "transfer",
        segment: "reversals",
        base: "/v1/transfers",
    };
    const FEE_REFUNDS: Sub = Sub {
        parent: ObjectType::ApplicationFee,
        child: ObjectType::FeeRefund,
        field: "fee",
        segment: "refunds",
        base: "/v1/application_fees",
    };
}

async fn sub_list(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    id: &str,
    query: Option<String>,
    sub: &Sub,
) -> Result<Json<Value>, ApiError> {
    let p = parse_list_params(decode(query), &[], &["expand"])?;
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    // Validate before writing: an unknown parent must not trigger a write.
    if !exists_now(&sim, &scope, sub.parent, id) {
        return Err(no_such(sub.parent, id));
    }
    pagination_write(&mut sim, &scope, &p, sub.child);
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
        page_size: sim.generators.nested_list_page_size(),
    };
    if view.visible(sub.parent, id).is_none() {
        return Err(no_such(sub.parent, id));
    }
    // Stripe lists a fee's refunds and a transfer's reversals newest first, like the
    // embedded lists.
    let mut all = view.store().children(sub.child, sub.field, id);
    all.reverse();
    let (page, has_more) = paginate(
        &all,
        |o| p.created.matches(o.created) && view.listed(o).is_some(),
        &p,
    )?;
    let mut out = v1_list(
        format!("{}/{id}/{}", sub.base, sub.segment),
        has_more,
        states(&view, &page),
    );
    expand(&view, sub.child, &mut out, expand_paths(&p))?;
    Ok(Json(out))
}

async fn sub_retrieve(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    id: &str,
    child_id: &str,
    query: Option<String>,
    sub: &Sub,
) -> Result<Json<Value>, ApiError> {
    let paths = expand_only(query)?;
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
        page_size: sim.generators.nested_list_page_size(),
    };
    if view.visible(sub.parent, id).is_none() {
        return Err(no_such(sub.parent, id));
    }
    let mut obj = view
        .visible(sub.child, child_id)
        .filter(|o| o[sub.field].as_str() == Some(id))
        .ok_or_else(|| no_such(sub.child, child_id))?;
    expand(&view, sub.child, &mut obj, &paths)?;
    Ok(Json(obj))
}

/// The view of an account sub-resource request: the account's own scope, which holds what
/// is listed. The account itself is checked in the platform's scope, at the same time.
fn account_view<'a>(sim: &'a mut Sim, acct: &str) -> Result<View<'a>, ApiError> {
    let at = stale_at(sim);
    let platform = View {
        engine: &sim.engine,
        at,
        scope: None,
        page_size: sim.generators.nested_list_page_size(),
    };
    if platform.visible(ObjectType::Account, acct).is_none() {
        return Err(no_such(ObjectType::Account, acct));
    }
    Ok(View {
        scope: Some(acct.to_string()),
        ..platform
    })
}

/// `GET /v1/accounts/{acct}/external_accounts`: bank accounts and cards together, in
/// creation order.
pub(crate) async fn external_accounts(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path(acct): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let p = parse_list_params(decode(query), &["object"], &["expand"])?;
    if let Some(want) = p.extra.get("object")
        && !["bank_account", "card"].contains(&want.as_str())
    {
        return Err(ApiError::bad_request(
            format!("Invalid object: {want}. Must be one of bank_account or card."),
            None,
            Some("object"),
        ));
    }
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    // Validate before writing: an unknown account must not trigger a write.
    if !exists_now(&sim, &None, ObjectType::Account, &acct) {
        return Err(no_such(ObjectType::Account, &acct));
    }
    pagination_write(&mut sim, &Some(acct.clone()), &p, ObjectType::BankAccount);
    let view = account_view(&mut sim, &acct)?;
    let mut all: Vec<&StoredObject> = [ObjectType::BankAccount, ObjectType::Card]
        .into_iter()
        .flat_map(|t| view.store().in_creation_order(t))
        .collect();
    // The log position of the first version is the global creation order.
    all.sort_by_key(|o| o.versions[0]);
    let (page, has_more) = paginate(
        &all,
        |o| {
            p.created.matches(o.created)
                && view.listed(o).is_some_and(|u| {
                    p.extra
                        .get("object")
                        .is_none_or(|want| u.state["object"].as_str() == Some(want))
                })
        },
        &p,
    )?;
    let mut out = v1_list(
        format!("/v1/accounts/{acct}/external_accounts"),
        has_more,
        states(&view, &page),
    );
    expand(&view, ObjectType::BankAccount, &mut out, expand_paths(&p))?;
    Ok(Json(out))
}

/// `GET /v1/accounts/{acct}/external_accounts/{id}`.
pub(crate) async fn external_account(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path((acct, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let paths = expand_only(query)?;
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    let view = account_view(&mut sim, &acct)?;
    let (t, mut obj) = [ObjectType::BankAccount, ObjectType::Card]
        .into_iter()
        .find_map(|t| view.visible(t, &id).map(|o| (t, o)))
        .ok_or_else(|| no_such(ObjectType::BankAccount, &id))?;
    expand(&view, t, &mut obj, &paths)?;
    Ok(Json(obj))
}

/// `GET /v1/accounts/{acct}/persons`.
pub(crate) async fn persons(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path(acct): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    // Phase 8 ruling P8: no `relationship[...]` filters, so they are `parameter_unknown`.
    let p = parse_list_params(decode(query), &[], &["expand"])?;
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    // Validate before writing: an unknown account must not trigger a write.
    if !exists_now(&sim, &None, ObjectType::Account, &acct) {
        return Err(no_such(ObjectType::Account, &acct));
    }
    pagination_write(&mut sim, &Some(acct.clone()), &p, ObjectType::Person);
    let view = account_view(&mut sim, &acct)?;
    let all = view.store().in_creation_order(ObjectType::Person);
    let (page, has_more) = paginate(
        &all,
        |o| p.created.matches(o.created) && view.listed(o).is_some(),
        &p,
    )?;
    let mut out = v1_list(
        format!("/v1/accounts/{acct}/persons"),
        has_more,
        states(&view, &page),
    );
    expand(&view, ObjectType::Person, &mut out, expand_paths(&p))?;
    Ok(Json(out))
}

/// `GET /v1/accounts/{acct}/persons/{id}`.
pub(crate) async fn person(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path((acct, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let paths = expand_only(query)?;
    let mut sim = shared.lock();
    ensure_connected(&sim.engine, &scope)?;
    let view = account_view(&mut sim, &acct)?;
    let mut obj = view
        .visible(ObjectType::Person, &id)
        .ok_or_else(|| no_such(ObjectType::Person, &id))?;
    expand(&view, ObjectType::Person, &mut obj, &paths)?;
    Ok(Json(obj))
}
