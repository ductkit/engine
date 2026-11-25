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

mod catalog;

use std::collections::BTreeMap;

use crate::engine::{Cause, ChangeKind, Engine};
use crate::model::catalog::PriceSpec;
use crate::model::customer::{
    apply_update, deleted_stub, new_customer_json, random_new_customer, random_update,
};
use crate::rng::{RngStreams, SimRng};
use crate::{ActionsConfig, Config, MockError, ObjectType, SimTime};

/// A business action to run now with [`crate::MockStripe::run_action`] (spec section 11,
/// "Triggers").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Create a random customer.
    NewCustomer,
    /// A random catalog change: new product, new price or an archive.
    CatalogChange,
    /// Archive a product (`prod_…`) or price (`price_…`) now. Control-only.
    Archive { id: String },
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
}

#[derive(Debug)]
struct Scheduled {
    kind: ActionKind,
    rate: f64,
    rng: SimRng,
    next_due: Option<SimTime>,
}

/// State shared by actions and continuations.
#[derive(Debug)]
pub(crate) struct Ctx {
    pub config: ActionsConfig,
    /// `payloads.nested_list_page_size`.
    #[allow(dead_code)] // TEMP: used from Task 4 (invoice lines)
    pub page_size: usize,
    queue: BTreeMap<(SimTime, u64), (Continuation, Cause)>,
    next_seq: u64,
    lifecycle: SimRng,
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
}

impl Generator {
    pub(crate) fn new(config: &Config, streams: &RngStreams, start: SimTime) -> Generator {
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
        ];
        let actions = specs
            .into_iter()
            .map(|(kind, rate, stream)| {
                let mut rng = streams.stream(stream);
                let next_due = rng.exp_interval_ms(rate).map(|ms| start.plus_ms(ms));
                Scheduled {
                    kind,
                    rate,
                    rng,
                    next_due,
                }
            })
            .collect();
        Generator {
            actions,
            ctx: Ctx {
                config: config.actions.clone(),
                page_size: config.payloads.nested_list_page_size,
                queue: BTreeMap::new(),
                next_seq: 0,
                lifecycle: streams.stream("generate.lifecycle"),
            },
            control: streams.stream("control.actions"),
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

    /// Turns generation off or on. Re-enabling redraws each Poisson action's next due
    /// time from `now`, using its own stream; continuations keep their times.
    pub(crate) fn set_enabled(&mut self, on: bool, now: SimTime) {
        if on && !self.enabled {
            for a in &mut self.actions {
                a.next_due = a.rng.exp_interval_ms(a.rate).map(|ms| now.plus_ms(ms));
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
        };
        if self.enabled {
            let now = engine.now();
            while self.ctx.next_due().is_some_and(|t| t <= now) {
                self.run_next_continuation(engine);
            }
        }
        Ok(cause)
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
    let state = new_customer_json(&id, engine.now().unix_seconds(), &prefix, &params);
    engine.create(t, id.clone(), state, cause.clone(), &["customer.created"]);
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
            let state = &engine.latest(t, &id).expect("live").state;
            let update = random_update(rng, state);
            let cause = engine.begin_action("customer_update");
            engine
                .update(
                    t,
                    &id,
                    |s| apply_update(s, &update),
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
            engine
                .delete(t, &id, deleted_stub(&id), cause, &["customer.deleted"])
                .expect("live customer");
        }
        ActionKind::CatalogChange => {
            catalog::change(rng, engine, ctx);
        }
        ActionKind::ProductUpdate => catalog::churn_update(rng, engine, ObjectType::Product),
        ActionKind::ProductArchive => catalog::churn_archive(rng, engine, ObjectType::Product),
        ActionKind::PriceUpdate => catalog::churn_update(rng, engine, ObjectType::Price),
        ActionKind::PriceArchive => catalog::churn_archive(rng, engine, ObjectType::Price),
    }
}

fn pick_live(rng: &mut SimRng, engine: &Engine) -> Option<String> {
    let live = engine.store().live_ids(ObjectType::Customer);
    if live.is_empty() {
        return None;
    }
    Some(live[rng.below(live.len() as u64) as usize].to_string())
}
