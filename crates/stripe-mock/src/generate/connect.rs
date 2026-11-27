//! Connect flows of a connected account's generator: the external accounts and persons of
//! an account the platform controls (spec section 5.4; phase 8 rulings P4, P8 and P9).
//! Later phase 8 tasks add their flows here.
//!
//! Every function runs in the account's own scope, with the account's generator streams.

use chrono::Datelike;

use super::{Ctx, ExternalAccountKind, pick};
use crate::engine::{Cause, ChangeKind, Engine};
use crate::model::connect::{
    BankAccountDraw, DebitCardDraw, NewApplicationFee, PersonDraw, apply_person_contact,
    deleted_bank_account, deleted_card, deleted_person, fee_refunds_url, is_representative,
    new_application_fee, new_application_fee_balance_transaction, new_bank_account, new_debit_card,
    new_fee_refund, new_fee_refund_balance_transaction, new_person, representative_due,
};
use crate::rng::SimRng;
use crate::stripe;
use crate::{AccountKind, MockError, ObjectType, SimTime};

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
    let was_representative = engine
        .latest_obj::<stripe::Person>(ObjectType::Person, &id)
        .is_some_and(|p| is_representative(&p));
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
    // Only the deletion of the last representative changes the requirements.
    if was_representative && !representative_left {
        engine
            .update_account_with(&acct, representative_due, &["account.updated"])
            .expect("the account is connected");
    }
    Ok(Some(cause))
}

/// Chance that an application fee refund with no given amount refunds everything left.
const FULL_FEE_REFUND_PROBABILITY: f64 = 0.7;

/// Whether the payment intent about to be created in the engine's scope carries an
/// application fee, and how much. Only a connected account's scope can: the platform's own
/// payments never do. With `accounts.application_fee.probability` at 0 nothing is drawn.
pub(super) fn draw_application_fee(engine: &Engine, ctx: &mut Ctx, amount: i64) -> Option<i64> {
    let (probability, percent) = (ctx.application_fee.probability, ctx.application_fee.percent);
    if engine.scope().is_none() || probability <= 0.0 || !ctx.fee_rng.chance(probability) {
        return None;
    }
    // Rounded half up; no fee when that is 0.
    let fee = (amount as f64 * percent / 100.0 + 0.5).floor() as i64;
    (fee > 0).then_some(fee)
}

/// What the platform needs to record the fee of one succeeded direct charge.
pub(super) struct FeeCollection<'a> {
    /// The fee's ID, drawn before the charge was built.
    pub fee: &'a str,
    pub charge: &'a str,
    pub amount: i64,
    pub currency: &'a str,
    /// The platform's application (`ca_...`).
    pub application: &'a str,
    /// The charge's `available_on`, which the fee's balance transaction shares.
    pub available_on: i64,
}

/// Collects the application fee of a succeeded direct charge, run in the connected
/// account's scope after `payment_intent.succeeded`. In the platform scope, under the same
/// cause: the fee's `pending` balance transaction, then the fee (`application_fee.created`).
/// The settlement of the balance transaction is queued with the platform as its scope.
pub(super) fn collect_application_fee(
    engine: &mut Engine,
    ctx: &mut Ctx,
    f: &FeeCollection,
    cause: &Cause,
) {
    let account = engine
        .scope()
        .clone()
        .expect("a direct charge is made in a connected account scope");
    let created = engine.now().unix_seconds();
    let bt_id = ctx.payments.id(ObjectType::BalanceTransaction.id_prefix());
    let bt = new_application_fee_balance_transaction(
        &bt_id,
        created,
        f.available_on,
        f.amount,
        f.currency,
        f.fee,
    );
    let fee = new_application_fee(&NewApplicationFee {
        id: f.fee,
        created,
        account: &account,
        application: f.application,
        charge: f.charge,
        amount: f.amount,
        currency: f.currency,
        balance_transaction: &bt_id,
    });
    engine.in_scope(None, |e| {
        e.create_obj(
            ObjectType::BalanceTransaction,
            bt_id.clone(),
            &bt,
            cause.clone(),
            &[],
        );
        e.create_obj(
            ObjectType::ApplicationFee,
            f.fee.to_string(),
            &fee,
            cause.clone(),
            &["application_fee.created"],
        );
    });
    ctx.schedule_settlement(
        SimTime(f.available_on * 1000),
        Some(None),
        bt_id,
        cause.clone(),
    );
}

/// What is left to refund of an application fee: `None` when it is fully refunded or its
/// account has disconnected.
fn refundable_fee(engine: &Engine, fee: &str) -> Option<i64> {
    let state = &engine.latest(ObjectType::ApplicationFee, fee)?.state;
    let account = state["account"].as_str()?;
    if !engine.account_meta(account).is_some_and(|m| m.connected) {
        return None;
    }
    let left = state["amount"].as_i64()? - state["amount_refunded"].as_i64()?;
    (left > 0).then_some(left)
}

/// Refunds `amount` (all of what is left with p = 0.7, else a uniform amount in
/// `[1, left]`, when `None`) of the named or a random application fee with something left.
/// Platform scope only. `None` when no fee was named and none can be refunded; a named fee
/// that cannot be refunded, or an `amount` outside `[1, left]`, is an error naming it.
///
/// Under one cause: the platform's balance transaction (`type: application_fee_refund`,
/// `-amount`, `available` at once), the `fee_refund` (no event), the fee with
/// `amount_refunded`, `refunded` and the refund first in `refunds`
/// (`application_fee.refunded`), then the account's balance transaction in its own scope
/// (`+amount`, `available` at once, no event).
pub(super) fn application_fee_refund(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    fee: Option<String>,
    amount: Option<i64>,
) -> Result<Option<Cause>, MockError> {
    if engine.scope().is_some() {
        return Err(MockError::InvalidAction(format!(
            "application_fee_refund runs in the platform scope, not {}",
            scope_name(engine)
        )));
    }
    let (fee, left) = match fee {
        Some(id) => {
            if engine.latest(ObjectType::ApplicationFee, &id).is_none() {
                return Err(MockError::NoSuchObject {
                    object_type: ObjectType::ApplicationFee,
                    id,
                });
            }
            let Some(left) = refundable_fee(engine, &id) else {
                return Err(MockError::InvalidAction(format!(
                    "application fee {id} cannot be refunded: it is fully refunded or its \
                     account has disconnected"
                )));
            };
            (id, left)
        }
        None => {
            let candidates: Vec<(&String, i64)> = engine
                .store()
                .live_ids(ObjectType::ApplicationFee)
                .iter()
                .filter_map(|id| refundable_fee(engine, id).map(|left| (id, left)))
                .collect();
            if candidates.is_empty() {
                return Ok(None);
            }
            let (id, left) = *pick(rng, &candidates);
            (id.clone(), left)
        }
    };
    let amount = match amount {
        Some(a) if (1..=left).contains(&a) => a,
        Some(a) => {
            return Err(MockError::InvalidAction(format!(
                "cannot refund {a} of application fee {fee}: {left} left to refund"
            )));
        }
        None if rng.chance(FULL_FEE_REFUND_PROBABILITY) => left,
        None => 1 + rng.below(left as u64) as i64,
    };
    let state = engine
        .latest(ObjectType::ApplicationFee, &fee)
        .expect("the fee exists")
        .state
        .clone();
    let account = state["account"]
        .as_str()
        .expect("a fee names its account")
        .to_string();
    let currency = state["currency"]
        .as_str()
        .expect("a fee has a currency")
        .to_string();
    let created = engine.now().unix_seconds();
    let refund_id = ctx.payments.id(ObjectType::FeeRefund.id_prefix());
    let bt_id = ctx.payments.id(ObjectType::BalanceTransaction.id_prefix());
    let account_bt_id = ctx.payments.id(ObjectType::BalanceTransaction.id_prefix());
    let cause = engine.begin_action("application_fee_refund");

    let bt = new_fee_refund_balance_transaction(&bt_id, created, -amount, &currency, &refund_id);
    engine.create_obj(
        ObjectType::BalanceTransaction,
        bt_id.clone(),
        &bt,
        cause.clone(),
        &[],
    );
    let refund = new_fee_refund(&refund_id, created, &fee, amount, &currency, &bt_id);
    engine.create_obj(
        ObjectType::FeeRefund,
        refund_id.clone(),
        &refund,
        cause.clone(),
        &[],
    );
    // Newest first, cut to the page size.
    let ids: Vec<String> = engine
        .store()
        .children(ObjectType::FeeRefund, "fee", &fee)
        .into_iter()
        .rev()
        .filter(|o| !o.deleted)
        .map(|o| o.latest.object_id.clone())
        .collect();
    let data: Vec<stripe::FeeRefund> = ids
        .iter()
        .take(ctx.page_size)
        .map(|id| {
            engine
                .latest_obj(ObjectType::FeeRefund, id)
                .expect("live fee refund")
        })
        .collect();
    let list = stripe::List {
        object: stripe::ListObject::List,
        data,
        has_more: ids.len() > ctx.page_size,
        url: fee_refunds_url(&fee),
        total_count: None,
    };
    engine
        .update_obj::<stripe::ApplicationFee>(
            ObjectType::ApplicationFee,
            &fee,
            |f| {
                f.amount_refunded += amount;
                f.refunded = f.amount_refunded == f.amount;
                f.refunds = list;
            },
            cause.clone(),
            ChangeKind::Updated,
            &["application_fee.refunded"],
        )
        .expect("live application fee");
    let account_bt =
        new_fee_refund_balance_transaction(&account_bt_id, created, amount, &currency, &refund_id);
    engine.in_scope(Some(account), |e| {
        e.create_obj(
            ObjectType::BalanceTransaction,
            account_bt_id,
            &account_bt,
            cause.clone(),
            &[],
        );
    });
    Ok(Some(cause))
}
