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

/// `GET path` against the control API.
pub async fn ctl_get(m: &MockStripe, path: &str) -> reqwest::Response {
    let url = m.control_url().expect("control API is open");
    reqwest::Client::new()
        .get(format!("{url}{path}"))
        .send()
        .await
        .expect("request sent")
}

/// `POST path` with a JSON body against the control API.
pub async fn ctl_post(m: &MockStripe, path: &str, body: serde_json::Value) -> reqwest::Response {
    let url = m.control_url().expect("control API is open");
    reqwest::Client::new()
        .post(format!("{url}{path}"))
        .json(&body)
        .send()
        .await
        .expect("request sent")
}

/// Starts a mock with the control API on `127.0.0.1:0`.
pub async fn mock_with_control(toml: &str) -> MockStripe {
    let config = Config::from_toml_str(toml).expect("valid config");
    let options = ductkit_stripe_mock::StartOptions {
        control_addr: Some("127.0.0.1:0".parse().expect("addr")),
        ..Default::default()
    };
    MockStripe::start_with(config, options)
        .await
        .expect("mock starts")
}

use futures_util::StreamExt;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

const BIN: &str = env!("CARGO_BIN_EXE_ductkit-stripe-mock");
pub const WAIT: Duration = Duration::from_secs(5);

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ductkit-stripe-mock-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn write(dir: &std::path::Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// A running binary; killed on drop so a failed test leaves no server behind.
pub struct Server {
    pub child: Child,
    pub api_url: String,
    pub control_url: String,
    pub seed: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    pub fn start(args: &[&str]) -> Server {
        let mut child = Command::new(BIN)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("binary spawns");
        let stdout = child.stdout.take().unwrap();
        // Drained continuously so the child never blocks on a full pipe.
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let mut stderr = child.stderr.take().unwrap();
        let sink = stderr_text.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        // Owned by the guard from here on, so a timeout below still kills it.
        let mut server = Server {
            child,
            api_url: String::new(),
            control_url: String::new(),
            seed: 0,
        };
        let line = rx.recv_timeout(Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(50));
        let stderr = stderr_text.lock().unwrap().clone();
        let line =
            line.unwrap_or_else(|e| panic!("no ready line within 10 s ({e}); stderr: {stderr}"));
        let ready: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("ready line {line:?} is not JSON ({e}); stderr: {stderr}"));
        server.api_url = ready["api_url"].as_str().unwrap().to_string();
        server.control_url = ready["control_url"].as_str().unwrap().to_string();
        server.seed = ready["seed"].as_u64().unwrap();
        server
    }

    pub fn serve(config: &std::path::Path, extra: &[&str]) -> Server {
        let mut args = vec![
            "serve",
            "--config",
            config.to_str().unwrap(),
            "--api-addr",
            "127.0.0.1:0",
            "--control-addr",
            "127.0.0.1:0",
        ];
        args.extend_from_slice(extra);
        Server::start(&args)
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        reqwest::get(format!("{}{path}", self.control_url))
            .await
            .unwrap()
    }

    pub async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}{path}", self.control_url))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    pub async fn now_ms(&self) -> i64 {
        let body: Value = self.get("/control/now").await.json().await.unwrap();
        chrono::DateTime::parse_from_rfc3339(body["now"].as_str().unwrap())
            .unwrap()
            .timestamp_millis()
    }

    pub async fn truth_log(&self) -> String {
        let mut all = Vec::new();
        let mut after = 0;
        loop {
            let page: Value = self
                .get(&format!("/control/truth_log?after_seq={after}&limit=500"))
                .await
                .json()
                .await
                .unwrap();
            let data = page["data"].as_array().unwrap();
            if let Some(last) = data.last() {
                after = last["seq"].as_u64().unwrap();
            }
            all.extend(data.iter().cloned());
            if page["has_more"] != true {
                break;
            }
        }
        serde_json::to_string(&all).unwrap()
    }

    pub fn wait_exit(&mut self, within: Duration) -> ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "process did not exit in time");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A parsed SSE stream: `next` yields `(event, data)` and skips comments.
pub struct Sse {
    stream: std::pin::Pin<Box<dyn futures_core::Stream<Item = reqwest::Result<Bytes>>>>,
    buf: String,
}

impl Sse {
    pub async fn open(m: &MockStripe, query: &str) -> Sse {
        let url = m.control_url().expect("control API is open");
        Sse::open_url(&url, query).await
    }

    /// Subscribes at `control_url` (a base URL such as `http://127.0.0.1:8421`).
    pub async fn open_url(url: &str, query: &str) -> Sse {
        let r = reqwest::get(format!("{url}/control/subscribe{query}"))
            .await
            .expect("connects");
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["content-type"], "text/event-stream");
        Sse {
            stream: Box::pin(r.bytes_stream()),
            buf: String::new(),
        }
    }

    /// The next frame, or `None` at the end of the stream.
    pub async fn next(&mut self) -> Option<(String, Value)> {
        loop {
            if let Some(i) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..i + 2).collect();
                let mut event = None;
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = Some(v.trim().to_string());
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim_start());
                    }
                }
                let Some(event) = event else { continue };
                return Some((event, serde_json::from_str(&data).expect("JSON data")));
            }
            match tokio::time::timeout(WAIT, self.stream.next())
                .await
                .expect("stream stalled")
            {
                Some(chunk) => self.buf.push_str(&String::from_utf8_lossy(&chunk.unwrap())),
                None => return None,
            }
        }
    }
}
