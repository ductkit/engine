#![allow(dead_code)]

use ductkit_stripe_mock::{Config, MockStripe};

/// Parses `toml` and starts a mock, panicking on error.
pub async fn mock_with(toml: &str) -> MockStripe {
    let config = Config::from_toml_str(toml).expect("valid config");
    MockStripe::start(config).await.expect("mock starts")
}
