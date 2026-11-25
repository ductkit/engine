//! End to end through the binary: serve, subscribe, drive, export, replay (spec sections 3, 10-12).

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{Receiver, Server, Sse, temp_dir, write};
use serde_json::{Value, json};

/// Spec example rates at one tenth.
const RATES: &str = r#"
seed = 11
[accounts]
connect = true
connected_accounts = { initial = 2, connect_rate = 0.05, deauthorize_rate = 0.01 }
[history]
duration = "3h"
[actions]
new_customer = 1.0
start_subscription = 0.4
cancel_subscription = 0.05
catalog_change = 0.02
one_off_invoice = 0.2
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
"#;

type Key = (Option<String>, String, String);

fn key(account: &Value, object_type: &str, id: &str) -> Key {
    (
        account.as_str().map(String::from),
        object_type.to_string(),
        id.to_string(),
    )
}

/// The latest live state per object, as the subscription delivered it.
fn flatten_updates(updates: &[Value]) -> BTreeMap<Key, Value> {
    let mut latest: BTreeMap<Key, &Value> = BTreeMap::new();
    for u in updates {
        let k = key(
            &u["account"],
            u["object_type"].as_str().unwrap(),
            u["object_id"].as_str().unwrap(),
        );
        if latest
            .get(&k)
            .is_none_or(|old| old["version"].as_u64() < u["version"].as_u64())
        {
            latest.insert(k, u);
        }
    }
    latest
        .into_iter()
        .filter(|(_, u)| u["kind"] != "deleted")
        .map(|(k, u)| (k, u["state"].clone()))
        .collect()
}

/// The same shape from `GET /control/state`.
fn flatten_state(state: &Value) -> BTreeMap<Key, Value> {
    let mut out = BTreeMap::new();
    for scope in state["scopes"].as_array().unwrap() {
        for (t, objects) in scope["objects"].as_object().unwrap() {
            for o in objects.as_array().unwrap() {
                out.insert(
                    key(&scope["account"], t, o["id"].as_str().unwrap()),
                    o.clone(),
                );
            }
        }
    }
    out
}

// Multi-threaded: `Server::start` blocks its thread while the replay delivers to `rx`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drive_subscribe_export_replay() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let dir = temp_dir();
    let toml = format!(
        "{RATES}[[destinations]]\nname = \"snap\"\nurl = \"{}\"\npayload = \"snapshot\"\nenabled_events = [\"*\"]\nsecret = \"whsec_test\"\nconnect = true\n",
        rx.url("/hook")
    );
    let config = write(&dir, "mock.toml", &toml);
    let s = Server::serve(&config, &["--clock", "virtual"]);

    let mut sse = Sse::open_url(&s.control_url, "").await;
    let target = std::sync::atomic::AtomicU64::new(u64::MAX);
    let ordering = std::sync::atomic::Ordering::SeqCst;

    let reader = async {
        let mut updates: Vec<Value> = Vec::new();
        let mut max_seq = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while max_seq < target.load(ordering) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no update reached the target seq within 60 s: last seq seen {max_seq}, target {}",
                target.load(ordering)
            );
            match tokio::time::timeout(Duration::from_millis(200), sse.next()).await {
                Ok(Some((event, data))) => {
                    assert_ne!(event, "lagged", "subscriber lagged");
                    if event == "update" {
                        max_seq = max_seq.max(data["seq"].as_u64().unwrap());
                        updates.push(data);
                    }
                }
                Ok(None) => panic!("stream ended"),
                Err(_) => {}
            }
        }
        updates
    };

    let driver = async {
        let ok = |r: reqwest::Response| async move {
            assert_eq!(r.status(), 200);
            r.json::<Value>().await.unwrap()
        };
        ok(s.post("/control/step", json!({"duration": "2h"})).await).await;
        let acct = ok(s
            .post(
                "/control/ops",
                json!({"op": "connect_account", "api": "v2"}),
            )
            .await)
        .await;
        let id = acct["id"].as_str().expect("account id").to_string();
        ok(s.post(
            "/control/ops",
            json!({"op": "create_customer", "account": id, "customer": {"email": "e2e@example.com"}}),
        )
        .await)
        .await;
        ok(s.post("/control/step", json!({"duration": "2h"})).await).await;
        ok(s.post("/control/quiesce", json!({})).await).await;
        let log: Vec<Value> = serde_json::from_str(&s.truth_log().await).unwrap();
        target.store(log.last().unwrap()["seq"].as_u64().unwrap(), ordering);
        id
    };
    let (updates, acct) = tokio::join!(reader, driver);

    // 1. The subscription's latest state equals the dumped state.
    let state: Value = s.get("/control/state").await.json().await.unwrap();
    let from_sse = flatten_updates(&updates);
    let from_state = flatten_state(&state);
    assert!(from_state.keys().any(|k| k.0.as_deref() == Some(&acct)));
    assert_eq!(
        from_sse.keys().collect::<Vec<_>>(),
        from_state.keys().collect::<Vec<_>>()
    );
    for (k, v) in &from_state {
        assert_eq!(&from_sse[k], v, "{k:?}");
    }

    // 2. The snapshot destination received events carrying `account`.
    let got = rx.wait_for(1, Duration::from_secs(5)).await;
    assert!(
        got.iter().any(|r| {
            serde_json::from_slice::<Value>(&r.body)
                .map(|b| b["account"].is_string())
                .unwrap_or(false)
        }),
        "no delivery carried `account` among {}",
        got.len()
    );

    // 3. Export, replay, and compare truth logs.
    let original = s.truth_log().await;
    let export: Value = s.get("/control/export").await.json().await.unwrap();
    assert_eq!(export["format"], "ductkit-stripe-mock/run@1");
    let file = write(&dir, "run.json", &export.to_string());
    let replayed = Server::start(&[
        "replay",
        "--run",
        file.to_str().unwrap(),
        "--api-addr",
        "127.0.0.1:0",
        "--control-addr",
        "127.0.0.1:0",
    ]);
    assert_eq!(replayed.truth_log().await, original);
}
