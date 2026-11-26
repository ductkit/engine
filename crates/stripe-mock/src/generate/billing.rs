//! Invoice lifecycle and billing actions (spec sections 5.1, 5.2, 6.1).
//!
//! Transitions are single engine mutations. Each checks the invoice's current
//! status and does nothing (returning `false`) when it no longer applies, so a
//! continuation that lost a race (e.g. its invoice was voided meanwhile) is a no-op.

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::catalog::{active_ids, create_product_now, draw_new_product};
use crate::generate::{Continuation, Ctx, create_random_customer, payments, pick};
use crate::model::invoice::{
    BillingReason, LineSpec, NewInvoice, NewLine, deleted_invoice, deleted_line, new_invoice,
    new_line,
};
use crate::rng::SimRng;
use crate::stripe::{Customer, Invoice, InvoicePayment, InvoiceStatus, Price};
use crate::{MockError, ObjectType};

/// How a one-off invoice ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceOutcome {
    DeleteDraft,
    Paid,
    Void,
    Uncollectible,
}

fn status(engine: &Engine, id: &str) -> Option<String> {
    let u = engine.latest(ObjectType::Invoice, id)?;
    if u.kind == ChangeKind::Deleted {
        return None;
    }
    u.state["status"].as_str().map(str::to_string)
}

/// Creates the lines (unannounced), then the draft invoice (`invoice.created`).
/// IDs come from the `"generate.billing"` stream. Returns the invoice ID.
#[allow(clippy::too_many_arguments)] // each argument is part of the invoice
pub(crate) fn create_draft_invoice(
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: &str,
    lines: &[LineSpec],
    reason: BillingReason,
    subscription: Option<&str>,
    period: (i64, i64),
    cause: &Cause,
) -> String {
    let id = ctx.billing.id(ObjectType::Invoice.id_prefix());
    let now = engine.now().unix_seconds();
    let mut lines_out = Vec::new();
    for spec in lines {
        let line_id = ctx.billing.id(ObjectType::InvoiceLineItem.id_prefix());
        let invoice_item = subscription.is_none().then(|| ctx.billing.id("ii"));
        let price: Price = engine
            .latest_obj(ObjectType::Price, &spec.price)
            .expect("billed price exists");
        let product_name = engine
            .latest(ObjectType::Product, price.product.id())
            .and_then(|p| p.state["name"].as_str())
            .unwrap_or("Item")
            .to_string();
        let line = new_line(&NewLine {
            id: &line_id,
            invoice: &id,
            spec,
            price: &price,
            product_name: &product_name,
            subscription,
            invoice_item: invoice_item.as_deref(),
        });
        engine.create_obj(
            ObjectType::InvoiceLineItem,
            line_id,
            &line,
            cause.clone(),
            &[],
        );
        lines_out.push(line);
    }
    let customer: Customer = engine
        .latest_obj(ObjectType::Customer, customer)
        .expect("billed customer exists");
    let currency = lines_out
        .first()
        .map_or(String::new(), |l| l.currency.clone());
    let invoice = new_invoice(&NewInvoice {
        id: &id,
        customer: &customer,
        currency: &currency,
        lines: &lines_out,
        reason,
        subscription,
        period,
        created: now,
        page_size: ctx.page_size,
    });
    engine.create_obj(
        ObjectType::Invoice,
        id.clone(),
        &invoice,
        cause.clone(),
        &["invoice.created"],
    );
    id
}

fn transition(
    engine: &mut Engine,
    id: &str,
    from: &[&str],
    f: impl FnOnce(&mut Invoice),
    kind: ChangeKind,
    events: &[&str],
    cause: &Cause,
) -> bool {
    if !status(engine, id).is_some_and(|s| from.contains(&s.as_str())) {
        return false;
    }
    engine
        .update_obj(ObjectType::Invoice, id, f, cause.clone(), kind, events)
        .expect("live invoice")
        .is_some()
}

/// `draft → open`: numbers the invoice from its customer, then bumps the customer's
/// `next_invoice_sequence` in an unannounced mutation.
pub(crate) fn finalize(engine: &mut Engine, id: &str, cause: &Cause) -> bool {
    let now = engine.now().unix_seconds();
    let customer = engine
        .latest(ObjectType::Invoice, id)
        .and_then(|u| u.state["customer"].as_str().map(str::to_string));
    let (number, live_customer) = match customer
        .as_deref()
        .and_then(|c| engine.latest(ObjectType::Customer, c))
    {
        Some(c) if c.kind != ChangeKind::Deleted => {
            let seq = c.state["next_invoice_sequence"].as_i64().unwrap_or(1);
            let prefix = c.state["invoice_prefix"].as_str().unwrap_or("MOCK");
            (format!("{prefix}-{seq:04}"), customer.clone())
        }
        _ => (format!("MOCK-{}", &id[3..]), None),
    };
    let done = transition(
        engine,
        id,
        &["draft"],
        |i| {
            i.status = Some(Some(InvoiceStatus::Open));
            i.number = Some(Some(number));
            i.effective_at = Some(Some(now));
            i.ending_balance = Some(Some(i.starting_balance));
            i.status_transitions.finalized_at = Some(Some(now));
        },
        ChangeKind::StateChanged,
        &["invoice.finalized"],
        cause,
    );
    if done && let Some(c) = live_customer {
        engine
            .update_obj::<crate::stripe::Customer>(
                ObjectType::Customer,
                &c,
                |c| c.next_invoice_sequence = Some(c.next_invoice_sequence.unwrap_or(1) + 1),
                cause.clone(),
                ChangeKind::Updated,
                &[],
            )
            .expect("live customer");
    }
    done
}

/// Puts the latest state of `payment` into the embedded `invoice.payments`: replaces its
/// entry, or, for a payment not yet listed and when `page_size` is given, counts it and
/// appends it while the page has room.
fn sync_payment(i: &mut Invoice, payment: &InvoicePayment, page_size: Option<usize>) {
    let Some(list) = i.payments.as_mut() else {
        return;
    };
    if let Some(entry) = list.data.iter_mut().find(|p| p.id == payment.id) {
        *entry = payment.clone();
    } else if let Some(page_size) = page_size {
        let count = list.total_count.unwrap_or(list.data.len() as i64) + 1;
        if list.data.len() < page_size {
            list.data.push(payment.clone());
        } else {
            list.has_more = true;
        }
        list.total_count = Some(count);
    }
}

/// `open → paid`, with `payment` (none for a zero-amount invoice) in `invoice.payments`.
pub(crate) fn pay(
    engine: &mut Engine,
    id: &str,
    payment: Option<&InvoicePayment>,
    page_size: usize,
    cause: &Cause,
) -> bool {
    let now = engine.now().unix_seconds();
    transition(
        engine,
        id,
        &["open"],
        |i| {
            i.status = Some(Some(InvoiceStatus::Paid));
            i.amount_paid = i.total;
            i.amount_remaining = 0;
            i.attempted = true;
            i.attempt_count += 1;
            i.status_transitions.paid_at = Some(Some(now));
            if let Some(p) = payment {
                sync_payment(i, p, Some(page_size));
            }
        },
        ChangeKind::StateChanged,
        &["invoice.paid", "invoice.payment_succeeded"],
        cause,
    )
}

/// A failed payment attempt on an open invoice, with its open `payment` in
/// `invoice.payments`.
pub(crate) fn fail_payment(
    engine: &mut Engine,
    id: &str,
    payment: &InvoicePayment,
    page_size: usize,
    cause: &Cause,
) -> bool {
    transition(
        engine,
        id,
        &["open"],
        |i| {
            i.attempted = true;
            i.attempt_count += 1;
            sync_payment(i, payment, Some(page_size));
        },
        ChangeKind::Updated,
        &["invoice.payment_failed"],
        cause,
    )
}

/// `open → void`. An open invoice payment (left by a failed attempt) is canceled first,
/// and its payment intent after.
pub(crate) fn void(engine: &mut Engine, id: &str, cause: &Cause) -> bool {
    if status(engine, id).as_deref() != Some("open") {
        return false;
    }
    let now = engine.now().unix_seconds();
    let payment = payments::cancel_invoice_payment(engine, id, cause);
    let done = transition(
        engine,
        id,
        &["open"],
        |i| {
            i.status = Some(Some(InvoiceStatus::Void));
            i.status_transitions.voided_at = Some(Some(now));
            if let Some(p) = &payment {
                sync_payment(i, p, None);
            }
        },
        ChangeKind::StateChanged,
        &["invoice.voided"],
        cause,
    );
    if let Some(p) = &payment {
        payments::cancel_intent(engine, p, cause);
    }
    done
}

/// `open → uncollectible`. Unlike [`void`], the invoice stays payable, so an open invoice
/// payment (left by a failed attempt) and its payment intent are left as they are.
pub(crate) fn mark_uncollectible(engine: &mut Engine, id: &str, cause: &Cause) -> bool {
    if status(engine, id).as_deref() != Some("open") {
        return false;
    }
    let now = engine.now().unix_seconds();
    transition(
        engine,
        id,
        &["open"],
        |i| {
            i.status = Some(Some(InvoiceStatus::Uncollectible));
            i.status_transitions.marked_uncollectible_at = Some(Some(now));
        },
        ChangeKind::StateChanged,
        &["invoice.marked_uncollectible"],
        cause,
    )
}

/// Deletes a draft: its lines first (unannounced), then the invoice (`invoice.deleted`).
pub(crate) fn delete_draft(engine: &mut Engine, id: &str, cause: &Cause) -> bool {
    if status(engine, id).as_deref() != Some("draft") {
        return false;
    }
    let line_ids: Vec<String> = engine
        .store()
        .children(ObjectType::InvoiceLineItem, "invoice", id)
        .into_iter()
        .filter(|o| !o.deleted)
        .map(|o| o.latest.object_id.clone())
        .collect();
    for line in line_ids {
        engine
            .delete(
                ObjectType::InvoiceLineItem,
                &line,
                &deleted_line(&line),
                cause.clone(),
                &[],
            )
            .expect("live line");
    }
    engine
        .delete(
            ObjectType::Invoice,
            id,
            &deleted_invoice(id),
            cause.clone(),
            &["invoice.deleted"],
        )
        .expect("live invoice");
    true
}

/// One payment attempt on an open invoice: fails with
/// `actions.payment_failure_probability` (drawn from `"generate.billing"`, as before
/// payment objects existed), and creates its payment objects (see
/// [`payments::pay_invoice`]). A zero-amount invoice charges nothing, so it is paid
/// whatever the draw. Returns whether it was paid; an invoice that is no longer open is
/// left alone.
pub(crate) fn attempt_payment(engine: &mut Engine, ctx: &mut Ctx, id: &str, cause: &Cause) -> bool {
    let succeed = !ctx.billing.chance(ctx.config.payment_failure_probability);
    if status(engine, id).as_deref() != Some("open") {
        return false;
    }
    match payments::pay_invoice(engine, ctx, id, succeed, cause) {
        Some(_) => succeed,
        None => true,
    }
}

/// A live customer: the named one, or a random one, or a new one.
pub(crate) fn resolve_customer(
    rng: &mut SimRng,
    engine: &mut Engine,
    named: Option<String>,
    cause: &Cause,
) -> Result<String, MockError> {
    if let Some(id) = named {
        return match engine.latest(ObjectType::Customer, &id) {
            None => Err(MockError::NoSuchObject {
                object_type: ObjectType::Customer,
                id,
            }),
            Some(u) if u.kind == ChangeKind::Deleted => Err(MockError::Deleted {
                object_type: ObjectType::Customer,
                id,
            }),
            Some(_) => Ok(id),
        };
    }
    let live = engine.store().live_ids(ObjectType::Customer);
    if live.is_empty() {
        return Ok(create_random_customer(rng, engine, cause));
    }
    Ok(pick(rng, live).clone())
}

/// `one_off_invoice`: a manual invoice for one customer, walked to `outcome`
/// (drawn when `None`: 10% draft deleted; else 80% paid, 10% void, 10% uncollectible).
pub(crate) fn one_off_invoice(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<String>,
    outcome: Option<InvoiceOutcome>,
) -> Result<Cause, MockError> {
    let cause = engine.begin_action("one_off_invoice");
    let customer = resolve_customer(rng, engine, customer, &cause)?;
    let outcome = outcome.unwrap_or_else(|| {
        if rng.below(10) == 0 {
            InvoiceOutcome::DeleteDraft
        } else {
            match rng.below(10) {
                0..=7 => InvoiceOutcome::Paid,
                8 => InvoiceOutcome::Void,
                _ => InvoiceOutcome::Uncollectible,
            }
        }
    });
    let mut prices = active_ids(engine, ObjectType::Price);
    if prices.is_empty() {
        let np = draw_new_product(rng);
        prices = create_product_now(engine, &np, &cause);
    }
    let first = pick(rng, &prices).clone();
    let currency = engine
        .latest(ObjectType::Price, &first)
        .expect("price")
        .state["currency"]
        .clone();
    let same: Vec<String> = prices
        .into_iter()
        .filter(|p| {
            engine.latest(ObjectType::Price, p).expect("price").state["currency"] == currency
        })
        .collect();
    let one_time: Vec<String> = same
        .iter()
        .filter(|p| engine.latest(ObjectType::Price, p).expect("price").state["type"] == "one_time")
        .cloned()
        .collect();
    let pool = if one_time.is_empty() { same } else { one_time };
    let now = engine.now().unix_seconds();
    let n = 1 + rng.below(3);
    let mut lines: Vec<LineSpec> = (0..n)
        .map(|_| LineSpec {
            price: pick(rng, &pool).clone(),
            quantity: 1 + rng.below(5),
            subscription_item: None,
            period: (now, now),
        })
        .collect();
    let units: Vec<i64> = lines
        .iter()
        .map(|l| payments::unit_amount(engine, &l.price))
        .collect();
    let mut quantities: Vec<u64> = lines.iter().map(|l| l.quantity).collect();
    payments::fit_quantities(&units, &mut quantities);
    for (l, q) in lines.iter_mut().zip(quantities) {
        l.quantity = q;
    }
    let id = create_draft_invoice(
        engine,
        ctx,
        &customer,
        &lines,
        BillingReason::Manual,
        None,
        (now, now),
        &cause,
    );
    let next = if outcome == InvoiceOutcome::DeleteDraft {
        Continuation::DeleteDraft { id }
    } else {
        Continuation::FinalizeOneOff { id, outcome }
    };
    ctx.then(engine, next, cause.clone());
    Ok(cause)
}

/// Runs one invoice continuation.
pub(crate) fn run(c: Continuation, cause: Cause, engine: &mut Engine, ctx: &mut Ctx) {
    match c {
        Continuation::DeleteDraft { id } => {
            delete_draft(engine, &id, &cause);
        }
        Continuation::FinalizeOneOff { id, outcome } => {
            if finalize(engine, &id, &cause) {
                ctx.then(engine, Continuation::SettleOneOff { id, outcome }, cause);
            }
        }
        Continuation::SettleOneOff { id, outcome } => match outcome {
            InvoiceOutcome::Paid => {
                if !attempt_payment(engine, ctx, &id, &cause) {
                    ctx.then(engine, Continuation::VoidInvoice { id }, cause);
                }
            }
            InvoiceOutcome::Void => {
                void(engine, &id, &cause);
            }
            InvoiceOutcome::Uncollectible => {
                mark_uncollectible(engine, &id, &cause);
            }
            InvoiceOutcome::DeleteDraft => unreachable!("drafts are deleted before finalizing"),
        },
        Continuation::VoidInvoice { id } => {
            void(engine, &id, &cause);
        }
        other => unreachable!("not an invoice continuation: {other:?}"),
    }
}

/// `churn.invoice.update`: a metadata change on a random live invoice.
pub(crate) fn churn_update(rng: &mut SimRng, engine: &mut Engine) {
    let ids = engine.store().live_ids(ObjectType::Invoice);
    if ids.is_empty() {
        return;
    }
    let id = pick(rng, ids).clone();
    let value = crate::model::customer::random_metadata_value(rng);
    let cause = engine.begin_action("invoice_update");
    engine
        .update_obj::<Invoice>(
            ObjectType::Invoice,
            &id,
            |i| {
                i.metadata
                    .get_or_insert_with(|| Some(Default::default()))
                    .get_or_insert_with(Default::default)
                    .insert("note".to_string(), value);
            },
            cause,
            ChangeKind::Updated,
            &["invoice.updated"],
        )
        .expect("live invoice");
}
