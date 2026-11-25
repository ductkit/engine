//! `GET /v2/core/events` and `GET /v2/core/events/{id}` (spec section 8.2).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::SimTime;
use crate::api::ApiError;
use crate::api::customers::decode;
use crate::api::events_v1::type_matches;
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
        return hex::decode(token)
            .ok()
            .and_then(|b| serde_json::from_slice::<Query>(&b).ok())
            .ok_or_else(|| bad("Invalid page token.", "parameter_invalid", "page"));
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
    let bytes = serde_json::to_vec(&token).expect("token serializes");
    format!("/v2/core/events?page={}", hex::encode(bytes))
}

pub(crate) async fn list(
    State(shared): State<Arc<Shared>>,
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
        .filter(|r| store.is_listed(r, now, retention) && matches(&q, r))
        .collect();
    let (start, end) = match (q.dir.as_deref(), q.cursor.as_deref()) {
        (Some(dir), Some(cursor)) => {
            let pos = filtered
                .iter()
                .position(|r| r.id == cursor)
                .ok_or_else(|| {
                    bad(
                        format!("No such event: '{cursor}'"),
                        "resource_missing",
                        "page",
                    )
                })?;
            if dir == "before" {
                (pos.saturating_sub(q.limit), pos)
            } else {
                (pos + 1, (pos + 1 + q.limit).min(filtered.len()))
            }
        }
        _ => (0, q.limit.min(filtered.len())),
    };
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
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let sim = shared.lock();
    let engine = &sim.engine;
    let store = engine.events();
    match store.get(&id) {
        Some(r)
            if r.format == EventFormat::Thin
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
