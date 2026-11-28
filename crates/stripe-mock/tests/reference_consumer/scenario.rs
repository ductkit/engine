//! The faulty run both consumers face, and the driver that steps it.

use std::time::Duration;

use ductkit_stripe_mock::{CustomerUpdate, MockStripe, ObjectType, SubscribeFilter};

use crate::hooks::SECRET;

/// The run every scenario shares: accounts, history and generation rates. Fault
/// sections are added per test, because TOML forbids a table twice.
///
/// Seed 7 gives the naive consumer stale, missing and resurrected entries (131, 22 and
/// 3 when chosen) while the correct one converges; a new seed must keep all of that.
pub const SCENARIO_BASE: &str = r#"
seed = 7
[accounts]
connect = true
connected_accounts = { initial = 2 }
[history]
duration = "1d"
[actions]
new_customer = 6.0
start_subscription = 3.0
cancel_subscription = 0.5
catalog_change = 0.5
[churn.customer]
update = 3.0
delete = 0.5
[churn.product]
update = 0.5
[churn.price]
update = 0.5
[churn.subscription]
update = 0.5
[churn.invoice]
update = 0.5
"#;

/// `api.stale_read.max_lag` in [`FAULTS`].
pub const STALE_MAX_LAG: Duration = Duration::from_secs(600);

/// Every fault the design turns on. `timeout` stays off: it holds a response for 80 s
/// of real time.
pub const FAULTS: &str = r#"
[events]
visibility_lag = { distribution = "lognormal", median_ms = 200, p99_ms = 5000 }
late_tail = { probability = 0.05, min = "30s", max = "10m" }
same_second_shuffle = true
[webhooks]
duplicate_probability = 0.1
reorder_window = "5s"
retry_schedule = ["30m"]
[api]
errors = { "500" = 0.05, "503" = 0.02, reset = 0.02 }
stale_read = { probability = 0.2, max_lag = "10m" }
pagination_writes = { probability = 0.2 }
"#;

/// A snapshot destination (Connect events included) and a thin one (`@self` and
/// `@accounts`), both signed with [`SECRET`].
pub fn destinations(snapshot_url: &str, thin_url: &str) -> String {
    format!(
        "[[destinations]]\nname = \"snapshot\"\nurl = \"{snapshot_url}\"\npayload = \"snapshot\"\n\
         enabled_events = [\"*\"]\nconnect = true\nsecret = \"{SECRET}\"\n\
         [[destinations]]\nname = \"thin\"\nurl = \"{thin_url}\"\npayload = \"thin\"\n\
         enabled_events = [\"*\"]\nevents_from = [\"@self\", \"@accounts\"]\nsecret = \"{SECRET}\"\n"
    )
}

/// Six simulated hours in 10-minute steps, each followed by `each_tick`. Then
/// quiesce, and step past the stale-read lag (generation is off, so nothing changes
/// and reads turn fresh), and tick once more for what quiesce flushed.
pub async fn run(m: &MockStripe, mut each_tick: impl AsyncFnMut(&MockStripe)) {
    for tick in 0..36 {
        m.step(Duration::from_secs(600)).await.unwrap();
        if tick == 18 {
            update_then_delete(m, 6).await;
        }
        each_tick(m).await;
    }
    m.quiesce().await.unwrap();
    m.step(STALE_MAX_LAG + Duration::from_secs(60))
        .await
        .unwrap();
    each_tick(m).await;
}

/// Updates `n` live platform customers, then deletes each a minute later. Generated
/// churn rarely updates and deletes one customer close together, so this makes sure
/// some update's failed first attempt is retried after its customer's delete.
async fn update_then_delete(m: &MockStripe, n: usize) {
    let filter = SubscribeFilter {
        object_types: Some(vec![ObjectType::Customer]),
        accounts: Some(vec![None]),
    };
    let ids: Vec<String> = m.dump_state(&filter)["scopes"][0]["objects"]["customer"]
        .as_array()
        .unwrap()
        .iter()
        .take(n)
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), n, "not enough live customers");
    for id in &ids {
        m.update_customer(id, CustomerUpdate::SetName(Some("Before delete".into())))
            .unwrap();
    }
    m.step(Duration::from_secs(60)).await.unwrap();
    for id in &ids {
        m.delete_customer(id).unwrap();
    }
}
