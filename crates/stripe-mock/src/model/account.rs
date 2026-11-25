//! Connected account JSON: the v1 truth state and its v2 view (spec section 5.4).

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat};
use serde_json::{Value, json};

use crate::rng::SimRng;

/// Which Accounts API created an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountApi {
    V1,
    V2,
}

/// The account's `type` in `/v1/accounts`. `V2` accounts show `type: "none"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    Standard,
    Express,
    Custom,
    V2,
}

/// An Accounts v2 configuration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Configuration {
    Customer,
    Merchant,
    Recipient,
}

impl Configuration {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Configuration::Customer => "customer",
            Configuration::Merchant => "merchant",
            Configuration::Recipient => "recipient",
        }
    }
}

impl AccountKind {
    fn type_str(self) -> &'static str {
        match self {
            AccountKind::Standard => "standard",
            AccountKind::Express => "express",
            AccountKind::Custom => "custom",
            AccountKind::V2 => "none",
        }
    }

    fn dashboard(self) -> &'static str {
        match self {
            AccountKind::Standard | AccountKind::V2 => "full",
            AccountKind::Express => "express",
            AccountKind::Custom => "none",
        }
    }
}

/// What the mock remembers about a connected account besides its truth state.
#[derive(Debug, Clone)]
pub(crate) struct AccountMeta {
    pub id: String,
    pub api: AccountApi,
    pub kind: AccountKind,
    pub configurations: Vec<Configuration>,
    pub connected: bool,
}

/// The fields churn changes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AccountContent {
    pub name: String,
    pub email: String,
    pub metadata: BTreeMap<String, String>,
}

const NAMES: [&str; 5] = [
    "Acme Goods",
    "Bluebird Studio",
    "Café Önder",
    "Delta & Sons",
    "山田 商店",
];
const METADATA_KEYS: [&str; 3] = ["region", "tier", "note"];

/// Random fields for a new or updated account.
pub(crate) fn random_content(rng: &mut SimRng) -> AccountContent {
    let name = rng.choose(&NAMES).to_string();
    let email = format!("owner{}@example.com", rng.below(1_000_000));
    let mut metadata = BTreeMap::new();
    for _ in 0..rng.below(3) {
        let key = rng.choose(&METADATA_KEYS).to_string();
        metadata.insert(key, format!("v{}", rng.below(100)));
    }
    AccountContent {
        name,
        email,
        metadata,
    }
}

/// The account's truth state: the v1 object.
pub(crate) fn v1_state(id: &str, kind: AccountKind, created: i64, c: &AccountContent) -> Value {
    let mut v = json!({
        "id": id,
        "object": "account",
        "type": kind.type_str(),
        "business_profile": {"name": c.name},
        "email": c.email,
        "country": "US",
        "default_currency": "usd",
        "charges_enabled": true,
        "payouts_enabled": true,
        "details_submitted": true,
        "created": created,
        "metadata": c.metadata,
    });
    if matches!(kind, AccountKind::V2 | AccountKind::Custom) {
        v["controller"] = json!({"type": "application", "is_controller": true});
    }
    v
}

/// Replaces the fields churn changes.
pub(crate) fn apply_content(state: &mut Value, c: &AccountContent) {
    state["business_profile"]["name"] = json!(c.name);
    state["email"] = json!(c.email);
    state["metadata"] = json!(c.metadata);
}

/// The `/v2/core/accounts` view of an account's v1 `state`.
pub(crate) fn v2_view(state: &Value, meta: &AccountMeta, closed: bool) -> Value {
    let mut applied: Vec<Configuration> = meta.configurations.clone();
    applied.sort();
    let entry = |c: Configuration| {
        if applied.contains(&c) {
            json!({"applied": true})
        } else {
            Value::Null
        }
    };
    let created = state["created"]
        .as_i64()
        .and_then(|s| DateTime::from_timestamp(s, 0))
        .map(|d| d.to_rfc3339_opts(SecondsFormat::Millis, true));
    json!({
        "id": state["id"],
        "object": "v2.core.account",
        "applied_configurations": applied.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "closed": closed,
        "configuration": {
            "customer": entry(Configuration::Customer),
            "merchant": entry(Configuration::Merchant),
            "recipient": entry(Configuration::Recipient),
        },
        "contact_email": state["email"],
        "created": created,
        "dashboard": meta.kind.dashboard(),
        "defaults": null,
        "display_name": state["business_profile"]["name"],
        "future_requirements": null,
        "identity": null,
        "livemode": false,
        "metadata": state["metadata"],
        "requirements": null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content() -> AccountContent {
        AccountContent {
            name: "Acme".into(),
            email: "a@example.com".into(),
            metadata: BTreeMap::from([("k".to_string(), "v".to_string())]),
        }
    }

    #[test]
    fn v1_shape() {
        let s = v1_state("acct_1", AccountKind::Express, 100, &content());
        assert_eq!(s["type"], "express");
        assert_eq!(s["business_profile"]["name"], "Acme");
        assert!(s.get("controller").is_none());
        let c = v1_state("acct_1", AccountKind::Custom, 100, &content());
        assert_eq!(c["controller"]["is_controller"], true);
        let v = v1_state("acct_1", AccountKind::V2, 100, &content());
        assert_eq!(
            (v["type"].as_str(), v["controller"]["type"].as_str()),
            (Some("none"), Some("application"))
        );
    }

    #[test]
    fn v2_shape() {
        let s = v1_state("acct_1", AccountKind::V2, 1_767_225_600, &content());
        let meta = AccountMeta {
            id: "acct_1".into(),
            api: AccountApi::V2,
            kind: AccountKind::V2,
            configurations: vec![Configuration::Recipient, Configuration::Merchant],
            connected: true,
        };
        let v = v2_view(&s, &meta, false);
        assert_eq!(v["object"], "v2.core.account");
        assert_eq!(
            v["applied_configurations"],
            json!(["merchant", "recipient"])
        );
        assert_eq!(v["configuration"]["customer"], Value::Null);
        assert_eq!(v["configuration"]["merchant"], json!({"applied": true}));
        assert_eq!(v["created"], "2026-01-01T00:00:00.000Z");
        assert_eq!(v["dashboard"], "full");
        assert_eq!(v["display_name"], "Acme");
        assert_eq!(v["contact_email"], "a@example.com");
        assert_eq!(v2_view(&s, &meta, true)["closed"], true);
    }
}
