//! `GET /v2/core/accounts` and `GET /v2/core/accounts/{id}` (spec section 5.4).
//!
//! `/v1/accounts` is an ordinary resource (`resources::ACCOUNTS`). Both read the
//! platform's Account objects through a [`View`], so stale reads apply.

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::events_v2::{decode_token, encode_token, window};
use crate::api::list::decode;
use crate::api::resources::{View, stale_at};
use crate::api::{ApiError, RequestScope};
use crate::engine::store::StoredObject;
use crate::engine::{ChangeKind, StateUpdate};
use crate::handle::{Shared, Sim};
use crate::model::account::{self, AccountMeta};
use crate::{Configuration, ObjectType};

/// Filters and cursor, carried inside the opaque page token.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Query {
    limit: usize,
    closed: bool,
    applied_configurations: Vec<String>,
    /// `"after"` or `"before"`, with `cursor`.
    dir: Option<String>,
    cursor: Option<String>,
}

fn bad(message: impl Into<String>, code: &'static str, param: &str) -> ApiError {
    ApiError::bad_request(message, Some(code), Some(param))
}

fn parse_query(pairs: Vec<(String, String)>) -> Result<Query, ApiError> {
    if let Some((_, token)) = pairs.iter().find(|(k, _)| k == "page") {
        if pairs.len() > 1 {
            return Err(ApiError::bad_request(
                "page cannot be combined with other parameters",
                None,
                Some("page"),
            ));
        }
        return decode_token(token);
    }
    let mut q = Query {
        limit: 10,
        ..Query::default()
    };
    for (key, value) in pairs {
        match key.as_str() {
            "limit" => {
                q.limit = value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=100).contains(n))
                    .ok_or_else(|| {
                        bad(
                            "limit must be an integer between 1 and 100.",
                            "parameter_invalid_integer",
                            "limit",
                        )
                    })?;
            }
            "closed" => {
                q.closed = match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(bad(
                            "Invalid boolean: closed",
                            "parameter_invalid_boolean",
                            "closed",
                        ));
                    }
                };
            }
            "applied_configurations" => {
                for name in value.split(',').filter(|n| !n.is_empty()) {
                    if ![
                        Configuration::Customer,
                        Configuration::Merchant,
                        Configuration::Recipient,
                    ]
                    .iter()
                    .any(|c| c.as_str() == name)
                    {
                        return Err(bad(
                            format!("Invalid configuration: {name}"),
                            "parameter_invalid",
                            "applied_configurations",
                        ));
                    }
                    q.applied_configurations.push(name.to_string());
                }
            }
            _ => {
                return Err(bad(
                    format!("Received unknown parameter: {key}"),
                    "parameter_unknown",
                    &key,
                ));
            }
        }
    }
    Ok(q)
}

fn page_url(q: &Query, dir: &str, cursor: &str) -> String {
    let token = Query {
        dir: Some(dir.to_string()),
        cursor: Some(cursor.to_string()),
        ..q.clone()
    };
    format!("/v2/core/accounts?page={}", encode_token(&token))
}

/// The version of `o` this view shows, and whether the account is closed in it. A
/// closed (disconnected) account shows its last live version.
fn live_version<'a>(view: &View<'a>, o: &'a StoredObject) -> Option<(&'a StateUpdate, bool)> {
    let shown = view.state(o)?;
    if shown.kind != ChangeKind::Deleted {
        return Some((shown, false));
    }
    let log = view.engine.log();
    let last = o
        .versions
        .iter()
        .rev()
        .map(|&pos| &log[pos])
        .find(|u| u.kind != ChangeKind::Deleted)?;
    Some((last, true))
}

fn no_such(id: &str) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        error_type: "not_found",
        message: format!("No such account: '{id}'"),
        code: Some("not_found"),
        param: None,
    }
}

/// `GET /v2/core/accounts`.
pub(crate) async fn list_v2(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let q = parse_query(decode(query))?;
    let mut sim = shared.lock();
    // A write during pagination runs before the view is taken, so this page sees it.
    if q.cursor.is_some() && sim.api.pagination_write() {
        let Sim {
            engine, generators, ..
        } = &mut *sim;
        generators.pagination_write(&scope, ObjectType::Account, engine);
    }
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
    };
    let mut items: Vec<(&str, &StateUpdate, &AccountMeta, bool)> = Vec::new();
    for o in view.store().list_order(ObjectType::Account) {
        let (Some((u, closed)), Some(meta)) = (
            live_version(&view, o),
            view.engine.account_meta(&o.latest.object_id),
        ) else {
            continue;
        };
        let configured = q.applied_configurations.is_empty()
            || meta
                .configurations
                .iter()
                .any(|c| q.applied_configurations.iter().any(|w| w == c.as_str()));
        if closed == q.closed && configured {
            items.push((&o.latest.object_id, u, meta, closed));
        }
    }
    let ids: Vec<&str> = items.iter().map(|(id, ..)| *id).collect();
    let (start, end) =
        window(&ids, q.limit, q.dir.as_deref(), q.cursor.as_deref()).ok_or_else(|| {
            let cursor = q.cursor.as_deref().unwrap_or_default();
            bad(
                format!("No such account: '{cursor}'"),
                "resource_missing",
                "page",
            )
        })?;
    let page = &items[start..end];
    let next = (end < items.len())
        .then(|| page.last().map(|(id, ..)| page_url(&q, "after", id)))
        .flatten();
    let previous = (q.cursor.is_some() && start > 0)
        .then(|| page.first().map(|(id, ..)| page_url(&q, "before", id)))
        .flatten();
    let data: Vec<Value> = page
        .iter()
        .map(|(_, u, meta, closed)| account::v2_view(&u.state, meta, *closed))
        .collect();
    Ok(Json(json!({
        "data": data,
        "next_page_url": next,
        "previous_page_url": previous,
    })))
}

/// `GET /v2/core/accounts/{id}`. A closed account is still served, with `closed: true`.
pub(crate) async fn retrieve_v2(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let mut sim = shared.lock();
    let at = stale_at(&mut sim);
    let view = View {
        engine: &sim.engine,
        at,
        scope,
    };
    let (u, closed) = view
        .store()
        .get(ObjectType::Account, &id)
        .and_then(|o| live_version(&view, o))
        .ok_or_else(|| no_such(&id))?;
    let meta = view.engine.account_meta(&id).ok_or_else(|| no_such(&id))?;
    Ok(Json(account::v2_view(&u.state, meta, closed)))
}
