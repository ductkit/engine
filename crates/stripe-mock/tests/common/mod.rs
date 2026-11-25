#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use ductkit_stripe_mock::{Action, Config, MockStripe, ObjectType};

/// Parses `toml` and starts a mock, panicking on error.
pub async fn mock_with(toml: &str) -> MockStripe {
    let config = Config::from_toml_str(toml).expect("valid config");
    MockStripe::start(config).await.expect("mock starts")
}

/// `GET path` against the mock's API with the default test key.
pub async fn get(m: &MockStripe, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{}{path}", m.api_url()))
        .bearer_auth("sk_test_mock")
        .send()
        .await
        .expect("request sent")
}

/// `GET path` with extra request headers.
pub async fn get_with(m: &MockStripe, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .get(format!("{}{path}", m.api_url()))
        .bearer_auth("sk_test_mock");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.send().await.expect("request sent")
}

/// `GET path` scoped to a connected account with `Stripe-Account`.
pub async fn get_as(m: &MockStripe, path: &str, acct: &str) -> reqwest::Response {
    get_with(m, path, &[("Stripe-Account", acct)]).await
}

/// `GET path` scoped to a connected account with `Stripe-Context`.
pub async fn get_ctx(m: &MockStripe, path: &str, acct: &str) -> reqwest::Response {
    get_with(m, path, &[("Stripe-Context", acct)]).await
}

/// One `[[destinations]]` block with `secret = "whsec_test"`.
pub fn dest(url: &str, payload: &str, events: &str) -> String {
    format!(
        "[[destinations]]\nname = \"{payload}\"\nurl = \"{url}\"\npayload = \"{payload}\"\nenabled_events = [\"{events}\"]\nsecret = \"whsec_test\"\n"
    )
}

/// One request a [`Receiver`] got.
#[derive(Debug, Clone)]
pub struct Received {
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub at: Instant,
}

type Respond = dyn Fn(usize) -> (u16, Duration) + Send + Sync;

#[derive(Clone)]
struct RxState {
    got: Arc<Mutex<Vec<Received>>>,
    respond: Arc<Respond>,
}

/// A local HTTP server that records webhook requests and answers with
/// `respond(request_index) -> (status, delay)`.
pub struct Receiver {
    addr: std::net::SocketAddr,
    got: Arc<Mutex<Vec<Received>>>,
}

async fn handle(State(s): State<RxState>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let index = {
        let mut got = s.got.lock().unwrap();
        got.push(Received {
            headers,
            body: body.to_vec(),
            at: Instant::now(),
        });
        got.len() - 1
    };
    let (status, delay) = (s.respond)(index);
    tokio::time::sleep(delay).await;
    StatusCode::from_u16(status).unwrap()
}

impl Receiver {
    /// Must be called inside a tokio runtime.
    pub fn start(respond: impl Fn(usize) -> (u16, Duration) + Send + Sync + 'static) -> Receiver {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let state = RxState {
            got: got.clone(),
            respond: Arc::new(respond),
        };
        let app = Router::new().fallback(handle).with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Receiver { addr, got }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn requests(&self) -> Vec<Received> {
        self.got.lock().unwrap().clone()
    }

    /// Waits until at least `n` requests arrived (or `timeout`), then returns all of them.
    pub async fn wait_for(&self, n: usize, timeout: Duration) -> Vec<Received> {
        let start = Instant::now();
        while self.got.lock().unwrap().len() < n && start.elapsed() < timeout {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.requests()
    }
}

/// Real Unix seconds now.
pub fn real_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Runs catalog changes until `n` active recurring prices share one currency and
/// `interval`; returns their IDs.
pub fn recurring_prices(m: &MockStripe, n: usize, interval: &str) -> Vec<String> {
    for _ in 0..200 {
        let mut groups: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for id in m
            .truth_log()
            .into_iter()
            .filter(|u| u.object_type == ObjectType::Price)
            .map(|u| u.object_id)
        {
            let p = m.get(ObjectType::Price, &id).unwrap().state;
            if p["active"] == true && p["recurring"]["interval"] == interval {
                let ids = groups
                    .entry(p["currency"].as_str().unwrap().to_string())
                    .or_default();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        if let Some(ids) = groups.values().find(|ids| ids.len() >= n) {
            return ids[..n].to_vec();
        }
        m.run_action(Action::CatalogChange).unwrap();
    }
    panic!("no {n} recurring {interval} prices after 200 catalog changes");
}
