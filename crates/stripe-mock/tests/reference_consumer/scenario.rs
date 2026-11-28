//! The faulty run both consumers face, and the driver that steps it.

use std::time::Duration;

use ductkit_stripe_mock::{CustomerUpdate, EventFormat, MockStripe, ObjectType, SubscribeFilter};

use crate::hooks::{SECRET, fails_first};

/// The run every scenario shares: accounts, history and generation rates. Fault
/// sections are added per test, because TOML forbids a table twice.
///
/// No assertion depends on the seed: the resurrection is scripted (`update_then_delete`),
/// and seeds 7 and 11 both pass. Seed 7 gives the naive consumer 128 stale, 22 missing
/// and 3 resurrected entries.
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

/// [`FAULTS`] with stale reads off and every other fault on. With nothing stale to
/// read, refetching each thin event's object must converge on its own, which pins the
/// consumer's event handling (scope header, deleted stubs) without a reconcile.
pub fn faults_without_stale_reads() -> String {
    let on = "stale_read = { probability = 0.2,";
    assert!(FAULTS.contains(on));
    FAULTS.replace(on, "stale_read = { probability = 0.0,")
}

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

/// Updates `n` live platform customers until each one's latest `customer.updated` has
/// a failing first attempt, then deletes them a minute later. That failed update is
/// retried after the delete, which is what resurrects the customer in the naive
/// consumer. Random churn almost never puts an update and a delete this close, and
/// choosing the events this way makes the pair independent of the seed.
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
        let chosen = (0..30).any(|i| {
            m.update_customer(
                id,
                CustomerUpdate::SetName(Some(format!("Before delete {i}"))),
            )
            .unwrap();
            fails_first(&last_snapshot_event(m, id, "customer.updated"))
        });
        assert!(chosen, "no failing update event for {id} in 30 tries");
    }
    m.step(Duration::from_secs(60)).await.unwrap();
    for id in &ids {
        m.delete_customer(id).unwrap();
    }
    // A delete whose own first attempt fails is retried after the update's retry and
    // removes the customer again, so at least one delete must go through first time.
    assert!(
        ids.iter()
            .any(|id| !fails_first(&last_snapshot_event(m, id, "customer.deleted"))),
        "every scripted delete fails its first attempt"
    );
}

/// The ID of the newest snapshot event of `event_type` about `object_id`.
pub fn last_snapshot_event(m: &MockStripe, object_id: &str, event_type: &str) -> String {
    m.event_timeline()
        .into_iter()
        .rev()
        .find(|e| {
            e.format == EventFormat::Snapshot
                && e.object_id == object_id
                && e.event_type == event_type
        })
        .unwrap_or_else(|| panic!("no {event_type} for {object_id}"))
        .id
}
