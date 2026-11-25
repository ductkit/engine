//! Spawns the `ductkit-stripe-mock` binary and talks to it over HTTP.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ductkit_stripe_mock::{Config, MockStripe, NewCustomer, RunExport};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_ductkit-stripe-mock");
const MINIMAL: &str = "seed = 3\n[history]\nduration = \"1h\"\n";

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ductkit-stripe-mock-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// A running binary; killed on drop so a failed test leaves no server behind.
struct Server {
    child: Child,
    api_url: String,
    control_url: String,
    seed: u64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(args: &[&str]) -> Server {
        let mut child = Command::new(BIN)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("binary spawns");
        let stdout = child.stdout.take().unwrap();
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
        let line = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("ready line within 10 s");
        let ready: Value = serde_json::from_str(&line).expect("ready line is JSON");
        server.api_url = ready["api_url"].as_str().unwrap().to_string();
        server.control_url = ready["control_url"].as_str().unwrap().to_string();
        server.seed = ready["seed"].as_u64().unwrap();
        server
    }

    fn serve(config: &std::path::Path, extra: &[&str]) -> Server {
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

    async fn get(&self, path: &str) -> reqwest::Response {
        reqwest::get(format!("{}{path}", self.control_url))
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}{path}", self.control_url))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn now_ms(&self) -> i64 {
        let body: Value = self.get("/control/now").await.json().await.unwrap();
        chrono::DateTime::parse_from_rfc3339(body["now"].as_str().unwrap())
            .unwrap()
            .timestamp_millis()
    }

    async fn truth_log(&self) -> String {
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

    fn wait_exit(&mut self, within: Duration) -> ExitStatus {
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

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Output {
    let out = Command::new(BIN).args(args).output().unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    out.stdout.as_slice().read_to_string(&mut stdout).unwrap();
    out.stderr.as_slice().read_to_string(&mut stderr).unwrap();
    Output {
        code: out.status.code(),
        stdout,
        stderr,
    }
}

#[tokio::test]
async fn serve_prints_ready_line_and_serves() {
    let dir = temp_dir();
    let config = write(&dir, "c.toml", MINIMAL);
    let s = Server::serve(&config, &[]);
    assert_eq!(s.seed, 3);
    let r = reqwest::Client::new()
        .get(format!("{}/v1/customers", s.api_url))
        .bearer_auth("sk_test_mock")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(s.get("/control/now").await.status(), 200);
}

#[tokio::test]
async fn seed_flag_overrides_config() {
    let dir = temp_dir();
    let config = write(&dir, "c.toml", MINIMAL);
    let s = Server::serve(&config, &["--seed", "99"]);
    assert_eq!(s.seed, 99);
}

#[tokio::test]
async fn standalone_defaults_to_wall() {
    let dir = temp_dir();
    let config = write(&dir, "c.toml", "seed = 1\n[clock]\nspeed = 60.0\n");
    let s = Server::serve(&config, &[]);
    let a = s.now_ms().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let b = s.now_ms().await;
    assert!(b > a, "wall clock should move: {a} -> {b}");
    let r = s.post("/control/step", json!({"duration": "1s"})).await;
    assert_eq!(r.status(), 409);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "clock_mode");

    let config = write(&dir, "v.toml", "seed = 1\n");
    let s = Server::serve(&config, &["--clock", "virtual"]);
    let a = s.now_ms().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.now_ms().await, a);
    let r = s.post("/control/step", json!({"duration": "1s"})).await;
    assert_eq!(r.status(), 200);
    assert_eq!(s.now_ms().await, a + 1000);
}

#[tokio::test]
async fn binary_exits_cleanly_and_exports() {
    let dir = temp_dir();
    let config = write(&dir, "c.toml", MINIMAL);
    let export = dir.join("run.json");
    let mut s = Server::serve(
        &config,
        &[
            "--clock",
            "virtual",
            "--export-on-exit",
            export.to_str().unwrap(),
        ],
    );
    let r = s
        .post(
            "/control/ops",
            json!({"op": "create_customer", "customer": {}}),
        )
        .await;
    assert_eq!(r.status(), 200);
    let r = s.post("/control/shutdown", json!({})).await;
    assert_eq!(r.status(), 202);
    let status = s.wait_exit(Duration::from_secs(5));
    assert_eq!(status.code(), Some(0));
    assert!(
        reqwest::get(format!("{}/v1/customers", s.api_url))
            .await
            .is_err()
    );
    assert!(
        reqwest::get(format!("{}/control/now", s.control_url))
            .await
            .is_err()
    );
    let run = RunExport::load(&export).expect("export loads");
    let journal = serde_json::to_string(&run.journal).unwrap();
    assert!(journal.contains("create_customer"), "{journal}");
}

#[tokio::test]
async fn replay_command_serves_replayed_state() {
    let dir = temp_dir();
    let m = MockStripe::start(Config::from_toml_str(MINIMAL).unwrap())
        .await
        .unwrap();
    m.create_customer(NewCustomer::default()).unwrap();
    m.step(Duration::from_secs(90)).await.unwrap();
    let file = dir.join("run.json");
    m.export_run().save(&file).unwrap();
    let original = serde_json::to_string(&m.truth_log()).unwrap();
    m.shutdown().await;

    let s = Server::start(&[
        "replay",
        "--run",
        file.to_str().unwrap(),
        "--api-addr",
        "127.0.0.1:0",
        "--control-addr",
        "127.0.0.1:0",
    ]);
    assert_eq!(s.truth_log().await, original);
}

#[test]
fn check_reports_errors() {
    let dir = temp_dir();
    let ok = write(&dir, "ok.toml", MINIMAL);
    let out = run(&["check", "--config", ok.to_str().unwrap()]);
    assert_eq!(out.code, Some(0));
    assert_eq!(out.stdout.trim(), "ok");
    let bad = write(
        &dir,
        "bad.toml",
        "[api]\nstale_read = { probability = 2.0 }\n",
    );
    let out = run(&["check", "--config", bad.to_str().unwrap()]);
    assert_eq!(out.code, Some(1));
    assert!(out.stderr.contains("stale_read"), "{}", out.stderr);
}

#[test]
fn bad_flags_exit_2() {
    let out = run(&["serve", "--nope"]);
    assert_eq!(out.code, Some(2));
    assert!(out.stderr.contains("usage"), "{}", out.stderr);
    let out = run(&["--help"]);
    assert_eq!(out.code, Some(0));
    assert!(out.stdout.contains("usage"), "{}", out.stdout);
}
