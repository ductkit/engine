//! `GET /v1/events` and `GET /v1/events/{id}` (spec section 8.1).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde_json::{Value, json};

use crate::api::ApiError;
use crate::api::list::decode;
use crate::api::list::{paginate, parse_list_params};
use crate::engine::Engine;
use crate::engine::events::{EventFormat, EventRecord};
use crate::handle::Shared;

/// `pattern` matches `event_type` exactly, as `*`, or as a `prefix.*` wildcard.
pub(crate) fn type_matches(pattern: &str, event_type: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => event_type.starts_with(prefix),
        None => pattern == event_type,
    }
}

/// The served form of a snapshot event: its payload with the current `pending_webhooks`.
pub(crate) fn served(engine: &Engine, r: &EventRecord) -> Value {
    let mut v = r.payload.clone();
    v["pending_webhooks"] = json!(engine.pending_webhooks(&r.id));
    v
}

fn missing_cursor(param: &str, id: &str) -> ApiError {
    ApiError::bad_request(
        format!("No such event: '{id}'"),
        Some("resource_missing"),
        Some(param),
    )
}

pub(crate) async fn list(
    State(shared): State<Arc<Shared>>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let p = parse_list_params(decode(query), &["type", "delivery_success"], &["types"])?;
    let single = p.extra.get("type").cloned();
    let types = p.extra_multi.get("types").cloned();
    if single.is_some() && types.is_some() {
        return Err(ApiError::bad_request(
            "You may only specify one of these parameters: type, types.",
            None,
            None,
        ));
    }
    if types.as_ref().is_some_and(|t| t.len() > 20) {
        return Err(ApiError::bad_request(
            "You may pass at most 20 types.",
            Some("parameter_invalid"),
            Some("types"),
        ));
    }
    let delivery_success = match p.extra.get("delivery_success").map(String::as_str) {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => {
            return Err(ApiError::bad_request(
                "Invalid boolean: delivery_success",
                Some("parameter_invalid_boolean"),
                Some("delivery_success"),
            ));
        }
    };
    let sim = shared.lock();
    let engine = &sim.engine;
    let (now, retention) = (engine.now(), engine.retention_ms());
    let store = engine.events();
    for (param, cursor) in [
        ("starting_after", &p.starting_after),
        ("ending_before", &p.ending_before),
    ] {
        if let Some(id) = cursor {
            let listed = store.get(id).is_some_and(|r| {
                r.format == EventFormat::Snapshot && store.is_listed(r, now, retention)
            });
            if !listed {
                return Err(missing_cursor(param, id));
            }
        }
    }
    let ordered = store.list_order(EventFormat::Snapshot);
    let (page, has_more) = paginate(
        &ordered,
        |r| {
            store.is_listed(r, now, retention)
                && p.created.matches(r.created.unix_seconds())
                && single
                    .as_deref()
                    .is_none_or(|t| type_matches(t, &r.event_type))
                && types
                    .as_ref()
                    .is_none_or(|ts| ts.iter().any(|t| type_matches(t, &r.event_type)))
                && delivery_success.is_none_or(|want| (engine.pending_webhooks(&r.id) == 0) == want)
        },
        &p,
    )?;
    let data: Vec<Value> = page.iter().map(|r| served(engine, r)).collect();
    Ok(Json(json!({
        "object": "list",
        "url": "/v1/events",
        "has_more": has_more,
        "data": data,
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
            if r.format == EventFormat::Snapshot
                && store.is_listed(r, engine.now(), engine.retention_ms()) =>
        {
            Ok(Json(served(engine, r)))
        }
        _ => Err(ApiError {
            param: Some("id".to_string()),
            code: Some("resource_missing"),
            ..ApiError::not_found(format!("No such event: '{id}'"))
        }),
    }
}
