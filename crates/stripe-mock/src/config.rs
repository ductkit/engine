//! Mock configuration (spec section 4). Rates are events per simulated hour.

use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::MockError;

/// The only Stripe API version the mock serves (spec section 7.1).
pub const PINNED_API_VERSION: &str = "2026-11-18.endive";

/// Spec keys inside supported sections that are not implemented yet, with an
/// optional reason appended to the error.
const UNIMPLEMENTED_KEYS: [(&str, &[&str], &str); 1] = [(
    "actions",
    &["one_off_payment", "refund", "dispute"],
    " (needs payment objects, phase 7)",
)];

/// The first spec section or key that is not implemented, e.g. `[history]` or
/// `api.latency`, with its reason.
fn first_unimplemented(table: &toml::Table) -> Option<String> {
    for (section, keys, reason) in UNIMPLEMENTED_KEYS {
        if let Some(inner) = table.get(section).and_then(|v| v.as_table())
            && let Some(key) = keys.iter().find(|k| inner.contains_key(**k))
        {
            return Some(format!("`{section}.{key}` is not implemented yet{reason}"));
        }
    }
    None
}

/// Full mock configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Seed for every random decision.
    pub seed: u64,
    /// Must equal [`PINNED_API_VERSION`].
    pub api_version: String,
    pub clock: ClockConfig,
    pub history: HistoryConfig,
    pub api: ApiConfig,
    pub actions: ActionsConfig,
    pub churn: ChurnConfig,
    pub events: EventsConfig,
    pub event_retention: EventRetentionConfig,
    pub webhooks: WebhooksConfig,
    pub destinations: Vec<DestinationConfig>,
    pub payloads: PayloadsConfig,
    pub accounts: AccountsConfig,
}

/// Connect: the platform and its connected accounts (spec section 4).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountsConfig {
    /// Only `1` is implemented.
    pub platforms: u32,
    /// Whether the platform has connected accounts.
    pub connect: bool,
    pub connected_accounts: ConnectedAccounts,
    /// Share of connected accounts created through each Accounts API.
    pub account_api: AccountApiMix,
    /// Type mix of v1 accounts.
    pub account_types: AccountTypeMix,
}

/// How many connected accounts exist and how often they come and go.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectedAccounts {
    /// Accounts connected at the start of history.
    pub initial: u32,
    /// New connections per simulated hour.
    pub connect_rate: f64,
    /// Disconnections per simulated hour.
    pub deauthorize_rate: f64,
}

/// Share of new accounts created through each Accounts API; sums to 1.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountApiMix {
    pub v1: f64,
    pub v2: f64,
}

/// Share of v1 accounts of each type; sums to 1.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountTypeMix {
    pub standard: f64,
    pub express: f64,
    pub custom: f64,
}

impl Default for AccountsConfig {
    fn default() -> Self {
        AccountsConfig {
            platforms: 1,
            connect: false,
            connected_accounts: ConnectedAccounts::default(),
            account_api: AccountApiMix::default(),
            account_types: AccountTypeMix::default(),
        }
    }
}

impl Default for AccountApiMix {
    fn default() -> Self {
        AccountApiMix { v1: 0.5, v2: 0.5 }
    }
}

impl Default for AccountTypeMix {
    fn default() -> Self {
        AccountTypeMix {
            standard: 0.5,
            express: 0.3,
            custom: 0.2,
        }
    }
}

impl AccountsConfig {
    pub(crate) fn validate(&self) -> Result<(), MockError> {
        if self.platforms != 1 {
            return config_err("`accounts.platforms` other than 1 is not implemented yet");
        }
        let c = &self.connected_accounts;
        check_rate("accounts.connected_accounts.connect_rate", c.connect_rate)?;
        check_rate(
            "accounts.connected_accounts.deauthorize_rate",
            c.deauthorize_rate,
        )?;
        if !self.connect && (c.initial != 0 || c.connect_rate != 0.0 || c.deauthorize_rate != 0.0) {
            return config_err("`accounts.connected_accounts` needs `accounts.connect = true`");
        }
        check_mix(
            "accounts.account_api",
            &[("v1", self.account_api.v1), ("v2", self.account_api.v2)],
        )?;
        let t = &self.account_types;
        check_mix(
            "accounts.account_types",
            &[
                ("standard", t.standard),
                ("express", t.express),
                ("custom", t.custom),
            ],
        )
    }
}

/// Weights must be finite, not negative and sum to 1.
fn check_mix(name: &str, weights: &[(&str, f64)]) -> Result<(), MockError> {
    for (key, w) in weights {
        if !w.is_finite() || *w < 0.0 {
            return config_err(format!("{name}.{key} must be a number >= 0, got {w}"));
        }
    }
    let sum: f64 = weights.iter().map(|(_, w)| w).sum();
    if (sum - 1.0).abs() > 1e-9 {
        return config_err(format!("{name} must sum to 1, got {sum}"));
    }
    Ok(())
}

/// How simulated time moves.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClockConfig {
    pub mode: ClockMode,
    /// Simulated time at start.
    pub start: DateTime<Utc>,
    /// Simulated seconds per real second; only used in wall mode.
    pub speed: f64,
}

/// Clock mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockMode {
    /// Time moves only when the test advances it.
    #[default]
    Virtual,
    /// Time follows the real clock multiplied by `speed`.
    Wall,
}

/// Pre-generated data that exists before the run starts (spec section 6.3).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    /// How far before `clock.start` generation begins. Default `"0s"`: no history.
    pub duration: DurationStr,
    /// Multiplies every action and churn rate during history.
    pub scale: f64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        HistoryConfig {
            duration: DurationStr(0),
            scale: 1.0,
        }
    }
}

/// Stripe-compatible API settings.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// Accepted secret keys; each must start with `sk_test_`.
    pub keys: Vec<String>,
    /// Real (wall) delay before each response; never moves simulated time.
    pub latency: Distribution,
    /// Token bucket on wall time; `None` (the default) means no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// Probabilities of injected failures per request.
    pub errors: ApiErrors,
    /// Retrieve and list may show objects as they were up to `max_lag` ago.
    pub stale_read: StaleRead,
    /// A list page after the first may follow a fresh mutation of the listed type.
    pub pagination_writes: PaginationWrites,
}

/// Writes during pagination (spec section 7.4).
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaginationWrites {
    /// Per list request that carries a cursor.
    pub probability: f64,
}

/// Stale reads on object retrieve and list (spec section 7.4).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaleRead {
    /// Per request.
    pub probability: f64,
    /// A stale read lags by a uniform draw in `1ms..=max_lag`.
    pub max_lag: DurationStr,
}

impl Default for StaleRead {
    fn default() -> Self {
        StaleRead {
            probability: 0.0,
            max_lag: DurationStr(2_000),
        }
    }
}

/// A token bucket: `burst` requests at once, refilled at `requests_per_second`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub requests_per_second: f64,
    pub burst: u32,
}

/// Per-request failure probabilities; their sum is at most 1.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiErrors {
    /// 500 `api_error`.
    #[serde(rename = "500")]
    pub e500: f64,
    /// 503 `api_error`.
    #[serde(rename = "503")]
    pub e503: f64,
    /// No response until the client gives up.
    pub timeout: f64,
    /// The connection is dropped without a complete response.
    pub reset: f64,
}

impl ApiConfig {
    pub(crate) fn validate(&self) -> Result<(), MockError> {
        if self.keys.is_empty() {
            return config_err("api.keys must contain at least one key");
        }
        if let Some(k) = self.keys.iter().find(|k| !k.starts_with("sk_test_")) {
            return config_err(format!("api key `{k}` must start with `sk_test_`"));
        }
        self.latency.validate("api.latency")?;
        if let Some(r) = &self.rate_limit
            && !(r.requests_per_second > 0.0 && r.requests_per_second.is_finite() && r.burst > 0)
        {
            return config_err("api.rate_limit needs requests_per_second > 0 and burst > 0");
        }
        let e = &self.errors;
        for (name, p) in [
            ("api.errors.500", e.e500),
            ("api.errors.503", e.e503),
            ("api.errors.timeout", e.timeout),
            ("api.errors.reset", e.reset),
        ] {
            check_probability(name, p)?;
        }
        check_probability("api.stale_read.probability", self.stale_read.probability)?;
        check_probability(
            "api.pagination_writes.probability",
            self.pagination_writes.probability,
        )?;
        if self.stale_read.max_lag.ms() < 1 {
            return config_err("api.stale_read.max_lag must be at least 1ms");
        }
        let sum = e.e500 + e.e503 + e.timeout + e.reset;
        if sum > 1.0 {
            return config_err(format!(
                "api.errors probabilities must sum to at most 1, got {sum}"
            ));
        }
        Ok(())
    }
}

/// Business action rates (per simulated hour) and action shaping (spec section 6).
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionsConfig {
    pub new_customer: f64,
    pub start_subscription: f64,
    pub cancel_subscription: f64,
    pub catalog_change: f64,
    pub one_off_invoice: f64,
    pub renew_subscription_cycle: RenewMode,
    /// Gap between consecutive steps of one action.
    pub step_delay: Distribution,
    /// Chance that an invoice payment attempt fails.
    pub payment_failure_probability: f64,
}

impl ActionsConfig {
    pub(crate) fn validate(&self) -> Result<(), MockError> {
        for (name, rate) in [
            ("actions.new_customer", self.new_customer),
            ("actions.start_subscription", self.start_subscription),
            ("actions.cancel_subscription", self.cancel_subscription),
            ("actions.catalog_change", self.catalog_change),
            ("actions.one_off_invoice", self.one_off_invoice),
        ] {
            check_rate(name, rate)?;
        }
        self.step_delay.validate("actions.step_delay")?;
        check_probability(
            "actions.payment_failure_probability",
            self.payment_failure_probability,
        )
    }
}

/// Whether subscriptions renew at the end of each billing period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RenewMode {
    /// Renew at each period end.
    #[default]
    Natural,
    /// Never renew.
    Off,
}

/// Per-type churn rates.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChurnConfig {
    pub customer: CustomerChurn,
    pub product: CatalogChurn,
    pub price: CatalogChurn,
    pub subscription: UpdateChurn,
    pub invoice: UpdateChurn,
    pub account: UpdateChurn,
}

/// Churn for catalog objects, which archive instead of deleting.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogChurn {
    pub update: f64,
    pub archive: f64,
}

/// Churn that only updates (metadata).
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateChurn {
    pub update: f64,
}

/// Shape of rendered payloads.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PayloadsConfig {
    /// Embedded lists show at most this many entries (1..=100).
    pub nested_list_page_size: usize,
}

impl Default for PayloadsConfig {
    fn default() -> Self {
        PayloadsConfig {
            nested_list_page_size: 10,
        }
    }
}

/// Customer churn rates.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
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
            history: HistoryConfig::default(),
            api: ApiConfig::default(),
            actions: ActionsConfig::default(),
            churn: ChurnConfig::default(),
            events: EventsConfig::default(),
            event_retention: EventRetentionConfig::default(),
            webhooks: WebhooksConfig::default(),
            destinations: Vec::new(),
            payloads: PayloadsConfig::default(),
            accounts: AccountsConfig::default(),
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
            latency: Distribution::default(),
            rate_limit: None,
            errors: ApiErrors::default(),
            stale_read: StaleRead::default(),
            pagination_writes: PaginationWrites::default(),
        }
    }
}

impl Config {
    /// Parses and validates a TOML config.
    pub fn from_toml_str(s: &str) -> Result<Config, MockError> {
        let table: toml::Table = s.parse().map_err(|e| MockError::Config(format!("{e}")))?;
        if let Some(message) = first_unimplemented(&table) {
            return Err(MockError::Config(message));
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
        self.actions.validate()?;
        self.accounts.validate()?;
        let scale = self.history.scale;
        if !scale.is_finite() || scale < 0.0 {
            return err(format!(
                "history.scale must be a finite number >= 0, got {scale}"
            ));
        }
        if self.history.duration.ms() < 0 {
            return err("history.duration must be >= 0".to_string());
        }
        let ch = &self.churn;
        for (name, rate) in [
            ("churn.customer.update", ch.customer.update),
            ("churn.customer.delete", ch.customer.delete),
            ("churn.product.update", ch.product.update),
            ("churn.product.archive", ch.product.archive),
            ("churn.price.update", ch.price.update),
            ("churn.price.archive", ch.price.archive),
            ("churn.subscription.update", ch.subscription.update),
            ("churn.invoice.update", ch.invoice.update),
            ("churn.account.update", ch.account.update),
        ] {
            check_rate(name, rate)?;
        }
        if ch.account.update != 0.0 && !self.accounts.connect {
            return err("`churn.account.update` needs `accounts.connect = true`".to_string());
        }
        if !(1..=100).contains(&self.payloads.nested_list_page_size) {
            return err(format!(
                "payloads.nested_list_page_size must be between 1 and 100, got {}",
                self.payloads.nested_list_page_size
            ));
        }
        if !self.clock.speed.is_finite() || self.clock.speed <= 0.0 {
            return err(format!("clock.speed must be > 0, got {}", self.clock.speed));
        }
        self.api.validate()?;
        self.events.validate()?;
        self.webhooks.validate()?;
        let mut names = std::collections::BTreeSet::new();
        for d in &self.destinations {
            d.validate()?;
            if !names.insert(d.name.as_str()) {
                return err(format!("destination name `{}` is used twice", d.name));
            }
        }
        Ok(())
    }
}

fn config_err<T>(m: impl Into<String>) -> Result<T, MockError> {
    Err(MockError::Config(m.into()))
}

fn check_rate(name: &str, rate: f64) -> Result<(), MockError> {
    if !rate.is_finite() || rate < 0.0 {
        return config_err(format!("{name} must be a finite rate >= 0, got {rate}"));
    }
    Ok(())
}

fn check_probability(name: &str, p: f64) -> Result<(), MockError> {
    if !(0.0..=1.0).contains(&p) {
        return config_err(format!("{name} must be between 0 and 1, got {p}"));
    }
    Ok(())
}

/// A duration written as `"<int><unit>"` with unit `ms`, `s`, `m`, `h` or `d`; stored in ms.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct DurationStr(pub i64);

impl DurationStr {
    /// Milliseconds.
    pub fn ms(self) -> i64 {
        self.0
    }

    fn parse(s: &str) -> Option<DurationStr> {
        let split = s.find(|c: char| !c.is_ascii_digit())?;
        if split == 0 {
            return None;
        }
        let (num, unit) = s.split_at(split);
        let n: i64 = num.parse().ok()?;
        let factor = match unit {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => return None,
        };
        n.checked_mul(factor).map(DurationStr)
    }
}

impl Serialize for DurationStr {
    /// The compact form: the largest unit that divides evenly.
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let ms = self.0;
        let text = if ms != 0 {
            [
                (86_400_000, "d"),
                (3_600_000, "h"),
                (60_000, "m"),
                (1_000, "s"),
            ]
            .iter()
            .find(|(f, _)| ms % f == 0)
            .map_or_else(|| format!("{ms}ms"), |(f, u)| format!("{}{u}", ms / f))
        } else {
            "0s".to_string()
        };
        s.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for DurationStr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        DurationStr::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid duration `{s}`; expected e.g. \"500ms\", \"30s\", \"10m\", \"2h\" or \"3d\""
            ))
        })
    }
}

/// A distribution of delays in milliseconds.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "distribution", rename_all = "lowercase", deny_unknown_fields)]
pub enum Distribution {
    Fixed { ms: i64 },
    Lognormal { median_ms: f64, p99_ms: f64 },
}

impl Default for Distribution {
    fn default() -> Self {
        Distribution::Fixed { ms: 0 }
    }
}

impl Distribution {
    fn validate(&self, name: &str) -> Result<(), MockError> {
        match *self {
            Distribution::Fixed { ms } if ms < 0 => config_err(format!("{name}.ms must be >= 0")),
            Distribution::Lognormal { median_ms, p99_ms }
                if !(median_ms > 0.0 && median_ms <= p99_ms && p99_ms.is_finite()) =>
            {
                config_err(format!("{name} needs 0 < median_ms <= p99_ms"))
            }
            _ => Ok(()),
        }
    }
}

/// Occasional extra visibility delay.
#[derive(Clone, Debug, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LateTail {
    pub probability: f64,
    pub min: DurationStr,
    pub max: DurationStr,
}

/// Events API projection settings (spec section 8).
#[derive(Clone, Debug, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventsConfig {
    pub visibility_lag: Distribution,
    pub late_tail: LateTail,
    pub same_second_shuffle: bool,
}

impl EventsConfig {
    pub(crate) fn validate(&self) -> Result<(), MockError> {
        self.visibility_lag.validate("events.visibility_lag")?;
        check_probability("events.late_tail.probability", self.late_tail.probability)?;
        if self.late_tail.min > self.late_tail.max {
            return config_err("events.late_tail.min must be <= max");
        }
        Ok(())
    }
}

/// How long events stay in the Events APIs.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EventRetentionConfig {
    pub window: DurationStr,
}

impl Default for EventRetentionConfig {
    fn default() -> Self {
        EventRetentionConfig {
            window: DurationStr(30 * 86_400_000),
        }
    }
}

/// Retry delays after failed attempts.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum RetrySchedule {
    /// 1m, 5m, 30m, 2h, 5h, then 10h repeated.
    #[default]
    StripeLike,
    Explicit(Vec<DurationStr>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrList {
    S(String),
    L(Vec<DurationStr>),
}

impl Serialize for RetrySchedule {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            RetrySchedule::StripeLike => s.serialize_str("stripe_like"),
            RetrySchedule::Explicit(l) => l.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for RetrySchedule {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match StringOrList::deserialize(d)? {
            StringOrList::S(s) if s == "stripe_like" => Ok(RetrySchedule::StripeLike),
            StringOrList::S(s) => Err(serde::de::Error::custom(format!(
                "retry_schedule must be \"stripe_like\" or a list of durations, got `{s}`"
            ))),
            StringOrList::L(l) => Ok(RetrySchedule::Explicit(l)),
        }
    }
}

/// When a persistently failing destination is disabled.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum DisableAfter {
    /// 3 days.
    #[default]
    StripeLike,
    After(DurationStr),
}

impl DisableAfter {
    /// Milliseconds.
    pub fn ms(&self) -> i64 {
        match self {
            DisableAfter::StripeLike => 3 * 86_400_000,
            DisableAfter::After(d) => d.ms(),
        }
    }
}

impl Serialize for DisableAfter {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            DisableAfter::StripeLike => s.serialize_str("stripe_like"),
            DisableAfter::After(d) => d.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for DisableAfter {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "stripe_like" {
            return Ok(DisableAfter::StripeLike);
        }
        DurationStr::parse(&s)
            .map(DisableAfter::After)
            .ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "disable_after_failures must be \"stripe_like\" or a duration, got `{s}`"
                ))
            })
    }
}

/// Signatures with timestamps outside the receiver's tolerance.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClockSkew {
    pub probability: f64,
    pub max: DurationStr,
}

impl Default for ClockSkew {
    fn default() -> Self {
        ClockSkew {
            probability: 0.0,
            max: DurationStr(600_000),
        }
    }
}

/// Webhook delivery behaviour (spec section 9).
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhooksConfig {
    pub delivery_delay: Distribution,
    pub duplicate_probability: f64,
    pub reorder_window: DurationStr,
    pub response_timeout: DurationStr,
    pub retry_schedule: RetrySchedule,
    pub max_retry_duration: DurationStr,
    pub disable_after_failures: DisableAfter,
    pub signature_tolerance: DurationStr,
    pub clock_skew: ClockSkew,
    pub invalid_signature_probability: f64,
}

impl Default for WebhooksConfig {
    fn default() -> Self {
        WebhooksConfig {
            delivery_delay: Distribution::default(),
            duplicate_probability: 0.0,
            reorder_window: DurationStr(0),
            response_timeout: DurationStr(10_000),
            retry_schedule: RetrySchedule::StripeLike,
            max_retry_duration: DurationStr(3 * 86_400_000),
            disable_after_failures: DisableAfter::StripeLike,
            signature_tolerance: DurationStr(300_000),
            clock_skew: ClockSkew::default(),
            invalid_signature_probability: 0.0,
        }
    }
}

impl WebhooksConfig {
    pub(crate) fn validate(&self) -> Result<(), MockError> {
        self.delivery_delay.validate("webhooks.delivery_delay")?;
        check_probability("webhooks.duplicate_probability", self.duplicate_probability)?;
        check_probability(
            "webhooks.clock_skew.probability",
            self.clock_skew.probability,
        )?;
        check_probability(
            "webhooks.invalid_signature_probability",
            self.invalid_signature_probability,
        )?;
        if self.response_timeout.ms() <= 0 {
            return config_err("webhooks.response_timeout must be > 0");
        }
        if self.clock_skew.probability > 0.0 && self.clock_skew.max <= self.signature_tolerance {
            return config_err(
                "webhooks.clock_skew.max must be greater than webhooks.signature_tolerance",
            );
        }
        if let RetrySchedule::Explicit(l) = &self.retry_schedule
            && (l.is_empty() || l.iter().any(|d| d.ms() <= 0))
        {
            return config_err("webhooks.retry_schedule must be non-empty with delays > 0");
        }
        Ok(())
    }
}

/// Snapshot or thin payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PayloadFormat {
    Snapshot,
    Thin,
}

/// A webhook delivery target (spec section 9).
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationConfig {
    pub name: String,
    pub url: String,
    pub payload: PayloadFormat,
    pub enabled_events: Vec<String>,
    #[serde(default)]
    pub connect: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events_from: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// `false` creates the destination disabled.
    #[serde(default = "enabled_default", skip_serializing_if = "is_enabled")]
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

fn is_enabled(enabled: &bool) -> bool {
    *enabled
}

impl DestinationConfig {
    pub(crate) fn validate(&self) -> Result<(), MockError> {
        let n = &self.name;
        if n.is_empty() {
            return config_err("destination name must not be empty");
        }
        if !self.url.starts_with("http://") {
            return config_err(format!(
                "destination `{n}`: url must start with http:// (https is not supported yet)"
            ));
        }
        if self.enabled_events.is_empty() {
            return config_err(format!(
                "destination `{n}`: enabled_events must not be empty"
            ));
        }
        for e in &self.enabled_events {
            let thin_name = e.starts_with("v1.");
            match self.payload {
                PayloadFormat::Snapshot if thin_name => {
                    return config_err(format!(
                        "destination `{n}`: snapshot destinations take snapshot event names, not v1.* thin names (`{e}`)"
                    ));
                }
                PayloadFormat::Thin if e != "*" && !thin_name => {
                    return config_err(format!(
                        "destination `{n}`: thin destinations take v1.* event names (`{e}`)"
                    ));
                }
                _ => {}
            }
        }
        if self.connect && self.payload == PayloadFormat::Thin {
            return config_err(format!(
                "destination `{n}`: connect only applies to snapshot destinations (use events_from)"
            ));
        }
        if let Some(from) = &self.events_from {
            if self.payload == PayloadFormat::Snapshot {
                return config_err(format!(
                    "destination `{n}`: events_from only applies to thin destinations"
                ));
            }
            if from.is_empty() {
                return config_err(format!("destination `{n}`: events_from must not be empty"));
            }
            for (i, v) in from.iter().enumerate() {
                if v.starts_with("@organization_members") {
                    return config_err(format!(
                        "`destinations.events_from` value `{v}` is not implemented yet (organizations)"
                    ));
                }
                if v != "@self" && v != "@accounts" {
                    return config_err(format!(
                        "destination `{n}`: events_from value `{v}` must be \"@self\" or \"@accounts\""
                    ));
                }
                if from[..i].contains(v) {
                    return config_err(format!("destination `{n}`: events_from lists `{v}` twice"));
                }
            }
        }
        if let Some(secret) = &self.secret
            && !secret.starts_with("whsec_")
        {
            return config_err(format!("destination `{n}`: secret must start with whsec_"));
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
        let e = Config::from_toml_str("[actions]\none_off_payment = 1").unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("`actions.one_off_payment`") && msg.contains("not implemented"),
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
            msg.contains("`actions.one_off_payment`") && msg.contains("not implemented"),
            "{msg}"
        );
    }

    #[test]
    fn nested_unimplemented_keys_are_named() {
        let msg = Config::from_toml_str("[actions]\ndispute = 0.05")
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("actions.dispute") && msg.contains("not implemented"),
            "{msg}"
        );
    }

    #[test]
    fn api_fault_probabilities_validated() {
        let toml = "[api]\nerrors = { \"500\" = 0.7, \"503\" = 0.5 }";
        let msg = Config::from_toml_str(toml).unwrap_err().to_string();
        assert!(msg.contains("api.errors"), "{msg}");
        let c = Config::from_toml_str(
            "[api]\nerrors = { \"500\" = 0.5, timeout = 0.25 }\nrate_limit = { requests_per_second = 25, burst = 25 }",
        )
        .unwrap();
        assert_eq!((c.api.errors.e500, c.api.errors.timeout), (0.5, 0.25));
        assert!(
            Config::from_toml_str("[api]\nrate_limit = { requests_per_second = 0, burst = 1 }")
                .is_err()
        );
    }

    #[test]
    fn history_defaults_are_empty() {
        let d = Config::default();
        assert_eq!((d.history.duration.ms(), d.history.scale), (0, 1.0));
        let c = Config::from_toml_str(
            "[history]
duration = \"90d\"
scale = 0.5",
        )
        .unwrap();
        assert_eq!(
            (c.history.duration.ms(), c.history.scale),
            (90 * 86_400_000, 0.5)
        );
    }

    #[test]
    fn not_implemented_message_is_tidy() {
        let msg = Config::from_toml_str(
            "[actions]
one_off_payment = 1",
        )
        .unwrap_err()
        .to_string();
        assert!(!msg.contains("  "), "{msg:?}");
    }

    #[test]
    fn durations_parse() {
        for (s, ms) in [
            ("250ms", 250),
            ("300s", 300_000),
            ("10m", 600_000),
            ("2h", 7_200_000),
            ("3d", 259_200_000),
        ] {
            let c = Config::from_toml_str(&format!("[event_retention]\nwindow = \"{s}\"")).unwrap();
            assert_eq!(c.event_retention.window.ms(), ms, "{s}");
        }
        for bad in ["10", "x5s", "5w", "-1s"] {
            assert!(
                Config::from_toml_str(&format!("[event_retention]\nwindow = \"{bad}\"")).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn phase2_defaults_are_fault_free() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.events.visibility_lag, Distribution::Fixed { ms: 0 });
        assert_eq!(c.event_retention.window.ms(), 30 * 86_400_000);
        assert_eq!(c.webhooks.duplicate_probability, 0.0);
        assert_eq!(c.webhooks.response_timeout.ms(), 10_000);
        assert_eq!(c.webhooks.retry_schedule, RetrySchedule::StripeLike);
        assert!(c.destinations.is_empty());
    }

    #[test]
    fn parses_spec_phase2_sections() {
        let c = Config::from_toml_str(
            r#"
            [events]
            visibility_lag = { distribution = "lognormal", median_ms = 200, p99_ms = 5000 }
            late_tail = { probability = 0.001, min = "30s", max = "10m" }
            same_second_shuffle = true
            [webhooks]
            retry_schedule = ["1s", "2s"]
            disable_after_failures = "1h"
            clock_skew = { probability = 0.1, max = "10m" }
            [[destinations]]
            name = "thin"
            url = "http://127.0.0.1:4000/hook"
            payload = "thin"
            enabled_events = ["v1.customer.*"]
            events_from = ["@self"]
            secret = "whsec_test"
        "#,
        )
        .unwrap();
        assert_eq!(
            c.events.visibility_lag,
            Distribution::Lognormal {
                median_ms: 200.0,
                p99_ms: 5000.0
            }
        );
        assert_eq!(
            c.webhooks.retry_schedule,
            RetrySchedule::Explicit(vec![DurationStr(1000), DurationStr(2000)])
        );
        assert_eq!(
            c.webhooks.disable_after_failures,
            DisableAfter::After(DurationStr(3_600_000))
        );
        assert_eq!(c.destinations[0].payload, PayloadFormat::Thin);
    }

    #[test]
    fn rejects_bad_destinations() {
        let d = |url: &str, events: &str, extra: &str| {
            format!(
                "[[destinations]]\nname = \"d\"\nurl = \"{url}\"\npayload = \"snapshot\"\nenabled_events = [\"{events}\"]\n{extra}"
            )
        };
        for (toml, needle) in [
            (d("https://x", "*", ""), "http://"),
            (d("http://x", "v1.customer.created", ""), "v1."),
            (d("http://x", "*", "secret = \"nope\""), "whsec_"),
            (
                d("http://x", "*", "events_from = [\"@self\"]"),
                "events_from",
            ),
        ] {
            let msg = Config::from_toml_str(&toml).unwrap_err().to_string();
            assert!(msg.contains(needle), "{toml}: {msg}");
        }
    }

    fn dest_toml(payload: &str, events: &str, extra: &str) -> String {
        format!(
            "[[destinations]]
name = \"d\"
url = \"http://x\"
payload = \"{payload}\"
enabled_events = [\"{events}\"]
{extra}"
        )
    }

    #[test]
    fn connect_destination_validates() {
        let c = Config::from_toml_str(&dest_toml("snapshot", "*", "connect = true")).unwrap();
        assert!(c.destinations[0].connect);
        let msg = Config::from_toml_str(&dest_toml("thin", "*", "connect = true"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("connect"), "{msg}");
    }

    #[test]
    fn events_from_rules() {
        for ok in [
            "[\"@self\"]",
            "[\"@accounts\"]",
            "[\"@self\", \"@accounts\"]",
            "[\"@accounts\", \"@self\"]",
        ] {
            let t = dest_toml("thin", "*", &format!("events_from = {ok}"));
            Config::from_toml_str(&t).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for (bad, needle) in [
            ("[]", "events_from"),
            ("[\"@self\", \"@self\"]", "events_from"),
            ("[\"@accounts\", \"@accounts\"]", "events_from"),
            ("[\"@nope\"]", "events_from"),
            (
                "[\"@organization_members\"]",
                "not implemented yet (organizations)",
            ),
            (
                "[\"@self\", \"@organization_members/@accounts\"]",
                "not implemented yet (organizations)",
            ),
        ] {
            let t = dest_toml("thin", "*", &format!("events_from = {bad}"));
            let msg = Config::from_toml_str(&t).unwrap_err().to_string();
            assert!(msg.contains(needle), "{bad}: {msg}");
        }
        let msg = Config::from_toml_str(&dest_toml("snapshot", "*", "events_from = [\"@self\"]"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("events_from"), "{msg}");
    }

    #[test]
    fn spec_example_destinations_parse() {
        let c = Config::from_toml_str(
            r#"
            [[destinations]]
            name = "snapshot"
            url = "http://localhost:4000/webhooks/stripe"
            payload = "snapshot"
            enabled_events = ["*"]
            connect = true

            [[destinations]]
            name = "thin"
            url = "http://localhost:4000/webhooks/stripe-thin"
            payload = "thin"
            enabled_events = ["v1.customer.*", "v1.invoice.*"]
            events_from = ["@self", "@accounts"]
        "#,
        )
        .unwrap();
        assert!(c.destinations[0].connect);
        assert_eq!(
            c.destinations[1].events_from,
            Some(vec!["@self".to_string(), "@accounts".to_string()])
        );
    }

    #[test]
    fn rejects_bad_fault_values() {
        for bad in [
            "[webhooks]\nduplicate_probability = 1.5",
            "[events]\nvisibility_lag = { distribution = \"lognormal\", median_ms = 500, p99_ms = 100 }",
            "[webhooks]\nclock_skew = { probability = 0.5, max = \"60s\" }",
            "[webhooks]\nretry_schedule = []",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
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

    #[test]
    fn phase3_keys_parse() {
        let c = Config::from_toml_str(
            "[actions]\nstart_subscription = 4.0\nrenew_subscription_cycle = \"natural\"\nstep_delay = { distribution = \"fixed\", ms = 250 }\n[churn.product]\nupdate = 0.5\narchive = 0.1\n[payloads]\nnested_list_page_size = 3",
        )
        .unwrap();
        assert_eq!(c.actions.start_subscription, 4.0);
        assert_eq!(c.actions.renew_subscription_cycle, RenewMode::Natural);
        assert_eq!(c.actions.step_delay, Distribution::Fixed { ms: 250 });
        assert_eq!(
            (c.churn.product.update, c.churn.product.archive),
            (0.5, 0.1)
        );
        assert_eq!(c.payloads.nested_list_page_size, 3);
        let d = Config::default();
        assert_eq!(d.payloads.nested_list_page_size, 10);
        assert_eq!(d.actions.renew_subscription_cycle, RenewMode::Natural);
        let off = Config::from_toml_str("[actions]\nrenew_subscription_cycle = \"off\"").unwrap();
        assert_eq!(off.actions.renew_subscription_cycle, RenewMode::Off);
    }

    #[test]
    fn payment_objects_not_implemented() {
        let msg = Config::from_toml_str("[actions]\nrefund = 0.4")
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("`actions.refund`") && msg.contains("phase 7"),
            "{msg}"
        );
    }

    #[test]
    fn page_size_bounds() {
        for bad in ["0", "101"] {
            let msg = Config::from_toml_str(&format!("[payloads]\nnested_list_page_size = {bad}"))
                .unwrap_err()
                .to_string();
            assert!(msg.contains("payloads.nested_list_page_size"), "{msg}");
        }
    }

    #[test]
    fn failure_probability_bounds() {
        assert!(Config::from_toml_str("[actions]\npayment_failure_probability = 1.5").is_err());
        assert!(Config::from_toml_str("[churn.invoice]\nupdate = -1.0").is_err());
    }

    #[test]
    fn accounts_defaults() {
        let a = Config::default().accounts;
        assert_eq!(a.platforms, 1);
        assert!(!a.connect);
        assert_eq!(
            (
                a.connected_accounts.initial,
                a.connected_accounts.connect_rate,
                a.connected_accounts.deauthorize_rate
            ),
            (0, 0.0, 0.0)
        );
        assert_eq!((a.account_api.v1, a.account_api.v2), (0.5, 0.5));
        assert_eq!(
            (
                a.account_types.standard,
                a.account_types.express,
                a.account_types.custom
            ),
            (0.5, 0.3, 0.2)
        );
        assert_eq!(Config::default().churn.account.update, 0.0);
        let c = Config::from_toml_str(
            "[accounts]
connect = true",
        )
        .unwrap();
        assert!(c.accounts.connect);
    }

    #[test]
    fn accounts_mix_must_sum_to_one() {
        for bad in [
            "account_api = { v1 = 0.5, v2 = 0.4 }",
            "account_api = { v1 = 1.5, v2 = -0.5 }",
            "account_types = { standard = 0.5, express = 0.5, custom = 0.5 }",
        ] {
            let msg = Config::from_toml_str(&format!(
                "[accounts]
connect = true
{bad}"
            ))
            .unwrap_err()
            .to_string();
            assert!(msg.contains("accounts.account_"), "{bad}: {msg}");
        }
        let ok = "[accounts]
account_api = { v1 = 1.0, v2 = 0.0 }
account_types = { standard = 1.0, express = 0.0, custom = 0.0 }";
        assert!(Config::from_toml_str(ok).is_ok());
    }

    #[test]
    fn accounts_need_connect() {
        for bad in [
            "initial = 2",
            "connect_rate = 0.5",
            "deauthorize_rate = 0.1",
        ] {
            let msg = Config::from_toml_str(&format!(
                "[accounts]
connected_accounts = {{ {bad} }}"
            ))
            .unwrap_err()
            .to_string();
            assert!(
                msg.contains("`accounts.connected_accounts` needs `accounts.connect = true`"),
                "{bad}: {msg}"
            );
        }
    }

    #[test]
    fn account_churn_needs_connect() {
        let msg = Config::from_toml_str(
            "[churn.account]
update = 0.5",
        )
        .unwrap_err()
        .to_string();
        assert!(
            msg.contains("churn.account.update") && msg.contains("accounts.connect = true"),
            "{msg}"
        );
        assert!(
            Config::from_toml_str(
                "[accounts]
connect = true
[churn.account]
update = 0.5"
            )
            .is_ok()
        );
    }

    #[test]
    fn platforms_other_than_one_not_implemented() {
        let msg = Config::from_toml_str(
            "[accounts]
platforms = 2",
        )
        .unwrap_err()
        .to_string();
        assert!(
            msg.contains("`accounts.platforms` other than 1 is not implemented yet"),
            "{msg}"
        );
    }

    #[test]
    fn spec_mentions_stripe_context() {
        let spec = include_str!("../../../docs/stripe-mock-spec.md");
        assert!(spec.contains("Stripe-Context"));
    }

    #[test]
    fn spec_example_accounts_section_parses() {
        let spec = include_str!("../../../docs/stripe-mock-spec.md");
        let toml = spec
            .split(
                "```toml
",
            )
            .nth(1)
            .unwrap()
            .split("```")
            .next()
            .unwrap();
        let start = toml.find("[accounts]").unwrap();
        let block: String = toml[start..]
            .lines()
            .take_while(|l| !l.starts_with("[history]"))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        let c = Config::from_toml_str(&format!(
            "{block}
[history]
"
        ))
        .unwrap();
        assert!(c.accounts.connect);
        assert_eq!(c.accounts.connected_accounts.initial, 20);
        assert_eq!(c.accounts.connected_accounts.connect_rate, 0.5);
        assert_eq!(c.accounts.connected_accounts.deauthorize_rate, 0.1);
        assert_eq!(c.accounts.account_types.express, 0.3);
    }
}
