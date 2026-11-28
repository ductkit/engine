//! Read-only Stripe-compatible HTTP API (spec section 7).

mod accounts;
mod destinations;
mod events_v1;
mod events_v2;
pub(crate) mod expand;
mod list;
mod resources;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::PINNED_API_VERSION;
use crate::engine::{Engine, Scope};
use crate::faults::api::Fault;
use crate::handle::Shared;
use crate::stripe;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

/// How long a `timeout` fault holds the response: Stripe's own request limit.
const TIMEOUT_HOLD: Duration = Duration::from_secs(80);

/// The `type` of an error: one of the v1 schema's types, or a v2 not-found error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorType {
    V1(stripe::ApiErrorsType),
    /// `/v2/` retrieve of an unknown ID: `type` and `code` `not_found`.
    V2NotFound,
}

const INVALID_REQUEST: ErrorType = ErrorType::V1(stripe::ApiErrorsType::InvalidRequestError);

/// A Stripe-style error response. On a `/v2/` path the gate renders it in the v2 envelope
/// (`stripe::V2Error`), everywhere else in the v1 one (`stripe::Error`).
#[derive(Debug, Clone)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    /// `invalid_request_error` except for server faults and v2 not-found errors.
    pub error_type: ErrorType,
    pub message: String,
    pub code: Option<&'static str>,
    pub param: Option<String>,
}

impl ApiError {
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
            error_type: INVALID_REQUEST,
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
            error_type: INVALID_REQUEST,
            message: message.into(),
            code,
            param: param.map(str::to_string),
        }
    }

    /// The scope header names an account that is unknown or no longer connected.
    pub(crate) fn account_invalid(acct: &str) -> Self {
        ApiError {
            status: StatusCode::FORBIDDEN,
            error_type: INVALID_REQUEST,
            message: format!(
                "The provided key does not have access to account '{acct}' (or that account does not exist). Application access may have been revoked."
            ),
            code: Some("account_invalid"),
            param: None,
        }
    }

    fn unauthorized() -> Self {
        ApiError {
            status: StatusCode::UNAUTHORIZED,
            error_type: INVALID_REQUEST,
            message: "Invalid API Key provided.".to_string(),
            code: None,
            param: None,
        }
    }

    /// A v2 retrieve of an unknown ID.
    pub(crate) fn v2_not_found(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
            error_type: ErrorType::V2NotFound,
            message: message.into(),
            code: Some("not_found"),
            param: None,
        }
    }
}

/// A fallback v2 `code` for an error without one: the v2 schema requires a `code`.
fn v2_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad_request",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit",
        StatusCode::SERVICE_UNAVAILABLE => "service_unavailable",
        _ => "internal_server_error",
    }
}

impl ApiError {
    /// The v1 body, `stripe::Error`. Presence: only what the mock knows. The card, payment
    /// and request-log fields describe a failed payment or a dashboard log the mock never has.
    fn v1_body(&self) -> stripe::Error {
        let type_ = match self.error_type {
            ErrorType::V1(t) => t,
            ErrorType::V2NotFound => stripe::ApiErrorsType::InvalidRequestError,
        };
        stripe::Error {
            error: stripe::ApiErrors {
                advice_code: None,
                charge: None,
                code: self.code.map(str::to_string),
                decline_code: None,
                doc_url: None,
                message: Some(self.message.clone()),
                network_advice_code: None,
                network_decline_code: None,
                param: self.param.clone(),
                payment_intent: None,
                payment_method: None,
                payment_method_type: None,
                request_log_url: None,
                setup_intent: None,
                source: None,
                type_,
            },
        }
    }

    /// The v2 body, `stripe::V2Error`: `code` (a fallback by status when the error has
    /// none), `message` and `type`. The schema has no `param`, so it is dropped; `doc_url`
    /// and `user_message` are absent because the mock has neither.
    fn v2_body(&self) -> stripe::V2Error {
        let type_ = match self.error_type {
            ErrorType::V1(t) => serde_json::to_value(t)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .expect("an error type serializes as a string"),
            ErrorType::V2NotFound => "not_found".to_string(),
        };
        stripe::V2Error {
            error: stripe::V2ErrorError {
                code: self
                    .code
                    .unwrap_or_else(|| v2_code(self.status))
                    .to_string(),
                doc_url: None,
                message: self.message.clone(),
                type_: Some(type_),
                user_message: None,
            },
        }
    }

    /// The response in the v1 (`v2 = false`) or v2 envelope.
    fn render(&self, v2: bool) -> Response {
        let mut r = if v2 {
            (self.status, axum::Json(self.v2_body())).into_response()
        } else {
            (self.status, axum::Json(self.v1_body())).into_response()
        };
        // Kept so the gate can re-render the error in the v2 envelope on `/v2/` paths.
        r.extensions_mut().insert(self.clone());
        r
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.render(self.error_type == ErrorType::V2NotFound)
    }
}

/// The account a request acts in, resolved by the gate: `None` is the platform.
#[derive(Debug, Clone)]
pub(crate) struct RequestScope(pub Scope);

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
        &resources::ACCOUNTS,
        &resources::PAYMENT_INTENTS,
        &resources::CHARGES,
        &resources::REFUNDS,
        &resources::DISPUTES,
        &resources::BALANCE_TRANSACTIONS,
        &resources::PAYMENT_METHODS,
        &resources::SETUP_INTENTS,
        &resources::CHECKOUT_SESSIONS,
        &resources::INVOICE_PAYMENTS,
        &resources::TRANSFERS,
        &resources::APPLICATION_FEES,
        &resources::PAYOUTS,
        &resources::COUPONS,
        &resources::PROMOTION_CODES,
        &resources::INVOICE_ITEMS,
        &resources::CREDIT_NOTES,
        &resources::TAX_RATES,
        &resources::SUBSCRIPTION_SCHEDULES,
    ] {
        router = resources::routes(router, r);
    }
    router
        .route("/v1/invoices/{id}/lines", get(resources::invoice_lines))
        .route(
            "/v1/credit_notes/{id}/lines",
            get(resources::credit_note_lines),
        )
        .route(
            "/v1/customers/{id}/payment_methods",
            get(resources::customer_payment_methods),
        )
        .route(
            "/v1/transfers/{id}/reversals",
            get(resources::transfer_reversals),
        )
        .route(
            "/v1/transfers/{id}/reversals/{rid}",
            get(resources::transfer_reversal),
        )
        .route(
            "/v1/application_fees/{id}/refunds",
            get(resources::fee_refunds),
        )
        .route(
            "/v1/application_fees/{id}/refunds/{rid}",
            get(resources::fee_refund),
        )
        .route(
            "/v1/accounts/{id}/external_accounts",
            get(resources::external_accounts),
        )
        .route(
            "/v1/accounts/{id}/external_accounts/{eid}",
            get(resources::external_account),
        )
        .route("/v1/accounts/{id}/persons", get(resources::persons))
        .route("/v1/accounts/{id}/persons/{pid}", get(resources::person))
        .route("/v2/core/accounts", get(accounts::list_v2))
        .route("/v2/core/accounts/{id}", get(accounts::retrieve_v2))
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
async fn gate(State(shared): State<Arc<Shared>>, mut req: Request, next: Next) -> Response {
    let v2 = is_under(req.uri().path(), "/v2");
    let (request_id, decision) = {
        let mut sim = shared.lock();
        let request_id = sim.api_rng.id("req");
        let decision = check(&sim.api.config().keys, &shared, &sim.engine, &req)
            .map(|scope| (scope, sim.api.decide(Instant::now())));
        (request_id, decision)
    };
    let mut response = match decision {
        Err(e) => e.into_response(),
        Ok((_, (Some(Fault::Timeout), _))) => {
            // Hold before any response exists, so no status line is ever sent;
            // then abort the connection like a reset.
            let mut shutdown = shared.shutdown.subscribe();
            tokio::select! {
                _ = tokio::time::sleep(TIMEOUT_HOLD) => {}
                _ = shutdown.wait_for(|stopping| *stopping) => {}
            }
            fault_response(Fault::Reset)
        }
        Ok((_, (Some(fault), _))) => fault_response(fault),
        Ok((scope, (None, latency_ms))) => {
            if latency_ms > 0 {
                tokio::time::sleep(Duration::from_millis(latency_ms)).await;
            }
            req.extensions_mut().insert(RequestScope(scope));
            next.run(req).await
        }
    };
    // Every error on a `/v2/` path, from the gate, a fault or a handler, uses the v2 envelope.
    if v2 && let Some(e) = response.extensions_mut().remove::<ApiError>() {
        let headers = response.headers().clone();
        response = e.render(true);
        response.extensions_mut().remove::<ApiError>();
        for (name, value) in &headers {
            if name != header::CONTENT_LENGTH && name != header::CONTENT_TYPE {
                response.headers_mut().insert(name.clone(), value.clone());
            }
        }
    }
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
            INVALID_REQUEST,
            Some("rate_limit"),
            "Too many requests hit the API too quickly. We recommend an exponential backoff of your requests.",
        ),
        Fault::ServerError => retryable(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorType::V1(stripe::ApiErrorsType::ApiError),
            None,
            "An unknown error occurred.",
        ),
        Fault::Unavailable => retryable(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorType::V1(stripe::ApiErrorsType::ApiError),
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

/// The request's checks, in spec order: auth, version, scope, method. Returns the
/// scope the request acts in.
fn check(
    keys: &[String],
    shared: &Shared,
    engine: &Engine,
    req: &Request,
) -> Result<Scope, ApiError> {
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
    let scope = resolve_scope(shared.config.accounts.connect, engine, req)?;
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
    Ok(scope)
}

/// Whether `path` is `base` or below it.
fn is_under(path: &str, base: &str) -> bool {
    path.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The scope headers: `Stripe-Account` or `Stripe-Context` select a connected
/// account that is connected right now (spec section 5.4).
fn resolve_scope(connect: bool, engine: &Engine, req: &Request) -> Result<Scope, ApiError> {
    let headers = req.headers();
    if !connect
        && (headers.contains_key("stripe-account") || headers.contains_key("stripe-context"))
    {
        return Err(ApiError::bad_request(
            "Connected accounts are not enabled in this mock.",
            None,
            None,
        ));
    }
    let value = |name: &str| {
        headers
            .get(name)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .filter(|v| !v.is_empty())
    };
    let (account, context) = (value("stripe-account"), value("stripe-context"));
    if let (Some(a), Some(c)) = (&account, &context)
        && a != c
    {
        return Err(ApiError::bad_request(
            "Stripe-Account and Stripe-Context name different accounts.",
            None,
            None,
        ));
    }
    let path = req.uri().path();
    if account.is_some() && path.starts_with("/v2/") {
        return Err(ApiError::bad_request(
            "Use the Stripe-Context header with v2 endpoints.",
            None,
            None,
        ));
    }
    let Some(id) = account.or(context) else {
        return Ok(None);
    };
    if !engine.account_meta(&id).is_some_and(|m| m.connected) {
        return Err(ApiError::account_invalid(&id));
    }
    if is_under(path, "/v1/accounts") || is_under(path, "/v2/core/accounts") {
        return Err(ApiError::bad_request(
            "Accounts endpoints must be called by the platform.",
            None,
            None,
        ));
    }
    Ok(Some(id))
}

/// Re-checks, under the lock a handler holds, that `scope` is still connected: a
/// disconnect can land between the gate and the handler.
pub(crate) fn ensure_connected(engine: &Engine, scope: &Scope) -> Result<(), ApiError> {
    match scope {
        Some(id) if !engine.account_meta(id).is_some_and(|m| m.connected) => {
            Err(ApiError::account_invalid(id))
        }
        _ => Ok(()),
    }
}
