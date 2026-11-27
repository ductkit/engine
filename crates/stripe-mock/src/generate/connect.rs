//! Connect flows of a connected account's generator: the external accounts and persons of
//! an account the platform controls (spec section 5.4; phase 8 rulings P4, P8 and P9).
//! Later phase 8 tasks add their flows here.
//!
//! Every function runs in the account's own scope, with the account's generator streams.

use chrono::Datelike;

use super::ExternalAccountKind;
use crate::engine::{Cause, ChangeKind, Engine};
use crate::model::connect::{
    BankAccountDraw, DebitCardDraw, PersonDraw, apply_person_contact, deleted_bank_account,
    deleted_card, deleted_person, is_representative, new_bank_account, new_debit_card, new_person,
    representative_due,
};
use crate::rng::SimRng;
use crate::stripe;
use crate::{AccountKind, MockError, ObjectType};

const BANKS: [&str; 4] = [
    "STRIPE TEST BANK",
    "JPMORGAN CHASE",
    "WELLS FARGO",
    "BANK OF AMERICA",
];
const FIRST_NAMES: [&str; 8] = [
    "Alex", "Blake", "Casey", "Dana", "Emery", "Finley", "Harper", "Jordan",
];
const LAST_NAMES: [&str; 8] = [
    "Adler", "Brooks", "Chen", "Diaz", "Ellis", "Fischer", "Grant", "Hughes",
];
const CARD_BRANDS: [&str; 2] = ["visa", "mastercard"];

/// The probability that a generated external account is a debit card.
const CARD_PROBABILITY: f64 = 0.3;
/// The probability that a generated bank account becomes the default.
const NEW_DEFAULT_PROBABILITY: f64 = 0.3;

/// Whether the engine's scope is a connected account the platform controls (P4).
pub(super) fn controlled(engine: &Engine) -> bool {
    engine
        .scope()
        .as_deref()
        .and_then(|id| engine.account_meta(id))
        .is_some_and(|m| m.kind != AccountKind::Standard)
}

/// The scope's account ID, or a 409 `invalid_action` naming the scope when it is the
/// platform or an account the platform does not control.
fn require_controlled(engine: &Engine, what: &str) -> Result<String, MockError> {
    match engine.scope().as_deref() {
        None => Err(MockError::InvalidAction(format!(
            "{what} needs a connected account the platform controls, not the platform scope"
        ))),
        Some(id) if !controlled(engine) => Err(MockError::InvalidAction(format!(
            "{what} needs a connected account the platform controls, but {id} is not controlled by the platform"
        ))),
        Some(id) => Ok(id.to_string()),
    }
}

/// The scope's name for messages.
pub(super) fn scope_name(engine: &Engine) -> String {
    engine
        .scope()
        .clone()
        .unwrap_or_else(|| "the platform scope".to_string())
}

fn draw_last4(rng: &mut SimRng) -> String {
    format!("{:04}", rng.below(10_000))
}

/// Sixteen random alphanumerics.
fn draw_fingerprint(rng: &mut SimRng) -> String {
    // `id_len` with an empty prefix yields `_` plus the characters.
    rng.id_len("", 16)[1..].to_string()
}

fn draw_bank(rng: &mut SimRng) -> BankAccountDraw {
    BankAccountDraw {
        id: rng.id(ObjectType::BankAccount.id_prefix()),
        bank_name: rng.choose(&BANKS).to_string(),
        last4: draw_last4(rng),
        fingerprint: draw_fingerprint(rng),
    }
}

fn draw_card(rng: &mut SimRng, engine: &Engine) -> DebitCardDraw {
    let year = i64::from(engine.now().to_datetime().year());
    DebitCardDraw {
        id: rng.id(ObjectType::Card.id_prefix()),
        brand: rng.choose(&CARD_BRANDS).to_string(),
        last4: draw_last4(rng),
        exp_month: 1 + rng.below(12) as i64,
        exp_year: year + 1 + rng.below(5) as i64,
        fingerprint: draw_fingerprint(rng),
    }
}

fn draw_person(rng: &mut SimRng) -> PersonDraw {
    PersonDraw {
        id: rng.id(ObjectType::Person.id_prefix()),
        first_name: rng.choose(&FIRST_NAMES).to_string(),
        last_name: rng.choose(&LAST_NAMES).to_string(),
    }
}

/// The setup of a newly connected controlled account, run in its scope under `cause`
/// (`account_setup`): one default bank account, 1 to 3 persons (the first is the
/// representative and an executive, P8) and the account's embedded list (P9).
pub(super) fn setup(rng: &mut SimRng, engine: &mut Engine, cause: &Cause, page_size: usize) {
    let acct = engine
        .scope()
        .clone()
        .expect("a controlled account is set up in its own scope");
    let bank = draw_bank(rng);
    engine.create_obj(
        ObjectType::BankAccount,
        bank.id.clone(),
        &new_bank_account(&acct, &bank, true),
        cause.clone(),
        &["account.external_account.created"],
    );
    let persons = 1 + rng.below(3);
    for i in 0..persons {
        let d = draw_person(rng);
        let person = new_person(&acct, engine.now().unix_seconds(), &d, i == 0);
        engine.create_obj(
            ObjectType::Person,
            d.id.clone(),
            &person,
            cause.clone(),
            &["person.created"],
        );
    }
    sync_embedded(engine, page_size);
}

/// The scope's live external accounts in creation order: type, ID and whether it is the
/// default for its currency.
fn live_external(engine: &Engine) -> Vec<(ObjectType, String, bool)> {
    let mut all: Vec<(usize, ObjectType, String, bool)> = Vec::new();
    for t in [ObjectType::BankAccount, ObjectType::Card] {
        for o in engine.store().in_creation_order(t) {
            if o.deleted {
                continue;
            }
            let default = o.latest.state["default_for_currency"] == true;
            // The log position of the first version is the global creation order.
            all.push((o.versions[0], t, o.latest.object_id.clone(), default));
        }
    }
    all.sort_by_key(|(pos, ..)| *pos);
    all.into_iter().map(|(_, t, id, d)| (t, id, d)).collect()
}

/// Brings the account's embedded `external_accounts` in step with its live external
/// accounts (P9): the default first, then the rest by creation, cut to the page size. A
/// silent account mutation: only `account.external_account.*` announces the change.
fn sync_embedded(engine: &mut Engine, page_size: usize) {
    let Some(acct) = engine.scope().clone() else {
        return;
    };
    let live = live_external(engine);
    let (defaults, rest): (Vec<_>, Vec<_>) = live.into_iter().partition(|(_, _, d)| *d);
    // The newest default first.
    let ordered: Vec<_> = defaults.into_iter().rev().chain(rest).collect();
    let has_more = ordered.len() > page_size;
    let data: Vec<stripe::AccountExternalAccountsUnion> = ordered
        .into_iter()
        .take(page_size)
        .map(|(t, id, _)| match t {
            ObjectType::BankAccount => stripe::AccountExternalAccountsUnion::BankAccount(
                engine.latest_obj(t, &id).expect("live"),
            ),
            _ => {
                stripe::AccountExternalAccountsUnion::Card(engine.latest_obj(t, &id).expect("live"))
            }
        })
        .collect();
    let list = stripe::List {
        object: stripe::ListObject::List,
        data,
        has_more,
        url: format!("/v1/accounts/{acct}/external_accounts"),
        total_count: None,
    };
    engine
        .update_account_with(&acct, |a| a.external_accounts = Some(list), &[])
        .expect("the account is connected");
}

/// Adds a bank account or debit card (`kind`, drawn when `None`): a new bank account
/// becomes the default with p = 0.3, and the old default is demoted first.
pub(super) fn create_external_account(
    rng: &mut SimRng,
    engine: &mut Engine,
    page_size: usize,
    kind: Option<ExternalAccountKind>,
) -> Result<Cause, MockError> {
    let acct = require_controlled(engine, "create_external_account")?;
    let card = match kind {
        Some(k) => k == ExternalAccountKind::Card,
        None => rng.chance(CARD_PROBABILITY),
    };
    let cause = engine.begin_action("external_account_create");
    if card {
        let d = draw_card(rng, engine);
        engine.create_obj(
            ObjectType::Card,
            d.id.clone(),
            &new_debit_card(&acct, &d),
            cause.clone(),
            &["account.external_account.created"],
        );
    } else {
        let default = rng.chance(NEW_DEFAULT_PROBABILITY);
        let d = draw_bank(rng);
        if default {
            let old = live_external(engine)
                .into_iter()
                .find(|(t, _, default)| *t == ObjectType::BankAccount && *default);
            if let Some((_, old, _)) = old {
                engine
                    .update_obj::<stripe::BankAccount>(
                        ObjectType::BankAccount,
                        &old,
                        |b| b.default_for_currency = Some(Some(false)),
                        cause.clone(),
                        ChangeKind::Updated,
                        &["account.external_account.updated"],
                    )
                    .expect("live bank account");
            }
        }
        engine.create_obj(
            ObjectType::BankAccount,
            d.id.clone(),
            &new_bank_account(&acct, &d, default),
            cause.clone(),
            &["account.external_account.created"],
        );
    }
    sync_embedded(engine, page_size);
    Ok(cause)
}

/// Hard-deletes the named or a random non-default external account. `None` when none is
/// deletable and no ID was named; a named one that is not a live external account of the
/// scope, or is the default, is an error naming it.
pub(super) fn delete_external_account(
    rng: &mut SimRng,
    engine: &mut Engine,
    page_size: usize,
    id: Option<String>,
) -> Result<Option<Cause>, MockError> {
    require_controlled(engine, "delete_external_account")?;
    let live = live_external(engine);
    let (t, id) = match id {
        Some(id) => match live.iter().find(|(_, i, _)| *i == id) {
            None => {
                return Err(MockError::InvalidAction(format!(
                    "{id} is not an external account of {}",
                    scope_name(engine)
                )));
            }
            Some((_, _, true)) => {
                return Err(MockError::InvalidAction(format!(
                    "{id} is the default external account of {} and cannot be deleted",
                    scope_name(engine)
                )));
            }
            Some((t, id, _)) => (*t, id.clone()),
        },
        None => {
            let deletable: Vec<_> = live.iter().filter(|(_, _, d)| !*d).collect();
            if deletable.is_empty() {
                return Ok(None);
            }
            let (t, id, _) = rng.choose(&deletable);
            (*t, id.clone())
        }
    };
    let cause = engine.begin_action("external_account_delete");
    let events = &["account.external_account.deleted"];
    match t {
        ObjectType::BankAccount => {
            engine.delete(t, &id, &deleted_bank_account(&id), cause.clone(), events)
        }
        _ => engine.delete(t, &id, &deleted_card(&id), cause.clone(), events),
    }
    .expect("live external account");
    sync_embedded(engine, page_size);
    Ok(Some(cause))
}

/// The named or a random live person of the scope; `None` when none is live and no ID
/// was named, an error naming a named one that is not a live person.
fn pick_person(
    rng: &mut SimRng,
    engine: &Engine,
    id: Option<String>,
) -> Result<Option<String>, MockError> {
    let live = engine.store().live_ids(ObjectType::Person);
    match id {
        Some(id) if live.contains(&id) => Ok(Some(id)),
        Some(id) => Err(MockError::InvalidAction(format!(
            "{id} is not a person of {}",
            scope_name(engine)
        ))),
        None if live.is_empty() => Ok(None),
        None => Ok(Some(rng.choose(live).clone())),
    }
}

/// Gives the named or a random person a new email and phone. `None` with no person to
/// update.
pub(super) fn update_person(
    rng: &mut SimRng,
    engine: &mut Engine,
    id: Option<String>,
) -> Result<Option<Cause>, MockError> {
    require_controlled(engine, "update_person")?;
    let Some(id) = pick_person(rng, engine, id)? else {
        return Ok(None);
    };
    let person: stripe::Person = engine
        .latest_obj(ObjectType::Person, &id)
        .expect("live person");
    let name = |n: &Option<Option<String>>| n.clone().flatten().unwrap_or_default().to_lowercase();
    let email = format!(
        "{}.{}{}@example.com",
        name(&person.first_name),
        name(&person.last_name),
        rng.below(1000)
    );
    let phone = format!("+1555{:07}", rng.below(10_000_000));
    let cause = engine.begin_action("person_update");
    engine
        .update_obj::<stripe::Person>(
            ObjectType::Person,
            &id,
            |p| apply_person_contact(p, &email, &phone),
            cause.clone(),
            ChangeKind::Updated,
            &["person.updated"],
        )
        .expect("live person");
    Ok(Some(cause))
}

/// Hard-deletes the named or a random person. When no representative is left, the
/// account's requirements ask for one (P8). `None` with no person to delete.
pub(super) fn delete_person(
    rng: &mut SimRng,
    engine: &mut Engine,
    id: Option<String>,
) -> Result<Option<Cause>, MockError> {
    let acct = require_controlled(engine, "delete_person")?;
    let Some(id) = pick_person(rng, engine, id)? else {
        return Ok(None);
    };
    let cause = engine.begin_action("person_delete");
    engine
        .delete(
            ObjectType::Person,
            &id,
            &deleted_person(&id),
            cause.clone(),
            &["person.deleted"],
        )
        .expect("live person");
    let representative_left = engine.store().live_ids(ObjectType::Person).iter().any(|p| {
        engine
            .latest_obj::<stripe::Person>(ObjectType::Person, p)
            .is_some_and(|p| is_representative(&p))
    });
    if !representative_left {
        engine
            .update_account_with(&acct, representative_due, &["account.updated"])
            .expect("the account is connected");
    }
    Ok(Some(cause))
}
