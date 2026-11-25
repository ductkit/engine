//! Read-only Stripe-compatible HTTP API (spec section 7).

mod destinations;
mod events_v1;
mod events_v2;
mod expand;
mod list;
mod resources;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};

use crate::PINNED_API_VERSION;
use crate::handle::Shared;

/// A Stripe-style error response.
#[derive(Debug, Clone)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    /// `invalid_request_error` except for v2 not-found errors.
    pub error_type: &'static str,
    pub message: String,
    pub code: Option<&'static str>,
    pub param: Option<String>,
}

impl ApiError {
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
            error_type: "invalid_request_error",
            message: message.into(),
            code: None,
            param: None,
        }
    }

    pub(crate) fn bad_request(
        message: impl Into<String>,
        code: Option<&'static str>,
        param: Option<&str>,
    ) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            message: message.into(),
            code,
            param: param.map(str::to_string),
        }
    }

    fn unauthorized() -> Self {
        ApiError {
            status: StatusCode::UNAUTHORIZED,
            error_type: "invalid_request_error",
            message: "Invalid API Key provided.".to_string(),
            code: None,
            param: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = Map::new();
        error.insert("type".into(), json!(self.error_type));
        error.insert("message".into(), json!(self.message));
        if let Some(code) = self.code {
            error.insert("code".into(), json!(code));
        }
        if let Some(param) = self.param {
            error.insert("param".into(), json!(param));
        }
        (
            self.status,
            axum::Json(json!({ "error": Value::Object(error) })),
        )
            .into_response()
    }
}

/// The router with every route and the request gate.
pub(crate) fn router(shared: Arc<Shared>) -> Router {
    let mut router = Router::new();
    for r in [
        &resources::CUSTOMERS,
        &resources::PRODUCTS,
        &resources::PRICES,
        &resources::SUBSCRIPTIONS,
        &resources::SUBSCRIPTION_ITEMS,
        &resources::INVOICES,
    ] {
        router = resources::routes(router, r);
    }
    router
        .route("/v1/invoices/{id}/lines", get(resources::invoice_lines))
        .route("/v1/events", get(events_v1::list))
        .route("/v1/events/{id}", get(events_v1::retrieve))
        .route("/v1/webhook_endpoints", get(destinations::list_v1))
        .route("/v2/core/event_destinations", get(destinations::list_v2))
        .route("/v2/core/events", get(events_v2::list))
        .route("/v2/core/events/{id}", get(events_v2::retrieve))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(shared.clone(), gate))
        .with_state(shared)
}

async fn fallback(req: Request) -> ApiError {
    ApiError::not_found(format!(
        "Unrecognized request URL ({}: {}).",
        req.method(),
        req.uri().path()
    ))
}

/// Checks that apply to every request, in spec order, and sets `Request-Id`.
async fn gate(State(shared): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let request_id = shared.lock().api_rng.id("req");
    let mut response = match check(&shared, &req) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    };
    response.headers_mut().insert(
        "request-id",
        HeaderValue::from_str(&request_id).expect("ascii id"),
    );
    response
}

fn check(shared: &Shared, req: &Request) -> Result<(), ApiError> {
    let headers = req.headers();
    let key = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    if !shared.config.api.keys.iter().any(|k| k == key) {
        return Err(ApiError::unauthorized());
    }
    if let Some(v) = headers.get("stripe-version")
        && v.as_bytes() != PINNED_API_VERSION.as_bytes()
    {
        return Err(ApiError::bad_request(
            format!(
                "Invalid Stripe API version: {}. This mock only serves {PINNED_API_VERSION}.",
                v.to_str().unwrap_or("<non-ascii>")
            ),
            None,
            None,
        ));
    }
    if headers.contains_key("stripe-account") {
        return Err(ApiError::bad_request(
            "Connected accounts are not enabled in this mock.",
            None,
            None,
        ));
    }
    if req.method() != Method::GET {
        return Err(ApiError::bad_request(
            format!(
                "This mock is read-only: {} {} is not supported.",
                req.method(),
                req.uri().path()
            ),
            None,
            None,
        ));
    }
    Ok(())
}
