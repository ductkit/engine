#![allow(dead_code)]

use ductkit_stripe_mock::{Config, MockStripe};

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
