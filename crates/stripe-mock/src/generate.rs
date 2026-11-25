//! Stochastic action generation (spec section 6), phase 1: customers only.
//!
//! Each action is a Poisson process with its own RNG stream, which drives its
//! timing, target choice and content. Changing one rate therefore never
//! changes another action's decisions.

use crate::engine::Engine;
use crate::model::customer::{
    apply_update, deleted_stub, new_customer_json, random_new_customer, random_update,
};
use crate::rng::{RngStreams, SimRng};
use crate::{Config, ObjectType, SimTime};

/// Generated actions, in tie-break order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    NewCustomer,
    CustomerUpdate,
    CustomerDelete,
}

#[derive(Debug)]
struct Scheduled {
    kind: ActionKind,
    rate: f64,
    rng: SimRng,
    next_due: Option<SimTime>,
}

/// Schedules and performs generated actions.
#[derive(Debug)]
pub(crate) struct Generator {
    actions: Vec<Scheduled>,
}

impl Generator {
    pub(crate) fn new(config: &Config, streams: &RngStreams, start: SimTime) -> Generator {
        let specs = [
            (
                ActionKind::NewCustomer,
                config.actions.new_customer,
                "generate.customer.create",
            ),
            (
                ActionKind::CustomerUpdate,
                config.churn.customer.update,
                "generate.customer.update",
            ),
            (
                ActionKind::CustomerDelete,
                config.churn.customer.delete,
                "generate.customer.delete",
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
        Generator { actions }
    }

    /// Performs every action due at or before `target`, then sets the time to `target`.
    pub(crate) fn run_until(&mut self, engine: &mut Engine, target: SimTime) {
        // Earliest due action; `min_by_key` keeps the first on ties, which is the fixed order.
        while let Some(i) = self
            .actions
            .iter()
            .enumerate()
            .filter_map(|(i, a)| a.next_due.map(|d| (i, d)))
            .filter(|(_, d)| *d <= target)
            .min_by_key(|(_, d)| *d)
            .map(|(i, _)| i)
        {
            let action = &mut self.actions[i];
            let due = action.next_due.expect("due");
            engine.set_now(due);
            perform(action.kind, &mut action.rng, engine);
            action.next_due = action
                .rng
                .exp_interval_ms(action.rate)
                .map(|ms| due.plus_ms(ms));
        }
        engine.set_now(target);
    }
}

fn perform(kind: ActionKind, rng: &mut SimRng, engine: &mut Engine) {
    let t = ObjectType::Customer;
    match kind {
        ActionKind::NewCustomer => {
            let id = rng.id(t.id_prefix());
            let prefix = rng.alnum_upper(8);
            let params = random_new_customer(rng);
            let state = new_customer_json(&id, engine.now().unix_seconds(), &prefix, &params);
            let cause = engine.begin_action("new_customer");
            engine.create(t, id, state, cause);
        }
        ActionKind::CustomerUpdate => {
            let Some(id) = pick_live(rng, engine) else {
                return;
            };
            let state = &engine.latest(t, &id).expect("live").state;
            let update = random_update(rng, state);
            let cause = engine.begin_action("customer_update");
            engine
                .update(t, &id, |s| apply_update(s, &update), cause)
                .expect("live customer");
        }
        ActionKind::CustomerDelete => {
            let Some(id) = pick_live(rng, engine) else {
                return;
            };
            let cause = engine.begin_action("customer_delete");
            engine
                .delete(t, &id, deleted_stub(&id), cause)
                .expect("live customer");
        }
    }
}

fn pick_live(rng: &mut SimRng, engine: &Engine) -> Option<String> {
    let live = engine.store().live_ids(ObjectType::Customer);
    if live.is_empty() {
        return None;
    }
    Some(live[rng.below(live.len() as u64) as usize].to_string())
}
