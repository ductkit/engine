//! Read-only Stripe-compatible HTTP API (spec section 7).

mod destinations;
mod events_v1;
mod events_v2;
mod expand;
mod list;
mod resources;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value, json};

use crate::PINNED_API_VERSION;
use crate::faults::api::Fault;
use crate::handle::Shared;

/// How long a `timeout` fault holds the response: Stripe's own request limit.
const TIMEOUT_HOLD: Duration = Duration::from_secs(80);

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

/// Checks that apply to every request, in spec order, then the injected faults
/// (rate limit, error, latency) for requests that pass, and sets `Request-Id`.
async fn gate(State(shared): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let (request_id, decision) = {
        let mut sim = shared.lock();
        let request_id = sim.api_rng.id("req");
        let decision = check(&sim.api.config().keys, &req).map(|()| sim.api.decide(Instant::now()));
        (request_id, decision)
    };
    let mut response = match decision {
        Err(e) => e.into_response(),
        Ok((Some(Fault::Timeout), _)) => {
            // Hold before any response exists, so no status line is ever sent;
            // then abort the connection like a reset.
            let mut shutdown = shared.shutdown.subscribe();
            tokio::select! {
                _ = tokio::time::sleep(TIMEOUT_HOLD) => {}
                _ = shutdown.wait_for(|stopping| *stopping) => {}
            }
            fault_response(Fault::Reset)
        }
        Ok((Some(fault), _)) => fault_response(fault),
        Ok((None, latency_ms)) => {
            if latency_ms > 0 {
                tokio::time::sleep(Duration::from_millis(latency_ms)).await;
            }
            next.run(req).await
        }
    };
    response.headers_mut().insert(
        "request-id",
        HeaderValue::from_str(&request_id).expect("ascii id"),
    );
    response
}

/// The response for an injected fault.
fn fault_response(fault: Fault) -> Response {
    let retryable = |status: StatusCode, error_type, code, message: &str| {
        let mut r = ApiError {
            status,
            error_type,
            message: message.to_string(),
            code,
            param: None,
        }
        .into_response();
        r.headers_mut()
            .insert("stripe-should-retry", HeaderValue::from_static("true"));
        r
    };
    let broken = |body: Body| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(body)
            .expect("static response")
    };
    let reset = || std::io::Error::new(std::io::ErrorKind::ConnectionReset, "injected fault");
    match fault {
        Fault::RateLimited => retryable(
            StatusCode::TOO_MANY_REQUESTS,
            "invalid_request_error",
            Some("rate_limit"),
            "Too many requests hit the API too quickly. We recommend an exponential backoff of your requests.",
        ),
        Fault::ServerError => retryable(
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
            None,
            "An unknown error occurred.",
        ),
        Fault::Unavailable => retryable(
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            None,
            "The server is temporarily unavailable. Please retry.",
        ),
        // A body that fails on its first poll makes hyper drop the connection
        // without writing the head. `Timeout` reaches here after its hold.
        Fault::Reset | Fault::Timeout => {
            broken(Body::from_stream(futures_util::stream::once(async move {
                Err::<Bytes, _>(reset())
            })))
        }
    }
}

fn check(keys: &[String], req: &Request) -> Result<(), ApiError> {
    let headers = req.headers();
    let key = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    if !keys.iter().any(|k| k == key) {
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
