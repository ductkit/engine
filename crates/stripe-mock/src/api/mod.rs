//! Read-only Stripe-compatible HTTP API (spec section 7).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::PINNED_API_VERSION;
use crate::handle::Shared;

/// A Stripe-style error response.
#[derive(Debug, Clone)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub code: Option<&'static str>,
    pub param: Option<String>,
}

impl ApiError {
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
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
            message: message.into(),
            code,
            param: param.map(str::to_string),
        }
    }

    fn unauthorized() -> Self {
        ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "Invalid API Key provided.".to_string(),
            code: None,
            param: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = Map::new();
        error.insert("type".into(), json!("invalid_request_error"));
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
    Router::new()
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
