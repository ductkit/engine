//! `GET /v1/customers/{id}` and `GET /v1/customers`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde_json::{Value, json};

use crate::ObjectType;
use crate::api::ApiError;
use crate::api::list::{paginate, parse_list_params};
use crate::handle::Shared;

fn decode(query: Option<String>) -> Vec<(String, String)> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

pub(crate) async fn retrieve(
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
    match sim.engine.latest(ObjectType::Customer, &id) {
        Some(latest) => Ok(Json(latest.state.clone())),
        None => Err(ApiError {
            param: Some("id".to_string()),
            code: Some("resource_missing"),
            ..ApiError::not_found(format!("No such customer: '{id}'"))
        }),
    }
}

pub(crate) async fn list(
    State(shared): State<Arc<Shared>>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let p = parse_list_params(decode(query), &["email"])?;
    let email = p.extra.get("email").cloned();
    let sim = shared.lock();
    let ordered = sim.engine.store().list_order(ObjectType::Customer);
    let (page, has_more) = paginate(
        &ordered,
        |o| {
            !o.deleted
                && p.created.matches(o.created)
                && email
                    .as_deref()
                    .is_none_or(|e| o.latest.state["email"].as_str() == Some(e))
        },
        &p,
    )?;
    let data: Vec<Value> = page.iter().map(|o| o.latest.state.clone()).collect();
    Ok(Json(json!({
        "object": "list",
        "url": "/v1/customers",
        "has_more": has_more,
        "data": data,
    })))
}
