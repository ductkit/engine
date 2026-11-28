//! A webhook endpoint that verifies signatures and fails the first attempt of a
//! deterministic third of events, so their retries arrive late with outdated payloads.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde_json::Value;

use crate::common::real_now;

/// The signing secret of every destination the harness configures.
pub const SECRET: &str = "whsec_ref";

/// Whether the first attempt of `event_id` is answered with a 500 (FNV-1a 64, `% 3 == 0`).
pub fn fails_first(event_id: &str) -> bool {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in event_id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h.is_multiple_of(3)
}

#[derive(Clone, Default)]
struct HookState {
    accepted: Arc<Mutex<Vec<Value>>>,
    /// Event IDs whose first attempt was already failed.
    failed_once: Arc<Mutex<BTreeSet<String>>>,
}

pub struct HookServer {
    addr: std::net::SocketAddr,
    accepted: Arc<Mutex<Vec<Value>>>,
}

async fn handle(State(s): State<HookState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let sig = headers
        .get("Stripe-Signature")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !ductkit_stripe_mock::verify_signature(sig, &body, SECRET, 300, real_now()) {
        return StatusCode::BAD_REQUEST;
    }
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let id = event["id"].as_str().unwrap_or_default().to_string();
    if fails_first(&id) && s.failed_once.lock().unwrap().insert(id) {
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    s.accepted.lock().unwrap().push(event);
    StatusCode::OK
}

impl HookServer {
    /// Must be called inside a tokio runtime.
    pub fn start() -> HookServer {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        let state = HookState::default();
        let accepted = state.accepted.clone();
        let app = Router::new().fallback(handle).with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        HookServer { addr, accepted }
    }

    pub fn url(&self) -> String {
        format!("http://{}/hooks", self.addr)
    }

    /// The events accepted since `cursor`, moving `cursor` past them.
    pub fn take_new(&self, cursor: &mut usize) -> Vec<Value> {
        let accepted = self.accepted.lock().unwrap();
        let new = accepted[*cursor..].to_vec();
        *cursor = accepted.len();
        new
    }
}
