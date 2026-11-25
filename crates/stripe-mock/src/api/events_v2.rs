//! `GET /v2/core/events` and `GET /v2/core/events/{id}` (spec section 8.2).

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::SimTime;
use crate::api::events_v1::type_matches;
use crate::api::list::decode;
use crate::api::{ApiError, RequestScope};
use crate::engine::events::{EventFormat, EventRecord};
use crate::handle::Shared;

/// Filters and cursor, carried inside the opaque page token.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Query {
    limit: usize,
    types: Vec<String>,
    object_id: Option<String>,
    created: Created,
    /// `"after"` or `"before"`, with `cursor`.
    dir: Option<String>,
    cursor: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Created {
    gt: Option<String>,
    gte: Option<String>,
    lt: Option<String>,
    lte: Option<String>,
}

/// The opaque `page` token for a v2 list: the query and cursor, hex-encoded JSON.
pub(crate) fn encode_token<Q: Serialize>(q: &Q) -> String {
    hex::encode(serde_json::to_vec(q).expect("token serializes"))
}

pub(crate) fn decode_token<Q: DeserializeOwned>(token: &str) -> Result<Q, ApiError> {
    hex::decode(token)
        .ok()
        .and_then(|b| serde_json::from_slice::<Q>(&b).ok())
        .ok_or_else(|| bad("Invalid page token.", "parameter_invalid", "page"))
}

/// The slice of `ids` one v2 page covers, from its direction and cursor. `None`
/// when the cursor is not among `ids`.
pub(crate) fn window(
    ids: &[&str],
    limit: usize,
    dir: Option<&str>,
    cursor: Option<&str>,
) -> Option<(usize, usize)> {
    Some(match (dir, cursor) {
        (Some(dir), Some(cursor)) => {
            let pos = ids.iter().position(|id| *id == cursor)?;
            if dir == "before" {
                (pos.saturating_sub(limit), pos)
            } else {
                (pos + 1, (pos + 1 + limit).min(ids.len()))
            }
        }
        _ => (0, limit.min(ids.len())),
    })
}

fn bad(message: impl Into<String>, code: &'static str, param: &str) -> ApiError {
    ApiError::bad_request(message, Some(code), Some(param))
}

fn parse_time(param: &str, v: &str) -> Result<SimTime, ApiError> {
    chrono::DateTime::parse_from_rfc3339(v)
        .map(|d| SimTime::from_datetime(d.with_timezone(&chrono::Utc)))
        .map_err(|_| {
            bad(
                format!("Invalid timestamp: {param}"),
                "parameter_invalid_timestamp",
                param,
            )
        })
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
        let is_types = key == "types"
            || key.strip_prefix("types[").is_some_and(|r| {
                r.strip_suffix(']')
                    .is_some_and(|i| i.bytes().all(|b| b.is_ascii_digit()))
            });
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
            "object_id" => q.object_id = Some(value),
            "created[gt]" | "created[gte]" | "created[lt]" | "created[lte]" => {
                parse_time(&key, &value)?;
                let slot = match key.as_str() {
                    "created[gt]" => &mut q.created.gt,
                    "created[gte]" => &mut q.created.gte,
                    "created[lt]" => &mut q.created.lt,
                    _ => &mut q.created.lte,
                };
                *slot = Some(value);
            }
            _ if is_types => q.types.push(value),
            _ => {
                return Err(bad(
                    format!("Received unknown parameter: {key}"),
                    "parameter_unknown",
                    &key,
                ));
            }
        }
    }
    if q.types.len() > 20 {
        return Err(bad(
            "You may pass at most 20 types.",
            "parameter_invalid",
            "types",
        ));
    }
    Ok(q)
}

fn matches(q: &Query, r: &EventRecord) -> bool {
    let t = |v: &Option<String>| v.as_deref().and_then(|s| parse_time("created", s).ok());
    let at = r.created;
    (q.types.is_empty() || q.types.iter().any(|p| type_matches(p, &r.event_type)))
        && q.object_id.as_deref().is_none_or(|id| id == r.object_id)
        && t(&q.created.gt).is_none_or(|v| at > v)
        && t(&q.created.gte).is_none_or(|v| at >= v)
        && t(&q.created.lt).is_none_or(|v| at < v)
        && t(&q.created.lte).is_none_or(|v| at <= v)
}

fn page_url(q: &Query, dir: &str, cursor: &str) -> String {
    let token = Query {
        dir: Some(dir.to_string()),
        cursor: Some(cursor.to_string()),
        ..q.clone()
    };
    format!("/v2/core/events?page={}", encode_token(&token))
}

pub(crate) async fn list(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let q = parse_query(decode(query))?;
    let sim = shared.lock();
    let engine = &sim.engine;
    let (now, retention) = (engine.now(), engine.retention_ms());
    let store = engine.events();
    let filtered: Vec<&EventRecord> = store
        .list_order(EventFormat::Thin)
        .into_iter()
        .filter(|r| r.account == scope && store.is_listed(r, now, retention) && matches(&q, r))
        .collect();
    let ids: Vec<&str> = filtered.iter().map(|r| r.id.as_str()).collect();
    let (start, end) =
        window(&ids, q.limit, q.dir.as_deref(), q.cursor.as_deref()).ok_or_else(|| {
            let cursor = q.cursor.as_deref().unwrap_or_default();
            bad(
                format!("No such event: '{cursor}'"),
                "resource_missing",
                "page",
            )
        })?;
    let page = &filtered[start..end];
    let next = (end < filtered.len())
        .then(|| page.last().map(|r| page_url(&q, "after", &r.id)))
        .flatten();
    let previous = (q.cursor.is_some() && start > 0)
        .then(|| page.first().map(|r| page_url(&q, "before", &r.id)))
        .flatten();
    let data: Vec<Value> = page.iter().map(|r| r.payload.clone()).collect();
    Ok(Json(json!({
        "data": data,
        "next_page_url": next,
        "previous_page_url": previous,
    })))
}

pub(crate) async fn retrieve(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let sim = shared.lock();
    let engine = &sim.engine;
    let store = engine.events();
    match store.get(&id) {
        Some(r)
            if r.format == EventFormat::Thin
                && r.account == scope
                && store.is_listed(r, engine.now(), engine.retention_ms()) =>
        {
            Ok(Json(r.payload.clone()))
        }
        _ => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            error_type: "not_found",
            message: format!("No such event: '{id}'"),
            code: Some("not_found"),
            param: None,
        }),
    }
}
