//! `GET /v2/core/accounts` and `GET /v2/core/accounts/{id}` (spec section 5.4).
//!
//! `/v1/accounts` is an ordinary resource (`resources::ACCOUNTS`). Both read the
//! platform's Account objects through a [`View`], so stale reads apply.

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::events_v2::{decode_token, encode_token};
use crate::api::list::decode;
use crate::api::resources::{View, stale_at};
use crate::api::{ApiError, RequestScope};
use crate::control::ControlOp;
use crate::engine::store::StoredObject;
use crate::engine::{ChangeKind, StateUpdate};
use crate::handle::{Shared, Sim};
use crate::model::account::{self, AccountMeta};
use crate::stripe;
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
    ApiError::v2_not_found(format!("No such account: '{id}'"))
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
        // Journalled first, so a replay runs the same write at the same time.
        sim.record(ControlOp::PaginationWrite {
            account: scope.clone(),
            object_type: ObjectType::Account,
        });
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
    // Every account in list order; `item` is set for those this view shows and the
    // filters keep. The cursor is located among all accounts, so it survives the
    // account closing or the view predating it.
    type Item<'a> = (&'a StateUpdate, &'a AccountMeta, bool);
    let mut entries: Vec<(&str, Option<Item>)> = Vec::new();
    for o in view.store().list_order(ObjectType::Account) {
        let id = o.latest.object_id.as_str();
        let item = match (live_version(&view, o), view.engine.account_meta(id)) {
            (Some((u, closed)), Some(meta)) => {
                let configured = q.applied_configurations.is_empty()
                    || meta
                        .configurations
                        .iter()
                        .any(|c| q.applied_configurations.iter().any(|w| w == c.as_str()));
                (closed == q.closed && configured).then_some((u, meta, closed))
            }
            _ => None,
        };
        entries.push((id, item));
    }
    let pos = match q.cursor.as_deref() {
        None => None,
        Some(c) => Some(entries.iter().position(|(id, _)| *id == c).ok_or_else(|| {
            bad(
                format!("No such account: '{c}'"),
                "resource_missing",
                "page",
            )
        })?),
    };
    let kept = |range: std::ops::Range<usize>| -> Vec<usize> {
        range.filter(|i| entries[*i].1.is_some()).collect()
    };
    let n = entries.len();
    let picked: Vec<usize> = match (q.dir.as_deref(), pos) {
        (Some("before"), Some(p)) => {
            let before = kept(0..p);
            before[before.len().saturating_sub(q.limit)..].to_vec()
        }
        (Some(_), Some(p)) => kept(p + 1..n).into_iter().take(q.limit).collect(),
        _ => kept(0..n).into_iter().take(q.limit).collect(),
    };
    let page: Vec<(&StateUpdate, &AccountMeta, bool)> =
        picked.iter().filter_map(|i| entries[*i].1).collect();
    let next = picked
        .last()
        .filter(|last| !kept(**last + 1..n).is_empty())
        .map(|last| page_url(&q, "after", entries[*last].0));
    let previous = picked
        .first()
        .filter(|first| !kept(0..**first).is_empty())
        .map(|first| page_url(&q, "before", entries[*first].0));
    let data: Vec<stripe::V2CoreAccount> = page
        .iter()
        .map(|(u, meta, closed)| account::v2_view(&account::from_state(&u.state), meta, *closed))
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
) -> Result<Json<stripe::V2CoreAccount>, ApiError> {
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
    Ok(Json(account::v2_view(
        &account::from_state(&u.state),
        meta,
        closed,
    )))
}
