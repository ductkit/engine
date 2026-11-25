mod common;

use std::time::{Duration, Instant};

use common::{get, mock_with};
use ductkit_stripe_mock::MockStripe;
use serde_json::Value;

async fn status(m: &MockStripe) -> u16 {
    get(m, "/v1/customers").await.status().as_u16()
}

#[tokio::test]
async fn rate_limit_returns_429_after_burst() {
    let m = mock_with("[api]\nrate_limit = { requests_per_second = 1, burst = 3 }").await;
    let mut statuses = Vec::new();
    let mut limited = Vec::new();
    for _ in 0..5 {
        let r = get(&m, "/v1/customers").await;
        statuses.push(r.status().as_u16());
        if r.status() == 429 {
            limited.push(r);
        }
    }
    assert_eq!(statuses, [200, 200, 200, 429, 429]);
    for r in limited {
        assert_eq!(r.headers()["stripe-should-retry"], "true");
        assert!(!r.headers()["request-id"].is_empty());
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["error"]["code"], "rate_limit");
        assert_eq!(v["error"]["type"], "invalid_request_error");
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(status(&m).await, 200);
}

#[tokio::test]
async fn server_errors_follow_config() {
    let m = mock_with("[api]\nerrors = { \"500\" = 1.0 }").await;
    let r = get(&m, "/v1/customers").await;
    assert_eq!(r.status(), 500);
    assert_eq!(r.headers()["stripe-should-retry"], "true");
    assert!(!r.headers()["request-id"].is_empty());
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "api_error");
    m.update_api_config(|a| {
        a.errors.e500 = 0.0;
        a.errors.e503 = 1.0;
    })
    .unwrap();
    assert_eq!(status(&m).await, 503);
    assert!(m.update_api_config(|a| a.errors.reset = 1.5).is_err());
}

#[tokio::test]
async fn faults_apply_after_auth() {
    let m = mock_with("[api]\nerrors = { \"500\" = 1.0 }").await;
    let r = reqwest::Client::new()
        .get(format!("{}/v1/customers", m.api_url()))
        .bearer_auth("sk_test_wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn reset_drops_connection() {
    let m = mock_with("[api]\nerrors = { reset = 1.0 }").await;
    let url = format!("{}/v1/customers", m.api_url());
    let result = async {
        reqwest::Client::new()
            .get(url)
            .bearer_auth("sk_test_mock")
            .send()
            .await?
            .bytes()
            .await
    }
    .await;
    assert!(result.is_err(), "{result:?}");
}

#[tokio::test]
async fn timeout_hangs_until_client_gives_up() {
    let m = mock_with("[api]\nerrors = { timeout = 1.0 }").await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let url = format!("{}/v1/customers", m.api_url());
    let result = async {
        client
            .get(url)
            .bearer_auth("sk_test_mock")
            .send()
            .await?
            .bytes()
            .await
    }
    .await;
    assert!(result.as_ref().is_err_and(|e| e.is_timeout()), "{result:?}");
    let started = Instant::now();
    tokio::time::timeout(Duration::from_secs(2), m.shutdown())
        .await
        .expect("shutdown does not wait for the held response");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn latency_delays_responses() {
    let m = mock_with("[api]\nlatency = { distribution = \"fixed\", ms = 300 }").await;
    let started = Instant::now();
    assert_eq!(status(&m).await, 200);
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn force_rate_limit_storm() {
    let m = mock_with("").await;
    m.force_rate_limit(Duration::from_millis(500));
    assert_eq!(status(&m).await, 429);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(status(&m).await, 200);
}

#[tokio::test]
async fn fault_sequence_is_deterministic() {
    let cfg = "seed = 9\n[api]\nerrors = { \"500\" = 0.5 }";
    let mut runs = Vec::new();
    for _ in 0..2 {
        let m = mock_with(cfg).await;
        let mut s = Vec::new();
        for _ in 0..30 {
            s.push(status(&m).await);
        }
        runs.push(s);
    }
    assert_eq!(runs[0], runs[1]);
    assert!(
        runs[0].contains(&200) && runs[0].contains(&500),
        "{:?}",
        runs[0]
    );
}
