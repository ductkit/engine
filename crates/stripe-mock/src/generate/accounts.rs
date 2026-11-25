//! Connected-account generation: who connects, who disconnects, and what changes
//! (spec section 5.4). The platform's own traffic is the main [`super::Generator`].
//!
//! Three Poisson clocks, each on its own RNG stream, plus a stream for the content
//! of new accounts. Changing one rate never changes another clock's draws.

use crate::engine::Engine;
use crate::model::account::{AccountContent, random_content};
use crate::rng::{RngStreams, SimRng};
use crate::{
    AccountApi, AccountApiMix, AccountKind, AccountTypeMix, AccountsConfig, Configuration, SimTime,
    UpdateChurn,
};

/// A change to the set of connected accounts, so per-account generators can follow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccountChange {
    Connected(String),
    Disconnected(String),
}

/// A Poisson clock with its own stream.
#[derive(Debug)]
struct Clock {
    base_rate: f64,
    rate: f64,
    rng: SimRng,
    next_due: Option<SimTime>,
}

impl Clock {
    fn new(rng: SimRng, rate: f64, now: SimTime) -> Clock {
        let mut c = Clock {
            base_rate: rate,
            rate,
            rng,
            next_due: None,
        };
        c.redraw(now);
        c
    }

    /// Draws the next due time from `now`.
    fn redraw(&mut self, now: SimTime) {
        self.next_due = self
            .rng
            .exp_interval_ms(self.rate)
            .map(|ms| now.plus_ms(ms));
    }

    fn scale(&mut self, factor: f64, now: SimTime) {
        let rate = self.base_rate * factor;
        if rate != self.rate {
            self.rate = rate;
            self.redraw(now);
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Connect,
    Deauthorize,
    Update,
}

/// Connects and disconnects accounts and churns their fields.
#[derive(Debug)]
pub(crate) struct AccountsGenerator {
    /// In tie-break order: connect, deauthorize, update.
    clocks: [(Kind, Clock); 3],
    content: SimRng,
    api_mix: AccountApiMix,
    type_mix: AccountTypeMix,
    /// Draws for updates triggered by pagination writes.
    pagination: SimRng,
    enabled: bool,
}

impl AccountsGenerator {
    pub(crate) fn new(
        cfg: &AccountsConfig,
        churn: &UpdateChurn,
        seed: u64,
        now: SimTime,
    ) -> AccountsGenerator {
        let streams = RngStreams::new(seed);
        let c = &cfg.connected_accounts;
        let clock = |name: &str, rate: f64| Clock::new(streams.stream(name), rate, now);
        AccountsGenerator {
            clocks: [
                (Kind::Connect, clock("accounts.connect", c.connect_rate)),
                (
                    Kind::Deauthorize,
                    clock("accounts.deauthorize", c.deauthorize_rate),
                ),
                (Kind::Update, clock("accounts.update", churn.update)),
            ],
            content: streams.stream("accounts.content"),
            api_mix: cfg.account_api.clone(),
            type_mix: cfg.account_types.clone(),
            pagination: streams.stream("api.pagination_writes.accounts"),
            enabled: true,
        }
    }

    /// The earliest due clock (`None` while disabled).
    pub(crate) fn next_due(&self) -> Option<SimTime> {
        if !self.enabled {
            return None;
        }
        self.clocks.iter().filter_map(|(_, c)| c.next_due).min()
    }

    /// Sets every rate to its configured rate x `factor`; clocks whose rate changed
    /// redraw their next due time from `now`.
    pub(crate) fn scale_rates(&mut self, factor: f64, now: SimTime) {
        for (_, c) in &mut self.clocks {
            c.scale(factor, now);
        }
    }

    /// Turns generation off or on. Re-enabling redraws each clock from `now`.
    pub(crate) fn set_enabled(&mut self, on: bool, now: SimTime) {
        if on && !self.enabled {
            for (_, c) in &mut self.clocks {
                c.redraw(now);
            }
        }
        self.enabled = on;
    }

    /// Performs everything due at or before `t`, in time order (connect, then
    /// disconnect, then update on ties). Leaves the engine's time at the last
    /// performed step.
    pub(crate) fn run_due(&mut self, engine: &mut Engine, t: SimTime) -> Vec<AccountChange> {
        let mut changes = Vec::new();
        if !self.enabled {
            return changes;
        }
        loop {
            // `min_by_key` keeps the first on ties, which is the fixed order.
            let Some((i, due)) = self
                .clocks
                .iter()
                .enumerate()
                .filter_map(|(i, (_, c))| c.next_due.map(|d| (i, d)))
                .filter(|(_, d)| *d <= t)
                .min_by_key(|(_, d)| *d)
            else {
                return changes;
            };
            engine.set_now(due);
            match self.clocks[i].0 {
                Kind::Connect => {
                    let (api, kind, configurations, content) = self.draw_new();
                    let id = engine.connect_account(api, kind, configurations, content);
                    changes.push(AccountChange::Connected(id));
                }
                Kind::Deauthorize => {
                    let connected = engine.connected_ids();
                    if !connected.is_empty() {
                        let rng = &mut self.clocks[i].1.rng;
                        let id = connected[rng.below(connected.len() as u64) as usize].clone();
                        engine
                            .disconnect_account(&id)
                            .expect("picked from the connected accounts");
                        changes.push(AccountChange::Disconnected(id));
                    }
                }
                Kind::Update => update_random(&mut self.clocks[i].1.rng, engine),
            }
            self.clocks[i].1.redraw(due);
        }
    }

    /// A write during pagination of the accounts list: one update of a random
    /// connected account. Does nothing while disabled.
    pub(crate) fn pagination_update(&mut self, engine: &mut Engine) {
        if self.enabled {
            update_random(&mut self.pagination, engine);
        }
    }

    /// Connects `n` accounts at the engine's current time.
    pub(crate) fn connect_initial(&mut self, engine: &mut Engine, n: u32) -> Vec<String> {
        (0..n)
            .map(|_| {
                let (api, kind, configurations, content) = self.draw_new();
                engine.connect_account(api, kind, configurations, content)
            })
            .collect()
    }

    /// Draws the API, type, configurations and fields of a new account.
    pub(crate) fn draw_new(
        &mut self,
    ) -> (AccountApi, AccountKind, Vec<Configuration>, AccountContent) {
        self.draw_new_with(None)
    }

    /// Like [`draw_new`](Self::draw_new), with the API fixed when given.
    pub(crate) fn draw_new_with(
        &mut self,
        api: Option<AccountApi>,
    ) -> (AccountApi, AccountKind, Vec<Configuration>, AccountContent) {
        let api = api.unwrap_or_else(|| {
            if self.content.unit_f64() < self.api_mix.v1 {
                AccountApi::V1
            } else {
                AccountApi::V2
            }
        });
        let (kind, configurations) = match api {
            AccountApi::V1 => {
                let u = self.content.unit_f64();
                let t = &self.type_mix;
                let kind = if u < t.standard {
                    AccountKind::Standard
                } else if u < t.standard + t.express {
                    AccountKind::Express
                } else {
                    AccountKind::Custom
                };
                (kind, vec![Configuration::Merchant])
            }
            AccountApi::V2 => {
                let mut configs: Vec<Configuration> = [
                    Configuration::Merchant,
                    Configuration::Customer,
                    Configuration::Recipient,
                ]
                .into_iter()
                .filter(|_| self.content.chance(0.5))
                .collect();
                if configs.is_empty() {
                    configs.push(Configuration::Merchant);
                }
                (AccountKind::V2, configs)
            }
        };
        let content = random_content(&mut self.content);
        (api, kind, configurations, content)
    }
}

/// Updates a random connected account, if there is one.
fn update_random(rng: &mut SimRng, engine: &mut Engine) {
    let connected = engine.connected_ids();
    if connected.is_empty() {
        return;
    }
    let id = connected[rng.below(connected.len() as u64) as usize].clone();
    let content = random_content(rng);
    engine
        .update_account(&id, content)
        .expect("picked from the connected accounts");
}
