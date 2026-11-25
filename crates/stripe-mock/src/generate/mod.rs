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
mod catalog;
mod mux;
mod subscriptions;

pub(crate) use accounts::{AccountChange, AccountsGenerator};
pub(crate) use mux::Generators;

pub(crate) use subscriptions::delete_customer_cascade;

pub use billing::InvoiceOutcome;

use std::collections::BTreeMap;

use crate::engine::{Cause, ChangeKind, Engine};
use crate::model::catalog::PriceSpec;
use crate::model::customer::{self, apply_update, random_new_customer, random_update};
use crate::rng::{RngStreams, SimRng};
use crate::stripe;
use crate::{ActionsConfig, Config, MockError, ObjectType, SimTime};

/// A business action to run now with [`crate::MockStripe::run_action`] (spec section 11,
/// "Triggers").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Create a random customer.
    NewCustomer,
    /// A random catalog change: new product, new price or an archive.
    CatalogChange,
    /// Archive a product (`prod_…`) or price (`price_…`) now. Control-only.
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
    next_seq: u64,
    lifecycle: SimRng,
    /// Draws made inside billing flows and continuations: invoice and line IDs,
    /// payment outcomes.
    pub billing: SimRng,
}

impl Ctx {
    /// Queues `c` at `at`; ties run in scheduling order.
    pub(crate) fn schedule(&mut self, at: SimTime, c: Continuation, cause: Cause) {
        self.queue.insert((at, self.next_seq), (c, cause));
        self.next_seq += 1;
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
}

/// Schedules and performs generated actions and continuations.
#[derive(Debug)]
pub(crate) struct Generator {
    actions: Vec<Scheduled>,
    ctx: Ctx,
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
        Generator {
            actions,
            scale: 1.0,
            ctx: Ctx {
                config: config.actions.clone(),
                page_size: config.payloads.nested_list_page_size,
                queue: BTreeMap::new(),
                next_seq: 0,
                lifecycle: stream("generate.lifecycle"),
                billing: stream("generate.billing"),
            },
            control: stream("control.actions"),
            pagination_rng: stream("api.pagination_writes.content"),
            enabled: true,
        }
    }

    /// The earliest due action or continuation (`None` while disabled).
    pub(crate) fn next_due(&self) -> Option<SimTime> {
        if !self.enabled {
            return None;
        }
        self.actions
            .iter()
            .filter_map(|a| a.next_due)
            .chain(self.ctx.next_due())
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
    /// subscription that has none pending.
    pub(crate) fn set_actions_config(&mut self, config: ActionsConfig, engine: &mut Engine) {
        let now = engine.now();
        let resume_renewals = self.ctx.config.renew_subscription_cycle != crate::RenewMode::Natural
            && config.renew_subscription_cycle == crate::RenewMode::Natural;
        for a in &mut self.actions {
            let rate = match a.kind {
                ActionKind::NewCustomer => config.new_customer,
                ActionKind::CatalogChange => config.catalog_change,
                ActionKind::OneOffInvoice => config.one_off_invoice,
                ActionKind::StartSubscription => config.start_subscription,
                ActionKind::CancelSubscription => config.cancel_subscription,
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
    pub(crate) fn run_until(&mut self, engine: &mut Engine, target: SimTime) {
        if !self.enabled {
            engine.set_now(target);
            return;
        }
        loop {
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
            ObjectType::SubscriptionItem | ObjectType::InvoiceLineItem | ObjectType::Account => {
                return;
            }
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
    }
}

fn pick_live(rng: &mut SimRng, engine: &Engine) -> Option<String> {
    let live = engine.store().live_ids(ObjectType::Customer);
    if live.is_empty() {
        return None;
    }
    Some(live[rng.below(live.len() as u64) as usize].clone())
}
