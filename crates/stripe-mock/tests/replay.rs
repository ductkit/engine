mod common;

use std::panic::AssertUnwindSafe;
use std::time::Duration;

use common::{get, get_as, mock_with};
use ductkit_stripe_mock::{
    AccountApi, Action, ControlOp, CustomerUpdate, DestinationConfig, DurationStr, MockError,
    MockStripe, NewCustomer, ObjectType, PayloadFormat, ReplayOptions, RunExport, SimTime,
    StateUpdate,
};
use serde_json::Value;

/// The spec example's `[actions]` and `[churn]` rates at one tenth, with Connect on.
const TENTH: &str = r#"
seed = 11
[actions]
new_customer = 1.0
start_subscription = 0.4
cancel_subscription = 0.05
catalog_change = 0.02
one_off_invoice = 0.2
step_delay = { distribution = "lognormal", median_ms = 200, p99_ms = 2000 }
payment_failure_probability = 0.1
[churn.customer]
update = 0.3
delete = 0.02
[churn.product]
update = 0.05
archive = 0.01
[churn.price]
update = 0.05
archive = 0.01
[churn.subscription]
update = 0.05
[churn.invoice]
update = 0.05
[churn.account]
update = 0.02
[accounts]
connect = true
connected_accounts = { initial = 2, connect_rate = 0.05, deauthorize_rate = 0.01 }
[history]
duration = "6h"
[webhooks]
retry_schedule = ["1s", "2s"]
max_retry_duration = "1h"
"#;

type Timeline = Vec<(String, String, SimTime, SimTime)>;

fn timeline(m: &MockStripe) -> Timeline {
    m.event_timeline()
        .into_iter()
        .map(|e| (e.id, e.event_type, e.created, e.visible_at))
        .collect()
}

fn truth(m: &MockStripe) -> String {
    serde_json::to_string(&m.truth_log()).unwrap()
}

fn unreachable_snapshot() -> DestinationConfig {
    DestinationConfig {
        name: "unreachable".into(),
        url: "http://127.0.0.1:9/hook".into(),
        payload: PayloadFormat::Snapshot,
        enabled_events: vec!["*".into()],
        connect: true,
        events_from: None,
        secret: None,
        enabled: true,
    }
}

/// A virtual run mixing every kind of control call with steps.
async fn virtual_run() -> MockStripe {
    let m = mock_with(TENTH).await;
    m.step(Duration::from_secs(2 * 3600)).await.unwrap();
    let c = m.create_customer(NewCustomer::default()).unwrap();
    m.update_customer(&c, CustomerUpdate::SetName(Some("Ada".into())))
        .unwrap();
    let a = m
        .accounts()
        .into_iter()
        .find(|a| a.connected)
        .expect("a connected account")
        .id;
    let ac = m
        .account(&a)
        .create_customer(NewCustomer::default())
        .unwrap();
    m.account(&a)
        .update_customer(&ac, CustomerUpdate::SetEmail(Some("a@example.com".into())))
        .unwrap();
    m.run_action(Action::StartSubscription {
        customer: None,
        prices: None,
    })
    .unwrap();
    m.update_actions_config(|c| c.new_customer *= 2.0).unwrap();
    let d = m.add_destination(unreachable_snapshot()).unwrap();
    // A long outage keeps every attempt off the network.
    m.start_outage(&d.id, Duration::from_secs(30 * 86_400))
        .unwrap();
    let v2 = m.connect_account(AccountApi::V2).unwrap();
    m.account(&v2)
        .create_customer(NewCustomer::default())
        .unwrap();
    m.disconnect_account(&v2).unwrap();
    m.delete_customer(&c).unwrap();
    // Failed calls are not journalled and leave no trace in later causes.
    assert!(
        m.update_customer("cus_nope", CustomerUpdate::SetName(None))
            .is_err()
    );
    assert!(
        m.run_action(Action::StartSubscription {
            customer: Some(c.clone()),
            prices: None,
        })
        .is_err()
    );
    assert!(m.remove_destination("we_nope").is_err());
    m.run_action(Action::NewCustomer).unwrap();
    m.step(Duration::from_secs(3 * 3600)).await.unwrap();
    m.quiesce().await.unwrap();
    m.resume_generation();
    m.step(Duration::from_secs(3600)).await.unwrap();
    m
}

#[tokio::test]
async fn virtual_run_replays_exactly() {
    let m = virtual_run().await;
    assert!(m.truth_log().len() > 50, "{}", m.truth_log().len());
    let export = m.export_run();
    assert!(!export.journal.is_empty());
    let r = MockStripe::replay(export.clone(), ReplayOptions::default())
        .await
        .unwrap();
    assert_eq!(r.now(), m.now());
    assert_eq!(truth(&r), truth(&m));
    assert_eq!(timeline(&r), timeline(&m));
    // The replay journals the same ops.
    assert_eq!(r.export_run().journal, export.journal);
}

fn free_addr() -> std::net::SocketAddr {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    probe.local_addr().unwrap()
}

/// Clients polling the replay's ports cannot reach it (and so cannot inject ops or
/// pagination writes) until it has reached `ended_at`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replay_serves_only_after_it_finished() {
    let m = virtual_run().await;
    m.step(Duration::from_secs(2 * 86_400)).await.unwrap();
    let export = m.export_run();
    let ended_at = serde_json::to_value(export.ended_at).unwrap();
    let (api_addr, control_addr) = (free_addr(), free_addr());

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let prober = tokio::spawn({
        let stop = stop.clone();
        async move {
            let client = reqwest::Client::new();
            let mut seen = Vec::new();
            let mut attempts = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                attempts += 1;
                let r = client
                    .get(format!("http://{control_addr}/control/now"))
                    .send()
                    .await;
                if let Ok(r) = r {
                    let status = r.status().as_u16();
                    let body: Value = r.json().await.unwrap_or(Value::Null);
                    seen.push((status, body));
                }
                if reqwest::get(format!("http://{api_addr}/v1/customers"))
                    .await
                    .is_ok()
                {
                    // The API listener only exists once the replay is done.
                    let r = client
                        .get(format!("http://{control_addr}/control/now"))
                        .send()
                        .await
                        .expect("control API is up with the Stripe API");
                    let body: Value = r.json().await.unwrap();
                    seen.push((200, body));
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            (attempts, seen)
        }
    });
    // Let the prober issue its first request before the replay starts.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let r = MockStripe::replay_with(
        export.clone(),
        ReplayOptions::default(),
        ductkit_stripe_mock::StartOptions {
            api_addr: Some(api_addr),
            control_addr: Some(control_addr),
        },
    )
    .await
    .unwrap();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let (attempts, seen) = prober.await.unwrap();
    assert!(attempts >= 1);
    for (status, body) in &seen {
        assert_eq!(*status, 200, "{body}");
        assert_eq!(body["now"], ended_at, "served before the replay finished");
    }
    assert_eq!(r.now(), m.now());
    assert_eq!(truth(&r), truth(&m));
    assert_eq!(timeline(&r), timeline(&m));
    assert_eq!(r.control_url(), Some(format!("http://{control_addr}")));
    assert_eq!(r.api_url(), format!("http://{api_addr}"));
}

#[tokio::test]
async fn drop_destinations_replays() {
    let m = virtual_run().await;
    let r = MockStripe::replay(
        m.export_run(),
        ReplayOptions {
            drop_destinations: true,
        },
    )
    .await
    .unwrap();
    assert!(r.destinations().is_empty());
    assert_eq!(truth(&r), truth(&m));
    assert_eq!(timeline(&r), timeline(&m));
}

#[tokio::test]
async fn wall_run_replays_to_same_truth() {
    let m = mock_with(&format!(
        "{TENTH}[clock]\nmode = \"wall\"\nspeed = 3600.0\n"
    ))
    .await;
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        m.create_customer(NewCustomer::default()).unwrap();
        m.run_action(Action::StartSubscription {
            customer: None,
            prices: None,
        })
        .unwrap();
    }
    m.pause().unwrap();
    assert!(m.truth_log().len() > 50, "{}", m.truth_log().len());
    let export = m.export_run();
    assert!(export.ended_at > SimTime::from_datetime(export.config.clock.start));
    let r = MockStripe::replay(export, ReplayOptions::default())
        .await
        .unwrap();
    assert_eq!(r.now(), m.now());
    assert_eq!(truth(&r), truth(&m));
}

async fn page(m: &MockStripe, path: &str, acct: Option<&str>) -> Value {
    let r = match acct {
        Some(a) => get_as(m, path, a).await,
        None => get(m, path).await,
    };
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

fn last_id(v: &Value) -> String {
    v["data"].as_array().unwrap().last().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn pagination_writes_replay() {
    let m = mock_with(
        "seed = 5\n[api]\npagination_writes = { probability = 1.0 }\n[accounts]\nconnect = true\nconnected_accounts = { initial = 1 }",
    )
    .await;
    for _ in 0..30 {
        m.create_customer(NewCustomer::default()).unwrap();
    }
    let a = m.accounts()[0].id.clone();
    for _ in 0..5 {
        m.account(&a)
            .create_customer(NewCustomer::default())
            .unwrap();
    }
    let mut v = page(&m, "/v1/customers?limit=5", None).await;
    for _ in 0..3 {
        let next = format!("/v1/customers?limit=5&starting_after={}", last_id(&v));
        v = page(&m, &next, None).await;
    }
    let first = page(&m, "/v1/customers?limit=2", Some(&a)).await;
    let next = format!("/v1/customers?limit=2&starting_after={}", last_id(&first));
    page(&m, &next, Some(&a)).await;

    let export = m.export_run();
    let writes: Vec<_> = export
        .journal
        .iter()
        .filter_map(|e| match &e.op {
            ControlOp::PaginationWrite {
                account,
                object_type,
            } => Some((account.clone(), *object_type)),
            _ => None,
        })
        .collect();
    assert!(writes.len() >= 2, "{writes:?}");
    assert!(writes.iter().all(|(_, t)| *t == ObjectType::Customer));
    assert!(writes.iter().any(|(acct, _)| acct.as_deref() == Some(&a)));
    // Replay runs the journalled writes directly; nothing calls the replay's API.
    let r = MockStripe::replay(export, ReplayOptions::default())
        .await
        .unwrap();
    assert_eq!(truth(&r), truth(&m));
    assert_eq!(timeline(&r), timeline(&m));
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("stripe-mock-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn export_round_trips_through_file() {
    let m = virtual_run().await;
    let export = m.export_run();
    assert_eq!(export.format, "ductkit-stripe-mock/run@1");
    assert_eq!(export.seed, 11);
    let dir = temp_dir("export");
    let path = dir.join("run.json");
    export.save(&path).unwrap();
    assert_eq!(RunExport::load(&path).unwrap(), export);

    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    v["format"] = "other".into();
    let other = dir.join("other.json");
    std::fs::write(&other, serde_json::to_string(&v).unwrap()).unwrap();
    match RunExport::load(&other) {
        Err(MockError::Config(msg)) => assert!(msg.contains("unsupported run format"), "{msg}"),
        r => panic!("expected a config error, got {r:?}"),
    }
    let mut bad = export.clone();
    bad.format = "other".into();
    assert!(matches!(
        MockStripe::replay(bad, ReplayOptions::default()).await,
        Err(MockError::Config(_))
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn run_script_applies_in_order() {
    let m = mock_with("[accounts]\nconnect = true").await;
    let steps = [
        ControlOp::CreateCustomer {
            account: None,
            customer: NewCustomer::default(),
        },
        ControlOp::Step {
            duration: DurationStr(3_600_000),
        },
        ControlOp::RunAction {
            account: None,
            action: Action::StartSubscription {
                customer: None,
                prices: None,
            },
        },
        ControlOp::DisconnectAccount {
            id: "acct_nope".into(),
        },
    ];
    let err = m.run_script(&steps).await.unwrap_err();
    assert_eq!(err.completed, 3);
    assert!(
        matches!(err.error, MockError::UnknownAccount(ref id) if id == "acct_nope"),
        "{:?}",
        err.error
    );
    let ops: Vec<ControlOp> = m.export_run().journal.into_iter().map(|e| e.op).collect();
    assert_eq!(ops.len(), 3, "{ops:?}");
    assert!(matches!(ops[0], ControlOp::CreateCustomer { .. }));
    assert!(matches!(ops[1], ControlOp::AdvanceTo { .. }));
    assert!(matches!(ops[2], ControlOp::RunAction { .. }));

    // A missing `account` key is the platform.
    let op: ControlOp = serde_json::from_str(r#"{"op":"create_customer","customer":{}}"#).unwrap();
    assert_eq!(
        op,
        ControlOp::CreateCustomer {
            account: None,
            customer: NewCustomer::default(),
        }
    );
}

#[tokio::test]
async fn pagination_write_op_is_rejected() {
    let m = mock_with("").await;
    let r = m
        .apply(ControlOp::PaginationWrite {
            account: None,
            object_type: ObjectType::Customer,
        })
        .await;
    assert!(matches!(r, Err(MockError::InvalidAction(_))), "{r:?}");
    assert!(m.export_run().journal.is_empty());
}

fn run_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            name.starts_with("run-") && name.ends_with(".json")
        })
        .collect()
}

#[tokio::test]
async fn replay_guard_writes_on_panic() {
    let m = mock_with("seed = 3").await;
    m.create_customer(NewCustomer::default()).unwrap();

    let quiet = temp_dir("guard-ok");
    {
        let _guard = m.replay_guard(&quiet);
    }
    assert!(run_files(&quiet).is_empty());

    let dir = temp_dir("guard");
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard = m.replay_guard(&dir);
        panic!("test failure");
    }));
    assert!(result.is_err());
    let files = run_files(&dir);
    assert_eq!(files.len(), 1, "{files:?}");
    let name = files[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(name.starts_with("run-3-"), "{name}");
    let loaded = RunExport::load(&files[0]).unwrap();
    assert_eq!(loaded, m.export_run());
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&quiet).unwrap();
}

/// Every phase 7 rate on, Connect on with two Standard accounts (controlled accounts get
/// setup objects from phase 8 on, so the guard uses Standard only).
const PHASE_7_FULL: &str = r#"
seed = 11
[actions]
new_customer = 10.0
start_subscription = 4.0
cancel_subscription = 0.5
one_off_payment = 6.0
refund = 0.4
dispute = 0.05
save_payment_method = 1.0
checkout = 2.0
catalog_change = 0.2
one_off_invoice = 2.0
step_delay = { distribution = "lognormal", median_ms = 200, p99_ms = 2000 }
payment_failure_probability = 0.1
[churn.customer]
update = 3.0
delete = 0.2
[churn.product]
update = 0.5
archive = 0.1
[churn.price]
update = 0.5
archive = 0.1
[churn.subscription]
update = 0.5
[churn.invoice]
update = 0.5
[churn.account]
update = 0.2
[churn.payment_method]
detach = 0.1
[accounts]
connect = true
connected_accounts = { initial = 2, connect_rate = 0.5, deauthorize_rate = 0.1 }
account_api = { v1 = 1.0, v2 = 0.0 }
account_types = { standard = 1.0, express = 0.0, custom = 0.0 }
"#;

/// The phase 9 object types (ruling Q1), by `StateUpdate.object_type` serde name (the
/// snake_case of the Q1 `ObjectType` variant names, which the variants added in phase 9 derive).
/// The projection drops their entries.
const PHASE_9_TYPES: [&str; 8] = [
    "coupon",
    "promotion_code",
    "discount",
    "invoice_item",
    "credit_note",
    "credit_note_line_item",
    "tax_rate",
    "subscription_schedule",
];

/// The truth log as phase 8 produced it (ruling Q12): entries of the phase 9 types removed, and
/// `seq` and `events` stripped from every remaining entry.
fn phase_8_projection(log: &[StateUpdate]) -> String {
    let entries: Vec<serde_json::Value> = log
        .iter()
        .map(|u| serde_json::to_value(u).unwrap())
        .filter(|v| !PHASE_9_TYPES.contains(&v["object_type"].as_str().unwrap()))
        .map(|mut v| {
            let o = v.as_object_mut().unwrap();
            o.remove("seq");
            o.remove("events");
            v
        })
        .collect();
    serde_json::to_string(&entries).unwrap()
}

/// SHA-256 (hex) of [`phase_8_projection`] of the truth log for [`PHASE_7_FULL`] stepped two
/// days. Recorded on the phase 9 base commit 6676626, before any phase 9 change, by printing
/// the digest. With every phase 9 key at its default, nothing else may change.
const PHASE_8_TRUTH_SHA256: &str =
    "7a7e5ae55fbb25021222415bf10aa85036803b3126c6ad239c0a64e4d4ba4da4";

#[tokio::test]
async fn default_config_replays_phase_8_run() {
    use sha2::{Digest, Sha256};
    let m = mock_with(PHASE_7_FULL).await;
    m.step(Duration::from_secs(2 * 86_400)).await.unwrap();
    let log = m.truth_log();
    assert!(log.len() > 200, "{}", log.len());
    let digest = hex::encode(Sha256::digest(phase_8_projection(&log).as_bytes()));
    assert_eq!(digest, PHASE_8_TRUTH_SHA256, "log len {}", log.len());
}
