//! External accounts and persons of controlled connected accounts: setup at connect,
//! churn and the control actions (spec sections 5.4 and 11; phase 8 rulings P4, P8, P9).

mod common;

use std::collections::BTreeSet;

use common::{get, mock_with};
use ductkit_stripe_mock::{
    AccountApi, Action, ChangeKind, EventFormat, ExternalAccountKind, MockError, MockStripe,
    ObjectType,
};
use serde_json::Value;

const CUSTOM: &str =
    "[accounts]\nconnect = true\naccount_types = { standard = 0.0, express = 0.0, custom = 1.0 }\n";
const STANDARD: &str =
    "[accounts]\nconnect = true\naccount_types = { standard = 1.0, express = 0.0, custom = 0.0 }\n";

async fn json(m: &MockStripe, path: &str) -> Value {
    let r = get(m, path).await;
    assert_eq!(r.status(), 200, "{path}");
    r.json().await.unwrap()
}

fn ids(list: &Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

/// The snapshot event types labelled with `acct`, in order.
fn snapshot_types(m: &MockStripe, acct: &str) -> Vec<String> {
    m.event_timeline()
        .into_iter()
        .filter(|e| e.format == EventFormat::Snapshot && e.account.as_deref() == Some(acct))
        .map(|e| e.event_type)
        .collect()
}

fn invalid_action(r: Result<impl std::fmt::Debug, MockError>) -> String {
    match r {
        Err(MockError::InvalidAction(m)) => m,
        other => panic!("expected invalid_action, got {other:?}"),
    }
}

#[tokio::test]
async fn controlled_account_starts_with_bank_account_and_persons() {
    let m = mock_with(CUSTOM).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let banks = json(&m, &format!("/v1/accounts/{a}/external_accounts")).await;
    let bank = &banks["data"][0];
    assert_eq!(banks["data"].as_array().unwrap().len(), 1);
    assert_eq!(bank["object"], "bank_account");
    assert_eq!(bank["default_for_currency"], true);
    assert_eq!(bank["account"], a.as_str());
    assert_eq!(bank["account_holder_type"], "company");
    assert_eq!(bank["routing_number"], "110000000");
    assert_eq!(
        (bank["country"].as_str(), bank["currency"].as_str()),
        (Some("US"), Some("usd"))
    );
    assert_eq!(bank["status"], "new");
    assert!(
        [
            "STRIPE TEST BANK",
            "JPMORGAN CHASE",
            "WELLS FARGO",
            "BANK OF AMERICA"
        ]
        .contains(&bank["bank_name"].as_str().unwrap())
    );
    assert_eq!(bank["last4"].as_str().unwrap().len(), 4);
    assert_eq!(bank["fingerprint"].as_str().unwrap().len(), 16);
    let persons = json(&m, &format!("/v1/accounts/{a}/persons")).await;
    let people = persons["data"].as_array().unwrap();
    assert!((1..=3).contains(&people.len()), "{}", people.len());
    for (i, p) in people.iter().enumerate() {
        let want = format!(
            "{}.{}@example.com",
            p["first_name"].as_str().unwrap().to_lowercase(),
            p["last_name"].as_str().unwrap().to_lowercase()
        );
        assert_eq!(p["email"], want.as_str());
        assert_eq!(p["account"], a.as_str());
        assert_eq!(p["relationship"]["representative"], i == 0);
        assert_eq!(p["relationship"]["executive"], i == 0);
    }
    // The account's embedded list holds the bank account.
    let account = json(&m, &format!("/v1/accounts/{a}")).await;
    assert_eq!(account["external_accounts"]["data"][0], *bank);
    assert_eq!(account["external_accounts"]["has_more"], false);
    // The events are labelled with the account, in order.
    let mut want = vec![
        "account.application.authorized".to_string(),
        "account.external_account.created".to_string(),
    ];
    want.extend(people.iter().map(|_| "person.created".to_string()));
    assert_eq!(snapshot_types(&m, &a), want);
    // The setup is one action; the embedded list is a silent account mutation.
    let setup: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| {
            u.cause
                .as_ref()
                .is_some_and(|c| c.action == "account_setup")
        })
        .collect();
    assert_eq!(setup.len(), 1 + people.len());
    assert!(setup.iter().all(|u| u.account.as_deref() == Some(&*a)));
    let silent: Vec<_> = m
        .truth_log()
        .into_iter()
        .filter(|u| u.object_type == ObjectType::Account && u.kind == ChangeKind::Updated)
        .collect();
    assert_eq!(silent.len(), 1);
    assert!(silent[0].events.snapshot.is_empty());
}

#[tokio::test]
async fn standard_account_has_none() {
    let m = mock_with(STANDARD).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    for sub in ["external_accounts", "persons"] {
        let list = json(&m, &format!("/v1/accounts/{a}/{sub}")).await;
        assert_eq!(list["data"], serde_json::json!([]), "{sub}");
    }
    assert_eq!(snapshot_types(&m, &a), ["account.application.authorized"]);
    let account = json(&m, &format!("/v1/accounts/{a}")).await;
    assert!(account.get("external_accounts").is_none());
}

fn scripted_account(m: &MockStripe, a: &str) {
    // Script 8 creates with mixed kinds.
    for i in 0..8 {
        let kind = if i % 3 == 0 {
            ExternalAccountKind::Card
        } else {
            ExternalAccountKind::BankAccount
        };
        m.account(a)
            .run_action(Action::CreateExternalAccount { kind: Some(kind) })
            .unwrap();
    }
}

#[tokio::test]
async fn external_account_list_is_polymorphic() {
    let m = mock_with(CUSTOM).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    scripted_account(&m, &a);
    for _ in 0..2 {
        m.account(&a)
            .run_action(Action::DeleteExternalAccount { id: None })
            .unwrap();
    }
    // What is live, from the truth log.
    let mut live = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    let mut cards = BTreeSet::new();
    for u in m.truth_log() {
        if !matches!(u.object_type, ObjectType::BankAccount | ObjectType::Card) {
            continue;
        }
        assert_eq!(u.account.as_deref(), Some(&*a));
        if u.object_type == ObjectType::Card {
            cards.insert(u.object_id.clone());
        }
        match u.kind {
            ChangeKind::Created => {
                live.insert(u.object_id.clone());
            }
            ChangeKind::Deleted => {
                live.remove(&u.object_id);
                deleted.insert(u.object_id.clone());
                // The tombstone.
                let want = if u.object_type == ObjectType::Card {
                    "card"
                } else {
                    "bank_account"
                };
                assert_eq!(u.state["object"], want);
                assert_eq!(u.state["deleted"], true);
                assert!(u.events.snapshot.len() == 1);
            }
            _ => {}
        }
    }
    assert_eq!(live.len(), 9 - 2);
    assert_eq!(deleted.len(), 2);
    assert!(!cards.is_empty() && cards.len() < 9);
    // Paginate by cursor, limit 3.
    let url = format!("/v1/accounts/{a}/external_accounts");
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let path = match &after {
            None => format!("{url}?limit=3"),
            Some(c) => format!("{url}?limit=3&starting_after={c}"),
        };
        let page = json(&m, &path).await;
        let page_ids = ids(&page);
        assert!(page_ids.len() <= 3);
        seen.extend(page_ids.clone());
        if page["has_more"] != true {
            break;
        }
        after = page_ids.last().cloned();
    }
    assert_eq!(seen.len(), live.len(), "{seen:?}");
    assert_eq!(seen.iter().cloned().collect::<BTreeSet<_>>(), live);
    assert!(seen.iter().all(|id| !deleted.contains(id)));
    // Both kinds are in the one list, in creation order.
    let all = json(&m, &format!("{url}?limit=100")).await;
    let kinds: BTreeSet<_> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["object"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds.len(), 2);
    let only_cards = json(&m, &format!("{url}?object=card&limit=100")).await;
    let card_ids = ids(&only_cards);
    assert!(!card_ids.is_empty());
    assert!(
        card_ids
            .iter()
            .all(|id| cards.contains(id) && live.contains(id))
    );
    assert_eq!(
        card_ids.len(),
        live.iter().filter(|id| cards.contains(*id)).count()
    );
    for o in only_cards["data"].as_array().unwrap() {
        assert_eq!(o["object"], "card");
        assert_eq!(o["funding"], "debit");
        assert_eq!(o["default_for_currency"], false);
    }
    // The default is never deleted: delete until nothing else is deletable.
    let default = ids(&all)
        .into_iter()
        .find(|id| {
            all["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["id"] == id.as_str() && o["default_for_currency"] == true)
        })
        .expect("a default bank account");
    loop {
        match m
            .account(&a)
            .run_action(Action::DeleteExternalAccount { id: None })
        {
            Ok(_) => {}
            Err(MockError::InvalidAction(msg)) => {
                assert!(msg.contains(&a), "{msg}");
                break;
            }
            Err(e) => panic!("{e}"),
        }
    }
    let left = ids(&json(&m, &url).await);
    assert!(left.contains(&default));
    assert!(
        left.iter().all(|id| {
            // Only defaults are left.
            let o = m
                .truth_log()
                .into_iter()
                .rfind(|u| u.object_id == *id)
                .unwrap();
            o.state["default_for_currency"] == true
        }),
        "{left:?}"
    );
    // A named default is a 409 naming it, and so is an unknown ID.
    let msg = invalid_action(m.account(&a).run_action(Action::DeleteExternalAccount {
        id: Some(default.clone()),
    }));
    assert!(msg.contains(&default), "{msg}");
    let msg = invalid_action(m.account(&a).run_action(Action::DeleteExternalAccount {
        id: Some("ba_nope".to_string()),
    }));
    assert!(msg.contains("ba_nope"), "{msg}");
    // The embedded list follows (P9).
    let account = json(&m, &format!("/v1/accounts/{a}")).await;
    assert_eq!(ids(&account["external_accounts"]), left);
}

#[tokio::test]
async fn embedded_list_orders_the_default_first_and_truncates() {
    let m = mock_with(&format!("{CUSTOM}[payloads]\nnested_list_page_size = 3\n")).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    for _ in 0..4 {
        m.account(&a)
            .run_action(Action::CreateExternalAccount {
                kind: Some(ExternalAccountKind::Card),
            })
            .unwrap();
    }
    let account = json(&m, &format!("/v1/accounts/{a}")).await;
    let embedded = &account["external_accounts"];
    assert_eq!(embedded["has_more"], true);
    assert_eq!(embedded["data"].as_array().unwrap().len(), 3);
    // The default bank account first, then the cards by creation.
    assert_eq!(embedded["data"][0]["object"], "bank_account");
    let listed = ids(&json(&m, &format!("/v1/accounts/{a}/external_accounts")).await);
    assert_eq!(ids(embedded), listed[..3]);
}

#[tokio::test]
async fn new_default_bank_account_updates_old_default() {
    let m = mock_with(CUSTOM).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let mut found = None;
    for _ in 0..60 {
        let cause = m
            .account(&a)
            .run_action(Action::CreateExternalAccount {
                kind: Some(ExternalAccountKind::BankAccount),
            })
            .unwrap();
        let entries: Vec<_> = m
            .truth_log()
            .into_iter()
            .filter(|u| {
                u.cause
                    .as_ref()
                    .is_some_and(|c| c.action_id == cause.action_id)
            })
            .filter(|u| u.object_type == ObjectType::BankAccount)
            .collect();
        if entries.len() == 2 {
            found = Some(entries);
            break;
        }
    }
    let entries = found.expect("a new default within 60 creates");
    // The old default is demoted first, then the new one is created.
    assert_eq!(entries[0].kind, ChangeKind::Updated);
    assert_eq!(entries[0].state["default_for_currency"], false);
    assert_eq!(entries[1].kind, ChangeKind::Created);
    assert_eq!(entries[1].state["default_for_currency"], true);
    assert_eq!(
        entries[0].events.snapshot.len() + entries[1].events.snapshot.len(),
        2
    );
    let types: Vec<_> = m
        .event_timeline()
        .into_iter()
        .filter(|e| e.format == EventFormat::Snapshot)
        .filter(|e| e.truth_seq == entries[0].seq || e.truth_seq == entries[1].seq)
        .map(|e| e.event_type)
        .collect();
    assert_eq!(
        types,
        [
            "account.external_account.updated",
            "account.external_account.created"
        ]
    );
    // Exactly one default is live.
    let list = json(&m, &format!("/v1/accounts/{a}/external_accounts?limit=100")).await;
    let defaults: Vec<_> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["default_for_currency"] == true)
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(defaults, [entries[1].object_id.clone()]);
    // The embedded list leads with it.
    let account = json(&m, &format!("/v1/accounts/{a}")).await;
    assert_eq!(
        account["external_accounts"]["data"][0]["id"],
        defaults[0].as_str()
    );
}

#[tokio::test]
async fn deleting_representative_sets_requirements() {
    let m = mock_with(CUSTOM).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let persons = json(&m, &format!("/v1/accounts/{a}/persons")).await;
    let rep = persons["data"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(persons["data"][0]["relationship"]["representative"], true);
    let before = json(&m, &format!("/v1/accounts/{a}")).await;
    assert_eq!(
        before["requirements"]["currently_due"],
        serde_json::json!([])
    );
    let updated_before = snapshot_types(&m, &a)
        .iter()
        .filter(|t| *t == "account.updated")
        .count();
    assert_eq!(updated_before, 0);
    m.account(&a)
        .run_action(Action::DeletePerson {
            id: Some(rep.clone()),
        })
        .unwrap();
    // The representative is gone from the list; the account owes one.
    let after = json(&m, &format!("/v1/accounts/{a}")).await;
    let due = serde_json::json!(["representative.first_name", "representative.last_name"]);
    assert_eq!(after["requirements"]["currently_due"], due);
    assert_eq!(after["requirements"]["eventually_due"], due);
    let types = snapshot_types(&m, &a);
    assert_eq!(types.last().map(String::as_str), Some("account.updated"));
    assert_eq!(types.iter().filter(|t| *t == "person.deleted").count(), 1);
    let n = types.len();
    assert!(types[n - 2] == "person.deleted", "{types:?}");
    assert!(!ids(&json(&m, &format!("/v1/accounts/{a}/persons")).await).contains(&rep));
    // Persons update with a new email and phone.
    if let Some(p) = json(&m, &format!("/v1/accounts/{a}/persons")).await["data"].get(0) {
        let id = p["id"].as_str().unwrap().to_string();
        m.account(&a)
            .run_action(Action::UpdatePerson {
                id: Some(id.clone()),
            })
            .unwrap();
        let p = json(&m, &format!("/v1/accounts/{a}/persons/{id}")).await;
        assert!(p["phone"].as_str().unwrap().starts_with("+1555"));
    }
    // A named person that is gone is a 409 naming it.
    let msg = invalid_action(m.account(&a).run_action(Action::DeletePerson {
        id: Some(rep.clone()),
    }));
    assert!(msg.contains(&rep), "{msg}");
}

#[tokio::test]
async fn external_account_actions_refuse_platform_scope() {
    let m = mock_with(STANDARD).await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let actions = [
        Action::CreateExternalAccount { kind: None },
        Action::DeleteExternalAccount { id: None },
        Action::UpdatePerson { id: None },
        Action::DeletePerson { id: None },
    ];
    for action in actions {
        let msg = invalid_action(m.run_action(action.clone()));
        assert!(msg.contains("platform"), "{action:?}: {msg}");
        // A Standard account is not controlled by the platform either.
        let msg = invalid_action(m.account(&a).run_action(action.clone()));
        assert!(msg.contains(&a), "{action:?}: {msg}");
    }
    // Nothing was written.
    assert!(m.truth_log().iter().all(|u| !matches!(
        u.object_type,
        ObjectType::BankAccount | ObjectType::Card | ObjectType::Person
    )));
}

#[tokio::test]
async fn churn_runs_on_controlled_accounts_only() {
    let toml = "[accounts]\nconnect = true\n\
        account_types = { standard = 0.5, express = 0.0, custom = 0.5 }\n\
        account_api = { v1 = 1.0, v2 = 0.0 }\n\
        [churn.external_account]\ncreate = 2.0\ndelete = 1.0\n\
        [churn.person]\nupdate = 2.0\ndelete = 0.2\n";
    let m = mock_with(toml).await;
    for _ in 0..6 {
        m.connect_account(AccountApi::V1).unwrap();
    }
    m.step(std::time::Duration::from_secs(48 * 3600))
        .await
        .unwrap();
    let kinds: std::collections::BTreeMap<String, ductkit_stripe_mock::AccountKind> =
        m.accounts().into_iter().map(|a| (a.id, a.kind)).collect();
    let mut created = 0;
    for u in m.truth_log() {
        if matches!(
            u.object_type,
            ObjectType::BankAccount | ObjectType::Card | ObjectType::Person
        ) {
            let acct = u.account.as_ref().expect("in an account's scope");
            assert_ne!(
                kinds[acct],
                ductkit_stripe_mock::AccountKind::Standard,
                "{u:?}"
            );
            if u.kind == ChangeKind::Created {
                created += 1;
            }
        }
    }
    assert!(created > 0);
    // Churn kept the lists consistent with the embedded ones.
    for (id, kind) in kinds {
        if kind == ductkit_stripe_mock::AccountKind::Standard {
            continue;
        }
        let list = json(
            &m,
            &format!("/v1/accounts/{id}/external_accounts?limit=100"),
        )
        .await;
        let account = json(&m, &format!("/v1/accounts/{id}")).await;
        let embedded = ids(&account["external_accounts"]);
        let live = ids(&list);
        // The default page size is 10.
        assert_eq!(embedded.len(), live.len().min(10), "{id}");
        assert!(embedded.iter().all(|e| live.contains(e)), "{id}");
    }
}

#[tokio::test]
async fn pagination_writes_validate_the_account_first_and_write_in_its_scope() {
    let m = mock_with(&format!(
        "{CUSTOM}[api]\npagination_writes = {{ probability = 1.0 }}\n"
    ))
    .await;
    let a = m.connect_account(AccountApi::V1).unwrap();
    let before = m.truth_log().len();
    // An unknown account is a 404 and writes nothing, whatever the cursor.
    for sub in ["external_accounts", "persons"] {
        let r = get(
            &m,
            &format!("/v1/accounts/acct_nope/{sub}?starting_after=x"),
        )
        .await;
        assert_eq!(r.status(), 404, "{sub}");
    }
    let r = get(&m, "/v1/transfers/tr_nope/reversals?starting_after=x").await;
    assert_eq!(r.status(), 404);
    let r = get(&m, "/v1/application_fees/fee_nope/refunds?starting_after=x").await;
    assert_eq!(r.status(), 404);
    assert_eq!(m.truth_log().len(), before);
    // A page after the first writes in the account's scope.
    let first = json(&m, &format!("/v1/accounts/{a}/external_accounts?limit=1")).await;
    let cursor = first["data"][0]["id"].as_str().unwrap().to_string();
    get(
        &m,
        &format!("/v1/accounts/{a}/external_accounts?limit=1&starting_after={cursor}"),
    )
    .await;
    let after = m.truth_log();
    let new: Vec<_> = after[before..]
        .iter()
        .filter(|u| matches!(u.object_type, ObjectType::BankAccount | ObjectType::Card))
        .collect();
    assert!(!new.is_empty());
    assert!(new.iter().all(|u| u.account.as_deref() == Some(&*a)));
    let first = json(&m, &format!("/v1/accounts/{a}/persons?limit=1")).await;
    let cursor = first["data"][0]["id"].as_str().unwrap().to_string();
    let n = m.truth_log().len();
    get(
        &m,
        &format!("/v1/accounts/{a}/persons?limit=1&starting_after={cursor}"),
    )
    .await;
    let log = m.truth_log();
    assert!(
        log[n..]
            .iter()
            .any(|u| u.object_type == ObjectType::Person && u.kind != ChangeKind::Created),
        "a person update"
    );
}

#[tokio::test]
async fn deleting_non_representative_after_representative_gone_emits_no_account_update() {
    let m = mock_with(CUSTOM).await;
    // Find a seed-independent account with at least two persons.
    let mut a = m.connect_account(AccountApi::V1).unwrap();
    for _ in 0..40 {
        let n = json(&m, &format!("/v1/accounts/{a}/persons")).await["data"]
            .as_array()
            .unwrap()
            .len();
        if n >= 2 {
            break;
        }
        a = m.connect_account(AccountApi::V1).unwrap();
    }
    let persons = json(&m, &format!("/v1/accounts/{a}/persons")).await;
    let people = ids(&persons);
    assert!(people.len() >= 2);
    m.account(&a)
        .run_action(Action::DeletePerson {
            id: Some(people[0].clone()),
        })
        .unwrap();
    let count = |m: &MockStripe| {
        snapshot_types(m, &a)
            .iter()
            .filter(|t| *t == "account.updated")
            .count()
    };
    let log_len = m.truth_log().len();
    assert_eq!(count(&m), 1);
    m.account(&a)
        .run_action(Action::DeletePerson {
            id: Some(people[1].clone()),
        })
        .unwrap();
    assert_eq!(count(&m), 1);
    // Only the person's deletion was recorded.
    assert_eq!(m.truth_log().len(), log_len + 1);
}
