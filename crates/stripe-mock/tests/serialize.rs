mod common;

use std::time::Duration;

use common::mock_with;
use ductkit_stripe_mock::{
    AccountApi, AccountInfo, AccountKind, Action, Config, Configuration, CustomerUpdate,
    DestinationConfig, DurationStr, InvoiceOutcome, MockError, NewCustomer, ObjectType,
    PayloadFormat, SubscribeFilter,
};
use serde_json::json;

const FULL: &str = r#"
seed = 7
[clock]
mode = "wall"
start = "2026-03-01T12:30:00Z"
speed = 60.0
[history]
duration = "90d"
scale = 0.5
[api]
keys = ["sk_test_a", "sk_test_b"]
latency = { distribution = "lognormal", median_ms = 20.0, p99_ms = 400.0 }
rate_limit = { requests_per_second = 25.0, burst = 30 }
errors = { "500" = 0.1, "503" = 0.05, timeout = 0.02, reset = 0.01 }
stale_read = { probability = 0.2, max_lag = "1500ms" }
pagination_writes = { probability = 0.3 }
[actions]
new_customer = 12.0
start_subscription = 4.0
cancel_subscription = 1.5
catalog_change = 0.5
one_off_invoice = 2.0
renew_subscription_cycle = "off"
step_delay = { distribution = "fixed", ms = 250 }
payment_failure_probability = 0.25
[churn.customer]
update = 3.0
delete = 0.2
[churn.product]
update = 0.5
archive = 0.1
[churn.price]
update = 0.4
archive = 0.2
[churn.subscription]
update = 1.0
[churn.invoice]
update = 0.7
[churn.account]
update = 0.6
[events]
visibility_lag = { distribution = "lognormal", median_ms = 200.0, p99_ms = 5000.0 }
late_tail = { probability = 0.001, min = "30s", max = "10m" }
same_second_shuffle = true
[event_retention]
window = "2d"
[webhooks]
delivery_delay = { distribution = "fixed", ms = 40 }
duplicate_probability = 0.1
reorder_window = "5s"
response_timeout = "3s"
retry_schedule = ["1s", "2s", "1m"]
max_retry_duration = "12h"
disable_after_failures = "1h"
signature_tolerance = "5m"
clock_skew = { probability = 0.1, max = "10m" }
invalid_signature_probability = 0.05
[payloads]
nested_list_page_size = 3
[accounts]
connect = true
connected_accounts = { initial = 4, connect_rate = 0.5, deauthorize_rate = 0.1 }
account_api = { v1 = 0.25, v2 = 0.75 }
account_types = { standard = 0.2, express = 0.3, custom = 0.5 }
[[destinations]]
name = "snapshot"
url = "http://127.0.0.1:4000/a"
payload = "snapshot"
enabled_events = ["*"]
connect = true
secret = "whsec_one"
[[destinations]]
name = "thin"
url = "http://127.0.0.1:4000/b"
payload = "thin"
enabled_events = ["v1.customer.*", "v1.invoice.*"]
events_from = ["@self", "@accounts"]
"#;

fn assert_round_trips(c: &Config) {
    let toml = toml::to_string(c).expect("serializes to TOML");
    assert_eq!(&Config::from_toml_str(&toml).expect("TOML parses"), c);
    let json = serde_json::to_value(c).expect("serializes to JSON");
    assert_eq!(&serde_json::from_value::<Config>(json).unwrap(), c);
}

#[test]
fn config_round_trips() {
    assert_round_trips(&Config::default());
    let c = Config::from_toml_str(FULL).unwrap();
    assert_eq!(c.destinations.len(), 2);
    assert_round_trips(&c);
    for (connect, v1, v2) in [(true, 1.0, 0.0), (true, 0.0, 1.0), (false, 0.5, 0.5)] {
        let mut c = Config::default();
        c.accounts.connect = connect;
        c.accounts.account_api.v1 = v1;
        c.accounts.account_api.v2 = v2;
        assert_round_trips(&c);
    }
    // Absent options stay absent.
    let toml = toml::to_string(&Config::default()).unwrap();
    assert!(!toml.contains("rate_limit"), "{toml}");
}

#[test]
fn duration_serializes_compactly() {
    for (ms, text) in [
        (90_000, "90s"),
        (3_600_000, "1h"),
        (1_500, "1500ms"),
        (172_800_000, "2d"),
    ] {
        assert_eq!(serde_json::to_value(DurationStr(ms)).unwrap(), json!(text));
        let back: DurationStr = serde_json::from_value(json!(text)).unwrap();
        assert_eq!(back, DurationStr(ms));
    }
}

#[test]
fn actions_round_trip() {
    let actions = vec![
        Action::NewCustomer,
        Action::CatalogChange,
        Action::Archive {
            id: "prod_1".into(),
        },
        Action::OneOffInvoice {
            customer: None,
            outcome: None,
        },
        Action::OneOffInvoice {
            customer: Some("cus_1".into()),
            outcome: Some(InvoiceOutcome::Uncollectible),
        },
        Action::StartSubscription {
            customer: None,
            prices: None,
        },
        Action::StartSubscription {
            customer: Some("cus_1".into()),
            prices: Some(vec!["price_1".into(), "price_2".into()]),
        },
        Action::CancelSubscription {
            id: None,
            at_period_end: None,
        },
        Action::CancelSubscription {
            id: Some("sub_1".into()),
            at_period_end: Some(true),
        },
    ];
    for a in actions {
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(serde_json::from_value::<Action>(v).unwrap(), a);
    }
    assert_eq!(
        serde_json::to_value(Action::NewCustomer).unwrap(),
        json!({"type": "new_customer"})
    );
    for o in [
        InvoiceOutcome::DeleteDraft,
        InvoiceOutcome::Paid,
        InvoiceOutcome::Void,
        InvoiceOutcome::Uncollectible,
    ] {
        let v = serde_json::to_value(o).unwrap();
        assert_eq!(serde_json::from_value::<InvoiceOutcome>(v).unwrap(), o);
    }
    let updates = vec![
        CustomerUpdate::SetEmail(Some("a@example.com".into())),
        CustomerUpdate::SetEmail(None),
        CustomerUpdate::SetName(Some("Zoe".into())),
        CustomerUpdate::SetName(None),
        CustomerUpdate::SetDescription(Some("d".into())),
        CustomerUpdate::SetDescription(None),
        CustomerUpdate::SetMetadata {
            key: "k".into(),
            value: "v".into(),
        },
        CustomerUpdate::RemoveMetadata { key: "k".into() },
    ];
    for u in updates {
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(serde_json::from_value::<CustomerUpdate>(v).unwrap(), u);
    }
    let mut new = NewCustomer::default();
    let v = serde_json::to_value(&new).unwrap();
    assert_eq!(serde_json::from_value::<NewCustomer>(v).unwrap(), new);
    new.email = Some("a@example.com".into());
    new.metadata.insert("k".into(), "v".into());
    let v = serde_json::to_value(&new).unwrap();
    assert_eq!(serde_json::from_value::<NewCustomer>(v).unwrap(), new);
}

#[test]
fn account_types_round_trip() {
    assert_eq!(serde_json::to_value(AccountApi::V1).unwrap(), json!("v1"));
    assert_eq!(serde_json::to_value(AccountApi::V2).unwrap(), json!("v2"));
    let info = AccountInfo {
        id: "acct_1".into(),
        api: AccountApi::V2,
        kind: AccountKind::V2,
        configurations: vec![Configuration::Merchant, Configuration::Recipient],
        connected: true,
    };
    let v = serde_json::to_value(&info).unwrap();
    assert_eq!(serde_json::from_value::<AccountInfo>(v).unwrap(), info);
}

#[tokio::test]
async fn dump_state_lists_live_objects() {
    let m = mock_with("[accounts]\nconnect = true").await;
    let keep = m.create_customer(NewCustomer::default()).unwrap();
    let gone = m.create_customer(NewCustomer::default()).unwrap();
    m.delete_customer(&gone).unwrap();
    let acct = m.connect_account(AccountApi::V1).unwrap();
    m.account(&acct)
        .create_customer(NewCustomer::default())
        .unwrap();

    let dump = m.dump_state(&SubscribeFilter::default());
    let scopes = dump["scopes"].as_array().unwrap();
    assert_eq!(scopes.len(), 2);
    assert_eq!(scopes[0]["account"], json!(null));
    assert_eq!(scopes[1]["account"], json!(acct));
    let platform = &scopes[0]["objects"];
    assert_eq!(platform["customer"].as_array().unwrap().len(), 1);
    assert_eq!(platform["customer"][0]["id"], json!(keep));
    assert_eq!(platform["account"].as_array().unwrap().len(), 1);
    assert_eq!(platform.as_object().unwrap().len(), 2);
    assert_eq!(
        scopes[1]["objects"]["customer"].as_array().unwrap().len(),
        1
    );
    assert_eq!(dump["at"], json!(m.now()));

    let only_platform = m.dump_state(&SubscribeFilter {
        accounts: Some(vec![None]),
        ..Default::default()
    });
    assert_eq!(only_platform["scopes"].as_array().unwrap().len(), 1);

    let customers = m.dump_state(&SubscribeFilter {
        object_types: Some(vec![ObjectType::Customer]),
        ..Default::default()
    });
    assert_eq!(
        customers["scopes"][0]["objects"].as_object().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn effective_config_reflects_updates() {
    let m = mock_with(
        "[[destinations]]\nname = \"snap\"\nurl = \"http://127.0.0.1:9/a\"\npayload = \"snapshot\"\nenabled_events = [\"*\"]",
    )
    .await;
    m.update_api_config(|c| c.stale_read.probability = 0.5)
        .unwrap();
    m.update_webhooks_config(|c| c.duplicate_probability = 0.25)
        .unwrap();
    m.update_events_config(|c| c.same_second_shuffle = true)
        .unwrap();
    m.update_actions_config(|c| c.new_customer = 3.0).unwrap();
    let info = m
        .add_destination(DestinationConfig {
            name: "thin".into(),
            url: "http://127.0.0.1:9/b".into(),
            payload: PayloadFormat::Thin,
            enabled_events: vec!["v1.customer.*".into()],
            connect: false,
            events_from: None,
            secret: None,
        })
        .unwrap();
    let c = m.effective_config();
    assert_eq!(c.api.stale_read.probability, 0.5);
    assert_eq!(c.webhooks.duplicate_probability, 0.25);
    assert!(c.events.same_second_shuffle);
    assert_eq!(c.actions.new_customer, 3.0);
    assert_eq!(c.destinations.len(), 2);
    assert_eq!(c.destinations[0].name, "snap");
    assert_eq!(
        c.destinations[0].secret.as_deref(),
        Some(m.destinations()[0].secret.as_str())
    );
    assert_eq!(
        c.destinations[1].events_from,
        Some(vec!["@self".to_string()])
    );
    assert_eq!(
        c.destinations[1].secret.as_deref(),
        Some(info.secret.as_str())
    );
    c.validate().unwrap();
    let toml = toml::to_string(&c).unwrap();
    assert_eq!(Config::from_toml_str(&toml).unwrap(), c);
}

#[tokio::test]
async fn set_speed_needs_wall() {
    let m = mock_with("").await;
    assert!(matches!(m.set_speed(2.0), Err(MockError::ClockMode(_))));
}

#[tokio::test]
async fn set_speed_changes_rate() {
    let m = mock_with("[clock]\nmode = \"wall\"\nspeed = 1.0").await;
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(
            matches!(m.set_speed(bad), Err(MockError::Config(_))),
            "{bad}"
        );
    }
    let before = m.now();
    m.set_speed(3600.0).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let minutes = (m.now().0 - before.0) as f64 / 60_000.0;
    assert!(
        (9.0..=15.0).contains(&minutes),
        "advanced {minutes} minutes"
    );
}
