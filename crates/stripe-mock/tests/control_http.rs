mod common;

use std::time::Duration;

use common::{ctl_get, ctl_post, get, mock_with, mock_with_control};
use ductkit_stripe_mock::{
    Config, ControlOp, JournalEntry, MockStripe, ObjectType, SimTime, StartOptions, SubscribeFilter,
};
use serde_json::{Value, json};

const WALL: &str = "clock = { mode = \"wall\", speed = 1.0 }";

async fn json_of(r: reqwest::Response) -> (u16, Value) {
    let status = r.status().as_u16();
    (status, r.json().await.expect("JSON body"))
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().expect("error.code")
}

fn count_creates(journal: &[JournalEntry]) -> usize {
    journal
        .iter()
        .filter(|e| matches!(e.op, ControlOp::CreateCustomer { .. }))
        .count()
}

#[tokio::test]
async fn ports_are_separate() {
    let m = mock_with_control("seed = 1").await;
    let api = reqwest::Client::new()
        .get(format!("{}/control/now", m.api_url()))
        .bearer_auth("sk_test_mock")
        .send()
        .await
        .unwrap();
    assert_eq!(api.status(), 404);
    let url = m.control_url().unwrap();
    let r = reqwest::get(format!("{url}/v1/customers")).await.unwrap();
    assert_eq!(r.status(), 404);
    let (status, body) = json_of(ctl_get(&m, "/control/now").await).await;
    assert_eq!(status, 200);
    assert_eq!(body["now"], serde_json::to_value(m.now()).unwrap());
    let plain = mock_with("seed = 1").await;
    assert_eq!(plain.control_url(), None);
}

#[tokio::test]
async fn ops_route_applies_and_journals() {
    let m = mock_with_control("seed = 1").await;
    let (status, body) = json_of(
        ctl_post(
            &m,
            "/control/ops",
            json!({"op": "create_customer", "customer": {}}),
        )
        .await,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["result"], "customer");
    let id = body["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("cus_"));
    let r = get(&m, &format!("/v1/customers/{id}")).await;
    assert_eq!(r.status(), 200);

    let (status, export) = json_of(ctl_get(&m, "/control/export").await).await;
    assert_eq!(status, 200);
    let journal: Vec<JournalEntry> = serde_json::from_value(export["journal"].clone()).unwrap();
    assert_eq!(count_creates(&journal), 1, "recorded exactly once");
    assert_eq!(count_creates(&m.export_run().journal), 1);
}

#[tokio::test]
async fn step_and_advance() {
    let m = mock_with_control("seed = 1").await;
    let before = m.now();
    let (status, _) = json_of(ctl_post(&m, "/control/step", json!({"duration": "1h"})).await).await;
    assert_eq!(status, 200);
    assert_eq!(m.now().0 - before.0, 3_600_000);
    let (status, body) = json_of(
        ctl_post(
            &m,
            "/control/advance_to",
            json!({"time": "2000-01-01T00:00:00Z"}),
        )
        .await,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(code(&body), "time_travel");
    let later = serde_json::to_value(SimTime(m.now().0 + 5_000)).unwrap();
    let (status, _) =
        json_of(ctl_post(&m, "/control/advance_to", json!({"time": later})).await).await;
    assert_eq!(status, 200);
    assert_eq!(m.now().0 - before.0, 3_605_000);
}

#[tokio::test]
async fn control_errors_are_4xx_json() {
    let virt = mock_with_control("seed = 1\n[accounts]\nconnect = true").await;
    let url = virt.control_url().unwrap();
    let check = |status: u16, body: &Value, want: u16, want_code: Option<&str>| {
        assert_eq!(status, want, "{body}");
        assert!(body["error"]["message"].is_string(), "{body}");
        let c = code(body);
        if let Some(w) = want_code {
            assert_eq!(c, w, "{body}");
        }
    };

    let r = reqwest::Client::new()
        .post(format!("{url}/control/ops"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    let (s, b) = json_of(r).await;
    check(s, &b, 400, Some("invalid_request"));

    let (s, b) = json_of(ctl_post(&virt, "/control/ops", json!({"op": "nope"})).await).await;
    check(s, &b, 400, Some("invalid_request"));

    let (s, b) = json_of(ctl_get(&virt, "/control/objects/customer/cus_nope").await).await;
    check(s, &b, 404, Some("no_such_object"));

    let (s, b) = json_of(
        ctl_post(
            &virt,
            "/control/ops",
            json!({"op": "set_api_config", "config": {"stale_read": {"probability": 2.0}}}),
        )
        .await,
    )
    .await;
    check(s, &b, 400, Some("config"));

    let (s, b) = json_of(
        ctl_post(
            &virt,
            "/control/ops",
            json!({"op": "disconnect_account", "id": "acct_nope"}),
        )
        .await,
    )
    .await;
    check(s, &b, 404, Some("unknown_account"));

    let (s, b) = json_of(ctl_get(&virt, "/control/state?types=bogus").await).await;
    check(s, &b, 400, Some("invalid_request"));

    let (s, b) = json_of(ctl_get(&virt, "/control/nothing").await).await;
    check(s, &b, 404, None);

    let wall = mock_with_control(WALL).await;
    let (s, b) = json_of(ctl_post(&wall, "/control/step", json!({"duration": "1s"})).await).await;
    check(s, &b, 409, Some("clock_mode"));
    let (s, b) = json_of(ctl_post(&virt, "/control/pause", json!({})).await).await;
    check(s, &b, 409, Some("clock_mode"));

    // Durations that overflow simulated time are config errors, and leave the
    // mock usable (the sim lock is not poisoned).
    let dest = json!({"op": "add_destination", "destination": {
        "name": "d", "url": "http://127.0.0.1:9/hook", "payload": "snapshot",
        "enabled_events": ["*"], "secret": "whsec_test",
    }});
    let (s, d) = json_of(ctl_post(&virt, "/control/ops", dest).await).await;
    assert_eq!(s, 200, "{d}");
    let dest_id = d["destination"]["id"].as_str().expect("destination id");
    let huge = "9223372036854775807ms";
    for op in [
        json!({"op": "start_outage", "id": dest_id, "duration": huge}),
        json!({"op": "rotate_secret", "id": dest_id, "overlap": huge}),
        json!({"op": "start_outage", "id": "nope", "duration": huge}),
    ] {
        let (s, b) = json_of(ctl_post(&virt, "/control/ops", op).await).await;
        check(s, &b, 400, Some("config"));
        let (s, _) = json_of(ctl_get(&virt, "/control/now").await).await;
        assert_eq!(s, 200);
    }

    // A plain GET on the WebSocket route (no upgrade headers).
    let (s, b) = json_of(ctl_get(&virt, "/control/subscribe/ws").await).await;
    assert!((400..500).contains(&s), "{s} {b}");
    check(s, &b, s, None);

    // A non-JSON content type.
    let r = reqwest::Client::new()
        .post(format!("{url}/control/ops"))
        .header("content-type", "text/plain")
        .body(r#"{"op":"quiesce"}"#)
        .send()
        .await
        .unwrap();
    let (s, b) = json_of(r).await;
    check(s, &b, 415, Some("unsupported_media_type"));

    let (s, b) = json_of(ctl_post(&virt, "/control/step", json!({"duration": "xyz"})).await).await;
    check(s, &b, 400, Some("invalid_request"));

    let (s, b) = json_of(ctl_get(&virt, "/control/truth_log?limit=abc").await).await;
    check(s, &b, 400, Some("invalid_request"));

    let r = reqwest::Client::new()
        .delete(format!("{url}/control/now"))
        .send()
        .await
        .unwrap();
    let (s, b) = json_of(r).await;
    check(s, &b, 405, Some("method_not_allowed"));
}

#[tokio::test]
async fn cross_origin_requests_are_rejected() {
    let m = mock_with_control("seed = 1").await;
    let url = m.control_url().unwrap();
    let port = url.rsplit(':').next().unwrap().to_string();
    let client = reqwest::Client::new();
    let forbidden = |r: (u16, Value)| {
        assert_eq!(r.0, 403, "{}", r.1);
        assert_eq!(code(&r.1), "forbidden", "{}", r.1);
    };

    // A page on another origin posting to the control API.
    let r = client
        .post(format!("{url}/control/quiesce"))
        .header("origin", "https://evil.example")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    forbidden(json_of(r).await);
    let r = client
        .post(format!("{url}/control/quiesce"))
        .header("origin", "https://evil.example")
        .header("content-type", "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap();
    forbidden(json_of(r).await);
    assert!(m.generation_enabled(), "quiesce did not run");

    // DNS rebinding: a foreign Host name.
    let r = client
        .get(format!("{url}/control/export"))
        .header("host", "evil.example:8421")
        .send()
        .await
        .unwrap();
    forbidden(json_of(r).await);

    // A form post (no CORS preflight) is not JSON.
    let waiting = tokio::spawn(m.shutdown_requested());
    let r = client
        .post(format!("{url}/control/shutdown"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("a=b")
        .send()
        .await
        .unwrap();
    let (s, b) = json_of(r).await;
    assert_eq!((s, code(&b)), (415, "unsupported_media_type"), "{b}");
    let r = client
        .post(format!("{url}/control/pause"))
        .send()
        .await
        .unwrap();
    let (s, b) = json_of(r).await;
    assert_eq!((s, code(&b)), (415, "unsupported_media_type"), "{b}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiting.is_finished(), "shutdown was requested");

    // A WebSocket upgrade from another origin.
    let ws = url.replace("http://", "ws://");
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("{ws}/control/subscribe/ws"),
    )
    .unwrap();
    req.headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    let err = tokio_tungstenite::connect_async(req).await.unwrap_err();
    assert!(
        matches!(&err, tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 403),
        "{err:?}"
    );

    // The same requests from loopback origins and hosts succeed.
    for (host, origin) in [
        (
            format!("localhost:{port}"),
            format!("http://localhost:{port}"),
        ),
        (
            format!("127.0.0.1:{port}"),
            "http://127.0.0.1:3000".to_string(),
        ),
        (format!("[::1]:{port}"), "http://[::1]:3000".to_string()),
    ] {
        let r = client
            .get(format!("{url}/control/export"))
            .header("host", &host)
            .header("origin", &origin)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{host} {origin}");
        let r = client
            .post(format!("{url}/control/quiesce"))
            .header("host", &host)
            .header("origin", &origin)
            .header("content-type", "application/json; charset=utf-8")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{host} {origin}");
    }
    let mut req = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        format!("{ws}/control/subscribe/ws"),
    )
    .unwrap();
    req.headers_mut().insert(
        "origin",
        format!("http://localhost:{port}").parse().unwrap(),
    );
    tokio_tungstenite::connect_async(req)
        .await
        .expect("loopback origin upgrades");
    let r = client
        .post(format!("{url}/control/shutdown"))
        .header("origin", format!("http://127.0.0.1:{port}"))
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("resolved within 1s")
        .unwrap();
}

#[tokio::test]
async fn script_route_reports_partial() {
    let m = mock_with_control("seed = 1").await;
    let (status, body) = json_of(
        ctl_post(
            &m,
            "/control/script",
            json!({"steps": [
                {"op": "create_customer", "customer": {}},
                {"op": "create_customer", "customer": {}},
                {"op": "delete_customer", "id": "cus_nope"},
                {"op": "create_customer", "customer": {}},
            ]}),
        )
        .await,
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(body["completed"], 2);
    assert_eq!(code(&body), "no_such_object");
    assert_eq!(count_creates(&m.export_run().journal), 2);

    let (status, body) = json_of(
        ctl_post(
            &m,
            "/control/script",
            json!({"steps": [{"op": "create_customer", "customer": {}}]}),
        )
        .await,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["result"], "customer");
}

#[tokio::test]
async fn inspection_routes() {
    let m = mock_with_control(
        "seed = 3\n[actions]\nnew_customer = 2.0\nstart_subscription = 1.0\n[history]\nduration = \"3d\"\n[accounts]\nconnect = true\n\n[[destinations]]\nname = \"d\"\nurl = \"http://127.0.0.1:9/hook\"\npayload = \"snapshot\"\nenabled_events = [\"*\"]\nsecret = \"whsec_test\"\n",
    )
    .await;
    let (_, state) = json_of(ctl_get(&m, "/control/state").await).await;
    assert_eq!(state, m.dump_state(&SubscribeFilter::default()));
    let (_, only) =
        json_of(ctl_get(&m, "/control/state?types=customer&account=platform").await).await;
    let scopes = only["scopes"].as_array().unwrap();
    assert_eq!(scopes.len(), 1);
    assert!(scopes[0]["account"].is_null());
    assert_eq!(scopes[0]["objects"].as_object().unwrap().len(), 1);

    let (_, config) = json_of(ctl_get(&m, "/control/config").await).await;
    let config: Config = serde_json::from_value(config).unwrap();
    assert_eq!(config, m.effective_config());

    let (_, page1) = json_of(ctl_get(&m, "/control/truth_log?after_seq=0&limit=5").await).await;
    assert_eq!(page1["data"].as_array().unwrap().len(), 5);
    assert_eq!(page1["has_more"], true);
    assert_eq!(page1["data"][0]["seq"], 1);
    let (_, page2) = json_of(ctl_get(&m, "/control/truth_log?after_seq=5&limit=5").await).await;
    assert_eq!(page2["data"][0]["seq"], 6);
    let total = m.truth_log().len();
    let (_, tail) =
        json_of(ctl_get(&m, &format!("/control/truth_log?after_seq={}", total - 1)).await).await;
    assert_eq!(tail["data"].as_array().unwrap().len(), 1);
    assert_eq!(tail["has_more"], false);

    let (_, accounts) = json_of(ctl_get(&m, "/control/accounts").await).await;
    assert_eq!(accounts, serde_json::to_value(m.accounts()).unwrap());
    let (_, dests) = json_of(ctl_get(&m, "/control/destinations").await).await;
    assert_eq!(dests, serde_json::to_value(m.destinations()).unwrap());
    assert_eq!(dests.as_array().unwrap().len(), 1);

    let (_, events) = json_of(ctl_get(&m, "/control/events?limit=3").await).await;
    assert_eq!(events["data"].as_array().unwrap().len(), 3);
    assert_eq!(events["has_more"], true);
    let last = events["data"][2]["id"].as_str().unwrap().to_string();
    let (_, next) =
        json_of(ctl_get(&m, &format!("/control/events?after={last}&limit=1")).await).await;
    assert_eq!(
        next["data"][0]["id"],
        serde_json::to_value(&m.event_timeline()[3].id).unwrap()
    );
    let (s, _) = json_of(ctl_get(&m, "/control/events?after=evt_nope").await).await;
    assert_eq!(s, 404);

    let (_, dl) = json_of(ctl_get(&m, "/control/deliveries?limit=2").await).await;
    assert!(dl["data"].as_array().unwrap().len() <= 2);
    assert!(dl["has_more"].is_boolean());

    let cus = m
        .truth_log()
        .iter()
        .find(|u| u.object_type == ObjectType::Customer)
        .unwrap()
        .object_id
        .clone();
    let (s, obj) = json_of(ctl_get(&m, &format!("/control/objects/customer/{cus}")).await).await;
    assert_eq!(s, 200);
    assert_eq!(obj["object_id"], cus);
}

#[tokio::test]
async fn quiesce_route() {
    let m = mock_with_control("seed = 1").await;
    let (s, _) = json_of(ctl_post(&m, "/control/quiesce", json!({})).await).await;
    assert_eq!(s, 200);
    let (_, g) = json_of(ctl_get(&m, "/control/generation").await).await;
    assert_eq!(g, json!({"enabled": false}));
    let (s, _) = json_of(ctl_post(&m, "/control/generation/resume", json!({})).await).await;
    assert_eq!(s, 200);
    let (_, g) = json_of(ctl_get(&m, "/control/generation").await).await;
    assert_eq!(g, json!({"enabled": true}));
}

#[tokio::test]
async fn wall_controls_and_rate_limit() {
    let m = mock_with_control(WALL).await;
    for path in ["/control/pause", "/control/resume"] {
        let (s, _) = json_of(ctl_post(&m, path, json!({})).await).await;
        assert_eq!(s, 200);
    }
    let (s, _) = json_of(ctl_post(&m, "/control/speed", json!({"speed": 2.0})).await).await;
    assert_eq!(s, 200);
    let (s, b) = json_of(ctl_post(&m, "/control/speed", json!({"speed": -1.0})).await).await;
    assert_eq!((s, code(&b)), (400, "config"));
    let (s, _) =
        json_of(ctl_post(&m, "/control/rate_limit", json!({"duration": "1s"})).await).await;
    assert_eq!(s, 200);
    assert_eq!(get(&m, "/v1/customers").await.status(), 429);
}

#[tokio::test]
async fn shutdown_route() {
    let m = mock_with_control("seed = 1").await;
    let waiting = tokio::spawn(m.shutdown_requested());
    let (s, _) = json_of(ctl_post(&m, "/control/shutdown", json!({})).await).await;
    assert_eq!(s, 202);
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("resolved within 1s")
        .unwrap();
    // The route does not tear the server down itself.
    assert_eq!(ctl_get(&m, "/control/now").await.status(), 200);
    let url = m.control_url().unwrap();
    m.shutdown().await;
    assert!(reqwest::get(format!("{url}/control/now")).await.is_err());
}

#[tokio::test]
async fn start_with_api_addr() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let m = MockStripe::start_with(
        Config::default(),
        StartOptions {
            api_addr: Some(addr),
            control_addr: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(m.api_url(), format!("http://{addr}"));
}

#[tokio::test]
async fn state_keys_match_subscription_object_types() {
    use futures_util::StreamExt;
    let m = mock_with_control(
        "seed = 5
[actions]
new_customer = 4
start_subscription = 3
[history]
duration = \"6h\"
",
    )
    .await;
    let mut from_stream = std::collections::BTreeSet::new();
    let mut s = m.subscribe(SubscribeFilter::default());
    while let Some(item) = s.next().await {
        match item {
            ductkit_stripe_mock::SubscriptionItem::Update(u) => {
                if u.kind != ductkit_stripe_mock::ChangeKind::Deleted {
                    from_stream.insert(
                        serde_json::to_value(u.object_type)
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .to_string(),
                    );
                }
            }
            _ => break,
        }
    }
    let state: Value = ctl_get(&m, "/control/state").await.json().await.unwrap();
    let mut from_state = std::collections::BTreeSet::new();
    for scope in state["scopes"].as_array().unwrap() {
        for k in scope["objects"].as_object().unwrap().keys() {
            from_state.insert(k.clone());
        }
    }
    assert!(from_state.contains("invoice_line_item"), "{from_state:?}");
    assert_eq!(from_stream, from_state);
}

#[tokio::test]
async fn unbounded_durations_are_rejected_and_the_mock_survives() {
    let m = mock_with_control("seed = 1").await;
    let huge = "9223372036854775807ms";
    let ops = |op: Value| ctl_post(&m, "/control/ops", op);
    let rejected = |s: u16, b: &Value| {
        assert_eq!(s, 400, "{b}");
        assert_eq!(code(b), "config", "{b}");
        let msg = b["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("100 years"), "names the limit: {msg}");
    };

    // The retry schedule that used to poison the sim lock.
    let (s, b) = json_of(
        ops(json!({"op": "set_webhooks_config", "config": {"retry_schedule": [huge]}})).await,
    )
    .await;
    rejected(s, &b);
    let (s, _) = json_of(ctl_get(&m, "/control/now").await).await;
    assert_eq!(s, 200);

    // Every other duration-typed value is bounded too.
    let fixed = json!({"distribution": "fixed", "ms": i64::MAX});
    let lognormal = json!({"distribution": "lognormal", "median_ms": 1.0, "p99_ms": 1e300});
    for op in [
        json!({"op": "set_webhooks_config", "config": {"reorder_window": huge}}),
        json!({"op": "set_webhooks_config", "config": {"response_timeout": huge}}),
        json!({"op": "set_webhooks_config", "config": {"max_retry_duration": huge}}),
        json!({"op": "set_webhooks_config", "config": {"disable_after_failures": huge}}),
        json!({"op": "set_webhooks_config", "config": {"signature_tolerance": huge}}),
        json!({"op": "set_webhooks_config", "config": {"delivery_delay": fixed}}),
        json!({"op": "set_webhooks_config", "config": {"delivery_delay": lognormal}}),
        json!({"op": "set_actions_config", "config": {"step_delay": fixed}}),
        json!({"op": "set_actions_config", "config": {"step_delay": lognormal}}),
        json!({"op": "set_events_config", "config": {"visibility_lag": fixed}}),
        json!({"op": "set_events_config", "config": {"late_tail": {"min": "1ms", "max": huge}}}),
        json!({"op": "set_api_config", "config": {"latency": fixed}}),
        json!({"op": "set_api_config", "config": {"stale_read": {"max_lag": huge}}}),
    ] {
        let (s, b) = json_of(ops(op.clone()).await).await;
        assert_eq!((s, code(&b)), (400, "config"), "{op}: {b}");
        let (s, _) = json_of(ctl_get(&m, "/control/now").await).await;
        assert_eq!(s, 200, "{op}");
    }

    // The full repro, which used to panic under the sim lock.
    let dest = json!({"op": "add_destination", "destination": {
        "name": "dead", "url": "http://127.0.0.1:9/hook", "payload": "snapshot",
        "enabled_events": ["*"], "secret": "whsec_test",
    }});
    let (s, b) = json_of(ops(dest).await).await;
    assert_eq!(s, 200, "{b}");
    let (s, b) = json_of(ops(json!({"op": "create_customer", "customer": {}})).await).await;
    assert_eq!(s, 200, "{b}");
    let (s, b) = json_of(ctl_post(&m, "/control/step", json!({"duration": "2h"})).await).await;
    assert_eq!(s, 200, "{b}");
    let (s, b) = json_of(ctl_get(&m, "/control/now").await).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(get(&m, "/v1/customers").await.status(), 200);

    // A value exactly at the limit is accepted and cannot overflow either.
    let at_limit = "3153600000000ms";
    let (s, b) = json_of(
        ops(
            json!({"op": "set_webhooks_config", "config": {"retry_schedule": [at_limit],
            "max_retry_duration": at_limit, "reorder_window": at_limit,
            "delivery_delay": {"distribution": "fixed", "ms": 3153600000000i64}}}),
        )
        .await,
    )
    .await;
    assert_eq!(s, 200, "{b}");
    let (s, b) = json_of(ops(json!({"op": "create_customer", "customer": {}})).await).await;
    assert_eq!(s, 200, "{b}");
    let (s, b) = json_of(ctl_post(&m, "/control/step", json!({"duration": "2h"})).await).await;
    assert_eq!(s, 200, "{b}");
    let (s, _) = json_of(ctl_get(&m, "/control/now").await).await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn wall_speed_is_capped() {
    let m = mock_with_control(WALL).await;
    for speed in [json!(1e300), json!(1000001.0)] {
        let (s, b) = json_of(ctl_post(&m, "/control/speed", json!({ "speed": speed })).await).await;
        assert_eq!((s, code(&b)), (400, "config"), "{b}");
    }
    // Let the ticker run a few times, then mutate: the wall lock is not poisoned.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (s, b) = json_of(
        ctl_post(
            &m,
            "/control/ops",
            json!({"op": "create_customer", "customer": {}}),
        )
        .await,
    )
    .await;
    assert_eq!(s, 200, "{b}");
    let (s, _) = json_of(ctl_post(&m, "/control/speed", json!({"speed": 1000000.0})).await).await;
    assert_eq!(s, 200, "the cap itself is allowed");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (s, _) = json_of(ctl_post(&m, "/control/pause", json!({})).await).await;
    assert_eq!(s, 200);
}
