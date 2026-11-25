//! Read-only `GET /v1/webhook_endpoints` and `GET /v2/core/event_destinations`.

use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::{Extension, Json};
use serde_json::{Value, json};

use crate::api::list::decode;
use crate::api::list::{Cursorable, paginate, parse_list_params};
use crate::api::{ApiError, RequestScope};
use crate::delivery::Destination;
use crate::handle::Shared;
use crate::{PINNED_API_VERSION, PayloadFormat};

impl Cursorable for Destination {
    fn cursor_id(&self) -> &str {
        &self.info.id
    }
}

fn status(d: &Destination) -> &'static str {
    if d.info.enabled {
        "enabled"
    } else {
        "disabled"
    }
}

fn v1_object(d: &Destination) -> Value {
    json!({
        "id": d.info.id,
        "object": "webhook_endpoint",
        "api_version": PINNED_API_VERSION,
        "application": null,
        "created": d.info.created.unix_seconds(),
        "description": d.info.name,
        "enabled_events": d.info.enabled_events,
        "livemode": false,
        "metadata": {},
        "status": status(d),
        "url": d.info.url,
    })
}

fn v2_object(d: &Destination) -> Value {
    json!({
        "id": d.info.id,
        "object": "v2.core.event_destination",
        "name": d.info.name,
        "description": "",
        "type": "webhook_endpoint",
        "event_payload": "thin",
        "enabled_events": d.info.enabled_events,
        "events_from": ["@self"],
        "snapshot_api_version": null,
        "status": status(d),
        "status_details": null,
        "webhook_endpoint": {"url": d.info.url, "signing_secret": null},
        "livemode": false,
        "metadata": {},
        "created": d.info.created.to_string(),
        "updated": d.info.created.to_string(),
    })
}

/// Destinations belong to the platform; a connected account has none of its own.
pub(crate) async fn list_v1(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    RawQuery(query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    let p = parse_list_params(decode(query), &[], &[])?;
    let sim = shared.lock();
    // Newest first, like other v1 lists.
    let ordered: Vec<&Destination> = sim
        .engine
        .destinations()
        .iter()
        .filter(|d| scope.is_none() && d.info.payload == PayloadFormat::Snapshot)
        .rev()
        .collect();
    let (page, has_more) = paginate(&ordered, |_| true, &p)?;
    let data: Vec<Value> = page.iter().map(|d| v1_object(d)).collect();
    Ok(Json(json!({
        "object": "list",
        "url": "/v1/webhook_endpoints",
        "has_more": has_more,
        "data": data,
    })))
}

pub(crate) async fn list_v2(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
) -> Json<Value> {
    let sim = shared.lock();
    let data: Vec<Value> = sim
        .engine
        .destinations()
        .iter()
        .filter(|d| scope.is_none() && d.info.payload == PayloadFormat::Thin)
        .map(v2_object)
        .collect();
    Json(json!({
        "data": data,
        "next_page_url": null,
        "previous_page_url": null,
    }))
}
