//! The HTTP control API (spec section 11), served on its own port.
//!
//! Handlers are thin: they parse, call the `MockStripe` method that does the work
//! (and journals it), and map errors to the control error envelope.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{ControlOp, duration, sse};
use crate::engine::Scope;
use crate::handle::Shared;
use crate::{DurationStr, MockError, MockStripe, ObjectType, SubscribeFilter};

const DEFAULT_LIMIT: usize = 1000;
const MAX_LIMIT: usize = 10_000;

/// The control API's router: `/control/*` only, no Stripe routes. `bound` is the
/// address the control listener is bound to (see [`guard`]).
pub(crate) fn router(shared: Arc<Shared>, bound: SocketAddr) -> Router {
    let routes = Router::new()
        .route("/control/now", get(now))
        .route("/control/step", post(step))
        .route("/control/advance_to", post(advance_to))
        .route("/control/pause", post(pause))
        .route("/control/resume", post(resume))
        .route("/control/speed", post(speed))
        .route("/control/quiesce", post(quiesce))
        .route("/control/generation/resume", post(resume_generation))
        .route("/control/generation", get(generation))
        .route("/control/ops", post(ops))
        .route("/control/script", post(script))
        .route("/control/config", get(config))
        .route("/control/rate_limit", post(rate_limit))
        .route("/control/destinations", get(destinations))
        .route("/control/accounts", get(accounts))
        .route("/control/objects/{object_type}/{id}", get(object))
        .route("/control/state", get(state))
        .route("/control/truth_log", get(truth_log))
        .route("/control/events", get(events))
        .route("/control/deliveries", get(deliveries))
        .route("/control/export", get(export))
        .route("/control/subscribe", get(sse::subscribe))
        .route("/control/subscribe/ws", get(sse::subscribe_ws))
        .route("/control/shutdown", post(shutdown))
        .fallback(|| async {
            ControlError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
        })
        .method_not_allowed_fallback(|| async {
            ControlError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "method not allowed",
            )
        })
        .with_state(shared);
    routes.layer(axum::middleware::from_fn_with_state(bound, guard))
}

// ---- cross-origin guard -------------------------------------------------------

/// Keeps web pages away from the unauthenticated control API (spec section 11,
/// "Security"). Every request, including the WebSocket upgrade and unknown routes:
///
/// - (a) `Host` must be a loopback name (`localhost`, `127.0.0.1`, `[::1]`, any
///   port) or the bound address's own host, which lets remote clients reach a
///   control API bound to a non-loopback IP. This defeats DNS rebinding.
/// - (b) an `Origin` header, if present, must be `http://` on a loopback host.
/// - (c) a `POST` must have `Content-Type: application/json` (parameters
///   allowed), even without a body, so a page cannot send it as a "simple"
///   cross-origin request (a form post or `text/plain` fetch) without a preflight.
///
/// (a) and (b) fail with 403 `forbidden`, (c) with 415 `unsupported_media_type`.
async fn guard(State(bound): State<SocketAddr>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()));
    if !host.is_some_and(|h| host_allowed(h, bound)) {
        return ControlError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "the Host header must name a loopback address or the control address",
        )
        .into_response();
    }
    if let Some(origin) = headers.get(header::ORIGIN)
        && !origin.to_str().is_ok_and(origin_allowed)
    {
        return ControlError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "cross-origin requests to the control API are not allowed",
        )
        .into_response();
    }
    if req.method() == Method::POST && !is_json(headers.get(header::CONTENT_TYPE)) {
        return ControlError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "POST requests need `Content-Type: application/json`",
        )
        .into_response();
    }
    next.run(req).await
}

/// The host part of `host[:port]` or `[v6][:port]`, lowercased.
fn host_name(authority: &str) -> String {
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(authority, |(h, _)| h),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host.to_ascii_lowercase()
}

fn is_loopback_name(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn host_allowed(authority: &str, bound: SocketAddr) -> bool {
    let host = host_name(authority);
    is_loopback_name(&host) || host == bound.ip().to_string()
}

fn origin_allowed(origin: &str) -> bool {
    origin.strip_prefix("http://").is_some_and(|authority| {
        !authority.contains('/') && is_loopback_name(&host_name(authority))
    })
}

fn is_json(content_type: Option<&HeaderValue>) -> bool {
    content_type
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

// ---- errors -----------------------------------------------------------------

/// A control API error: `{"error": {"code", "message"}}` with an HTTP status.
#[derive(Debug)]
pub(crate) struct ControlError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ControlError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ControlError {
            status,
            code,
            message: message.into(),
        }
    }

    /// A malformed request: bad JSON, bad query, unknown op or type.
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        ControlError::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    fn body(&self) -> Value {
        json!({"error": {"code": self.code, "message": self.message}})
    }
}

impl From<MockError> for ControlError {
    fn from(e: MockError) -> Self {
        let (status, code) = match &e {
            MockError::NoSuchObject { .. } => (StatusCode::NOT_FOUND, "no_such_object"),
            MockError::NoSuchDestination(_) => (StatusCode::NOT_FOUND, "no_such_destination"),
            MockError::UnknownAccount(_) => (StatusCode::NOT_FOUND, "unknown_account"),
            MockError::ClockMode(_) => (StatusCode::CONFLICT, "clock_mode"),
            MockError::TimeTravel { .. } => (StatusCode::CONFLICT, "time_travel"),
            MockError::Deleted { .. } => (StatusCode::CONFLICT, "deleted"),
            MockError::InvalidAction(_) => (StatusCode::CONFLICT, "invalid_action"),
            MockError::ConnectDisabled => (StatusCode::CONFLICT, "connect_disabled"),
            MockError::Config(_) => (StatusCode::BAD_REQUEST, "config"),
            MockError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "io"),
        };
        ControlError::new(status, code, e.to_string())
    }
}

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        (self.status, axum::Json(self.body())).into_response()
    }
}

type Reply<T = Value> = Result<axum::Json<T>, ControlError>;

/// `axum::Json` whose rejections are control errors, not plain text.
struct Json<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Json<T> {
    type Rejection = ControlError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ControlError> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Json(v)),
            Err(rejection) => Err(ControlError::invalid(rejection.body_text())),
        }
    }
}

/// `axum::extract::Path` whose rejections are control errors.
struct Path<T>(T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Path<T> {
    type Rejection = ControlError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ControlError> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(v)) => Ok(Path(v)),
            Err(rejection) => Err(ControlError::invalid(rejection.body_text())),
        }
    }
}

/// `axum::extract::ws::WebSocketUpgrade` whose rejections are control errors,
/// keeping the rejection's status (e.g. 400, 405, 426).
pub(super) struct WebSocketUpgrade(pub(super) axum::extract::ws::WebSocketUpgrade);

impl<S: Send + Sync> FromRequestParts<S> for WebSocketUpgrade {
    type Rejection = ControlError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ControlError> {
        match axum::extract::ws::WebSocketUpgrade::from_request_parts(parts, state).await {
            Ok(upgrade) => Ok(WebSocketUpgrade(upgrade)),
            Err(rejection) => Err(ControlError::new(
                rejection.status(),
                "invalid_request",
                rejection.body_text(),
            )),
        }
    }
}

/// `axum::extract::Query` whose rejections are control errors.
pub(super) struct Query<T>(pub(super) T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Query<T> {
    type Rejection = ControlError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ControlError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Query(v)),
            Err(rejection) => Err(ControlError::invalid(rejection.body_text())),
        }
    }
}

// ---- helpers ----------------------------------------------------------------

type Mock = State<Arc<Shared>>;

pub(super) fn mock(shared: Arc<Shared>) -> MockStripe {
    MockStripe::view(shared)
}

fn to_json<T: serde::Serialize>(v: &T) -> Reply {
    serde_json::to_value(v).map(axum::Json).map_err(|e| {
        ControlError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    })
}

fn now_body(m: &MockStripe) -> Reply {
    to_json(&json!({"now": m.now()}))
}

fn limit(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// `{"data": items, "has_more": bool}` from up to `limit + 1` items.
fn page<T: serde::Serialize>(mut items: Vec<T>, limit: usize) -> Reply {
    let has_more = items.len() > limit;
    items.truncate(limit);
    to_json(&json!({"data": items, "has_more": has_more}))
}

fn parse_object_type(s: &str) -> Result<ObjectType, ControlError> {
    ObjectType::ALL
        .into_iter()
        .find(|t| t.as_str() == s || t.serde_name() == s)
        .ok_or_else(|| ControlError::invalid(format!("unknown object type `{s}`")))
}

/// `types=a,b` and `account=platform,acct_…` into a filter; either may be absent.
pub(crate) fn parse_filter(
    m: &MockStripe,
    types: Option<&str>,
    account: Option<&str>,
) -> Result<SubscribeFilter, ControlError> {
    let object_types = types
        .map(|t| t.split(',').map(|s| parse_object_type(s.trim())).collect())
        .transpose()?;
    let accounts = account
        .map(|a| {
            let connected: Vec<String> = m
                .accounts()
                .into_iter()
                .filter(|a| a.connected)
                .map(|a| a.id)
                .collect();
            a.split(',')
                .map(|s| s.trim())
                .map(|s| {
                    if s == "platform" {
                        Ok(None)
                    } else if connected.iter().any(|c| c == s) {
                        Ok(Some(s.to_string()))
                    } else {
                        Err(ControlError::invalid(format!(
                            "unknown or disconnected account `{s}`"
                        )))
                    }
                })
                .collect::<Result<Vec<Scope>, _>>()
        })
        .transpose()?;
    Ok(SubscribeFilter {
        object_types,
        accounts,
    })
}

// ---- handlers ---------------------------------------------------------------

async fn now(State(s): Mock) -> Reply {
    now_body(&mock(s))
}

#[derive(Deserialize)]
struct StepBody {
    duration: DurationStr,
}

async fn step(State(s): Mock, Json(b): Json<StepBody>) -> Reply {
    let m = mock(s);
    m.apply(ControlOp::Step {
        duration: b.duration,
    })
    .await?;
    now_body(&m)
}

#[derive(Deserialize)]
struct AdvanceBody {
    time: chrono::DateTime<chrono::Utc>,
}

async fn advance_to(State(s): Mock, Json(b): Json<AdvanceBody>) -> Reply {
    let m = mock(s);
    m.apply(ControlOp::AdvanceTo { time: b.time }).await?;
    now_body(&m)
}

async fn pause(State(s): Mock) -> Reply {
    let m = mock(s);
    m.pause()?;
    now_body(&m)
}

async fn resume(State(s): Mock) -> Reply {
    let m = mock(s);
    m.resume()?;
    now_body(&m)
}

#[derive(Deserialize)]
struct SpeedBody {
    speed: f64,
}

async fn speed(State(s): Mock, Json(b): Json<SpeedBody>) -> Reply {
    let m = mock(s);
    m.set_speed(b.speed)?;
    now_body(&m)
}

async fn quiesce(State(s): Mock) -> Reply {
    mock(s).apply(ControlOp::Quiesce).await?;
    to_json(&json!({}))
}

async fn resume_generation(State(s): Mock) -> Reply {
    mock(s).apply(ControlOp::ResumeGeneration).await?;
    to_json(&json!({}))
}

async fn generation(State(s): Mock) -> Reply {
    to_json(&json!({"enabled": mock(s).generation_enabled()}))
}

async fn ops(State(s): Mock, Json(op): Json<ControlOp>) -> Reply {
    to_json(&mock(s).apply(op).await?)
}

#[derive(Deserialize)]
struct ScriptBody {
    steps: Vec<ControlOp>,
}

async fn script(State(s): Mock, Json(b): Json<ScriptBody>) -> Response {
    match mock(s).run_script(&b.steps).await {
        Ok(results) => match to_json(&results) {
            Ok(body) => body.into_response(),
            Err(e) => e.into_response(),
        },
        Err(e) => {
            let completed = e.completed;
            let e = ControlError::from(e.error);
            let mut body = e.body();
            body["completed"] = json!(completed);
            (e.status, axum::Json(body)).into_response()
        }
    }
}

async fn config(State(s): Mock) -> Reply {
    to_json(&mock(s).effective_config())
}

#[derive(Deserialize)]
struct RateLimitBody {
    duration: DurationStr,
}

async fn rate_limit(State(s): Mock, Json(b): Json<RateLimitBody>) -> Reply {
    mock(s).force_rate_limit(duration(b.duration)?);
    to_json(&json!({}))
}

async fn destinations(State(s): Mock) -> Reply {
    to_json(&mock(s).destinations())
}

async fn accounts(State(s): Mock) -> Reply {
    to_json(&mock(s).accounts())
}

#[derive(Deserialize)]
struct ObjectQuery {
    account: Option<String>,
}

async fn object(
    State(s): Mock,
    Path((object_type, id)): Path<(String, String)>,
    Query(q): Query<ObjectQuery>,
) -> Reply {
    let t = parse_object_type(&object_type)?;
    let m = mock(s);
    let found = match &q.account {
        Some(account) if account != "platform" => m.account(account).get(t, &id)?,
        _ => m.get(t, &id),
    };
    match found {
        Some(update) => to_json(&update),
        None => Err(MockError::NoSuchObject { object_type: t, id }.into()),
    }
}

#[derive(Deserialize)]
struct StateQuery {
    account: Option<String>,
    types: Option<String>,
}

async fn state(State(s): Mock, Query(q): Query<StateQuery>) -> Reply {
    let m = mock(s);
    let filter = parse_filter(&m, q.types.as_deref(), q.account.as_deref())?;
    Ok(axum::Json(m.dump_state(&filter)))
}

#[derive(Deserialize)]
struct TruthLogQuery {
    after_seq: Option<u64>,
    limit: Option<usize>,
}

async fn truth_log(State(s): Mock, Query(q): Query<TruthLogQuery>) -> Reply {
    let limit = limit(q.limit);
    let after = q.after_seq.unwrap_or(0);
    let items: Vec<_> = {
        let sim = s.lock();
        let log = sim.engine.log();
        let start = log.partition_point(|u| u.seq <= after);
        log[start..].iter().take(limit + 1).cloned().collect()
    };
    page(items, limit)
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<String>,
    limit: Option<usize>,
}

async fn events(State(s): Mock, Query(q): Query<EventsQuery>) -> Reply {
    let limit = limit(q.limit);
    let timeline = mock(s).event_timeline();
    let start = match &q.after {
        None => 0,
        Some(after) => {
            timeline
                .iter()
                .position(|e| &e.id == after || &e.pair_id == after)
                .ok_or_else(|| {
                    ControlError::new(
                        StatusCode::NOT_FOUND,
                        "no_such_object",
                        format!("no such event: {after}"),
                    )
                })?
                + 1
        }
    };
    page(
        timeline.into_iter().skip(start).take(limit + 1).collect(),
        limit,
    )
}

#[derive(Deserialize)]
struct DeliveriesQuery {
    limit: Option<usize>,
}

async fn deliveries(State(s): Mock, Query(q): Query<DeliveriesQuery>) -> Reply {
    let limit = limit(q.limit);
    let log = mock(s).delivery_log();
    let has_more = log.len() > limit;
    let tail = &log[log.len().saturating_sub(limit)..];
    to_json(&json!({"data": tail, "has_more": has_more}))
}

async fn export(State(s): Mock) -> Reply {
    to_json(&mock(s).export_run())
}

async fn shutdown(State(s): Mock) -> Response {
    s.shutdown_requested.send_replace(true);
    (StatusCode::ACCEPTED, axum::Json(json!({}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_names() {
        let lo: SocketAddr = "127.0.0.1:8421".parse().unwrap();
        let lan: SocketAddr = "192.168.1.5:8421".parse().unwrap();
        for h in [
            "localhost",
            "LOCALHOST:1",
            "127.0.0.1:8421",
            "[::1]:9",
            "[::1]",
        ] {
            assert!(host_allowed(h, lo), "{h}");
        }
        for h in [
            "evil.example:8421",
            "192.168.1.5:8421",
            "localhost.evil.example",
            "",
        ] {
            assert!(!host_allowed(h, lo), "{h}");
        }
        assert!(host_allowed("192.168.1.5:8421", lan));
        assert!(host_allowed("localhost:8421", lan));
        let v6: SocketAddr = "[fd00::5]:8421".parse().unwrap();
        assert!(host_allowed("[fd00::5]:8421", v6));

        for o in [
            "http://localhost:3000",
            "http://127.0.0.1",
            "http://[::1]:80",
        ] {
            assert!(origin_allowed(o), "{o}");
        }
        for o in [
            "https://evil.example",
            "null",
            "https://localhost:3000",
            "http://localhost.evil.example",
            "http://192.168.1.5:8421",
        ] {
            assert!(!origin_allowed(o), "{o}");
        }

        let ct = |s: &str| is_json(Some(&HeaderValue::from_str(s).unwrap()));
        assert!(ct("application/json"));
        assert!(ct("Application/JSON; charset=utf-8"));
        assert!(!ct("text/plain"));
        assert!(!ct("application/x-www-form-urlencoded"));
        assert!(!is_json(None));
    }
}
