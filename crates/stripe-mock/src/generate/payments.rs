//! Payments (spec sections 5.5 and 6.1): the objects an invoice payment creates.
//!
//! Every draw here (IDs, card details, risk scores) comes from `ctx.payments`
//! (stream `"generate.payments"`); whether a payment succeeds is decided by the caller.

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::{Ctx, billing};
use crate::model::payments::{
    CardDraw, NewCharge, NewPaymentIntent, decline_error, new_card_payment_method, new_charge,
    new_charge_balance_transaction, new_invoice_payment, new_payment_intent,
};
use crate::rng::SimRng;
use crate::stripe::{
    BalanceTransaction, Expandable, Invoice, InvoicePayment, PaymentIntent,
    PaymentIntentCancellationReason, PaymentIntentStatus, PaymentMethod,
};
use crate::{ObjectType, SimTime};

/// The objects one payment attempt created or used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaymentOutcome {
    pub payment_intent: String,
    pub charge: String,
    /// The charge's balance transaction, for a succeeded charge.
    pub balance_transaction: Option<String>,
    /// The invoice payment, for an invoice's payment.
    pub invoice_payment: Option<String>,
}

fn draw_card(rng: &mut SimRng, now: SimTime) -> CardDraw {
    use chrono::Datelike;
    let year = i64::from(now.to_datetime().year());
    CardDraw {
        exp_month: 1 + rng.below(12) as i64,
        exp_year: year + 1 + rng.below(5) as i64,
        // `id_len` with an empty prefix yields `_` plus the characters.
        fingerprint: rng.id_len("", 16)[1..].to_string(),
    }
}

/// Creates a card payment method, attached to `customer` when given (announced as
/// `payment_method.attached`; an unattached one has no event).
fn create_payment_method(
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<&str>,
    cause: &Cause,
) -> String {
    let id = ctx.payments.id(ObjectType::PaymentMethod.id_prefix());
    let card = draw_card(&mut ctx.payments, engine.now());
    let pm = new_card_payment_method(&id, engine.now().unix_seconds(), customer, &card);
    let events: &[&str] = if customer.is_some() {
        &["payment_method.attached"]
    } else {
        &[]
    };
    engine.create_obj(
        ObjectType::PaymentMethod,
        id.clone(),
        &pm,
        cause.clone(),
        events,
    );
    id
}

/// The customer's newest attached card payment method, or a new one created and attached
/// (`payment_method.attached`).
pub(crate) fn ensure_payment_method(
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: &str,
    cause: &Cause,
) -> String {
    let newest = engine
        .store()
        .children(ObjectType::PaymentMethod, "customer", customer)
        .into_iter()
        .rev()
        .filter(|o| !o.deleted)
        .find(|o| o.latest.state["type"] == "card" && o.latest.state["customer"] == customer)
        .map(|o| o.latest.object_id.clone());
    match newest {
        Some(id) => id,
        None => create_payment_method(engine, ctx, Some(customer), cause),
    }
}

/// Creates a payment intent (`payment_intent.created`), `requires_confirmation`.
fn create_payment_intent(
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<&str>,
    payment_method: &str,
    amount: i64,
    currency: &str,
    cause: &Cause,
) -> String {
    let id = ctx.payments.id(ObjectType::PaymentIntent.id_prefix());
    let secret = format!("{id}_{}", ctx.payments.id_len("secret", 25));
    let pi = new_payment_intent(&NewPaymentIntent {
        id: &id,
        created: engine.now().unix_seconds(),
        amount,
        currency,
        customer,
        payment_method,
        client_secret: &secret,
    });
    engine.create_obj(
        ObjectType::PaymentIntent,
        id.clone(),
        &pi,
        cause.clone(),
        &["payment_intent.created"],
    );
    id
}

/// One confirmation of the payment intent `pi` with the payment method `pm_id`: a new
/// charge, then the intent's new status.
///
/// Success: the `pending` balance transaction (no event, available after two days), the
/// `succeeded` charge (`charge.succeeded`), and the intent `succeeded`
/// (`payment_intent.succeeded`). Failure: a `failed` charge (`charge.failed`) and the
/// intent back to `requires_payment_method`, its `payment_method` cleared and the card
/// named in `last_payment_error` (`payment_intent.payment_failed`).
fn confirm(
    engine: &mut Engine,
    ctx: &mut Ctx,
    pi: &str,
    pm_id: &str,
    succeed: bool,
    cause: &Cause,
) -> PaymentOutcome {
    let intent: PaymentIntent = engine
        .latest_obj(ObjectType::PaymentIntent, pi)
        .expect("payment intent exists");
    let amount = intent.amount.unwrap_or(0);
    let currency = intent.currency.clone().unwrap_or_default();
    let customer = intent
        .customer
        .clone()
        .flatten()
        .map(|c| c.id().to_string());
    let pm: PaymentMethod = engine
        .latest_obj(ObjectType::PaymentMethod, pm_id)
        .expect("payment method exists");
    let now = engine.now();
    let created = now.unix_seconds();
    let charge = ctx.payments.id(ObjectType::Charge.id_prefix());
    let risk_score = ctx.payments.below(65) as i64;
    let network_transaction_id = format!("{:015}", ctx.payments.below(1_000_000_000_000_000));
    let balance_transaction = succeed.then(|| {
        let id = ctx.payments.id(ObjectType::BalanceTransaction.id_prefix());
        let txn = new_charge_balance_transaction(&id, created, amount, &currency, &charge);
        engine.create_obj(
            ObjectType::BalanceTransaction,
            id.clone(),
            &txn,
            cause.clone(),
            &[],
        );
        ctx.schedule_settlement(SimTime(txn.available_on * 1000), id.clone(), cause.clone());
        id
    });
    let ch = new_charge(&NewCharge {
        id: &charge,
        created,
        amount,
        currency: &currency,
        customer: customer.as_deref(),
        payment_intent: pi,
        payment_method: &pm,
        succeeded: succeed,
        balance_transaction: balance_transaction.as_deref(),
        risk_score,
        network_transaction_id: &network_transaction_id,
    });
    let charge_event = if succeed {
        "charge.succeeded"
    } else {
        "charge.failed"
    };
    engine.create_obj(
        ObjectType::Charge,
        charge.clone(),
        &ch,
        cause.clone(),
        &[charge_event],
    );
    let latest = Some(Some(Expandable::Id(charge.clone())));
    let error = (!succeed).then(|| Box::new(decline_error(&charge, &pm)));
    let event = if succeed {
        "payment_intent.succeeded"
    } else {
        "payment_intent.payment_failed"
    };
    engine
        .update_obj(
            ObjectType::PaymentIntent,
            pi,
            |p: &mut PaymentIntent| {
                p.latest_charge = latest;
                if succeed {
                    p.status = PaymentIntentStatus::Succeeded;
                    p.amount_received = Some(amount);
                    p.payment_method = Some(Some(Expandable::Id(pm_id.to_string())));
                } else {
                    // As in Stripe: a declined card leaves the intent without a payment
                    // method; the card is named in `last_payment_error`.
                    p.status = PaymentIntentStatus::RequiresPaymentMethod;
                    p.payment_method = Some(None);
                }
                // A success clears the error of an earlier failed attempt.
                p.last_payment_error = Some(error);
            },
            cause.clone(),
            ChangeKind::StateChanged,
            &[event],
        )
        .expect("live payment intent");
    PaymentOutcome {
        payment_intent: pi.to_string(),
        charge,
        balance_transaction,
        invoice_payment: None,
    }
}

/// The shared core of a payment: the payment method (the customer's card, or a new
/// unattached one without a customer), a new payment intent, and one confirmation.
#[allow(clippy::too_many_arguments)] // each argument is part of the payment
#[allow(dead_code)] // used by `one_off_payment` (phase 7, task 10)
pub(crate) fn charge_once(
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<&str>,
    amount: i64,
    currency: &str,
    succeed: bool,
    cause: &Cause,
) -> PaymentOutcome {
    let pm = match customer {
        Some(c) => ensure_payment_method(engine, ctx, c, cause),
        None => create_payment_method(engine, ctx, None, cause),
    };
    let pi = create_payment_intent(engine, ctx, customer, &pm, amount, currency, cause);
    confirm(engine, ctx, &pi, &pm, succeed, cause)
}

/// The invoice's `open` invoice payment (left by a failed attempt), if any.
pub(crate) fn open_invoice_payment(engine: &Engine, invoice: &str) -> Option<InvoicePayment> {
    let id = engine
        .store()
        .children(ObjectType::InvoicePayment, "invoice", invoice)
        .into_iter()
        .rev()
        .find(|o| o.latest.state["status"] == "open")
        .map(|o| o.latest.object_id.clone())?;
    engine.latest_obj(ObjectType::InvoicePayment, &id)
}

fn intent_of(ip: &InvoicePayment) -> String {
    ip.payment
        .payment_intent
        .as_ref()
        .map(|p| p.id().to_string())
        .expect("invoice payments pay through a payment intent")
}

/// One payment attempt on the open invoice `invoice`; `None` for a zero-amount invoice,
/// which is paid directly with no payment objects.
///
/// The first attempt creates the payment intent (after the customer's payment method)
/// and the `open` invoice payment linked to it (no event). A retry finds the intent
/// through that open invoice payment and pays with the customer's card again. On
/// success, the invoice payment becomes `paid` (`invoice_payment.paid`), then the paid
/// invoice (`invoice.paid`, `invoice.payment_succeeded`); on failure,
/// `invoice.payment_failed`. Either invoice mutation carries the invoice payment in
/// `invoice.payments`.
pub(crate) fn pay_invoice(
    engine: &mut Engine,
    ctx: &mut Ctx,
    invoice: &str,
    succeed: bool,
    cause: &Cause,
) -> Option<PaymentOutcome> {
    let inv: Invoice = engine
        .latest_obj(ObjectType::Invoice, invoice)
        .expect("open invoice exists");
    let amount = inv.amount_remaining;
    if amount == 0 {
        billing::pay(engine, invoice, None, ctx.page_size, cause);
        return None;
    }
    let customer = inv.customer.id().to_string();
    let pm = ensure_payment_method(engine, ctx, &customer, cause);
    let (pi, ip_id) = match open_invoice_payment(engine, invoice) {
        Some(ip) => (intent_of(&ip), ip.id),
        None => {
            let pi = create_payment_intent(
                engine,
                ctx,
                Some(&customer),
                &pm,
                amount,
                &inv.currency,
                cause,
            );
            let id = ctx.payments.id(ObjectType::InvoicePayment.id_prefix());
            let ip = new_invoice_payment(
                &id,
                engine.now().unix_seconds(),
                invoice,
                amount,
                &inv.currency,
                &pi,
            );
            engine.create_obj(
                ObjectType::InvoicePayment,
                id.clone(),
                &ip,
                cause.clone(),
                &[],
            );
            (pi, id)
        }
    };
    let mut outcome = confirm(engine, ctx, &pi, &pm, succeed, cause);
    if succeed {
        let now = engine.now().unix_seconds();
        engine
            .update_obj::<InvoicePayment>(
                ObjectType::InvoicePayment,
                &ip_id,
                |p| {
                    p.status = "paid".to_string();
                    p.amount_paid = Some(Some(amount));
                    p.status_transitions.paid_at = Some(Some(now));
                },
                cause.clone(),
                ChangeKind::StateChanged,
                &["invoice_payment.paid"],
            )
            .expect("live invoice payment");
        let ip: InvoicePayment = engine
            .latest_obj(ObjectType::InvoicePayment, &ip_id)
            .expect("invoice payment");
        billing::pay(engine, invoice, Some(&ip), ctx.page_size, cause);
    } else {
        let ip: InvoicePayment = engine
            .latest_obj(ObjectType::InvoicePayment, &ip_id)
            .expect("invoice payment");
        billing::fail_payment(engine, invoice, &ip, ctx.page_size, cause);
    }
    outcome.invoice_payment = Some(ip_id);
    Some(outcome)
}

/// Before an open invoice is voided or marked uncollectible: its open invoice payment
/// becomes `canceled` (no event; the pinned API has no `invoice_payment.canceled`).
/// Returns the canceled payment, for the invoice mutation to carry.
pub(crate) fn cancel_invoice_payment(
    engine: &mut Engine,
    invoice: &str,
    cause: &Cause,
) -> Option<InvoicePayment> {
    let ip = open_invoice_payment(engine, invoice)?;
    let now = engine.now().unix_seconds();
    engine
        .update_obj::<InvoicePayment>(
            ObjectType::InvoicePayment,
            &ip.id,
            |p| {
                p.status = "canceled".to_string();
                p.status_transitions.canceled_at = Some(Some(now));
            },
            cause.clone(),
            ChangeKind::StateChanged,
            &[],
        )
        .expect("live invoice payment");
    engine.latest_obj(ObjectType::InvoicePayment, &ip.id)
}

/// After the invoice was voided or marked uncollectible: cancels the payment intent of
/// its canceled invoice payment (`payment_intent.canceled`, `cancellation_reason:
/// void_invoice`) when it waits for a payment method (after a failed attempt).
pub(crate) fn cancel_intent(engine: &mut Engine, ip: &InvoicePayment, cause: &Cause) {
    let pi = intent_of(ip);
    let waiting = engine
        .latest(ObjectType::PaymentIntent, &pi)
        .is_some_and(|u| u.state["status"] == "requires_payment_method");
    if !waiting {
        return;
    }
    let now = engine.now().unix_seconds();
    engine
        .update_obj::<PaymentIntent>(
            ObjectType::PaymentIntent,
            &pi,
            |p| {
                p.status = PaymentIntentStatus::Canceled;
                p.canceled_at = Some(Some(now));
                p.cancellation_reason = Some(Some(PaymentIntentCancellationReason::VoidInvoice));
            },
            cause.clone(),
            ChangeKind::StateChanged,
            &["payment_intent.canceled"],
        )
        .expect("live payment intent");
}

/// `pending → available` at `available_on`, silently (balance transactions have no events).
pub(crate) fn make_available(engine: &mut Engine, id: &str, cause: &Cause) {
    let pending = engine
        .latest(ObjectType::BalanceTransaction, id)
        .is_some_and(|u| u.state["status"] == "pending");
    if pending {
        engine
            .update_obj::<BalanceTransaction>(
                ObjectType::BalanceTransaction,
                id,
                |t| t.status = "available".to_string(),
                cause.clone(),
                ChangeKind::StateChanged,
                &[],
            )
            .expect("live balance transaction");
    }
}
