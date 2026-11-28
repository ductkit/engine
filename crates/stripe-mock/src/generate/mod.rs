//! Action generation (spec section 6).
//!
//! Two kinds of work, run in simulated-time order:
//! - **Poisson actions**: each has a rate and its own RNG stream, which drives its
//!   timing, target choice and content. Changing one rate never changes another
//!   action's decisions.
//! - **Continuations**: follow-up steps of an action (create the prices of a new
//!   product, finalize an invoice, ...), queued at a time and run with the cause
//!   of the action that scheduled them. Their content is drawn when the action
//!   starts; only `step_delay` is drawn when they are scheduled.

mod accounts;
mod billing;
mod billing_catalog;
mod catalog;
mod checkout;
mod connect;
mod credit_notes;
mod discounts;
mod mux;
mod payments;
mod schedules;
mod subscriptions;

pub(crate) use accounts::{AccountChange, AccountsGenerator};
pub(crate) use mux::Generators;

pub(crate) use subscriptions::delete_customer_cascade;

pub use billing::InvoiceOutcome;
pub use checkout::CheckoutMode;
pub use payments::DisputeOutcome;

use std::collections::BTreeMap;

use crate::engine::{Cause, ChangeKind, Engine, Scope};
use crate::model::catalog::PriceSpec;
use crate::model::customer::{self, apply_update, random_new_customer, random_update};
use crate::model::schedule::ScheduleEndBehavior;
use crate::rng::{RngStreams, SimRng};
use crate::stripe;
use crate::{
    ActionsConfig, ApplicationFeeConfig, ChargePattern, Config, MockError, ObjectType,
    PlatformPaymentsMix, SimTime,
};

/// A business action to run now with [`crate::MockStripe::run_action`] (spec section 11,
/// "Triggers").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Create a random customer.
    NewCustomer,
    /// A random catalog change: new product, new price or an archive.
    CatalogChange,
    /// Archive a product (`prod_…`), price (`price_…`), promotion code (`promo_…`, set
    /// inactive) or tax rate (`txr_…`) now. Control-only.
    Archive { id: String },
    /// A manual invoice for the named or a random customer, walked to `outcome`
    /// (drawn when `None`). A fixed outcome is a control-only hook.
    OneOffInvoice {
        customer: Option<String>,
        outcome: Option<InvoiceOutcome>,
    },
    /// Start a subscription for the named or a random customer, on the named
    /// prices (active, recurring, one currency and interval) or random ones.
    StartSubscription {
        customer: Option<String>,
        prices: Option<Vec<String>>,
    },
    /// Cancel the named or a random `active`/`past_due` subscription, now or at
    /// period end (drawn when `None`).
    CancelSubscription {
        id: Option<String>,
        at_period_end: Option<bool>,
    },
    /// A payment with no invoice for the named customer, or a random one or none (drawn
    /// when `None`). `amount` and the outcome (`succeed`) are drawn when `None`. In the
    /// platform scope, `pattern` fixes the Connect charge pattern (drawn from
    /// `accounts.platform_payments` when `None`) and `recipient` the connected account a
    /// non-`own` pattern pays (drawn when `None`). A non-`own` pattern with no connected
    /// account, a recipient that is not connected, a recipient with pattern `own`, or
    /// either field in a connected account's scope is an error.
    OneOffPayment {
        customer: Option<String>,
        amount: Option<i64>,
        succeed: Option<bool>,
        #[serde(default)]
        pattern: Option<ChargePattern>,
        #[serde(default)]
        recipient: Option<String>,
    },
    /// Refund `amount` (drawn when `None`) of the named or a random refundable charge.
    /// A named charge that cannot be refunded is an error naming it.
    Refund {
        charge: Option<String>,
        amount: Option<i64>,
    },
    /// Dispute the named or a random disputable charge (succeeded, nothing refunded, not
    /// yet disputed), ending in `outcome` (drawn when `None`). A named charge that cannot
    /// be disputed is an error naming it.
    Dispute {
        charge: Option<String>,
        outcome: Option<DisputeOutcome>,
    },
    /// Save a card for the named or a random customer through a setup intent.
    SavePaymentMethod { customer: Option<String> },
    /// A Checkout session for the named or a random customer, in `mode` (drawn when
    /// `None`), completed or left to expire (`complete`, drawn when `None`).
    Checkout {
        customer: Option<String>,
        mode: Option<CheckoutMode>,
        complete: Option<bool>,
    },
    /// Detach the named or a random attached payment method. A named one that is not
    /// attached is an error naming it.
    DetachPaymentMethod { id: Option<String> },
    /// Add a bank account or debit card (`kind`, drawn when `None`) to the connected
    /// account whose scope this runs in. A platform scope, or an account the platform does
    /// not control, is an error naming the scope.
    CreateExternalAccount { kind: Option<ExternalAccountKind> },
    /// Delete the named or a random non-default external account of the account. With
    /// none deletable, or a named one that is the default or unknown, an error naming it.
    DeleteExternalAccount { id: Option<String> },
    /// Give the named or a random person of the account a new email and phone.
    UpdatePerson { id: Option<String> },
    /// Delete the named or a random person of the account. Deleting the last
    /// representative sets the account's `requirements`.
    DeletePerson { id: Option<String> },
    /// Refund `amount` (drawn like a refund's when `None`) of the named or a random
    /// application fee with something left, in the platform scope. A named fee with
    /// nothing left, or whose account has disconnected, is an error naming it; so is a
    /// connected account's scope.
    ApplicationFeeRefund {
        fee: Option<String>,
        amount: Option<i64>,
    },
    /// Reverse `amount` (drawn like a refund's when `None`) of the named or a random
    /// transfer with something left whose destination is still connected, in the platform
    /// scope. A named transfer with nothing left, or to a disconnected account, is an error
    /// naming it; so is a connected account's scope.
    TransferReversal {
        transfer: Option<String>,
        amount: Option<i64>,
    },
    /// Pay out every currency of the scope's available balance now, as the midnight run of
    /// `actions.payout = "daily"` does, whatever that setting. Control-only. With no
    /// balance above 0, an error naming the scope.
    Payout,
    /// A random coupon, promotion code or tax rate change (phase 9 ruling Q6): create one,
    /// deactivate or archive one, or delete a coupon.
    BillingCatalogChange,
    /// Delete the coupon `id`, deactivating its active promotion codes first. A missing or
    /// already deleted coupon is an error naming it. Control-only.
    DeleteCoupon { id: String },
    /// A pending invoice item for the named or a random customer (phase 9 ruling Q4), which
    /// joins that customer's next invoice. It bills `price` (an active one-time price; drawn
    /// when `None`) `quantity` times (drawn in 1-3 when `None`). A missing or deleted
    /// customer, a price that is not active and one-time, or a quantity of 0 is an error
    /// naming it. `price` and `quantity` are control-only hooks.
    CreateInvoiceItem {
        customer: Option<String>,
        #[serde(default)]
        price: Option<String>,
        #[serde(default)]
        quantity: Option<u64>,
    },
    /// Delete the named or a random pending invoice item. A named item that is missing,
    /// already deleted, or on an invoice that is not a draft is an error naming it; so is
    /// no ID when nothing is pending. Deleting an item on a draft takes its line with it.
    DeleteInvoiceItem { id: Option<String> },
    /// A discount (phase 9 rulings Q2 and Q7) on the named customer or subscription (drawn
    /// when neither is named), redeeming the named coupon or promotion code (drawn when
    /// neither is named). Naming both targets, or both a coupon and a code, is an error; so
    /// is a target that is missing, ended or already discounted, a coupon that is not valid,
    /// a code that is not active, or no target to draw.
    AddDiscount {
        #[serde(default)]
        customer: Option<String>,
        #[serde(default)]
        subscription: Option<String>,
        #[serde(default)]
        coupon: Option<String>,
        #[serde(default)]
        promotion_code: Option<String>,
    },
    /// Remove the named or a random live discount from its parent. A named one that is
    /// missing or already removed, or none to pick, is an error naming it.
    RemoveDiscount { id: Option<String> },
    /// A credit note (phase 9 ruling Q8) on the named or a random eligible invoice for
    /// `amount` (drawn when `None`): a pre-payment note on an `open` invoice, which lowers
    /// `amount_due`, or a post-payment note on a `paid` one, which refunds its charge. A
    /// named invoice that is missing, neither open nor paid, or has nothing left to credit,
    /// or an `amount` outside `[1, amount_remaining − 1]` (open) or `[1, left to refund]`
    /// (paid), is an error naming it; so is no invoice the amount fits.
    CreditNote {
        invoice: Option<String>,
        amount: Option<i64>,
    },
    /// Void the named or a random issued pre-payment credit note whose invoice is still
    /// open, giving the credit back. A named note that is missing, void, post-payment or
    /// on an invoice that is no longer open, or none to void, is an error naming it.
    VoidCreditNote { id: Option<String> },
    /// A subscription for the named or a random customer managed by a new subscription
    /// schedule (phase 9 ruling Q9) of 2-3 phases, ending in `end_behavior` (drawn when
    /// `None`). A named customer that does not exist or is deleted is an error naming it.
    ScheduleSubscription {
        customer: Option<String>,
        end_behavior: Option<ScheduleEndBehavior>,
    },
    /// Cancel the named or a random `active` subscription schedule, then its subscription,
    /// now. A named schedule that does not exist or is not active, or none active, is an
    /// error naming it.
    CancelSubscriptionSchedule { id: Option<String> },
}

/// The kind of an external account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAccountKind {
    BankAccount,
    Card,
}

/// Generated actions, in tie-break order. New kinds are appended, so existing
/// streams keep their draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    NewCustomer,
    CustomerUpdate,
    CustomerDelete,
    CatalogChange,
    ProductUpdate,
    ProductArchive,
    PriceUpdate,
    PriceArchive,
    OneOffInvoice,
    InvoiceUpdate,
    StartSubscription,
    CancelSubscription,
    SubscriptionUpdate,
    OneOffPayment,
    Refund,
    Dispute,
    SavePaymentMethod,
    Checkout,
    PaymentMethodDetach,
    ExternalAccountCreate,
    ExternalAccountDelete,
    PersonUpdate,
    PersonDelete,
    ApplicationFeeRefund,
    TransferReversal,
    BillingCatalogChange,
    CouponUpdate,
    InvoiceItem,
    InvoiceItemDelete,
    DiscountChange,
    CreditNote,
    ScheduleSubscription,
}

/// A follow-up step of an action.
#[derive(Debug, Clone)]
pub(crate) enum Continuation {
    /// Create these prices for a new product, then set its default price.
    CreatePrices {
        product: String,
        prices: Vec<PriceSpec>,
        set_default: bool,
    },
    SetDefaultPrice {
        product: String,
        price: String,
    },
    DeleteDraft {
        id: String,
    },
    FinalizeOneOff {
        id: String,
        outcome: InvoiceOutcome,
    },
    SettleOneOff {
        id: String,
        outcome: InvoiceOutcome,
    },
    VoidInvoice {
        id: String,
    },
    /// Create the first invoice of a new subscription.
    SubscriptionInvoice {
        sub: String,
    },
    FinalizeFirstInvoice {
        sub: String,
        invoice: String,
    },
    PayFirstInvoice {
        sub: String,
        invoice: String,
    },
    ExpireIncomplete {
        sub: String,
        invoice: String,
    },
    /// Renew at the end of the current period.
    Renew {
        sub: String,
    },
    FinalizeRenewal {
        sub: String,
        invoice: String,
    },
    PayRenewal {
        sub: String,
        invoice: String,
    },
    /// The one retry of a failed renewal payment.
    DunningRetry {
        sub: String,
        invoice: String,
    },
    /// Evidence submitted: the dispute goes under review.
    DisputeUpdate {
        id: String,
    },
    /// The dispute closes with its outcome, drawn when it opened.
    DisputeClose {
        id: String,
        outcome: DisputeOutcome,
    },
    /// The Checkout session completes: a payment, or the drawn subscription (`plan`).
    CheckoutComplete {
        id: String,
        plan: Option<subscriptions::SubscriptionDraw>,
    },
    /// The Checkout session expires, unless it completed.
    CheckoutExpire {
        id: String,
    },
    /// The transfer of a separate charge's full amount to `account`, unless it has
    /// disconnected since.
    SeparateTransfer {
        charge: String,
        account: String,
        group: String,
    },
    /// The scope's midnight payout run (`actions.payout = "daily"`); it queues the next one.
    /// Each run begins its own `payout` cause, so the queued cause is unused.
    PayoutDue,
    /// A payout goes `in_transit`, an hour after its creation.
    PayoutInTransit {
        id: String,
    },
    /// A payout arrives at its `arrival_date`: paid, or failed when `fail` was drawn at its
    /// creation.
    PayoutArrive {
        id: String,
        fail: bool,
    },
    /// A `repeating` discount ends at its `end`, unless it was removed earlier (phase 9
    /// ruling Q2).
    DiscountEnd {
        id: String,
    },
}

/// One simulated day.
const DAY_MS: i64 = 86_400_000;

/// The first tie-break key of scheduled work. The keys below it are the day numbers of the
/// midnight payout runs, which go first at their instant.
const FIRST_SEQ: u64 = 1 << 32;

/// The first midnight UTC after `t`.
fn next_midnight(t: SimTime) -> SimTime {
    SimTime((t.0.div_euclid(DAY_MS) + 1) * DAY_MS)
}

/// The queued cause of a [`Continuation::PayoutDue`], which each run replaces with its own.
fn payout_due_cause() -> Cause {
    Cause {
        action: "payout".to_string(),
        action_id: String::new(),
    }
}

#[derive(Debug)]
struct Scheduled {
    kind: ActionKind,
    /// The configured rate.
    base_rate: f64,
    /// `base_rate` × the generator's scale.
    rate: f64,
    rng: SimRng,
    next_due: Option<SimTime>,
}

/// State shared by actions and continuations.
#[derive(Debug)]
pub(crate) struct Ctx {
    pub config: ActionsConfig,
    /// `payloads.nested_list_page_size`.
    pub page_size: usize,
    queue: BTreeMap<(SimTime, u64), (Continuation, Cause)>,
    /// Balance transactions to make available, at their `available_on`. Stripe's
    /// settlement clock, not a business action: it runs while generation is off too.
    settlements: BTreeMap<(SimTime, u64), (Option<Scope>, String, Cause)>,
    next_seq: u64,
    lifecycle: SimRng,
    /// Draws made inside billing flows and continuations: invoice and line IDs,
    /// payment outcomes.
    pub billing: SimRng,
    /// Draws for payment objects (stream `"generate.payments"`): their IDs and card
    /// details. Whether a payment succeeds stays a `billing` draw.
    pub payments: SimRng,
    /// `accounts.application_fee`.
    pub application_fee: ApplicationFeeConfig,
    /// Whether a payment carries an application fee (stream `"generate.application_fee"`).
    pub fee_rng: SimRng,
    /// `accounts.platform_payments`.
    pub platform_payments: PlatformPaymentsMix,
    /// A platform payment intent's charge pattern and recipient, and a separate transfer's
    /// delay (stream `"generate.platform_payments"`).
    pub pattern_rng: SimRng,
    /// The recipient of each separate-charges payment intent that has not succeeded yet.
    pub separate_recipients: BTreeMap<String, String>,
    /// Whether a payout fails, and the scope's payout destination when it has no bank
    /// account (stream `"generate.payout"`).
    pub payout_rng: SimRng,
    /// The `ba_` ID the scope's payouts go to when it is not a controlled account, drawn at
    /// its first payout (P4).
    pub payout_destination: Option<String>,
    /// Whether a new subscription or one-off invoice takes a tax rate, and which one
    /// (stream `"generate.tax"`, phase 9 ruling Q5). Draws only while an active rate exists.
    pub tax_rng: SimRng,
    /// The subscription item IDs of each phase of each schedule (ruling Q9), drawn when the
    /// schedule is created and used when a phase begins; dropped when the schedule ends.
    pub schedule_items: BTreeMap<String, Vec<Vec<String>>>,
}

impl Ctx {
    /// Queues `c` at `at`; ties run in scheduling order.
    pub(crate) fn schedule(&mut self, at: SimTime, c: Continuation, cause: Cause) {
        self.queue.insert((at, self.next_seq), (c, cause));
        self.next_seq += 1;
    }

    /// Queues the balance transaction `id` to become available at `at`. `scope`
    /// `None` is the generator's own scope; `Some(s)` is scope `s` (`Some(None)` is
    /// the platform).
    pub(crate) fn schedule_settlement(
        &mut self,
        at: SimTime,
        scope: Option<Scope>,
        id: String,
        cause: Cause,
    ) {
        self.settlements
            .insert((at, self.next_seq), (scope, id, cause));
        self.next_seq += 1;
    }

    fn next_settlement(&self) -> Option<SimTime> {
        self.settlements.keys().next().map(|(t, _)| *t)
    }

    /// One `actions.step_delay` draw from stream `"generate.lifecycle"`.
    pub(crate) fn step_delay(&mut self) -> i64 {
        self.lifecycle.sample_ms(&self.config.step_delay)
    }

    /// Queues `c` one `step_delay` after now.
    pub(crate) fn then(&mut self, engine: &Engine, c: Continuation, cause: Cause) {
        let at = engine.now().plus_ms(self.step_delay());
        self.schedule(at, c, cause);
    }

    fn next_due(&self) -> Option<SimTime> {
        self.queue.keys().next().map(|(t, _)| *t)
    }

    /// Queues the scope's next midnight payout run after `now`. It goes before every other
    /// continuation due at that midnight, so a payout arriving (or failing) then is swept at
    /// the following one: its tie-break key is the day number, below every [`FIRST_SEQ`]
    /// key.
    fn schedule_payout_due(&mut self, now: SimTime) {
        let at = next_midnight(now);
        let day = at.0.div_euclid(DAY_MS) as u64;
        debug_assert!(day < FIRST_SEQ);
        self.queue
            .insert((at, day), (Continuation::PayoutDue, payout_due_cause()));
    }
}

/// Schedules and performs generated actions and continuations.
#[derive(Debug)]
pub(crate) struct Generator {
    actions: Vec<Scheduled>,
    ctx: Ctx,
    /// Draws for the setup of a newly connected controlled account.
    setup: SimRng,
    /// Draws for actions triggered through `run_action`.
    control: SimRng,
    enabled: bool,
    /// Multiplies every configured Poisson rate (`history.scale` during history).
    scale: f64,
    /// Draws for writes during pagination (spec section 7.4).
    pagination_rng: SimRng,
}

impl Generator {
    pub(crate) fn new(config: &Config, streams: &RngStreams, start: SimTime) -> Generator {
        Generator::new_with_prefix(config, streams, start, "")
    }

    /// Like [`new`](Self::new), with every stream name prefixed by `prefix`: a
    /// connected account's generator uses `account/{acct}/` (spec section 5.4).
    pub(crate) fn new_with_prefix(
        config: &Config,
        streams: &RngStreams,
        start: SimTime,
        prefix: &str,
    ) -> Generator {
        let stream = |name: &str| streams.stream(&format!("{prefix}{name}"));
        let (a, c) = (&config.actions, &config.churn);
        let specs = [
            (
                ActionKind::NewCustomer,
                a.new_customer,
                "generate.customer.create",
            ),
            (
                ActionKind::CustomerUpdate,
                c.customer.update,
                "generate.customer.update",
            ),
            (
                ActionKind::CustomerDelete,
                c.customer.delete,
                "generate.customer.delete",
            ),
            (
                ActionKind::CatalogChange,
                a.catalog_change,
                "generate.catalog.change",
            ),
            (
                ActionKind::ProductUpdate,
                c.product.update,
                "generate.product.update",
            ),
            (
                ActionKind::ProductArchive,
                c.product.archive,
                "generate.product.archive",
            ),
            (
                ActionKind::PriceUpdate,
                c.price.update,
                "generate.price.update",
            ),
            (
                ActionKind::PriceArchive,
                c.price.archive,
                "generate.price.archive",
            ),
            (
                ActionKind::OneOffInvoice,
                a.one_off_invoice,
                "generate.invoice.one_off",
            ),
            (
                ActionKind::InvoiceUpdate,
                c.invoice.update,
                "generate.invoice.update",
            ),
            (
                ActionKind::StartSubscription,
                a.start_subscription,
                "generate.subscription.start",
            ),
            (
                ActionKind::CancelSubscription,
                a.cancel_subscription,
                "generate.subscription.cancel",
            ),
            (
                ActionKind::SubscriptionUpdate,
                c.subscription.update,
                "generate.subscription.update",
            ),
            (
                ActionKind::OneOffPayment,
                a.one_off_payment,
                "generate.one_off_payment",
            ),
            (ActionKind::Refund, a.refund, "generate.refund"),
            (ActionKind::Dispute, a.dispute, "generate.dispute"),
            (
                ActionKind::SavePaymentMethod,
                a.save_payment_method,
                "generate.save_payment_method",
            ),
            (ActionKind::Checkout, a.checkout, "generate.checkout"),
            (
                ActionKind::PaymentMethodDetach,
                c.payment_method.detach,
                "generate.payment_method.detach",
            ),
            (
                ActionKind::ExternalAccountCreate,
                c.external_account.create,
                "generate.external_account.create",
            ),
            (
                ActionKind::ExternalAccountDelete,
                c.external_account.delete,
                "generate.external_account.delete",
            ),
            (
                ActionKind::PersonUpdate,
                c.person.update,
                "generate.person.update",
            ),
            (
                ActionKind::PersonDelete,
                c.person.delete,
                "generate.person.delete",
            ),
            (
                ActionKind::ApplicationFeeRefund,
                a.application_fee_refund,
                "generate.application_fee_refund",
            ),
            (
                ActionKind::TransferReversal,
                a.transfer_reversal,
                "generate.transfer_reversal",
            ),
            (
                ActionKind::BillingCatalogChange,
                a.billing_catalog_change,
                "generate.billing_catalog.change",
            ),
            (
                ActionKind::CouponUpdate,
                c.coupon.update,
                "generate.coupon.update",
            ),
            (
                ActionKind::InvoiceItem,
                a.invoice_item,
                "generate.invoice_item",
            ),
            (
                ActionKind::InvoiceItemDelete,
                c.invoiceitem.delete,
                "generate.invoice_item.delete",
            ),
            (
                ActionKind::DiscountChange,
                a.discount_change,
                "generate.discount_change",
            ),
            (
                ActionKind::CreditNote,
                a.credit_note,
                "generate.credit_note",
            ),
            (
                ActionKind::ScheduleSubscription,
                a.schedule_subscription,
                "generate.subscription.schedule",
            ),
        ];
        let actions = specs
            .into_iter()
            .map(|(kind, rate, stream_name)| {
                let mut rng = stream(stream_name);
                let next_due = rng.exp_interval_ms(rate).map(|ms| start.plus_ms(ms));
                Scheduled {
                    kind,
                    base_rate: rate,
                    rate,
                    rng,
                    next_due,
                }
            })
            .collect();
        let mut generator = Generator {
            actions,
            scale: 1.0,
            ctx: Ctx {
                config: config.actions.clone(),
                page_size: config.payloads.nested_list_page_size,
                queue: BTreeMap::new(),
                settlements: BTreeMap::new(),
                next_seq: FIRST_SEQ,
                lifecycle: stream("generate.lifecycle"),
                billing: stream("generate.billing"),
                payments: stream("generate.payments"),
                application_fee: config.accounts.application_fee.clone(),
                fee_rng: stream("generate.application_fee"),
                platform_payments: config.accounts.platform_payments.clone(),
                pattern_rng: stream("generate.platform_payments"),
                separate_recipients: BTreeMap::new(),
                payout_rng: stream("generate.payout"),
                payout_destination: None,
                tax_rng: stream("generate.tax"),
                schedule_items: BTreeMap::new(),
            },
            setup: stream("generate.account_setup"),
            control: stream("control.actions"),
            pagination_rng: stream("api.pagination_writes.content"),
            enabled: true,
        };
        if config.actions.payout == crate::PayoutMode::Daily {
            generator.ctx.schedule_payout_due(start);
        }
        generator
    }

    /// Runs once, in the account's scope, right after the account connects. A controlled
    /// account (Express, Custom or v2, P4) gets its bank account and persons under the
    /// cause `account_setup`; a Standard account gets nothing and draws nothing.
    pub(crate) fn on_connect(&mut self, engine: &mut Engine, kind: crate::AccountKind) {
        if kind == crate::AccountKind::Standard {
            return;
        }
        let cause = engine.begin_action("account_setup");
        connect::setup(&mut self.setup, engine, &cause, self.ctx.page_size);
    }

    /// Removes the queued settlements, in due order: `(available_on, balance
    /// transaction, cause)`, each scope resolved (`own` is this generator's scope). A
    /// disconnected account's generator hands them on so they still settle.
    pub(crate) fn take_settlements(&mut self, own: &Scope) -> Vec<(SimTime, Scope, String, Cause)> {
        std::mem::take(&mut self.ctx.settlements)
            .into_iter()
            .map(|((at, _), (scope, id, cause))| {
                (at, scope.unwrap_or_else(|| own.clone()), id, cause)
            })
            .collect()
    }

    /// The earliest due action, continuation or settlement (only settlements while
    /// disabled).
    pub(crate) fn next_due(&self) -> Option<SimTime> {
        if !self.enabled {
            return self.ctx.next_settlement();
        }
        self.actions
            .iter()
            .filter_map(|a| a.next_due)
            .chain(self.ctx.next_due())
            .chain(self.ctx.next_settlement())
            .min()
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn actions_config(&self) -> &ActionsConfig {
        &self.ctx.config
    }

    /// Sets every Poisson rate to its configured rate × `factor` (history uses
    /// `history.scale`); actions whose rate changed redraw their next due time from `now`.
    pub(crate) fn scale_rates(&mut self, factor: f64, now: SimTime) {
        self.scale = factor;
        for a in &mut self.actions {
            let rate = a.base_rate * factor;
            if rate != a.rate {
                a.rate = rate;
                a.next_due = a.rng.exp_interval_ms(rate).map(|ms| now.plus_ms(ms));
            }
        }
    }

    /// Replaces `[actions]`. Poisson actions whose rate changed redraw their next
    /// due time from `now`; everything else applies to later draws.
    /// Turning natural renewals back on queues a renewal for every live
    /// subscription that has none pending. Turning `payout` to `daily` queues the next
    /// midnight's run, and `off` drops the pending one (payouts already made still arrive).
    pub(crate) fn set_actions_config(&mut self, config: ActionsConfig, engine: &mut Engine) {
        let now = engine.now();
        let resume_renewals = self.ctx.config.renew_subscription_cycle != crate::RenewMode::Natural
            && config.renew_subscription_cycle == crate::RenewMode::Natural;
        match (self.ctx.config.payout, config.payout) {
            (crate::PayoutMode::Off, crate::PayoutMode::Daily) => {
                self.ctx.schedule_payout_due(now);
            }
            (crate::PayoutMode::Daily, crate::PayoutMode::Off) => {
                self.ctx
                    .queue
                    .retain(|_, (c, _)| !matches!(c, Continuation::PayoutDue));
            }
            _ => {}
        }
        for a in &mut self.actions {
            let rate = match a.kind {
                ActionKind::NewCustomer => config.new_customer,
                ActionKind::CatalogChange => config.catalog_change,
                ActionKind::OneOffInvoice => config.one_off_invoice,
                ActionKind::StartSubscription => config.start_subscription,
                ActionKind::CancelSubscription => config.cancel_subscription,
                ActionKind::OneOffPayment => config.one_off_payment,
                ActionKind::Refund => config.refund,
                ActionKind::Dispute => config.dispute,
                ActionKind::SavePaymentMethod => config.save_payment_method,
                ActionKind::Checkout => config.checkout,
                ActionKind::ApplicationFeeRefund => config.application_fee_refund,
                ActionKind::TransferReversal => config.transfer_reversal,
                ActionKind::BillingCatalogChange => config.billing_catalog_change,
                ActionKind::InvoiceItem => config.invoice_item,
                ActionKind::DiscountChange => config.discount_change,
                ActionKind::CreditNote => config.credit_note,
                ActionKind::ScheduleSubscription => config.schedule_subscription,
                _ => continue,
            };
            a.base_rate = rate;
            let rate = rate * self.scale;
            if rate != a.rate {
                a.rate = rate;
                a.next_due = a.rng.exp_interval_ms(rate).map(|ms| now.plus_ms(ms));
            }
        }
        self.ctx.config = config;
        if resume_renewals {
            let queued: Vec<String> = self
                .ctx
                .queue
                .values()
                .filter_map(|(c, _)| match c {
                    Continuation::Renew { sub } => Some(sub.clone()),
                    _ => None,
                })
                .collect();
            let subs: Vec<String> = engine
                .store()
                .live_ids(ObjectType::Subscription)
                .iter()
                .filter(|id| subscriptions::renewable(engine, id) && !queued.contains(id))
                .cloned()
                .collect();
            if !subs.is_empty() {
                let cause = engine.begin_action("control.resume_renewals");
                for sub in subs {
                    subscriptions::schedule_renewal(engine, &mut self.ctx, &sub, &cause);
                }
            }
        }
    }

    /// Turns generation off or on. Re-enabling redraws each Poisson action's next due
    /// time from `now`, using its own stream; continuations keep their times.
    pub(crate) fn set_enabled(&mut self, on: bool, now: SimTime) {
        if on && !self.enabled {
            for a in &mut self.actions {
                a.next_due = a.rng.exp_interval_ms(a.rate).map(|ms| now.plus_ms(ms));
            }
            // Continuations that fell due while paused run now, in queue order.
            let later = self.ctx.queue.split_off(&(now, 0));
            let overdue = std::mem::replace(&mut self.ctx.queue, later);
            for ((_, seq), entry) in overdue {
                self.ctx.queue.insert((now, seq), entry);
            }
        }
        self.enabled = on;
    }

    /// Performs every action and continuation due at or before `target`, in time
    /// order (continuations first on ties), then sets the time to `target`.
    /// Settlements run even while generation is off, and go first on ties.
    pub(crate) fn run_until(&mut self, engine: &mut Engine, target: SimTime) {
        loop {
            if let Some(at) = self.ctx.next_settlement().filter(|t| *t <= target) {
                let before_others = !self.enabled
                    || self
                        .actions
                        .iter()
                        .filter_map(|a| a.next_due)
                        .all(|d| at <= d)
                        && self.ctx.next_due().is_none_or(|c| at <= c);
                if before_others {
                    let Some(((at, _), (scope, id, cause))) = self.ctx.settlements.pop_first()
                    else {
                        unreachable!("a settlement is due");
                    };
                    engine.set_now(at);
                    match scope {
                        None => payments::make_available(engine, &id, &cause),
                        Some(s) => engine.in_scope(s, |e| payments::make_available(e, &id, &cause)),
                    }
                    continue;
                }
            }
            if !self.enabled {
                break;
            }
            let cont = self.ctx.next_due().filter(|t| *t <= target);
            // `min_by_key` keeps the first on ties, which is the fixed order.
            let action = self
                .actions
                .iter()
                .enumerate()
                .filter_map(|(i, a)| a.next_due.map(|d| (i, d)))
                .filter(|(_, d)| *d <= target)
                .min_by_key(|(_, d)| *d);
            match (cont, action) {
                (Some(c), a) if a.is_none_or(|(_, d)| c <= d) => self.run_next_continuation(engine),
                (_, Some((i, due))) => {
                    engine.set_now(due);
                    let action = &mut self.actions[i];
                    perform(action.kind, &mut action.rng, engine, &mut self.ctx);
                    action.next_due = action
                        .rng
                        .exp_interval_ms(action.rate)
                        .map(|ms| due.plus_ms(ms));
                }
                _ => break,
            }
        }
        engine.set_now(target);
    }

    fn run_next_continuation(&mut self, engine: &mut Engine) {
        let Some(((at, _), (c, cause))) = self.ctx.queue.pop_first() else {
            return;
        };
        engine.set_now(at);
        run_continuation(c, cause, engine, &mut self.ctx);
    }

    /// Runs `a` now, then every continuation due at or before now (when generation
    /// is on). Returns the action's cause.
    pub(crate) fn run_action(
        &mut self,
        engine: &mut Engine,
        a: Action,
    ) -> Result<Cause, MockError> {
        let cause = match a {
            Action::NewCustomer => new_customer(&mut self.control, engine),
            Action::CatalogChange => catalog::change(&mut self.control, engine, &mut self.ctx),
            Action::Archive { id } => catalog::archive_by_id(engine, &id)?,
            Action::BillingCatalogChange => billing_catalog::change(&mut self.control, engine),
            Action::DeleteCoupon { id } => billing_catalog::delete_coupon_by_id(engine, &id)?,
            Action::CreateInvoiceItem {
                customer,
                price,
                quantity,
            } => {
                billing::create_invoice_item(&mut self.control, engine, customer, price, quantity)?
            }
            Action::DeleteInvoiceItem { id } => {
                billing::delete_invoice_item(&mut self.control, engine, self.ctx.page_size, id)?
            }
            Action::AddDiscount {
                customer,
                subscription,
                coupon,
                promotion_code,
            } => discounts::add_discount(
                &mut self.control,
                engine,
                &mut self.ctx,
                customer,
                subscription,
                coupon,
                promotion_code,
            )?,
            Action::RemoveDiscount { id } => {
                discounts::remove_discount(&mut self.control, engine, id)?
            }
            Action::CreditNote { invoice, amount } => credit_notes::credit_note_hook(
                &mut self.control,
                engine,
                &mut self.ctx,
                invoice,
                amount,
            )?,
            Action::VoidCreditNote { id } => {
                credit_notes::void_credit_note(&mut self.control, engine, id)?
            }
            Action::ScheduleSubscription {
                customer,
                end_behavior,
            } => schedules::schedule_subscription(
                &mut self.control,
                engine,
                &mut self.ctx,
                customer,
                end_behavior,
            )?,
            Action::CancelSubscriptionSchedule { id } => {
                schedules::cancel_subscription_schedule(&mut self.control, engine, id)?
            }
            Action::OneOffInvoice { customer, outcome } => billing::one_off_invoice(
                &mut self.control,
                engine,
                &mut self.ctx,
                customer,
                outcome,
            )?,
            Action::StartSubscription { customer, prices } => subscriptions::start_subscription(
                &mut self.control,
                engine,
                &mut self.ctx,
                customer,
                prices,
            )?,
            Action::CancelSubscription { id, at_period_end } => {
                subscriptions::cancel_subscription(&mut self.control, engine, id, at_period_end)?
                    .ok_or_else(|| {
                        MockError::InvalidAction(
                            "no active or past_due subscription to cancel".to_string(),
                        )
                    })?
            }
            Action::OneOffPayment {
                customer,
                amount,
                succeed,
                pattern,
                recipient,
            } => payments::one_off_payment(
                &mut self.control,
                engine,
                &mut self.ctx,
                payments::OneOffPayment {
                    customer,
                    amount,
                    succeed,
                    pattern,
                    recipient,
                },
            )?,
            Action::Refund { charge, amount } => {
                payments::refund_action(&mut self.control, engine, &mut self.ctx, charge, amount)?
                    .ok_or_else(|| {
                    MockError::InvalidAction("no succeeded charge to refund".to_string())
                })?
            }
            Action::Dispute { charge, outcome } => {
                payments::dispute_action(&mut self.control, engine, &mut self.ctx, charge, outcome)?
                    .ok_or_else(|| {
                        MockError::InvalidAction(
                            "no succeeded, unrefunded, undisputed charge to dispute".to_string(),
                        )
                    })?
            }
            Action::SavePaymentMethod { customer } => {
                payments::save_payment_method(&mut self.control, engine, &mut self.ctx, customer)?
                    .ok_or_else(|| {
                    MockError::InvalidAction(
                        "no live customer to save a payment method for".to_string(),
                    )
                })?
            }
            Action::Checkout {
                customer,
                mode,
                complete,
            } => checkout::checkout_action(
                &mut self.control,
                engine,
                &mut self.ctx,
                customer,
                mode,
                complete,
            )?
            .ok_or_else(|| MockError::InvalidAction("no live customer to check out".to_string()))?,
            Action::DetachPaymentMethod { id } => {
                payments::detach_action(&mut self.control, engine, id)?.ok_or_else(|| {
                    MockError::InvalidAction("no attached payment method to detach".to_string())
                })?
            }
            Action::CreateExternalAccount { kind } => connect::create_external_account(
                &mut self.control,
                engine,
                self.ctx.page_size,
                kind,
            )?,
            Action::DeleteExternalAccount { id } => {
                connect::delete_external_account(&mut self.control, engine, self.ctx.page_size, id)?
                    .ok_or_else(|| {
                        MockError::InvalidAction(format!(
                            "no non-default external account to delete in {}",
                            connect::scope_name(engine)
                        ))
                    })?
            }
            Action::UpdatePerson { id } => connect::update_person(&mut self.control, engine, id)?
                .ok_or_else(|| {
                MockError::InvalidAction(format!(
                    "no person to update in {}",
                    connect::scope_name(engine)
                ))
            })?,
            Action::DeletePerson { id } => connect::delete_person(&mut self.control, engine, id)?
                .ok_or_else(|| {
                MockError::InvalidAction(format!(
                    "no person to delete in {}",
                    connect::scope_name(engine)
                ))
            })?,
            Action::ApplicationFeeRefund { fee, amount } => connect::application_fee_refund(
                &mut self.control,
                engine,
                &mut self.ctx,
                fee,
                amount,
            )?
            .ok_or_else(|| {
                MockError::InvalidAction(
                    "no application fee with something left to refund".to_string(),
                )
            })?,
            Action::TransferReversal { transfer, amount } => connect::transfer_reversal(
                &mut self.control,
                engine,
                &mut self.ctx,
                transfer,
                amount,
            )?
            .ok_or_else(|| {
                MockError::InvalidAction("no transfer with something left to reverse".to_string())
            })?,
            Action::Payout => connect::pay_out(Some(&mut self.control), engine, &mut self.ctx)
                .ok_or_else(|| {
                    MockError::InvalidAction(format!(
                        "no available balance above 0 to pay out in {}",
                        connect::scope_name(engine)
                    ))
                })?,
        };
        self.run_due_now(engine);
        Ok(cause)
    }

    /// Runs every continuation due at or before now (when generation is on).
    fn run_due_now(&mut self, engine: &mut Engine) {
        if self.enabled {
            let now = engine.now();
            while self.ctx.next_due().is_some_and(|t| t <= now) {
                self.run_next_continuation(engine);
            }
        }
    }

    /// A write during pagination: one random action for the listed type `t`, then
    /// any continuations due now. Does nothing while generation is off or for
    /// sub-list types.
    pub(crate) fn pagination_write(&mut self, engine: &mut Engine, t: ObjectType) {
        if !self.enabled {
            return;
        }
        let kinds: &[ActionKind] = match t {
            ObjectType::Customer => &[
                ActionKind::NewCustomer,
                ActionKind::CustomerUpdate,
                ActionKind::CustomerDelete,
            ],
            ObjectType::Product => &[
                ActionKind::CatalogChange,
                ActionKind::ProductUpdate,
                ActionKind::ProductArchive,
            ],
            ObjectType::Price => &[
                ActionKind::CatalogChange,
                ActionKind::PriceUpdate,
                ActionKind::PriceArchive,
            ],
            ObjectType::Subscription => &[
                ActionKind::StartSubscription,
                ActionKind::SubscriptionUpdate,
                ActionKind::CancelSubscription,
            ],
            ObjectType::Invoice => &[ActionKind::OneOffInvoice, ActionKind::InvoiceUpdate],
            ObjectType::PaymentIntent | ObjectType::Charge | ObjectType::BalanceTransaction => {
                &[ActionKind::OneOffPayment, ActionKind::Refund]
            }
            ObjectType::Refund => &[ActionKind::Refund],
            ObjectType::InvoicePayment => &[ActionKind::OneOffInvoice],
            ObjectType::Dispute => &[ActionKind::Dispute],
            ObjectType::PaymentMethod => &[
                ActionKind::SavePaymentMethod,
                ActionKind::PaymentMethodDetach,
            ],
            ObjectType::SetupIntent => &[ActionKind::SavePaymentMethod],
            ObjectType::CheckoutSession => &[ActionKind::Checkout],
            ObjectType::SubscriptionItem | ObjectType::InvoiceLineItem | ObjectType::Account => {
                return;
            }
            // External accounts and persons exist only in accounts the platform controls.
            ObjectType::BankAccount | ObjectType::Card if connect::controlled(engine) => {
                &[ActionKind::ExternalAccountCreate]
            }
            ObjectType::Person if connect::controlled(engine) => &[ActionKind::PersonUpdate],
            ObjectType::ApplicationFee | ObjectType::FeeRefund => {
                &[ActionKind::ApplicationFeeRefund]
            }
            ObjectType::Transfer | ObjectType::TransferReversal => &[ActionKind::TransferReversal],
            // The write kinds for these arrive with their tasks, so the arms return early
            // until then. A payout has no write.
            ObjectType::Payout
            | ObjectType::BankAccount
            | ObjectType::Card
            | ObjectType::Person => return,
            // Phase 9 ruling Q10: the matching Q6 creation, drawn from the pagination stream.
            ObjectType::Coupon | ObjectType::PromotionCode | ObjectType::TaxRate => {
                billing_catalog::pagination_create(&mut self.pagination_rng, engine, t);
                self.run_due_now(engine);
                return;
            }
            // Phase 9 ruling Q10: a new pending item, drawn from the pagination stream.
            ObjectType::InvoiceItem => {
                billing::create_invoice_item(&mut self.pagination_rng, engine, None, None, None)
                    .expect("random targets are valid");
                self.run_due_now(engine);
                return;
            }
            // Phase 9 ruling Q10: a `metadata.note` update, drawn from the pagination stream.
            ObjectType::CreditNote => {
                credit_notes::pagination_update(&mut self.pagination_rng, engine);
                self.run_due_now(engine);
                return;
            }
            // Phase 9 ruling Q10: a `metadata.note` update, drawn from the pagination stream.
            ObjectType::SubscriptionSchedule => {
                schedules::pagination_update(&mut self.pagination_rng, engine);
                self.run_due_now(engine);
                return;
            }
            // Not listable on their own.
            ObjectType::Discount | ObjectType::CreditNoteLineItem => return,
        };
        let kind = *pick(&mut self.pagination_rng, kinds);
        perform(kind, &mut self.pagination_rng, engine, &mut self.ctx);
        self.run_due_now(engine);
    }
}

fn run_continuation(c: Continuation, cause: Cause, engine: &mut Engine, ctx: &mut Ctx) {
    match c {
        Continuation::CreatePrices {
            product,
            prices,
            set_default,
        } => {
            catalog::create_prices(engine, &product, &prices, &cause);
            if set_default && let Some(first) = prices.first() {
                ctx.then(
                    engine,
                    Continuation::SetDefaultPrice {
                        product,
                        price: first.id.clone(),
                    },
                    cause,
                );
            }
        }
        Continuation::SetDefaultPrice { product, price } => {
            catalog::set_default_price(engine, &product, &price, &cause);
        }
        c @ (Continuation::SubscriptionInvoice { .. }
        | Continuation::FinalizeFirstInvoice { .. }
        | Continuation::PayFirstInvoice { .. }
        | Continuation::ExpireIncomplete { .. }
        | Continuation::Renew { .. }
        | Continuation::FinalizeRenewal { .. }
        | Continuation::PayRenewal { .. }
        | Continuation::DunningRetry { .. }) => subscriptions::run(c, cause, engine, ctx),
        Continuation::DisputeUpdate { id } => payments::dispute_update(engine, &id, &cause),
        Continuation::DisputeClose { id, outcome } => {
            payments::dispute_close(engine, ctx, &id, outcome, &cause);
        }
        Continuation::CheckoutComplete { id, plan } => {
            checkout::run_complete(engine, ctx, &id, plan, &cause);
        }
        Continuation::CheckoutExpire { id } => checkout::run_expire(engine, &id, &cause),
        Continuation::SeparateTransfer {
            charge,
            account,
            group,
        } => connect::separate_transfer(engine, ctx, &charge, &account, &group, &cause),
        Continuation::PayoutDue => {
            connect::pay_out(None, engine, ctx);
            ctx.schedule_payout_due(engine.now());
        }
        Continuation::PayoutInTransit { id } => connect::payout_in_transit(engine, &id, &cause),
        Continuation::PayoutArrive { id, fail } => {
            connect::payout_arrive(engine, ctx, &id, fail, &cause);
        }
        Continuation::DiscountEnd { id } => discounts::run_end(engine, &id, &cause),
        c => billing::run(c, cause, engine, ctx),
    }
}

/// A random element of a non-empty slice.
pub(crate) fn pick<'a, T>(rng: &mut SimRng, items: &'a [T]) -> &'a T {
    &items[rng.below(items.len() as u64) as usize]
}

/// Creates a random customer now as a `new_customer` action.
fn new_customer(rng: &mut SimRng, engine: &mut Engine) -> Cause {
    let cause = engine.begin_action("new_customer");
    create_random_customer(rng, engine, &cause);
    cause
}

/// Creates a random customer under `cause`; returns its ID.
pub(crate) fn create_random_customer(
    rng: &mut SimRng,
    engine: &mut Engine,
    cause: &Cause,
) -> String {
    let t = ObjectType::Customer;
    let id = rng.id(t.id_prefix());
    let prefix = rng.alnum_upper(8);
    let params = random_new_customer(rng);
    let customer = customer::new_customer(&id, engine.now().unix_seconds(), &prefix, &params);
    engine.create_obj(
        t,
        id.clone(),
        &customer,
        cause.clone(),
        &["customer.created"],
    );
    id
}

fn perform(kind: ActionKind, rng: &mut SimRng, engine: &mut Engine, ctx: &mut Ctx) {
    let t = ObjectType::Customer;
    match kind {
        ActionKind::NewCustomer => {
            new_customer(rng, engine);
        }
        ActionKind::CustomerUpdate => {
            let Some(id) = pick_live(rng, engine) else {
                return;
            };
            let customer: stripe::Customer = engine.latest_obj(t, &id).expect("live");
            let update = random_update(rng, &customer);
            let cause = engine.begin_action("customer_update");
            engine
                .update_obj::<stripe::Customer>(
                    t,
                    &id,
                    |c| apply_update(c, &update),
                    cause,
                    ChangeKind::Updated,
                    &["customer.updated"],
                )
                .expect("live customer");
        }
        ActionKind::CustomerDelete => {
            let Some(id) = pick_live(rng, engine) else {
                return;
            };
            let cause = engine.begin_action("customer_delete");
            subscriptions::delete_customer_cascade(engine, &id, &cause).expect("live customer");
        }
        ActionKind::CatalogChange => {
            catalog::change(rng, engine, ctx);
        }
        ActionKind::ProductUpdate => catalog::churn_update(rng, engine, ObjectType::Product),
        ActionKind::ProductArchive => catalog::churn_archive(rng, engine, ObjectType::Product),
        ActionKind::PriceUpdate => catalog::churn_update(rng, engine, ObjectType::Price),
        ActionKind::PriceArchive => catalog::churn_archive(rng, engine, ObjectType::Price),
        ActionKind::OneOffInvoice => {
            billing::one_off_invoice(rng, engine, ctx, None, None)
                .expect("random targets are valid");
        }
        ActionKind::InvoiceUpdate => billing::churn_update(rng, engine),
        ActionKind::StartSubscription => {
            subscriptions::start_subscription(rng, engine, ctx, None, None)
                .expect("random targets are valid");
        }
        ActionKind::CancelSubscription => {
            subscriptions::cancel_subscription(rng, engine, None, None)
                .expect("random targets are valid");
        }
        ActionKind::SubscriptionUpdate => subscriptions::churn_update(rng, engine),
        ActionKind::OneOffPayment => {
            payments::one_off_payment(rng, engine, ctx, payments::OneOffPayment::default())
                .expect("random targets are valid");
        }
        ActionKind::Refund => {
            payments::refund_action(rng, engine, ctx, None, None)
                .expect("random targets are valid");
        }
        ActionKind::Dispute => {
            payments::dispute_action(rng, engine, ctx, None, None)
                .expect("random targets are valid");
        }
        ActionKind::SavePaymentMethod => {
            payments::save_payment_method(rng, engine, ctx, None)
                .expect("random targets are valid");
        }
        ActionKind::Checkout => {
            checkout::checkout_action(rng, engine, ctx, None, None, None)
                .expect("random targets are valid");
        }
        ActionKind::PaymentMethodDetach => {
            payments::detach_action(rng, engine, None).expect("random targets are valid");
        }
        // Only the accounts the platform controls have these.
        ActionKind::ExternalAccountCreate => {
            if connect::controlled(engine) {
                connect::create_external_account(rng, engine, ctx.page_size, None)
                    .expect("a controlled scope");
            }
        }
        ActionKind::ExternalAccountDelete => {
            if connect::controlled(engine) {
                connect::delete_external_account(rng, engine, ctx.page_size, None)
                    .expect("random targets are valid");
            }
        }
        ActionKind::PersonUpdate => {
            if connect::controlled(engine) {
                connect::update_person(rng, engine, None).expect("random targets are valid");
            }
        }
        ActionKind::PersonDelete => {
            if connect::controlled(engine) {
                connect::delete_person(rng, engine, None).expect("random targets are valid");
            }
        }
        // Fees live in the platform scope; a connected account's generator does nothing.
        ActionKind::ApplicationFeeRefund => {
            if engine.scope().is_none() {
                connect::application_fee_refund(rng, engine, ctx, None, None)
                    .expect("random targets are valid");
            }
        }
        // Transfers live in the platform scope too.
        ActionKind::TransferReversal => {
            if engine.scope().is_none() {
                connect::transfer_reversal(rng, engine, ctx, None, None)
                    .expect("random targets are valid");
            }
        }
        ActionKind::BillingCatalogChange => {
            billing_catalog::change(rng, engine);
        }
        ActionKind::CouponUpdate => billing_catalog::coupon_update(rng, engine),
        ActionKind::InvoiceItem => {
            billing::create_invoice_item(rng, engine, None, None, None)
                .expect("random targets are valid");
        }
        ActionKind::InvoiceItemDelete => billing::churn_delete_item(rng, engine),
        ActionKind::DiscountChange => discounts::discount_change(rng, engine, ctx),
        ActionKind::CreditNote => credit_notes::credit_note_action(rng, engine, ctx),
        ActionKind::ScheduleSubscription => {
            schedules::schedule_subscription(rng, engine, ctx, None, None)
                .expect("random targets are valid");
        }
    }
}

fn pick_live(rng: &mut SimRng, engine: &Engine) -> Option<String> {
    let live = engine.store().live_ids(ObjectType::Customer);
    if live.is_empty() {
        return None;
    }
    Some(live[rng.below(live.len() as u64) as usize].clone())
}
