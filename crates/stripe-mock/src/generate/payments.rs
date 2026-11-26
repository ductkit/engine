//! Payments (spec sections 5.5 and 6.1): the objects an invoice payment creates, and the
//! `one_off_payment`, `refund`, `dispute`, `save_payment_method` and payment method detach
//! actions.
//!
//! Every object draw here (IDs, card details, risk scores) comes from `ctx.payments`
//! (stream `"generate.payments"`); whether a payment succeeds, and an action's targets and
//! amounts, are decided by the caller or drawn from the action's own stream.

use crate::engine::{Cause, ChangeKind, Engine};
use crate::generate::catalog::active_ids;
use crate::generate::{Continuation, Ctx, billing, pick};
use crate::model::catalog::{random_currency, random_nonzero_amount};
use crate::model::payments::{
    BRAND, CardDraw, LAST4, NewCharge, NewDispute, NewPaymentIntent, NewRefund, decline_error,
    new_card_payment_method, new_charge, new_charge_balance_transaction, new_dispute,
    new_dispute_balance_transaction, new_invoice_payment, new_payment_intent, new_refund,
    new_refund_balance_transaction, new_setup_intent, push_refund, setup_decline_error,
    submit_evidence,
};
use crate::rng::SimRng;
use crate::stripe::{
    BalanceTransaction, Charge, Dispute, DisputeStatus, Expandable, Invoice, InvoicePayment,
    PaymentIntent, PaymentIntentCancellationReason, PaymentIntentStatus, PaymentMethod,
    SetupIntent, SetupIntentStatus,
};
use crate::{MockError, ObjectType, SimTime};

/// Chance that a generated one-off payment has no customer.
const NO_CUSTOMER_PROBABILITY: f64 = 0.2;
/// Chance that a refund with no given amount refunds everything left.
const FULL_REFUND_PROBABILITY: f64 = 0.7;

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

/// The card a payment uses when its customer has none saved: the Visa test card `4242`,
/// with a drawn expiry and fingerprint.
fn draw_card(rng: &mut SimRng, now: SimTime) -> CardDraw {
    use chrono::Datelike;
    let year = i64::from(now.to_datetime().year());
    CardDraw {
        brand: BRAND.to_string(),
        last4: LAST4.to_string(),
        exp_month: 1 + rng.below(12) as i64,
        exp_year: year + 1 + rng.below(5) as i64,
        // `id_len` with an empty prefix yields `_` plus the characters.
        fingerprint: rng.id_len("", 16)[1..].to_string(),
    }
}

/// Brands a saved card is drawn from.
const SAVED_BRANDS: [&str; 3] = ["visa", "mastercard", "amex"];

/// A card saved through a setup intent: a drawn brand (visa, mastercard or amex), four
/// random digits, an expiry 1-5 years ahead and a fingerprint.
fn draw_saved_card(rng: &mut SimRng, now: SimTime) -> CardDraw {
    let brand = pick(rng, &SAVED_BRANDS).to_string();
    let last4 = format!("{:04}", rng.below(10_000));
    let rest = draw_card(rng, now);
    CardDraw {
        brand,
        last4,
        exp_month: rest.exp_month,
        exp_year: rest.exp_year,
        fingerprint: rest.fingerprint,
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

/// What is left to refund on `charge`: `None` unless it `succeeded` with something left
/// and no chargeback blocks it (a dispute with `is_charge_refundable: false`).
fn refundable(engine: &Engine, charge: &str) -> Option<i64> {
    let c = engine.latest(ObjectType::Charge, charge)?;
    if c.state["status"] != "succeeded" {
        return None;
    }
    if c.state["disputed"] == true {
        let allowed = engine
            .store()
            .children(ObjectType::Dispute, "charge", charge)
            .last()
            .is_some_and(|d| d.latest.state["is_charge_refundable"] == true);
        if !allowed {
            return None;
        }
    }
    let amount = c.state["amount"].as_i64()?;
    let refunded = c.state["amount_refunded"].as_i64()?;
    (refunded < amount).then_some(amount - refunded)
}

/// Refunds `amount` of `charge`: the refund's balance transaction (`type: refund`,
/// `-amount`, `available` at once, no event), the refund, `succeeded`
/// (`refund.created`), then the charge with `amount_refunded`, `refunded` and the refund
/// first in `charge.refunds` (`charge.refunded`). Returns the refund, or `None` if the
/// charge has not succeeded or has less than `amount` (or nothing) left to refund.
pub(crate) fn refund(
    engine: &mut Engine,
    ctx: &mut Ctx,
    charge: &str,
    amount: i64,
    cause: &Cause,
) -> Option<String> {
    let left = refundable(engine, charge)?;
    if amount < 1 || amount > left {
        return None;
    }
    let ch: Charge = engine.latest_obj(ObjectType::Charge, charge)?;
    let created = engine.now().unix_seconds();
    let id = ctx.payments.id(ObjectType::Refund.id_prefix());
    let txn = ctx.payments.id(ObjectType::BalanceTransaction.id_prefix());
    let bt = new_refund_balance_transaction(&txn, created, amount, &ch.currency, &id);
    engine.create_obj(
        ObjectType::BalanceTransaction,
        txn.clone(),
        &bt,
        cause.clone(),
        &[],
    );
    let re = new_refund(&NewRefund {
        id: &id,
        created,
        amount,
        balance_transaction: &txn,
        charge: &ch,
    });
    engine.create_obj(
        ObjectType::Refund,
        id.clone(),
        &re,
        cause.clone(),
        &["refund.created"],
    );
    let page_size = ctx.page_size;
    engine
        .update_obj::<Charge>(
            ObjectType::Charge,
            charge,
            |c| {
                c.amount_refunded += amount;
                c.refunded = c.amount_refunded == c.amount;
                push_refund(c, re, page_size);
            },
            cause.clone(),
            ChangeKind::Updated,
            &["charge.refunded"],
        )
        .expect("live charge");
    Some(id)
}

/// The amount and currency of a one-off payment, drawn like a one-off invoice line: an
/// active price with a non-zero amount (one-time prices first) times a quantity of 1–5.
/// With no such price, a non-zero catalog amount in a random currency.
pub(super) fn draw_amount(rng: &mut SimRng, engine: &Engine) -> (i64, String) {
    let priced: Vec<(String, i64, bool)> = active_ids(engine, ObjectType::Price)
        .into_iter()
        .filter_map(|id| {
            let p = &engine.latest(ObjectType::Price, &id)?.state;
            let amount = p["unit_amount"].as_i64().filter(|a| *a > 0)?;
            let currency = p["currency"].as_str()?.to_string();
            Some((currency, amount, p["type"] == "one_time"))
        })
        .collect();
    let one_time: Vec<&(String, i64, bool)> = priced.iter().filter(|p| p.2).collect();
    let pool: Vec<&(String, i64, bool)> = if one_time.is_empty() {
        priced.iter().collect()
    } else {
        one_time
    };
    if pool.is_empty() {
        let amount = random_nonzero_amount(rng);
        return (amount, random_currency(rng).to_string());
    }
    let (currency, unit, _) = *pick(rng, &pool);
    let quantity = 1 + rng.below(5) as i64;
    (unit * quantity, currency.clone())
}

pub(super) fn live_customer(engine: &Engine, id: String) -> Result<String, MockError> {
    match engine.latest(ObjectType::Customer, &id) {
        None => Err(MockError::NoSuchObject {
            object_type: ObjectType::Customer,
            id,
        }),
        Some(u) if u.kind == ChangeKind::Deleted => Err(MockError::Deleted {
            object_type: ObjectType::Customer,
            id,
        }),
        Some(_) => Ok(id),
    }
}

/// `one_off_payment`: a payment with no invoice (see [`charge_once`]), for the named
/// customer, or a random live one, or none (probability 0.2, or when there is no live
/// customer). `amount` and `succeed` are drawn when `None`; a drawn outcome fails with
/// `actions.payment_failure_probability`. Every draw but the payment objects' comes from
/// `rng`.
pub(crate) fn one_off_payment(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<String>,
    amount: Option<i64>,
    succeed: Option<bool>,
) -> Result<Cause, MockError> {
    if let Some(a) = amount
        && a < 1
    {
        return Err(MockError::InvalidAction(format!(
            "one_off_payment amount must be at least 1, got {a}"
        )));
    }
    let customer = match customer {
        Some(id) => Some(live_customer(engine, id)?),
        None => {
            let live = engine.store().live_ids(ObjectType::Customer);
            if rng.chance(NO_CUSTOMER_PROBABILITY) || live.is_empty() {
                None
            } else {
                Some(pick(rng, live).clone())
            }
        }
    };
    let (drawn, currency) = draw_amount(rng, engine);
    let amount = amount.unwrap_or(drawn);
    let succeed = succeed.unwrap_or_else(|| !rng.chance(ctx.config.payment_failure_probability));
    let cause = engine.begin_action("one_off_payment");
    charge_once(
        engine,
        ctx,
        customer.as_deref(),
        amount,
        &currency,
        succeed,
        &cause,
    );
    Ok(cause)
}

/// `refund`: refunds the named charge, or a random `succeeded` one with something left
/// to refund. With no `amount`, everything left is refunded with probability 0.7, else a
/// uniform amount in `[1, left]`. Returns `None` when no charge was named and none is
/// refundable; a named charge that cannot be refunded, or an `amount` outside
/// `[1, left]`, is an `InvalidAction` error naming the charge.
pub(crate) fn refund_action(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    charge: Option<String>,
    amount: Option<i64>,
) -> Result<Option<Cause>, MockError> {
    let (charge, left) = match charge {
        Some(id) => {
            if engine.latest(ObjectType::Charge, &id).is_none() {
                return Err(MockError::NoSuchObject {
                    object_type: ObjectType::Charge,
                    id,
                });
            }
            let Some(left) = refundable(engine, &id) else {
                return Err(MockError::InvalidAction(format!(
                    "charge {id} cannot be refunded: it did not succeed, is fully refunded or \
                     has a chargeback"
                )));
            };
            (id, left)
        }
        None => {
            let candidates: Vec<(&String, i64)> = engine
                .store()
                .live_ids(ObjectType::Charge)
                .iter()
                .filter_map(|id| refundable(engine, id).map(|left| (id, left)))
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
                "cannot refund {a} of charge {charge}: {left} left to refund"
            )));
        }
        None if rng.chance(FULL_REFUND_PROBABILITY) => left,
        None => 1 + rng.below(left as u64) as i64,
    };
    let cause = engine.begin_action("refund");
    refund(engine, ctx, &charge, amount, &cause).expect("the charge is refundable");
    Ok(Some(cause))
}

/// How a dispute ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisputeOutcome {
    /// A chargeback won: `won`, the amount and fee reinstated.
    Won,
    /// A chargeback lost: `lost`, the funds stay withdrawn.
    Lost,
    /// An inquiry that closed without becoming a chargeback: `warning_closed`, no funds moved.
    WarningClosed,
}

/// Dispute reasons drawn for a new dispute.
const DISPUTE_REASONS: [&str; 5] = [
    "fraudulent",
    "product_not_received",
    "duplicate",
    "subscription_canceled",
    "general",
];
/// Chance of each drawn outcome: won 0.4, lost 0.5, the rest (0.1) `warning_closed`.
const WON_PROBABILITY: f64 = 0.4;
const LOST_PROBABILITY: f64 = 0.5;

const DAY_MS: i64 = 86_400_000;
/// Open to evidence submitted (`under_review`).
const REVIEW_AFTER_MS: (i64, i64) = (2 * DAY_MS, 7 * DAY_MS);
/// Evidence submitted to closed.
const CLOSE_AFTER_MS: (i64, i64) = (7 * DAY_MS, 60 * DAY_MS);

/// Whether `charge` can be disputed: it `succeeded`, has a non-zero amount, nothing
/// refunded and no dispute yet.
fn disputable(engine: &Engine, charge: &str) -> bool {
    engine.latest(ObjectType::Charge, charge).is_some_and(|c| {
        let s = &c.state;
        s["status"] == "succeeded"
            && s["amount"].as_i64().is_some_and(|a| a > 0)
            && s["amount_refunded"] == 0
            && s["disputed"] == false
    })
}

fn draw_outcome(rng: &mut SimRng) -> DisputeOutcome {
    let u = rng.unit_f64();
    if u < WON_PROBABILITY {
        DisputeOutcome::Won
    } else if u < WON_PROBABILITY + LOST_PROBABILITY {
        DisputeOutcome::Lost
    } else {
        DisputeOutcome::WarningClosed
    }
}

/// Opens a dispute on `charge`. A chargeback first withdraws the funds: its balance
/// transaction (`type: adjustment`, `-amount` plus the dispute fee, `available` at once,
/// no event). Then the dispute (`charge.dispute.created`, and
/// `charge.dispute.funds_withdrawn` for a chargeback) and the charge with `disputed: true`
/// (no event). An inquiry moves no funds.
fn open_dispute(
    engine: &mut Engine,
    ctx: &mut Ctx,
    charge: &str,
    reason: &str,
    inquiry: bool,
    cause: &Cause,
) -> String {
    let ch: Charge = engine
        .latest_obj(ObjectType::Charge, charge)
        .expect("disputable charge");
    let created = engine.now().unix_seconds();
    let id = ctx.payments.id(ObjectType::Dispute.id_prefix());
    let mut balance_transactions = Vec::new();
    if !inquiry {
        let txn = ctx.payments.id(ObjectType::BalanceTransaction.id_prefix());
        let bt = new_dispute_balance_transaction(
            &txn,
            created,
            ch.amount,
            &ch.currency,
            &id,
            charge,
            false,
        );
        engine.create_obj(ObjectType::BalanceTransaction, txn, &bt, cause.clone(), &[]);
        balance_transactions.push(bt);
    }
    let du = new_dispute(&NewDispute {
        id: &id,
        created,
        charge: &ch,
        reason,
        inquiry,
        balance_transactions,
    });
    let events: &[&str] = if inquiry {
        &["charge.dispute.created"]
    } else {
        &["charge.dispute.created", "charge.dispute.funds_withdrawn"]
    };
    engine.create_obj(ObjectType::Dispute, id.clone(), &du, cause.clone(), events);
    engine
        .update_obj::<Charge>(
            ObjectType::Charge,
            charge,
            |c| c.disputed = true,
            cause.clone(),
            ChangeKind::Updated,
            &[],
        )
        .expect("live charge");
    id
}

/// Evidence submitted: `under_review` (`warning_under_review` for an inquiry),
/// `charge.dispute.updated`.
pub(crate) fn dispute_update(engine: &mut Engine, id: &str, cause: &Cause) {
    engine
        .update_obj::<Dispute>(
            ObjectType::Dispute,
            id,
            submit_evidence,
            cause.clone(),
            ChangeKind::StateChanged,
            &["charge.dispute.updated"],
        )
        .expect("live dispute");
}

/// Closes the dispute with `outcome` (`charge.dispute.closed`). A won dispute first gets
/// its reinstating balance transaction (`+amount`, the fee returned, `available` at once,
/// no event) and announces `charge.dispute.funds_reinstated` too.
pub(crate) fn dispute_close(
    engine: &mut Engine,
    ctx: &mut Ctx,
    id: &str,
    outcome: DisputeOutcome,
    cause: &Cause,
) {
    let du: Dispute = engine
        .latest_obj(ObjectType::Dispute, id)
        .expect("live dispute");
    let reinstated = (outcome == DisputeOutcome::Won).then(|| {
        let txn = ctx.payments.id(ObjectType::BalanceTransaction.id_prefix());
        let bt = new_dispute_balance_transaction(
            &txn,
            engine.now().unix_seconds(),
            du.amount,
            &du.currency,
            id,
            du.charge.id(),
            true,
        );
        engine.create_obj(ObjectType::BalanceTransaction, txn, &bt, cause.clone(), &[]);
        bt
    });
    let (status, events): (DisputeStatus, &[&str]) = match outcome {
        DisputeOutcome::Won => (
            DisputeStatus::Won,
            &["charge.dispute.closed", "charge.dispute.funds_reinstated"],
        ),
        DisputeOutcome::Lost => (DisputeStatus::Lost, &["charge.dispute.closed"]),
        DisputeOutcome::WarningClosed => (DisputeStatus::WarningClosed, &["charge.dispute.closed"]),
    };
    engine
        .update_obj::<Dispute>(
            ObjectType::Dispute,
            id,
            |d| {
                d.status = status;
                d.balance_transactions.extend(reinstated);
            },
            cause.clone(),
            ChangeKind::StateChanged,
            events,
        )
        .expect("live dispute");
}

/// `dispute`: disputes the named charge, or a random disputable one (see
/// [`disputable`]). Every draw happens now, from `rng`: the target, the reason (uniform),
/// the outcome when `None` (won 0.4, lost 0.5, `warning_closed` 0.1), the time to
/// evidence (2–7 days) and from evidence to close (7–60 days). Both later steps are
/// queued now. Returns `None` when no charge was named and none is disputable; a named
/// charge that cannot be disputed is an `InvalidAction` error naming it.
pub(crate) fn dispute_action(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    charge: Option<String>,
    outcome: Option<DisputeOutcome>,
) -> Result<Option<Cause>, MockError> {
    let charge = match charge {
        Some(id) => {
            if engine.latest(ObjectType::Charge, &id).is_none() {
                return Err(MockError::NoSuchObject {
                    object_type: ObjectType::Charge,
                    id,
                });
            }
            if !disputable(engine, &id) {
                return Err(MockError::InvalidAction(format!(
                    "charge {id} cannot be disputed: it did not succeed, is refunded or is \
                     already disputed"
                )));
            }
            id
        }
        None => {
            let candidates: Vec<&String> = engine
                .store()
                .live_ids(ObjectType::Charge)
                .iter()
                .filter(|id| disputable(engine, id))
                .collect();
            if candidates.is_empty() {
                return Ok(None);
            }
            (*pick(rng, &candidates)).clone()
        }
    };
    let reason = *pick(rng, &DISPUTE_REASONS);
    let outcome = outcome.unwrap_or_else(|| draw_outcome(rng));
    let review_after = rng.uniform_ms(REVIEW_AFTER_MS.0, REVIEW_AFTER_MS.1);
    let close_after = rng.uniform_ms(CLOSE_AFTER_MS.0, CLOSE_AFTER_MS.1);
    let cause = engine.begin_action("dispute");
    let inquiry = outcome == DisputeOutcome::WarningClosed;
    let id = open_dispute(engine, ctx, &charge, reason, inquiry, &cause);
    let review_at = engine.now().plus_ms(review_after);
    ctx.schedule(
        review_at,
        Continuation::DisputeUpdate { id: id.clone() },
        cause.clone(),
    );
    ctx.schedule(
        review_at.plus_ms(close_after),
        Continuation::DisputeClose { id, outcome },
        cause.clone(),
    );
    Ok(Some(cause))
}

/// The named live customer, or a random live one; `None` when none was named and none is
/// live.
pub(super) fn named_or_random_customer(
    rng: &mut SimRng,
    engine: &Engine,
    named: Option<String>,
) -> Result<Option<String>, MockError> {
    match named {
        Some(id) => live_customer(engine, id).map(Some),
        None => {
            let live = engine.store().live_ids(ObjectType::Customer);
            Ok((!live.is_empty()).then(|| pick(rng, live).clone()))
        }
    }
}

/// `save_payment_method`: saves a card for the named or a random live customer through a
/// setup intent. The setup intent is created `requires_payment_method`
/// (`setup_intent.created`); then the card is created and attached
/// (`payment_method.attached`) and the setup intent `succeeded` with it
/// (`setup_intent.succeeded`). With `actions.payment_failure_probability` the card is
/// declined instead: no payment method, and the setup intent stays
/// `requires_payment_method` with `last_setup_error` (`setup_intent.setup_failed`). The
/// target and the outcome come from `rng`, the objects' draws from `ctx.payments`. Returns
/// `None` when no customer was named and none is live.
pub(crate) fn save_payment_method(
    rng: &mut SimRng,
    engine: &mut Engine,
    ctx: &mut Ctx,
    customer: Option<String>,
) -> Result<Option<Cause>, MockError> {
    let Some(customer) = named_or_random_customer(rng, engine, customer)? else {
        return Ok(None);
    };
    let succeed = !rng.chance(ctx.config.payment_failure_probability);
    let cause = engine.begin_action("save_payment_method");
    let now = engine.now();
    let id = ctx.payments.id(ObjectType::SetupIntent.id_prefix());
    let secret = format!("{id}_{}", ctx.payments.id_len("secret", 25));
    let si = new_setup_intent(&id, now.unix_seconds(), &customer, &secret);
    engine.create_obj(
        ObjectType::SetupIntent,
        id.clone(),
        &si,
        cause.clone(),
        &["setup_intent.created"],
    );
    if succeed {
        let pm = ctx.payments.id(ObjectType::PaymentMethod.id_prefix());
        let card = draw_saved_card(&mut ctx.payments, now);
        let obj = new_card_payment_method(&pm, now.unix_seconds(), Some(&customer), &card);
        engine.create_obj(
            ObjectType::PaymentMethod,
            pm.clone(),
            &obj,
            cause.clone(),
            &["payment_method.attached"],
        );
        engine
            .update_obj::<SetupIntent>(
                ObjectType::SetupIntent,
                &id,
                |s| {
                    s.status = SetupIntentStatus::Succeeded;
                    s.payment_method = Some(Some(Expandable::Id(pm)));
                },
                cause.clone(),
                ChangeKind::StateChanged,
                &["setup_intent.succeeded"],
            )
            .expect("live setup intent");
    } else {
        engine
            .update_obj::<SetupIntent>(
                ObjectType::SetupIntent,
                &id,
                |s| s.last_setup_error = Some(Some(Box::new(setup_decline_error()))),
                cause.clone(),
                ChangeKind::Updated,
                &["setup_intent.setup_failed"],
            )
            .expect("live setup intent");
    }
    Ok(Some(cause))
}

/// Whether the payment method `id` is attached to a customer.
fn attached(engine: &Engine, id: &str) -> bool {
    engine
        .latest(ObjectType::PaymentMethod, id)
        .is_some_and(|u| u.state["customer"].is_string())
}

/// `churn.payment_method.detach`: detaches the named or a random attached payment method:
/// `customer: null` (`payment_method.detached`). It stays retrievable and leaves its
/// customer's lists, and no payment uses it again. Returns `None` when none was named and
/// none is attached; a named one that is unknown or already detached is an error.
pub(crate) fn detach_action(
    rng: &mut SimRng,
    engine: &mut Engine,
    id: Option<String>,
) -> Result<Option<Cause>, MockError> {
    let id = match id {
        Some(id) => {
            if engine.latest(ObjectType::PaymentMethod, &id).is_none() {
                return Err(MockError::NoSuchObject {
                    object_type: ObjectType::PaymentMethod,
                    id,
                });
            }
            if !attached(engine, &id) {
                return Err(MockError::InvalidAction(format!(
                    "payment method {id} is not attached to a customer"
                )));
            }
            id
        }
        None => {
            let candidates: Vec<&String> = engine
                .store()
                .live_ids(ObjectType::PaymentMethod)
                .iter()
                .filter(|id| attached(engine, id))
                .collect();
            if candidates.is_empty() {
                return Ok(None);
            }
            (*pick(rng, &candidates)).clone()
        }
    };
    let cause = engine.begin_action("payment_method_detach");
    engine
        .update_obj::<PaymentMethod>(
            ObjectType::PaymentMethod,
            &id,
            |p| p.customer = Some(None),
            cause.clone(),
            ChangeKind::Updated,
            &["payment_method.detached"],
        )
        .expect("live payment method");
    Ok(Some(cause))
}
