mod common;

use std::time::{Duration, Instant};

use axum::body::Bytes;
use common::{ctl_get, mock_with_control};
use ductkit_stripe_mock::{AccountApi, Action, MockStripe, NewCustomer};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

const SPEC_KEYS: [&str; 10] = [
    "seq",
    "at",
    "account",
    "object_type",
    "object_id",
    "version",
    "kind",
    "state",
    "events",
    "cause",
];
const WAIT: Duration = Duration::from_secs(5);

/// A parsed SSE stream: `next` yields `(event, data)` and skips comments.
struct Sse {
    stream: std::pin::Pin<Box<dyn futures_core::Stream<Item = reqwest::Result<Bytes>>>>,
    buf: String,
}

impl Sse {
    async fn open(m: &MockStripe, query: &str) -> Sse {
        let url = m.control_url().expect("control API is open");
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
    async fn next(&mut self) -> Option<(String, Value)> {
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

fn assert_spec_keys(update: &Value) {
    for key in SPEC_KEYS {
        assert!(update.get(key).is_some(), "missing `{key}` in {update}");
    }
}

fn customers(m: &MockStripe, n: usize) {
    for _ in 0..n {
        m.create_customer(NewCustomer::default()).unwrap();
    }
}

#[tokio::test]
async fn sse_snapshot_then_live() {
    let m = mock_with_control("seed = 1").await;
    customers(&m, 3);
    let mut sse = Sse::open(&m, "?types=customer").await;
    for _ in 0..3 {
        let (event, data) = sse.next().await.unwrap();
        assert_eq!(event, "update");
        assert_spec_keys(&data);
        assert_eq!(data["kind"], "snapshot");
        assert_eq!(data["object_type"], "customer");
    }
    assert_eq!(sse.next().await.unwrap(), ("synced".into(), json!({})));
    let id = m.create_customer(NewCustomer::default()).unwrap();
    let (event, data) = sse.next().await.unwrap();
    assert_eq!(event, "update");
    assert_spec_keys(&data);
    assert_eq!(data["kind"], "created");
    assert_eq!(data["object_id"], id);
}

#[tokio::test]
async fn sse_filters_by_account() {
    let m = mock_with_control("seed = 1\n[accounts]\nconnect = true").await;
    let acct = m.connect_account(AccountApi::V1).unwrap();
    m.account(&acct)
        .create_customer(NewCustomer::default())
        .unwrap();
    customers(&m, 1);

    let mut platform = Sse::open(&m, "?account=platform&types=customer").await;
    let mut scoped = Sse::open(&m, &format!("?account={acct}&types=customer")).await;
    m.account(&acct)
        .create_customer(NewCustomer::default())
        .unwrap();
    customers(&m, 1);
    for (sse, want) in [(&mut platform, Value::Null), (&mut scoped, json!(acct))] {
        let mut updates = 0;
        loop {
            let (event, data) = sse.next().await.unwrap();
            if event == "synced" {
                break;
            }
            assert_eq!(data["account"], want);
            updates += 1;
        }
        assert_eq!(updates, 1, "one snapshot update for {want}");
        let (_, live) = sse.next().await.unwrap();
        assert_eq!(live["account"], want);
        assert_eq!(live["kind"], "created");
    }

    for query in ["?account=acct_nope", "?types=nope"] {
        let r = ctl_get(&m, &format!("/control/subscribe{query}")).await;
        assert_eq!(r.status(), 400, "{query}");
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["error"]["code"], "invalid_request");
    }
}

#[tokio::test]
async fn state_rejects_unknown_account() {
    let m = mock_with_control("seed = 1").await;
    let r = ctl_get(&m, "/control/state?account=acct_nope").await;
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_request");
    let ok = ctl_get(&m, "/control/state?account=platform").await;
    assert_eq!(ok.status(), 200);
}

async fn ws_connect(
    m: &MockStripe,
    query: &str,
) -> impl futures_core::Stream<Item = Message> + use<> {
    let url = m.control_url().unwrap().replace("http://", "ws://");
    let (ws, _) = tokio_tungstenite::connect_async(format!("{url}/control/subscribe/ws{query}"))
        .await
        .expect("upgrades");
    ws.map(|m| m.expect("frame"))
}

async fn ws_json<S: futures_core::Stream<Item = Message> + Unpin>(ws: &mut S) -> Option<Value> {
    loop {
        match tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("ws stalled")
        {
            Some(Message::Text(t)) => return Some(serde_json::from_str(t.as_str()).unwrap()),
            Some(Message::Close(_)) | None => return None,
            Some(_) => {}
        }
    }
}

#[tokio::test]
async fn ws_snapshot_then_live() {
    let m = mock_with_control("seed = 1").await;
    customers(&m, 3);
    let mut ws = Box::pin(ws_connect(&m, "?types=customer").await);
    for _ in 0..3 {
        let f = ws_json(&mut ws).await.unwrap();
        assert_eq!(f["type"], "update");
        assert_spec_keys(&f["update"]);
        assert_eq!(f["update"]["kind"], "snapshot");
    }
    assert_eq!(ws_json(&mut ws).await.unwrap(), json!({"type": "synced"}));
    let id = m.create_customer(NewCustomer::default()).unwrap();
    let f = ws_json(&mut ws).await.unwrap();
    assert_eq!(f["type"], "update");
    assert_eq!(f["update"]["kind"], "created");
    assert_eq!(f["update"]["object_id"], id);
}

#[tokio::test]
async fn ws_rejects_bad_filter_before_upgrade() {
    let m = mock_with_control("seed = 1").await;
    let url = m.control_url().unwrap().replace("http://", "ws://");
    let err = tokio_tungstenite::connect_async(format!("{url}/control/subscribe/ws?types=nope"))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 400
    ));
}

#[tokio::test]
async fn slow_sse_client_gets_lagged() {
    let m = mock_with_control("seed = 1").await;
    let mut sse = Sse::open(&m, "?types=customer").await;
    // Do not read. The default buffer is 1024 live updates; the socket's own
    // buffers absorb some more, so generate far beyond both.
    for _ in 0..12_000 {
        m.run_action(Action::NewCustomer).unwrap();
    }
    let t = Instant::now();
    m.step(Duration::from_secs(1)).await.unwrap();
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "step blocked on a stalled subscriber: {:?}",
        t.elapsed()
    );

    let (mut updates, mut lagged) = (0, false);
    while let Some((event, _)) = sse.next().await {
        assert!(!lagged, "frame `{event}` after lagged");
        match event.as_str() {
            "update" => updates += 1,
            "synced" => {}
            "lagged" => lagged = true,
            other => panic!("unexpected event {other}"),
        }
    }
    assert!(updates > 0, "some updates arrive before lagged");
    assert!(lagged, "stream ends with lagged");
}

#[tokio::test]
async fn streams_end_on_shutdown() {
    let m = mock_with_control("seed = 1").await;
    let mut sse = Sse::open(&m, "").await;
    let mut ws = Box::pin(ws_connect(&m, "").await);
    assert_eq!(sse.next().await.unwrap().0, "synced");
    assert_eq!(ws_json(&mut ws).await.unwrap()["type"], "synced");
    let t = Instant::now();
    m.shutdown().await;
    let ended = tokio::time::timeout(Duration::from_secs(1), async {
        assert!(sse.next().await.is_none());
        assert!(ws_json(&mut ws).await.is_none());
    })
    .await;
    assert!(
        ended.is_ok(),
        "streams still open {:?} after shutdown",
        t.elapsed()
    );
}
