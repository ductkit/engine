//! Credit notes (phase 9 ruling Q8): the `credit_note` action, the `CreditNote` and
//! `VoidCreditNote` control hooks, and the credit-note pagination write.
//!
//! An `open` invoice gets a **pre-payment** note, which lowers what is left to pay; a
//! `paid` one gets a **post-payment** note, which refunds part of its charge. Voiding a
//! pre-payment note on an invoice that is still open gives the credit back.

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::{Ctx, payments, pick};
use crate::model::amounts::share;
use crate::model::credit_note::{NewCreditNote, REASONS, credit_note, line_item};
use crate::model::customer::random_metadata_value;
use crate::rng::SimRng;
use crate::stripe::{CreditNote, CreditNoteStatus, CreditNoteType, Invoice};
use crate::{MockError, ObjectType};

/// Chance that the action voids a note, when one is voidable.
const VOID_PROBABILITY: f64 = 0.25;

/// What an invoice can be credited, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Credit {
    /// An `open` invoice: a pre-payment note of at most `amount_remaining − 1`, so it is
    /// never fully credited.
    Pre { remaining: i64 },
    /// A `paid` invoice: a post-payment note of at most what is left to refund on its
    /// charge.
    Post { left: i64 },
}

impl Credit {
    /// The largest note the invoice takes; below 1 when it takes none.
    fn max(self) -> i64 {
        match self {
            Credit::Pre { remaining } => remaining - 1,
            Credit::Post { left, .. } => left,
        }
    }

    /// A drawn amount (ruling Q8): `max(1, base × p / 100)` of `amount_remaining` with `p`
    /// in 10–50, or of what is left to refund with `p` in 10–100.
    fn draw(self, rng: &mut SimRng) -> i64 {
        let (base, p) = match self {
            Credit::Pre { remaining } => (remaining, 10 + rng.below(41) as i64),
            Credit::Post { left, .. } => (left, 10 + rng.below(91) as i64),
        };
        (base * p / 100).max(1)
    }
}

fn invoice_status(engine: &Engine, invoice: &str) -> Option<String> {
    engine.latest(ObjectType::Invoice, invoice)?.state["status"]
        .as_str()
        .map(str::to_string)
}

/// The `ch_` that paid `invoice` and what is left to refund on it: `amount −
/// amount_refunded`. A disputed charge, or one with nothing left, gives `None`.
fn refundable_charge(engine: &Engine, invoice: &str) -> Option<(String, i64)> {
    let charge = payments::invoice_charge(engine, invoice)?;
    let c = &engine.latest(ObjectType::Charge, &charge)?.state;
    if c["status"] != "succeeded" || c["disputed"] == true {
        return None;
    }
    let left = c["amount"].as_i64()? - c["amount_refunded"].as_i64()?;
    (left > 0).then_some((charge, left))
}

/// How `invoice` can be credited: `None` unless it is `open` with `amount_remaining ≥ 2`, or
/// `paid` with something left to refund on its charge.
fn credit_of(engine: &Engine, invoice: &str) -> Option<Credit> {
    match invoice_status(engine, invoice)?.as_str() {
        "open" => {
            let remaining =
                engine.latest(ObjectType::Invoice, invoice)?.state["amount_remaining"].as_i64()?;
            (remaining >= 2).then_some(Credit::Pre { remaining })
        }
        "paid" => {
            let (_, left) = refundable_charge(engine, invoice)?;
            Some(Credit::Post { left })
        }
        _ => None,
    }
}

/// Invoices a note can credit, in creation order.
fn eligible(engine: &Engine) -> Vec<(String, Credit)> {
    engine
        .store()
        .live_ids(ObjectType::Invoice)
        .iter()
        .filter_map(|id| credit_of(engine, id).map(|c| (id.clone(), c)))
        .collect()
}

/// Whether `note` can be voided: an `issued` pre-payment note whose invoice is still
/// `open`.
fn voidable_note(engine: &Engine, note: &str) -> bool {
    let Some(u) = engine.latest(ObjectType::CreditNote, note) else {
        return false;
    };
    u.state["status"] == "issued"
        && u.state["type"] == "pre_payment"
        && u.state["invoice"]
            .as_str()
            .is_some_and(|i| invoice_status(engine, i).as_deref() == Some("open"))
}

/// Voidable notes, in creation order.
fn voidable(engine: &Engine) -> Vec<String> {
    engine
        .store()
        .live_ids(ObjectType::CreditNote)
        .iter()
        .filter(|n| voidable_note(engine, n))
        .cloned()
        .collect()
}

/// The invoice's lines with `amount > 0`, in creation order: `(id, description, amount)`.
fn creditable_lines(engine: &Engine, invoice: &str) -> Vec<(String, Option<String>, i64)> {
    engine
        .store()
        .children(ObjectType::InvoiceLineItem, "invoice", invoice)
        .into_iter()
        .filter(|o| !o.deleted)
        .filter_map(|o| {
            let s = &o.latest.state;
            let amount = s["amount"].as_i64().filter(|a| *a > 0)?;
            let description = s["description"].as_str().map(str::to_string);
            Some((o.latest.object_id.clone(), description, amount))
        })
        .collect()
}

/// Issues a note for `amount` on `invoice`, credited as `credit` (ruling Q8), under
/// `cause`. Draws the reason, the note's ID and one line item ID per creditable line from
/// `rng`.
///
/// Order: a post-payment note's refund first (`refund.created`, `charge.refunded`), so the
/// note's `refunds` names a refund that exists; then the line items (no event), the note
/// (`credit_note.created`) and the invoice (`invoice.updated`).
fn issue(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    invoice: &str,
    amount: i64,
    cause: &Cause,
) -> String {
    let reason = *pick(rng, &REASONS);
    let id = rng.id(ObjectType::CreditNote.id_prefix());
    let lines = creditable_lines(engine, invoice);
    let line_ids: Vec<String> = lines
        .iter()
        .map(|_| rng.id(ObjectType::CreditNoteLineItem.id_prefix()))
        .collect();
    let inv: Invoice = engine
        .latest_obj(ObjectType::Invoice, invoice)
        .expect("checked invoice");
    let credit = credit_of(engine, invoice).expect("checked invoice");
    let refund = match credit {
        Credit::Pre { .. } => None,
        Credit::Post { .. } => {
            let (charge, _) = refundable_charge(engine, invoice).expect("checked charge");
            Some(
                payments::refund(engine, ctx, &charge, amount, cause)
                    .expect("checked refundable amount"),
            )
        }
    };
    let weights: Vec<i64> = lines.iter().map(|l| l.2).collect();
    let shares = share(amount, &weights);
    let items: Vec<_> = lines
        .iter()
        .zip(&line_ids)
        .zip(shares)
        .map(|(((il, description, _), cnli), s)| line_item(cnli, il, description.clone(), s))
        .collect();
    for item in &items {
        engine.create_obj(
            ObjectType::CreditNoteLineItem,
            item.id.clone(),
            item,
            cause.clone(),
            &[],
        );
    }
    engine.record_credit_note_lines(&id, line_ids);
    let n = engine
        .store()
        .children(ObjectType::CreditNote, "invoice", invoice)
        .len()
        + 1;
    let number = format!(
        "{}-CN-{n:02}",
        inv.number
            .clone()
            .flatten()
            .expect("a finalized invoice has a number")
    );
    let type_ = match credit {
        Credit::Pre { .. } => CreditNoteType::PrePayment,
        Credit::Post { .. } => CreditNoteType::PostPayment,
    };
    let note = credit_note(&NewCreditNote {
        id: &id,
        number: &number,
        invoice,
        customer: inv.customer.id(),
        currency: &inv.currency,
        amount,
        type_,
        reason,
        refund: refund.as_deref(),
        lines: &items,
        page_size: ctx.page_size,
        created: engine.now().unix_seconds(),
    });
    engine.create_obj(
        ObjectType::CreditNote,
        id.clone(),
        &note,
        cause.clone(),
        &["credit_note.created"],
    );
    engine
        .update_obj::<Invoice>(
            ObjectType::Invoice,
            invoice,
            |i| match credit {
                Credit::Pre { .. } => {
                    i.amount_due -= amount;
                    i.amount_remaining -= amount;
                    i.pre_payment_credit_notes_amount += amount;
                }
                Credit::Post { .. } => i.post_payment_credit_notes_amount += amount,
            },
            cause.clone(),
            ChangeKind::Updated,
            &["invoice.updated"],
        )
        .expect("live invoice");
    id
}

/// Voids the voidable `note` (ruling Q8): the note `void` with `voided_at`
/// (`credit_note.voided`), then its invoice's pre-payment changes reversed
/// (`invoice.updated`).
fn void(engine: &mut Engine, note: &str, cause: &Cause) {
    let now = engine.now().unix_seconds();
    let n: CreditNote = engine
        .latest_obj(ObjectType::CreditNote, note)
        .expect("checked note");
    engine
        .update_obj::<CreditNote>(
            ObjectType::CreditNote,
            note,
            |n| {
                n.status = CreditNoteStatus::Void;
                n.voided_at = Some(Some(now));
            },
            cause.clone(),
            ChangeKind::StateChanged,
            &["credit_note.voided"],
        )
        .expect("checked note");
    let amount = n.amount;
    engine
        .update_obj::<Invoice>(
            ObjectType::Invoice,
            n.invoice.id(),
            |i| {
                i.amount_due += amount;
                i.amount_remaining += amount;
                i.pre_payment_credit_notes_amount -= amount;
            },
            cause.clone(),
            ChangeKind::Updated,
            &["invoice.updated"],
        )
        .expect("open invoice");
}

/// `credit_note` (ruling Q8): with probability 0.25, when a note is voidable, voids a
/// random one; otherwise credits a random eligible invoice with a drawn amount. With no
/// eligible invoice, nothing happens.
pub(crate) fn credit_note_action(rng: &mut SimRng, engine: &mut Engine, ctx: &mut Ctx) {
    let notes = voidable(engine);
    if !notes.is_empty() && rng.chance(VOID_PROBABILITY) {
        let note = pick(rng, &notes).clone();
        let cause = engine.begin_action("credit_note");
        void(engine, &note, &cause);
        return;
    }
    let invoices = eligible(engine);
    if invoices.is_empty() {
        return;
    }
    let (invoice, credit) = pick(rng, &invoices).clone();
    let amount = credit.draw(rng);
    let cause = engine.begin_action("credit_note");
    issue(rng, engine, ctx, &invoice, amount, &cause);
}

fn invalid(msg: String) -> MockError {
    MockError::InvalidAction(msg)
}

/// The `CreditNote` control hook: a note on the named invoice, or a random eligible one,
/// for `amount` (drawn as the action draws it when `None`). A named invoice that is missing,
/// neither `open` nor `paid`, or has nothing left to credit, and an `amount` outside `[1,
/// amount_remaining − 1]` (open) or `[1, left to refund]` (paid), are 409s naming the
/// invoice. With no invoice named, a random one the amount fits; none is a 409.
pub(crate) fn credit_note_hook(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    invoice: Option<String>,
    amount: Option<i64>,
) -> Result<Cause, MockError> {
    let fits = |c: Credit| amount.is_none_or(|a| (1..=c.max()).contains(&a));
    let (invoice, credit) = match invoice {
        Some(id) => {
            let Some(status) = invoice_status(engine, &id) else {
                return Err(invalid(format!("invoice {id} does not exist")));
            };
            if status != "open" && status != "paid" {
                return Err(invalid(format!(
                    "invoice {id} is {status}: only an open or paid invoice takes a credit note"
                )));
            }
            let Some(credit) = credit_of(engine, &id) else {
                return Err(invalid(format!("invoice {id} has nothing left to credit")));
            };
            if !fits(credit) {
                return Err(invalid(format!(
                    "credit amount {} is outside [1, {}] for invoice {id}",
                    amount.unwrap_or_default(),
                    credit.max()
                )));
            }
            (id, credit)
        }
        None => {
            let invoices: Vec<(String, Credit)> = eligible(engine)
                .into_iter()
                .filter(|(_, c)| fits(*c))
                .collect();
            if invoices.is_empty() {
                return Err(invalid(
                    "no open or paid invoice the credit note fits".to_string(),
                ));
            }
            pick(rng, &invoices).clone()
        }
    };
    let amount = match amount {
        Some(a) => a,
        None => credit.draw(rng),
    };
    let cause = engine.begin_action("credit_note");
    issue(rng, engine, ctx, &invoice, amount, &cause);
    Ok(cause)
}

/// The `VoidCreditNote` control hook: voids the named note, or a random voidable one. A
/// named note that is missing, already void, post-payment, or whose invoice is no longer
/// open, or none voidable, is a 409 naming it.
pub(crate) fn void_credit_note(
    rng: &mut SimRng,
    engine: &mut Engine,
    id: Option<String>,
) -> Result<Cause, MockError> {
    let note = match id {
        Some(id) => {
            let Some(u) = engine.latest(ObjectType::CreditNote, &id) else {
                return Err(invalid(format!("credit note {id} does not exist")));
            };
            if u.state["status"] == "void" {
                return Err(invalid(format!("credit note {id} is already void")));
            }
            if u.state["type"] != "pre_payment" {
                return Err(invalid(format!(
                    "credit note {id} is post-payment: only a pre-payment note can be voided"
                )));
            }
            if !voidable_note(engine, &id) {
                return Err(invalid(format!(
                    "credit note {id}'s invoice is no longer open"
                )));
            }
            id
        }
        None => {
            let notes = voidable(engine);
            if notes.is_empty() {
                return Err(invalid(
                    "no issued pre-payment credit note on an open invoice to void".to_string(),
                ));
            }
            pick(rng, &notes).clone()
        }
    };
    let cause = engine.begin_action("void_credit_note");
    void(engine, &note, &cause);
    Ok(cause)
}

/// The credit-note pagination write (ruling Q10): a new `metadata.note` on a random note
/// (`credit_note.updated`). With no note, nothing happens.
pub(crate) fn pagination_update(rng: &mut SimRng, engine: &mut Engine) {
    let ids = engine.store().live_ids(ObjectType::CreditNote).to_vec();
    if ids.is_empty() {
        return;
    }
    let id = pick(rng, &ids).clone();
    let note = random_metadata_value(rng);
    let cause = engine.begin_action("credit_note_update");
    engine
        .update_obj::<CreditNote>(
            ObjectType::CreditNote,
            &id,
            |n| {
                n.metadata
                    .get_or_insert_with(|| Some(Default::default()))
                    .get_or_insert_with(Default::default)
                    .insert("note".to_string(), note);
            },
            cause,
            ChangeKind::Updated,
            &["credit_note.updated"],
        )
        .expect("live credit note");
}
