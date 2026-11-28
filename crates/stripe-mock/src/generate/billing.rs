//! Invoice lifecycle and billing actions (spec sections 5.1, 5.2, 6.1).
//!
//! Transitions are single engine mutations. Each checks the invoice's current
//! status and does nothing (returning `false`) when it no longer applies, so a
//! continuation that lost a race (e.g. its invoice was voided meanwhile) is a no-op.

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::catalog::{active_ids, create_product_now, draw_new_product};
use crate::generate::discounts::{applied_by_id, applied_discount, end_once};
use crate::generate::{Continuation, Ctx, create_random_customer, payments, pick};
use crate::model::amounts::{AppliedDiscount, AppliedTax, InvoiceAmounts, LineAmount, compute};
use crate::model::discounts::tax_bp;
use crate::model::embedded::embedded_list;
use crate::model::invoice::{
    BillingReason, LineSpec, NewInvoice, NewInvoiceItem, NewLine, deleted_invoice,
    deleted_invoice_item, deleted_line, line_amount, lines_url, new_invoice, new_invoice_item,
    new_line, set_invoice_amounts, set_line_amounts,
};
use crate::rng::SimRng;
use crate::stripe::{
    Customer, Expandable, Invoice, InvoiceStatus, Invoiceitem, LineItem, Price, Subscription,
    TaxRate,
};
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

/// One line of a draft, before it is created.
struct PlannedLine {
    id: String,
    spec: LineSpec,
    price: Price,
    product_name: String,
    /// The item it bills: a one-off line's own item, or a joining pending item.
    item: Option<String>,
}

fn product_name(engine: &Engine, price: &Price) -> String {
    engine
        .latest(ObjectType::Product, price.product.id())
        .and_then(|p| p.state["name"].as_str())
        .unwrap_or("Item")
        .to_string()
}

fn billed_price(engine: &Engine, id: &str) -> Price {
    engine
        .latest_obj(ObjectType::Price, id)
        .expect("billed price exists")
}

/// The line ID of a pending item joining an invoice: `il_` and the item's own suffix, so
/// joining draws nothing.
fn joined_line_id(item: &str) -> String {
    let suffix = item.strip_prefix("ii_").unwrap_or(item);
    format!("{}_{suffix}", ObjectType::InvoiceLineItem.id_prefix())
}

/// The live invoice items of `customer` in creation order, with their current state.
fn customer_items(engine: &Engine, customer: &str) -> Vec<Invoiceitem> {
    engine
        .store()
        .children(ObjectType::InvoiceItem, "customer", customer)
        .into_iter()
        .filter(|o| !o.deleted)
        .map(|o| {
            engine
                .latest_obj(ObjectType::InvoiceItem, &o.latest.object_id)
                .expect("listed item exists")
        })
        .collect()
}

fn item_invoice(item: &Invoiceitem) -> Option<&str> {
    item.invoice
        .as_ref()
        .and_then(|i| i.as_ref())
        .map(|i| i.id())
}

/// The price an item bills.
fn item_price(item: &Invoiceitem) -> String {
    item.pricing
        .as_ref()
        .and_then(|p| p.as_ref())
        .and_then(|p| p.price_details.as_ref())
        .expect("generated items bill a price")
        .price
        .id()
        .to_string()
}

/// The pending invoice items of `customer` (on no invoice yet), oldest first.
pub(crate) fn pending_items(engine: &Engine, customer: &str) -> Vec<String> {
    customer_items(engine, customer)
        .into_iter()
        .filter(|i| item_invoice(i).is_none())
        .map(|i| i.id)
        .collect()
}

/// The live items on `invoice`, by their current `invoice` (the store's index holds only
/// the invoice an item was created on).
fn invoice_items(engine: &Engine, invoice: &str) -> Vec<String> {
    let Some(customer) = engine
        .latest(ObjectType::Invoice, invoice)
        .and_then(|u| u.state["customer"].as_str().map(str::to_string))
    else {
        return Vec::new();
    };
    customer_items(engine, &customer)
        .into_iter()
        .filter(|i| item_invoice(i) == Some(invoice))
        .map(|i| i.id)
        .collect()
}

/// The amounts of an invoice (ruling Q3), and the tax it carries: an invoice whose total
/// with `tax` would exceed [`payments::MAX_CHARGE_AMOUNT`] is created without it.
fn invoice_amounts<'t>(
    lines: &[LineAmount],
    discount: Option<&AppliedDiscount>,
    tax: Option<&'t AppliedTax>,
) -> (InvoiceAmounts, Option<&'t AppliedTax>) {
    let amounts = compute(lines, discount, tax);
    if tax.is_some() && amounts.total > payments::MAX_CHARGE_AMOUNT {
        return (compute(lines, discount, None), None);
    }
    (amounts, tax)
}

/// Draws the tax rate a new subscription or one-off invoice takes (phase 9 ruling Q5):
/// while at least one active tax rate exists, one with probability 0.5, uniformly over the
/// active ones, from `ctx.tax_rng` (stream `"generate.tax"`). With no active rate nothing is
/// drawn, so runs without tax rates draw as before. The rate is returned as it is now: the
/// snapshot the subscription or invoice embeds.
pub(crate) fn draw_tax_rate(engine: &Engine, ctx: &mut Ctx) -> Option<TaxRate> {
    let active = active_ids(engine, ObjectType::TaxRate);
    if active.is_empty() || !ctx.tax_rng.chance(0.5) {
        return None;
    }
    let id = pick(&mut ctx.tax_rng, &active);
    engine.latest_obj(ObjectType::TaxRate, id)
}

fn applied_tax(rate: &TaxRate) -> AppliedTax {
    AppliedTax {
        rate: rate.clone(),
        bp: tax_bp(rate),
    }
}

/// The tax rate a subscription's invoices apply: the snapshot in its `default_tax_rates`.
pub(crate) fn subscription_tax_rate(engine: &Engine, sub: &str) -> Option<TaxRate> {
    let sub: Subscription = engine.latest_obj(ObjectType::Subscription, sub)?;
    sub.default_tax_rates.flatten()?.into_iter().next()
}

/// Creates a draft invoice in one instant (phase 9 ruling Q4): a one-off invoice's own
/// items (`invoiceitem.created`) and the customer's pending items that join it (oldest
/// first, after its own lines; a silent update of their `invoice`), then the lines
/// (unannounced), then the draft (`invoice.created`). A pending item joins when it is in
/// the invoice's currency and keeps the subtotal within [`payments::MAX_CHARGE_AMOUNT`];
/// otherwise it stays pending. Invoice, line and one-off item IDs come from the
/// `"generate.billing"` stream, drawn as before items existed; joining draws nothing.
/// With `take_pending` false (a Checkout subscription's first invoice, whose amount the
/// session fixed), no pending item joins and no discount applies. Otherwise the invoice
/// applies [`applied_discount`] (ruling Q2), and every amount comes from
/// [`compute`] (ruling Q3). Every line is taxed with `tax_rate` (ruling Q5), unless the
/// total with tax would exceed the maximum charge; a one-off invoice (no `subscription`)
/// also stores the rate in its own `default_tax_rates`, even when its tax was dropped.
/// Returns the invoice ID.
#[allow(clippy::too_many_arguments)] // each argument is part of the invoice
pub(crate) fn create_draft_invoice(
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: &str,
    lines: &[LineSpec],
    reason: BillingReason,
    subscription: Option<&str>,
    period: (i64, i64),
    take_pending: bool,
    tax_rate: Option<&TaxRate>,
    cause: &Cause,
) -> String {
    let id = ctx.billing.id(ObjectType::Invoice.id_prefix());
    let now = engine.now().unix_seconds();
    let mut planned = Vec::new();
    for spec in lines {
        let line_id = ctx.billing.id(ObjectType::InvoiceLineItem.id_prefix());
        let item = subscription
            .is_none()
            .then(|| ctx.billing.id(ObjectType::InvoiceItem.id_prefix()));
        let price = billed_price(engine, &spec.price);
        planned.push(PlannedLine {
            id: line_id,
            spec: spec.clone(),
            product_name: product_name(engine, &price),
            price,
            item,
        });
    }
    let own_lines = planned.len();
    let currency = planned.first().map(|l| l.price.currency.clone());
    let mut subtotal: i64 = planned
        .iter()
        .map(|l| line_amount(&l.price, l.spec.quantity))
        .sum();
    let mut joined = Vec::new();
    let candidates = if take_pending {
        customer_items(engine, customer)
    } else {
        Vec::new()
    };
    for item in candidates {
        let fits = subtotal + item.amount <= payments::MAX_CHARGE_AMOUNT;
        if item_invoice(&item).is_some() || Some(&item.currency) != currency.as_ref() || !fits {
            continue;
        }
        subtotal += item.amount;
        let price = billed_price(engine, &item_price(&item));
        planned.push(PlannedLine {
            id: joined_line_id(&item.id),
            spec: LineSpec {
                price: price.id.clone(),
                quantity: item.quantity as u64,
                subscription_item: None,
                period: (item.period.start, item.period.end),
            },
            product_name: product_name(engine, &price),
            price,
            item: Some(item.id.clone()),
        });
        joined.push(item.id);
    }
    for l in &planned[..own_lines] {
        let Some(item_id) = l.item.as_deref() else {
            continue;
        };
        let item = new_invoice_item(&NewInvoiceItem {
            id: item_id,
            customer,
            invoice: Some(&id),
            price: &l.price,
            product_name: &l.product_name,
            quantity: l.spec.quantity,
            period: l.spec.period,
            date: now,
        });
        engine.create_obj(
            ObjectType::InvoiceItem,
            item_id.to_string(),
            &item,
            cause.clone(),
            &["invoiceitem.created"],
        );
    }
    for item in &joined {
        let invoice = id.clone();
        engine
            .update_obj::<Invoiceitem>(
                ObjectType::InvoiceItem,
                item,
                |i| i.invoice = Some(Some(Expandable::Id(invoice))),
                cause.clone(),
                ChangeKind::Updated,
                &[],
            )
            .expect("pending item is live");
    }
    let currency = currency.unwrap_or_default();
    // The session of a Checkout subscription fixed its first invoice's amount, so no
    // discount applies to it either.
    let discount = take_pending
        .then(|| applied_discount(engine, customer, subscription, &currency))
        .flatten();
    let inputs: Vec<LineAmount> = planned
        .iter()
        .map(|l| LineAmount {
            amount: line_amount(&l.price, l.spec.quantity),
        })
        .collect();
    let applied = tax_rate.map(applied_tax);
    let (amounts, tax) = invoice_amounts(&inputs, discount.as_ref(), applied.as_ref());
    let di = discount.as_ref().map(|d| d.id.as_str());
    let mut lines_out = Vec::new();
    for (l, la) in planned.iter().zip(&amounts.lines) {
        let line = new_line(&NewLine {
            id: &l.id,
            invoice: &id,
            spec: &l.spec,
            price: &l.price,
            product_name: &l.product_name,
            subscription,
            invoice_item: l.item.as_deref(),
            amounts: la,
            discount: di,
            tax,
        });
        engine.create_obj(
            ObjectType::InvoiceLineItem,
            l.id.clone(),
            &line,
            cause.clone(),
            &[],
        );
        lines_out.push(line);
    }
    let customer: Customer = engine
        .latest_obj(ObjectType::Customer, customer)
        .expect("billed customer exists");
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
        amounts: &amounts,
        discount: di,
        tax,
        default_tax_rate: subscription.is_none().then_some(tax_rate).flatten(),
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
/// `next_invoice_sequence` in an unannounced mutation. A `once` discount the invoice
/// carries ends in the same instant (phase 9 ruling Q2).
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
    if done {
        end_once(engine, id, cause);
    }
    done
}

/// `open → paid`. `amount_paid` is the `amount_remaining` the payment charged (phase 9
/// ruling Q3).
pub(crate) fn pay(engine: &mut Engine, id: &str, cause: &Cause) -> bool {
    let now = engine.now().unix_seconds();
    transition(
        engine,
        id,
        &["open"],
        |i| {
            i.status = Some(Some(InvoiceStatus::Paid));
            i.amount_paid = i.amount_remaining;
            i.amount_remaining = 0;
            i.attempted = true;
            i.attempt_count += 1;
            i.status_transitions.paid_at = Some(Some(now));
        },
        ChangeKind::StateChanged,
        &["invoice.paid", "invoice.payment_succeeded"],
        cause,
    )
}

/// A failed payment attempt on an open invoice (its invoice payment stays `open`).
pub(crate) fn fail_payment(engine: &mut Engine, id: &str, cause: &Cause) -> bool {
    transition(
        engine,
        id,
        &["open"],
        |i| {
            i.attempted = true;
            i.attempt_count += 1;
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

/// Deletes a draft: its lines first (unannounced), then its items (`invoiceitem.deleted`;
/// phase 9 ruling Q4: joined pending items too), then the invoice (`invoice.deleted`).
pub(crate) fn delete_draft(engine: &mut Engine, id: &str, cause: &Cause) -> bool {
    if status(engine, id).as_deref() != Some("draft") {
        return false;
    }
    let items = invoice_items(engine, id);
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
    for item in items {
        delete_item(engine, &item, cause);
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
    let tax_rate = draw_tax_rate(engine, ctx);
    let id = create_draft_invoice(
        engine,
        ctx,
        &customer,
        &lines,
        BillingReason::Manual,
        None,
        (now, now),
        true,
        tax_rate.as_ref(),
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

fn delete_item(engine: &mut Engine, id: &str, cause: &Cause) {
    engine
        .delete(
            ObjectType::InvoiceItem,
            id,
            &deleted_invoice_item(id),
            cause.clone(),
            &["invoiceitem.deleted"],
        )
        .expect("live item");
}

/// Deletes the pending items of `customer`, oldest first (`invoiceitem.deleted` each).
pub(crate) fn delete_pending_items(engine: &mut Engine, customer: &str, cause: &Cause) {
    for item in pending_items(engine, customer) {
        delete_item(engine, &item, cause);
    }
}

/// Every pending invoice item, in creation order.
fn all_pending(engine: &Engine) -> Vec<String> {
    engine
        .store()
        .live_ids(ObjectType::InvoiceItem)
        .iter()
        .filter(|id| {
            engine
                .latest(ObjectType::InvoiceItem, id)
                .is_some_and(|u| u.state["invoice"].is_null())
        })
        .cloned()
        .collect()
}

/// Active one-time prices, or every active price when there is none.
fn item_prices(engine: &Engine) -> Vec<String> {
    let active = active_ids(engine, ObjectType::Price);
    let one_time: Vec<String> = active
        .iter()
        .filter(|p| engine.latest(ObjectType::Price, p).expect("price").state["type"] == "one_time")
        .cloned()
        .collect();
    if one_time.is_empty() {
        active
    } else {
        one_time
    }
}

/// `invoice_item` (phase 9 ruling Q4): a pending invoice item (`invoice: null`) for the
/// named or a random customer, which joins that customer's next invoice. It bills the named
/// price, or a random active one-time price (any active price when there is none; a new
/// product when there is no active price), `quantity` times (drawn in 1-3 when `None`).
/// A named customer that is missing or deleted, a named price that is not an active
/// one-time price, and a quantity of 0 are errors naming them.
pub(crate) fn create_invoice_item(
    rng: &mut SimRng,
    engine: &mut Engine,
    customer: Option<String>,
    price: Option<String>,
    quantity: Option<u64>,
) -> Result<Cause, MockError> {
    if let Some(c) = &customer {
        let live = engine
            .latest(ObjectType::Customer, c)
            .is_some_and(|u| u.kind != ChangeKind::Deleted);
        if !live {
            return Err(MockError::InvalidAction(format!(
                "customer {c} does not exist or is deleted"
            )));
        }
    }
    if let Some(p) = &price {
        let usable = engine.latest(ObjectType::Price, p).is_some_and(|u| {
            u.kind != ChangeKind::Deleted
                && u.state["active"] == true
                && u.state["type"] == "one_time"
        });
        if !usable {
            return Err(MockError::InvalidAction(format!(
                "price {p} is not an active one-time price"
            )));
        }
    }
    if quantity == Some(0) {
        return Err(MockError::InvalidAction(
            "an invoice item needs a quantity of at least 1".to_string(),
        ));
    }
    // A named quantity must keep the item's amount within the maximum charge (and in range
    // of an `i64`). With a drawn price, a quantity up to the maximum keeps `unit × quantity`
    // in range (every unit amount is at most the maximum); a larger amount stays pending.
    if let Some(q) = quantity
        && q > payments::MAX_CHARGE_AMOUNT as u64
    {
        return Err(MockError::InvalidAction(format!(
            "quantity {q} exceeds the maximum charge of {}",
            payments::MAX_CHARGE_AMOUNT
        )));
    }
    if let (Some(q), Some(p)) = (quantity, &price) {
        let unit = payments::unit_amount(engine, p);
        let amount = i64::try_from(q).ok().and_then(|q| unit.checked_mul(q));
        if amount.is_none_or(|a| a > payments::MAX_CHARGE_AMOUNT) {
            return Err(MockError::InvalidAction(format!(
                "quantity {q} of price {p} exceeds the maximum charge of {}",
                payments::MAX_CHARGE_AMOUNT
            )));
        }
    }
    let cause = engine.begin_action("invoice_item");
    let customer = match customer {
        Some(c) => c,
        None => resolve_customer(rng, engine, None, &cause)?,
    };
    let id = rng.id(ObjectType::InvoiceItem.id_prefix());
    let price = match price {
        Some(p) => p,
        None => {
            let mut pool = item_prices(engine);
            if pool.is_empty() {
                let np = draw_new_product(rng);
                create_product_now(engine, &np, &cause);
                pool = item_prices(engine);
            }
            pick(rng, &pool).clone()
        }
    };
    let quantity = quantity.unwrap_or_else(|| 1 + rng.below(3));
    let price = billed_price(engine, &price);
    let now = engine.now().unix_seconds();
    let item = new_invoice_item(&NewInvoiceItem {
        id: &id,
        customer: &customer,
        invoice: None,
        price: &price,
        product_name: &product_name(engine, &price),
        quantity,
        period: (now, now),
        date: now,
    });
    engine.create_obj(
        ObjectType::InvoiceItem,
        id,
        &item,
        cause.clone(),
        &["invoiceitem.created"],
    );
    Ok(cause)
}

/// The `DeleteInvoiceItem` control hook: deletes the named item, or a random pending one.
/// Only a pending item or one on a draft can be deleted (ruling Q4); an item on a draft
/// takes its line with it, and the draft's totals follow (`invoice.updated`). A missing or
/// deleted item, an item on an invoice that is not a draft, or no pending item to pick is
/// an error naming it.
pub(crate) fn delete_invoice_item(
    rng: &mut SimRng,
    engine: &mut Engine,
    page_size: usize,
    id: Option<String>,
) -> Result<Cause, MockError> {
    let id = match id {
        Some(id) => {
            let live = engine
                .latest(ObjectType::InvoiceItem, &id)
                .is_some_and(|u| u.kind != ChangeKind::Deleted);
            if !live {
                return Err(MockError::InvalidAction(format!(
                    "invoice item {id} does not exist or is already deleted"
                )));
            }
            id
        }
        None => {
            let pending = all_pending(engine);
            if pending.is_empty() {
                return Err(MockError::InvalidAction(
                    "no pending invoice item to delete".to_string(),
                ));
            }
            pick(rng, &pending).clone()
        }
    };
    let item: Invoiceitem = engine
        .latest_obj(ObjectType::InvoiceItem, &id)
        .expect("live item");
    let invoice = item_invoice(&item).map(str::to_string);
    if let Some(inv) = &invoice
        && status(engine, inv).as_deref() != Some("draft")
    {
        return Err(MockError::InvalidAction(format!(
            "invoice item {id} is on invoice {inv}, which is not a draft"
        )));
    }
    let cause = engine.begin_action("delete_invoice_item");
    match invoice {
        None => delete_item(engine, &id, &cause),
        Some(inv) => remove_from_draft(engine, &inv, &id, page_size, &cause),
    }
    Ok(cause)
}

/// The live lines of an invoice, in creation order.
fn live_lines(engine: &Engine, invoice: &str) -> Vec<LineItem> {
    engine
        .store()
        .children(ObjectType::InvoiceLineItem, "invoice", invoice)
        .into_iter()
        .filter(|o| !o.deleted)
        .map(|o| {
            engine
                .latest_obj(ObjectType::InvoiceLineItem, &o.latest.object_id)
                .expect("listed line exists")
        })
        .collect()
}

/// Deletes `item` from the draft `invoice`: its line (unannounced), the item
/// (`invoiceitem.deleted`), then the draft's lines and totals from the lines left
/// (`invoice.updated`).
fn remove_from_draft(
    engine: &mut Engine,
    invoice: &str,
    item: &str,
    page_size: usize,
    cause: &Cause,
) {
    let line = live_lines(engine, invoice).into_iter().find(|l| {
        l.parent
            .as_ref()
            .and_then(|p| p.as_ref())
            .and_then(|p| p.invoice_item_details.as_ref())
            .and_then(|d| d.as_ref())
            .is_some_and(|d| d.invoice_item == item)
    });
    if let Some(line) = line {
        engine
            .delete(
                ObjectType::InvoiceLineItem,
                &line.id,
                &deleted_line(&line.id),
                cause.clone(),
                &[],
            )
            .expect("live line");
    }
    delete_item(engine, item, cause);
    let draft: Invoice = engine
        .latest_obj(ObjectType::Invoice, invoice)
        .expect("live draft");
    // The draft keeps the discount and tax rate it was created with, even if the discount
    // has been removed or the rate archived since; its shares follow the lines left.
    let discount = draft
        .discounts
        .first()
        .and_then(|d| applied_by_id(engine, d.id(), &draft.currency));
    let tax = draft_tax_rate(engine, &draft).map(|r| applied_tax(&r));
    let lines = live_lines(engine, invoice);
    let inputs: Vec<LineAmount> = lines
        .iter()
        .map(|l| LineAmount { amount: l.amount })
        .collect();
    let (amounts, tax) = invoice_amounts(&inputs, discount.as_ref(), tax.as_ref());
    let di = discount.as_ref().map(|d| d.id.as_str());
    for (l, la) in lines.iter().zip(&amounts.lines) {
        engine
            .update_obj::<LineItem>(
                ObjectType::InvoiceLineItem,
                &l.id,
                |l| set_line_amounts(l, la, di, tax),
                cause.clone(),
                ChangeKind::Updated,
                &[],
            )
            .expect("live line");
    }
    let lines = live_lines(engine, invoice);
    let rendered = embedded_list(&lines, page_size, &lines_url(invoice));
    engine
        .update_obj::<Invoice>(
            ObjectType::Invoice,
            invoice,
            |i| {
                i.lines = rendered;
                set_invoice_amounts(i, &amounts, di, tax);
            },
            cause.clone(),
            ChangeKind::Updated,
            &["invoice.updated"],
        )
        .expect("live draft");
}

/// The tax rate a draft was built with (ruling Q5): the snapshot in a one-off invoice's
/// own `default_tax_rates`, or in its subscription's. Never the rate's live state. Only a
/// draft that took pending items can lose one, and such a draft applies its subscription's
/// rate (only a Checkout subscription's first invoice, which takes no pending item, skips
/// it). The over-the-limit rule of [`invoice_amounts`] applies again to the lines left.
fn draft_tax_rate(engine: &Engine, draft: &Invoice) -> Option<TaxRate> {
    if let Some(rate) = draft.default_tax_rates.first() {
        return Some(rate.clone());
    }
    let sub = draft
        .parent
        .as_ref()
        .and_then(|p| p.as_ref())
        .and_then(|p| p.subscription_details.as_ref())
        .and_then(|d| d.as_ref())
        .map(|d| d.subscription.id().to_string())?;
    subscription_tax_rate(engine, &sub)
}

/// `churn.invoiceitem.delete`: deletes a random pending invoice item.
pub(crate) fn churn_delete_item(rng: &mut SimRng, engine: &mut Engine) {
    let pending = all_pending(engine);
    if pending.is_empty() {
        return;
    }
    let id = pick(rng, &pending).clone();
    let cause = engine.begin_action("invoice_item_delete");
    delete_item(engine, &id, &cause);
}
