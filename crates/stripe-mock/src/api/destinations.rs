//! Read-only `GET /v1/webhook_endpoints` and `GET /v2/core/event_destinations`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::{Extension, Json};

use crate::api::list::decode;
use crate::api::list::{Cursorable, paginate, parse_list_params};
use crate::api::{ApiError, RequestScope};
use crate::delivery::Destination;
use crate::handle::Shared;
use crate::stripe;
use crate::{PINNED_API_VERSION, PayloadFormat};

impl Cursorable for Destination {
    fn cursor_id(&self) -> &str {
        &self.info.id
    }
}

/// A snapshot destination as a v1 webhook endpoint. `secret` is absent: Stripe returns it
/// only when the endpoint is created.
fn v1_object(d: &Destination) -> stripe::WebhookEndpoint {
    stripe::WebhookEndpoint {
        api_version: Some(Some(PINNED_API_VERSION.to_string())),
        application: Some(None),
        created: d.info.created.unix_seconds(),
        description: Some(Some(d.info.name.clone())),
        enabled_events: d.info.enabled_events.clone(),
        id: d.info.id.clone(),
        livemode: false,
        metadata: BTreeMap::new(),
        object: stripe::WebhookEndpointObject::WebhookEndpoint,
        secret: None,
        status: if d.info.enabled {
            "enabled"
        } else {
            "disabled"
        }
        .to_string(),
        url: d.info.url.clone(),
    }
}

/// A thin destination as a v2 event destination. Absent, because none is nullable and the
/// mock has no value: the EventBridge and Event Grid configs (webhook destinations only),
/// `snapshot_api_version` (snapshot destinations only), `status_details` (none of its
/// reasons, which name a missing AWS or Azure source or a user action, fits the mock's
/// disables) and `webhook_endpoint.signing_secret` (never shown on a read).
fn v2_object(d: &Destination) -> stripe::V2CoreEventDestination {
    stripe::V2CoreEventDestination {
        amazon_eventbridge: None,
        azure_event_grid: None,
        created: d.info.created.to_string(),
        description: String::new(),
        enabled_events: d.info.enabled_events.clone(),
        event_payload: stripe::V2CoreEventDestinationEventPayload::Thin,
        events_from: Some(d.info.events_from.clone()),
        id: d.info.id.clone(),
        livemode: false,
        metadata: Some(BTreeMap::new()),
        name: d.info.name.clone(),
        object: stripe::V2CoreEventDestinationObject::V2CoreEventDestination,
        snapshot_api_version: None,
        status: if d.info.enabled {
            stripe::V2CoreEventDestinationStatus::Enabled
        } else {
            stripe::V2CoreEventDestinationStatus::Disabled
        },
        status_details: None,
        type_: stripe::V2CoreEventDestinationType::WebhookEndpoint,
        updated: d.info.created.to_string(),
        webhook_endpoint: Some(stripe::V2CoreEventDestinationWebhookEndpoint {
            signing_secret: None,
            url: Some(d.info.url.clone()),
        }),
    }
}

/// Destinations belong to the platform; a connected account has none of its own.
pub(crate) async fn list_v1(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
    RawQuery(query): RawQuery,
) -> Result<Json<stripe::List<stripe::WebhookEndpoint>>, ApiError> {
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
    Ok(Json(stripe::List {
        object: stripe::ListObject::List,
        data: page.iter().map(|d| v1_object(d)).collect(),
        has_more,
        url: "/v1/webhook_endpoints".to_string(),
        total_count: None,
    }))
}

pub(crate) async fn list_v2(
    State(shared): State<Arc<Shared>>,
    Extension(RequestScope(scope)): Extension<RequestScope>,
) -> Json<stripe::V2List<stripe::V2CoreEventDestination>> {
    let sim = shared.lock();
    let data: Vec<stripe::V2CoreEventDestination> = sim
        .engine
        .destinations()
        .iter()
        .filter(|d| scope.is_none() && d.info.payload == PayloadFormat::Thin)
        .map(v2_object)
        .collect();
    Json(stripe::V2List {
        data,
        next_page_url: None,
        previous_page_url: None,
    })
}
