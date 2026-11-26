//! Payments (spec sections 5.5 and 6.1): the objects an invoice payment creates.
//!
//! Every draw here (IDs, card details, risk scores) comes from `ctx.payments`
//! (stream `"generate.payments"`); whether a payment succeeds is decided by the caller.

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::{Continuation, Ctx};
use crate::model::payments::{
    AVAILABLE_AFTER_SECS, CardDraw, NewCharge, NewPaymentIntent, decline_error,
    new_card_payment_method, new_charge, new_charge_balance_transaction, new_invoice_payment,
    new_payment_intent,
};
use crate::rng::SimRng;
use crate::stripe::{
    BalanceTransaction, Expandable, Invoice, PaymentIntent, PaymentIntentCancellationReason,
    PaymentIntentStatus, PaymentMethod,
};
use crate::{ObjectType, SimTime};

/// The objects one payment attempt created or used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaymentOutcome {
    pub payment_intent: String,
    pub charge: String,
    /// The charge's balance transaction, for a succeeded charge.
    pub balance_transaction: Option<String>,
    /// The invoice payment, for a paid invoice.
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
#[allow(clippy::too_many_arguments)] // each argument is part of the intent
fn create_payment_intent(
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<&str>,
    payment_method: &str,
    amount: i64,
    currency: &str,
    invoice: Option<&str>,
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
        invoice,
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

/// One confirmation of the payment intent `pi`: a new charge, then the intent's new status.
///
/// Success: the `pending` balance transaction (no event, available after two days), the
/// `succeeded` charge (`charge.succeeded`), and the intent `succeeded`
/// (`payment_intent.succeeded`). Failure: a `failed` charge (`charge.failed`) and the
/// intent back to `requires_payment_method` with `last_payment_error`
/// (`payment_intent.payment_failed`).
fn confirm(
    engine: &mut Engine,
    ctx: &mut Ctx,
    pi: &str,
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
    let pm_id = intent
        .payment_method
        .clone()
        .flatten()
        .map(|p| p.id().to_string())
        .expect("the intent has a payment method");
    let pm: PaymentMethod = engine
        .latest_obj(ObjectType::PaymentMethod, &pm_id)
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
        ctx.schedule(
            now.plus_ms(AVAILABLE_AFTER_SECS * 1000),
            Continuation::BalanceAvailable { id: id.clone() },
            cause.clone(),
        );
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
                } else {
                    p.status = PaymentIntentStatus::RequiresPaymentMethod;
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
    let pi = create_payment_intent(engine, ctx, customer, &pm, amount, currency, None, cause);
    confirm(engine, ctx, &pi, succeed, cause)
}

/// The payment intent of an invoice: the newest of its customer's intents whose
/// `payment_details.order_reference` is the invoice.
pub(crate) fn invoice_intent(engine: &Engine, invoice: &str) -> Option<String> {
    let customer = engine.latest(ObjectType::Invoice, invoice)?.state["customer"]
        .as_str()?
        .to_string();
    engine
        .store()
        .children(ObjectType::PaymentIntent, "customer", &customer)
        .into_iter()
        .rev()
        .find(|o| o.latest.state["payment_details"]["order_reference"] == invoice)
        .map(|o| o.latest.object_id.clone())
}

/// One payment attempt on the open invoice `invoice`. The first attempt creates the
/// payment intent (after the customer's payment method); a retry reuses it. On success,
/// the invoice payment (`invoice_payment.paid`) and the paid invoice (`invoice.paid`,
/// `invoice.payment_succeeded`, with the payment appended to `invoice.payments`); on
/// failure, `invoice.payment_failed`.
pub(crate) fn pay_invoice(
    engine: &mut Engine,
    ctx: &mut Ctx,
    invoice: &str,
    succeed: bool,
    cause: &Cause,
) -> PaymentOutcome {
    let inv: Invoice = engine
        .latest_obj(ObjectType::Invoice, invoice)
        .expect("open invoice exists");
    let customer = inv.customer.id().to_string();
    let amount = inv.amount_remaining;
    let pi = match invoice_intent(engine, invoice) {
        Some(pi) => pi,
        None => {
            let pm = ensure_payment_method(engine, ctx, &customer, cause);
            create_payment_intent(
                engine,
                ctx,
                Some(&customer),
                &pm,
                amount,
                &inv.currency,
                Some(invoice),
                cause,
            )
        }
    };
    let mut outcome = confirm(engine, ctx, &pi, succeed, cause);
    if succeed {
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
            &["invoice_payment.paid"],
        );
        crate::generate::billing::pay(engine, invoice, &ip, ctx.page_size, cause);
        outcome.invoice_payment = Some(id);
    } else {
        crate::generate::billing::fail_payment(engine, invoice, cause);
    }
    outcome
}

/// Cancels the invoice's payment intent after the invoice was voided or marked
/// uncollectible (`payment_intent.canceled`, `cancellation_reason: void_invoice`). Does
/// nothing unless the intent is waiting for a payment method (after a failed attempt).
pub(crate) fn cancel_invoice_intent(engine: &mut Engine, invoice: &str, cause: &Cause) {
    let Some(pi) = invoice_intent(engine, invoice) else {
        return;
    };
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
