//! Mock configuration (spec section 4). Rates are events per simulated hour.

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;

use crate::MockError;

/// The only Stripe API version the mock serves (spec section 7.1).
pub const PINNED_API_VERSION: &str = "2026-11-18.endive";

/// Top-level spec sections that phase 1 does not implement yet.
const UNIMPLEMENTED_SECTIONS: [&str; 7] = [
    "accounts",
    "history",
    "event_retention",
    "events",
    "webhooks",
    "payloads",
    "destinations",
];

/// Full mock configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Seed for every random decision.
    pub seed: u64,
    /// Must equal [`PINNED_API_VERSION`].
    pub api_version: String,
    pub clock: ClockConfig,
    pub api: ApiConfig,
    pub actions: ActionsConfig,
    pub churn: ChurnConfig,
}

/// How simulated time moves.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClockConfig {
    pub mode: ClockMode,
    /// Simulated time at start.
    pub start: DateTime<Utc>,
    /// Simulated seconds per real second; only used in wall mode.
    pub speed: f64,
}

/// Clock mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockMode {
    /// Time moves only when the test advances it.
    #[default]
    Virtual,
    /// Time follows the real clock multiplied by `speed`.
    Wall,
}

/// Stripe-compatible API settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// Accepted secret keys; each must start with `sk_test_`.
    pub keys: Vec<String>,
}

/// Business action rates.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionsConfig {
    pub new_customer: f64,
}

/// Per-type churn rates.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChurnConfig {
    pub customer: CustomerChurn,
}

/// Customer churn rates.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CustomerChurn {
    pub update: f64,
    pub delete: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            seed: 0,
            api_version: PINNED_API_VERSION.to_string(),
            clock: ClockConfig::default(),
            api: ApiConfig::default(),
            actions: ActionsConfig::default(),
            churn: ChurnConfig::default(),
        }
    }
}

impl Default for ClockConfig {
    fn default() -> Self {
        ClockConfig {
            mode: ClockMode::Virtual,
            start: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            speed: 1.0,
        }
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        ApiConfig {
            keys: vec!["sk_test_mock".to_string()],
        }
    }
}

impl Config {
    /// Parses and validates a TOML config.
    pub fn from_toml_str(s: &str) -> Result<Config, MockError> {
        let table: toml::Table = s.parse().map_err(|e| MockError::Config(format!("{e}")))?;
        for section in UNIMPLEMENTED_SECTIONS {
            if table.contains_key(section) {
                return Err(MockError::Config(format!(
                    "`[{section}]` is not implemented yet (phase 1 supports seed, api_version,                      clock, api.keys, actions.new_customer, churn.customer)"
                )));
            }
        }
        let config: Config = toml::from_str(s).map_err(|e| MockError::Config(format!("{e}")))?;
        config.validate()?;
        Ok(config)
    }

    /// Reads, parses and validates a TOML config file.
    pub fn from_file(path: &Path) -> Result<Config, MockError> {
        Config::from_toml_str(&std::fs::read_to_string(path)?)
    }

    /// Checks values that the types alone cannot.
    pub fn validate(&self) -> Result<(), MockError> {
        let err = |m: String| Err(MockError::Config(m));
        if self.api_version != PINNED_API_VERSION {
            return err(format!(
                "api_version `{}` is not supported; the mock only serves `{PINNED_API_VERSION}`",
                self.api_version
            ));
        }
        for (name, rate) in [
            ("actions.new_customer", self.actions.new_customer),
            ("churn.customer.update", self.churn.customer.update),
            ("churn.customer.delete", self.churn.customer.delete),
        ] {
            if !rate.is_finite() || rate < 0.0 {
                return err(format!("{name} must be a finite rate >= 0, got {rate}"));
            }
        }
        if !self.clock.speed.is_finite() || self.clock.speed <= 0.0 {
            return err(format!("clock.speed must be > 0, got {}", self.clock.speed));
        }
        if self.api.keys.is_empty() {
            return err("api.keys must contain at least one key".to_string());
        }
        if let Some(k) = self.api.keys.iter().find(|k| !k.starts_with("sk_test_")) {
            return err(format!("api key `{k}` must start with `sk_test_`"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.api_version, "2026-11-18.endive");
        assert_eq!(c.clock.mode, ClockMode::Virtual);
        assert_eq!(c.clock.start.to_rfc3339(), "2026-01-01T00:00:00+00:00");
        assert_eq!(c.api.keys, vec!["sk_test_mock".to_string()]);
        assert_eq!(c.actions.new_customer, 0.0);
    }

    #[test]
    fn parses_phase1_keys() {
        let c = Config::from_toml_str(
            r#"
            seed = 42
            clock = { mode = "wall", start = "2026-03-01T00:00:00Z", speed = 60.0 }
            [api]
            keys = ["sk_test_a"]
            [actions]
            new_customer = 10.0
            [churn.customer]
            update = 3.0
            delete = 0.2
        "#,
        )
        .unwrap();
        assert_eq!(
            (c.seed, c.clock.mode, c.clock.speed),
            (42, ClockMode::Wall, 60.0)
        );
        assert_eq!(
            (c.churn.customer.update, c.churn.customer.delete),
            (3.0, 0.2)
        );
    }

    #[test]
    fn rejects_other_api_version() {
        let e = Config::from_toml_str(r#"api_version = "2025-01-01.acacia""#).unwrap_err();
        assert!(e.to_string().contains("2026-11-18.endive"));
    }

    #[test]
    fn unimplemented_section_is_named() {
        let e = Config::from_toml_str("[webhooks]\nduplicate_probability = 0.01").unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("[webhooks]") && msg.contains("not implemented"),
            "{msg}"
        );
    }

    #[test]
    fn spec_example_config_fails_clearly() {
        let spec = include_str!("../../../docs/stripe-mock-spec.md");
        let toml = spec
            .split("```toml\n")
            .nth(1)
            .unwrap()
            .split("```")
            .next()
            .unwrap();
        let msg = Config::from_toml_str(toml).unwrap_err().to_string();
        assert!(
            msg.contains("[accounts]") && msg.contains("not implemented"),
            "{msg}"
        );
    }

    #[test]
    fn rejects_bad_values() {
        for bad in [
            "[actions]\nnew_customer = -1.0",
            "clock = { speed = 0.0 }",
            "[api]\nkeys = []",
            "[api]\nkeys = [\"sk_live_x\"]",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
    }
}
