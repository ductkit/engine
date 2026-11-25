//! All generators of the mock: the platform's, the accounts generator and one per
//! connected account (spec section 5.4).
//!
//! A connected account's generator is built from the platform's current `[actions]`
//! and `[churn]` config, with every stream name prefixed `account/{acct}/`, and runs
//! inside that account's scope. Dropping it discards its pending continuations.

use crate::engine::{Cause, Engine, Scope};
use crate::generate::{AccountChange, AccountsGenerator, Action, Generator};
use crate::rng::RngStreams;
use crate::{AccountApi, ActionsConfig, Config, MockError, ObjectType, SimTime};

#[derive(Debug)]
pub(crate) struct Generators {
    platform: Generator,
    accounts: AccountsGenerator,
    /// In connection order.
    connected: Vec<(String, Generator)>,
    /// Builds the generators of accounts connected later.
    config: Config,
    streams: RngStreams,
    scale: f64,
    enabled: bool,
}

impl Generators {
    pub(crate) fn new(config: &Config, streams: &RngStreams, start: SimTime) -> Generators {
        Generators {
            platform: Generator::new(config, streams, start),
            accounts: AccountsGenerator::new(
                &config.accounts,
                &config.churn.account,
                config.seed,
                start,
            ),
            connected: Vec::new(),
            config: config.clone(),
            streams: streams.clone(),
            scale: 1.0,
            enabled: true,
        }
    }

    /// Connects `n` accounts at the engine's current time, each with a generator.
    pub(crate) fn connect_initial(&mut self, engine: &mut Engine, n: u32) {
        let ids = self.accounts.connect_initial(engine, n);
        let changes: Vec<_> = ids.into_iter().map(AccountChange::Connected).collect();
        self.apply(engine, &changes);
    }

    /// Connects an account drawn like a generated one, with its API fixed.
    pub(crate) fn connect_account(&mut self, engine: &mut Engine, api: AccountApi) -> String {
        let (api, kind, configurations, content) = self.accounts.draw_new_with(Some(api));
        let id = engine.connect_account(api, kind, configurations, content);
        self.apply(engine, &[AccountChange::Connected(id.clone())]);
        id
    }

    /// Disconnects an account; its generator and pending continuations go with it.
    pub(crate) fn disconnect_account(
        &mut self,
        engine: &mut Engine,
        id: &str,
    ) -> Result<(), MockError> {
        engine.disconnect_account(id)?;
        self.apply(engine, &[AccountChange::Disconnected(id.to_string())]);
        Ok(())
    }

    /// Follows changes to the set of connected accounts. A new generator starts at
    /// the engine's current time (the connect instant).
    pub(crate) fn apply(&mut self, engine: &Engine, changes: &[AccountChange]) {
        for change in changes {
            match change {
                AccountChange::Connected(id) => {
                    let now = engine.now();
                    let mut config = self.config.clone();
                    config.actions = self.platform.actions_config().clone();
                    let mut g = Generator::new_with_prefix(
                        &config,
                        &self.streams,
                        now,
                        &format!("account/{id}/"),
                    );
                    g.scale_rates(self.scale, now);
                    if !self.enabled {
                        g.set_enabled(false, now);
                    }
                    self.connected.push((id.clone(), g));
                }
                AccountChange::Disconnected(id) => self.connected.retain(|(a, _)| a != id),
            }
        }
    }

    /// The earliest due action of any generator.
    pub(crate) fn next_due(&self) -> Option<SimTime> {
        self.platform
            .next_due()
            .into_iter()
            .chain(self.accounts.next_due())
            .chain(self.connected.iter().filter_map(|(_, g)| g.next_due()))
            .min()
    }

    /// Runs every due action up to `target` in time order, then sets the time to
    /// `target`. Ties go to the platform, then the accounts generator, then connected
    /// accounts in connection order.
    pub(crate) fn run_until(&mut self, engine: &mut Engine, target: SimTime) {
        loop {
            // Candidates in tie-break order; `min_by_key` keeps the first on ties.
            let platform = self.platform.next_due().map(|d| (0, d));
            let accounts = self.accounts.next_due().map(|d| (1, d));
            let connected = self
                .connected
                .iter()
                .enumerate()
                .filter_map(|(i, (_, g))| g.next_due().map(|d| (2 + i, d)));
            let Some((who, due)) = platform
                .into_iter()
                .chain(accounts)
                .chain(connected)
                .filter(|(_, d)| *d <= target)
                .min_by_key(|(_, d)| *d)
            else {
                break;
            };
            match who {
                0 => self.platform.run_until(engine, due),
                1 => {
                    let changes = self.accounts.run_due(engine, due);
                    self.apply(engine, &changes);
                }
                i => {
                    let (id, g) = &mut self.connected[i - 2];
                    engine.in_scope(Some(id.clone()), |e| g.run_until(e, due));
                }
            }
        }
        self.platform.run_until(engine, target);
    }

    pub(crate) fn set_enabled(&mut self, on: bool, now: SimTime) {
        self.enabled = on;
        self.platform.set_enabled(on, now);
        self.accounts.set_enabled(on, now);
        for (_, g) in &mut self.connected {
            g.set_enabled(on, now);
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.platform.enabled()
    }

    pub(crate) fn actions_config(&self) -> &ActionsConfig {
        self.platform.actions_config()
    }

    /// Applies `[actions]` to every generator, each in its own scope.
    pub(crate) fn set_actions_config(&mut self, config: ActionsConfig, engine: &mut Engine) {
        self.platform.set_actions_config(config.clone(), engine);
        for (id, g) in &mut self.connected {
            engine.in_scope(Some(id.clone()), |e| {
                g.set_actions_config(config.clone(), e)
            });
        }
    }

    pub(crate) fn scale_rates(&mut self, factor: f64, now: SimTime) {
        self.scale = factor;
        self.platform.scale_rates(factor, now);
        self.accounts.scale_rates(factor, now);
        for (_, g) in &mut self.connected {
            g.scale_rates(factor, now);
        }
    }

    /// Runs `a` now in `scope`, on that scope's generator.
    pub(crate) fn run_action(
        &mut self,
        scope: &Scope,
        engine: &mut Engine,
        a: Action,
    ) -> Result<Cause, MockError> {
        match scope {
            None => self.platform.run_action(engine, a),
            Some(id) => {
                let g = self.connected_mut(id)?;
                engine.in_scope(scope.clone(), |e| g.run_action(e, a))
            }
        }
    }

    /// A write during pagination of a `t` list in `scope`. The platform's Accounts
    /// list gets an account update; an unknown scope does nothing.
    pub(crate) fn pagination_write(&mut self, scope: &Scope, t: ObjectType, engine: &mut Engine) {
        match scope {
            None if t == ObjectType::Account => self.accounts.pagination_update(engine),
            None => self.platform.pagination_write(engine, t),
            Some(id) => {
                if let Ok(g) = self.connected_mut(id) {
                    engine.in_scope(scope.clone(), |e| g.pagination_write(e, t));
                }
            }
        }
    }

    fn connected_mut(&mut self, id: &str) -> Result<&mut Generator, MockError> {
        self.connected
            .iter_mut()
            .find(|(a, _)| a == id)
            .map(|(_, g)| g)
            .ok_or_else(|| MockError::UnknownAccount(id.to_string()))
    }
}
