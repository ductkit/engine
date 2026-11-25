mod common;

use std::time::Duration;

use common::{Receiver, dest, get, mock_with};
use ductkit_stripe_mock::{
    AccountApi, ChangeKind, EventFormat, EventInfo, MockError, MockStripe, ObjectType, SimTime,
};
use serde_json::Value;

const CONNECT: &str = "[accounts]\nconnect = true\n";

fn events_of(m: &MockStripe, id: &str) -> Vec<EventInfo> {
    m.event_timeline()
        .into_iter()
        .filter(|e| e.object_id == id)
        .collect()
}

fn types(events: &[EventInfo], format: EventFormat) -> Vec<&str> {
    events
        .iter()
        .filter(|e| e.format == format)
        .map(|e| e.event_type.as_str())
        .collect()
}

async fn payload(m: &MockStripe, path: &str) -> Value {
    let r = get(m, path).await;
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

#[tokio::test]
async fn connect_v1_emits_authorized() {
    let m = mock_with(CONNECT).await;
    let id = m.connect_account(AccountApi::V1).unwrap();
    assert!(id.starts_with("acct_"), "{id}");
    let evs = events_of(&m, &id);
    assert_eq!(
        types(&evs, EventFormat::Snapshot),
        ["account.application.authorized"]
    );
    assert_eq!(
        types(&evs, EventFormat::Thin),
        ["v1.account.application.authorized"]
    );
    assert!(evs.iter().all(|e| e.account.as_deref() == Some(&*id)));
    let snap = evs
        .iter()
        .find(|e| e.format == EventFormat::Snapshot)
        .unwrap();
    let body = payload(&m, &format!("/v1/events/{}", snap.id)).await;
    assert_eq!(body["data"]["object"]["object"], "application");
    assert_eq!(body["account"], id.as_str());
    let thin = evs.iter().find(|e| e.format == EventFormat::Thin).unwrap();
    let body = payload(&m, &format!("/v2/core/events/{}", thin.id)).await;
    assert_eq!(body["related_object"]["id"], id.as_str());
    assert_eq!(body["related_object"]["type"], "account");
    assert_eq!(body["context"], id.as_str());
    let created: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::Account)
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].kind, ChangeKind::Created);
    assert_eq!(created[0].account, None);
}

#[tokio::test]
async fn connect_v2_also_emits_thin_created() {
    let m = mock_with(CONNECT).await;
    let id = m.connect_account(AccountApi::V2).unwrap();
    let evs = events_of(&m, &id);
    assert_eq!(
        types(&evs, EventFormat::Snapshot),
        ["account.application.authorized"]
    );
    assert_eq!(
        types(&evs, EventFormat::Thin),
        [
            "v1.account.application.authorized",
            "v2.core.account.created"
        ]
    );
    let created = evs
        .iter()
        .find(|e| e.event_type == "v2.core.account.created")
        .unwrap();
    assert_eq!(created.account, None);
    assert_eq!(created.pair_id, "");
}

#[tokio::test]
async fn disconnect_emits_deauthorized_and_closed() {
    let m = mock_with(CONNECT).await;
    let v1 = m.connect_account(AccountApi::V1).unwrap();
    let v2 = m.connect_account(AccountApi::V2).unwrap();
    m.disconnect_account(&v1).unwrap();
    m.disconnect_account(&v2).unwrap();
    let e1 = events_of(&m, &v1);
    assert_eq!(
        types(&e1, EventFormat::Snapshot),
        [
            "account.application.authorized",
            "account.application.deauthorized"
        ]
    );
    assert!(!types(&e1, EventFormat::Thin).contains(&"v2.core.account.closed"));
    let e2 = events_of(&m, &v2);
    assert_eq!(
        types(&e2, EventFormat::Snapshot),
        [
            "account.application.authorized",
            "account.application.deauthorized"
        ]
    );
    let closed = e2
        .iter()
        .find(|e| e.event_type == "v2.core.account.closed")
        .expect("closed event");
    assert_eq!(closed.account, None);
    let deauth = e2
        .iter()
        .find(|e| e.event_type == "account.application.deauthorized")
        .unwrap();
    assert_eq!(deauth.account.as_deref(), Some(&*v2));
    let body = payload(&m, &format!("/v1/events/{}", deauth.id)).await;
    assert_eq!(body["data"]["object"]["object"], "application");
    for a in m.accounts() {
        assert!(!a.connected, "{a:?}");
    }
    let last = m.get(ObjectType::Account, &v1).unwrap();
    assert_eq!(last.kind, ChangeKind::Deleted);
    assert_eq!(last.state["deleted"], true);
    assert!(matches!(
        m.disconnect_account(&v1),
        Err(MockError::UnknownAccount(id)) if id == v1
    ));
    assert!(matches!(
        m.disconnect_account("acct_nope"),
        Err(MockError::UnknownAccount(_))
    ));
}

#[tokio::test]
async fn connect_disabled_by_default() {
    let m = mock_with("").await;
    assert!(matches!(
        m.connect_account(AccountApi::V1),
        Err(MockError::ConnectDisabled)
    ));
    assert!(matches!(
        m.disconnect_account("acct_x"),
        Err(MockError::ConnectDisabled)
    ));
    assert!(m.accounts().is_empty());
}

#[tokio::test]
async fn initial_accounts_at_history_start() {
    let rx = Receiver::start(|_| (200, Duration::ZERO));
    let m = mock_with(&format!(
        "{CONNECT}connected_accounts = {{ initial = 3 }}\n[history]\nduration = \"1h\"\n{}",
        dest(&rx.url("/h"), "snapshot", "*")
    ))
    .await;
    let accounts = m.accounts();
    assert_eq!(accounts.len(), 3);
    assert!(accounts.iter().all(|a| a.connected));
    let start = m.now().unix_seconds() - 3600;
    for a in &accounts {
        let u = m.get(ObjectType::Account, &a.id).unwrap();
        assert_eq!(u.state["created"], start);
        assert_eq!(u.at, SimTime(start * 1000));
    }
    m.quiesce().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rx.requests().is_empty(), "history was delivered");
}

#[tokio::test]
async fn account_rates_follow_config() {
    let m = mock_with(
        "[accounts]\nconnect = true\nconnected_accounts = { connect_rate = 10.0, deauthorize_rate = 2.0 }\n[churn.account]\nupdate = 5.0\n",
    )
    .await;
    m.step(Duration::from_secs(10 * 3600)).await.unwrap();
    let log = m.truth_log();
    let count = |kind: ChangeKind| {
        log.iter()
            .filter(|u| u.object_type == ObjectType::Account && u.kind == kind)
            .count()
    };
    let connects = count(ChangeKind::Created);
    assert!((60..=140).contains(&connects), "{connects}");
    assert!(count(ChangeKind::Deleted) >= 1);
    let updated = m
        .event_timeline()
        .iter()
        .filter(|e| e.event_type == "account.updated")
        .count();
    assert!(updated >= 20, "{updated}");
    assert_eq!(
        m.accounts().iter().filter(|a| a.connected).count(),
        connects - count(ChangeKind::Deleted)
    );
}

#[tokio::test]
async fn same_seed_same_accounts() {
    let cfg = "seed = 9\n[accounts]\nconnect = true\nconnected_accounts = { initial = 2, connect_rate = 3.0, deauthorize_rate = 1.0 }\n[churn.account]\nupdate = 2.0\n";
    let run = || async {
        let m = mock_with(cfg).await;
        m.step(Duration::from_secs(5 * 3600)).await.unwrap();
        (m.accounts(), m.truth_log())
    };
    let (a1, t1) = run().await;
    let (a2, t2) = run().await;
    assert!(a1.len() > 2);
    assert_eq!(a1, a2);
    assert_eq!(t1, t2);
}

#[tokio::test]
async fn v2_accounts_have_configurations() {
    let m = mock_with(CONNECT).await;
    for _ in 0..20 {
        m.connect_account(AccountApi::V2).unwrap();
    }
    for a in m.accounts() {
        assert!(!a.configurations.is_empty(), "{a:?}");
    }
}
