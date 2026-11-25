//! The public control handle (spec section 11, in-process).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::control::{ControlOp, JournalEntry};
use crate::delivery::sender::send;
use crate::delivery::{DeliveryRecord, DestinationInfo, PreparedAttempt};
use crate::engine::events::EventInfo;
use crate::engine::{Cause, ChangeKind, Engine, Scope, StateUpdate};
use crate::faults::api::ApiFaults;
use crate::generate::{Action, Generators, delete_customer_cascade};
use crate::model::customer::{CustomerUpdate, NewCustomer, apply_update, new_customer_json};
use crate::rng::{RngStreams, SimRng};
use crate::stream::{SubscribeFilter, SubscribeOptions, Subscription};
use crate::{
    AccountApi, AccountKind, ActionsConfig, ApiConfig, ClockMode, Config, Configuration,
    DestinationConfig, DurationStr, EventsConfig, MockError, ObjectType, PayloadFormat, SimTime,
    WebhooksConfig,
};

/// Everything behind the lock.
#[derive(Debug)]
pub(crate) struct Sim {
    pub engine: Engine,
    /// The platform's, the accounts and every connected account's generator.
    pub generators: Generators,
    pub control_rng: SimRng,
    /// Draws `Request-Id` values.
    pub api_rng: SimRng,
    /// Runtime `[api]` settings and fault state.
    pub api: ApiFaults,
    /// Every op that changed truth, the projection or delivery, in order.
    pub journal: Vec<JournalEntry>,
}

impl Sim {
    /// Journals `op` at the current simulated time. Draws nothing.
    pub(crate) fn record(&mut self, op: ControlOp) {
        let at = self.engine.now();
        self.journal.push(JournalEntry { at, op });
    }

    /// `UnknownAccount` unless `scope` is the platform or a connected account.
    fn check_scope(&self, scope: &Scope) -> Result<(), MockError> {
        match scope {
            Some(id) if !self.engine.account_meta(id).is_some_and(|m| m.connected) => {
                Err(MockError::UnknownAccount(id.clone()))
            }
            _ => Ok(()),
        }
    }

    /// Runs a journalled mutation. On success `f`'s op is recorded; on failure the
    /// action numbers `f` took are given back, so a failed call leaves no trace.
    fn journalled<R>(
        &mut self,
        f: impl FnOnce(&mut Sim) -> Result<(R, ControlOp), MockError>,
    ) -> Result<R, MockError> {
        let mark = self.engine.action_mark();
        let logged = self.engine.log().len();
        match f(self) {
            Ok((r, op)) => {
                self.record(op);
                Ok(r)
            }
            Err(e) => {
                debug_assert_eq!(self.engine.log().len(), logged, "failed op mutated truth");
                self.engine.rewind_actions(mark);
                Err(e)
            }
        }
    }
}

/// State shared by the handle, the HTTP server and the wall-clock ticker.
#[derive(Debug)]
pub(crate) struct Shared {
    pub sim: Mutex<Sim>,
    pub config: Config,
    /// Serializes `step`/`advance_to`/`quiesce`.
    pub step_lock: tokio::sync::Mutex<()>,
    /// Wall-clock pacing; unused in virtual mode.
    pub wall: Mutex<WallState>,
    /// Sends webhooks.
    pub client: reqwest::Client,
    /// Set to `true` on shutdown, so held (`timeout`) responses end.
    pub shutdown: tokio::sync::watch::Sender<bool>,
    /// Set to `true` by `POST /control/shutdown`; the process owner acts on it.
    pub shutdown_requested: tokio::sync::watch::Sender<bool>,
}

/// Maps real time to simulated time in wall mode.
#[derive(Debug)]
pub(crate) struct WallState {
    anchor_real: Instant,
    anchor_sim: SimTime,
    paused: bool,
    /// Simulated seconds per real second; starts at `clock.speed`.
    speed: f64,
}

const TICK: Duration = Duration::from_millis(10);

impl Shared {
    /// Wall mode: advances simulated time to match real time (no-op while paused),
    /// then starts every delivery attempt that is due.
    fn tick(self: &Arc<Self>) {
        let wall = self.wall.lock().expect("wall lock poisoned");
        if wall.paused {
            return;
        }
        let elapsed_ms = wall.anchor_real.elapsed().as_secs_f64() * 1000.0;
        let target = wall.anchor_sim.plus_ms((elapsed_ms * wall.speed) as i64);
        let mut sim = self.lock();
        if target > sim.engine.now() {
            let Sim {
                engine, generators, ..
            } = &mut *sim;
            generators.run_until(engine, target);
        }
        let now = sim.engine.now();
        let mut due = Vec::new();
        while let Some((_, batch)) = sim.engine.due_batch(now) {
            due.extend(batch);
        }
        drop(sim);
        if !due.is_empty() {
            self.spawn_batch(due, None);
        }
    }

    /// Sends `batch` on its own task, so a cancelled caller loses nothing, and records
    /// each outcome as it arrives. Attempts to one destination go one at a time in
    /// queue order; destinations proceed concurrently. Outcomes are recorded at `at`,
    /// or at the sim time of arrival when `None`.
    fn spawn_batch(
        self: &Arc<Self>,
        batch: Vec<PreparedAttempt>,
        at: Option<SimTime>,
    ) -> JoinHandle<()> {
        let mut lanes: Vec<Vec<PreparedAttempt>> = Vec::new();
        for a in batch {
            match lanes
                .iter_mut()
                .find(|l| l[0].destination_id == a.destination_id)
            {
                Some(lane) => lane.push(a),
                None => lanes.push(vec![a]),
            }
        }
        let shared = self.clone();
        tokio::spawn(async move {
            let lanes = lanes.into_iter().map(|lane| {
                let shared = shared.clone();
                async move {
                    for a in lane {
                        let (outcome, elapsed) = send(&shared.client, &a).await;
                        let mut sim = shared.lock();
                        let now = at.unwrap_or_else(|| sim.engine.now());
                        sim.engine.record_outcome(&a, outcome, elapsed, now);
                    }
                }
            });
            futures_util::future::join_all(lanes).await;
        })
    }
}

/// Leaves quiesce mode when `quiesce` returns or is cancelled.
struct QuiesceGuard<'a>(&'a Shared);

impl Drop for QuiesceGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut sim) = self.0.sim.lock() {
            sim.engine.end_quiesce();
        }
    }
}

/// Waits for a delivery task, re-raising its panic.
async fn join(h: JoinHandle<()>) {
    if let Err(e) = h.await
        && e.is_panic()
    {
        std::panic::resume_unwind(e.into_panic());
    }
}

impl Shared {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Sim> {
        self.sim.lock().expect("sim lock poisoned")
    }
}

/// A connected account, as the control API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub id: String,
    pub api: AccountApi,
    pub kind: AccountKind,
    pub configurations: Vec<Configuration>,
    /// `false` once the account has been disconnected.
    pub connected: bool,
}

/// A running mock. Drop it or call `shutdown` to stop it.
#[derive(Debug)]
pub struct MockStripe {
    pub(crate) shared: Arc<Shared>,
    api_addr: SocketAddr,
    control_addr: Option<SocketAddr>,
    stop: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<()>>,
    control_stop: Option<oneshot::Sender<()>>,
    control_server: Option<JoinHandle<()>>,
    ticker: Option<JoinHandle<()>>,
}

/// Where [`MockStripe::start_with`] listens.
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    /// The Stripe API's address. Default `127.0.0.1:0`.
    pub api_addr: Option<SocketAddr>,
    /// The control API's address. `None` opens no control port.
    pub control_addr: Option<SocketAddr>,
}

/// Serves `app` on `listener` until `stopped` resolves.
fn serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    stopped: oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await;
    })
}

impl MockStripe {
    /// A handle over `shared` for the control API's handlers. It owns no server,
    /// so dropping it stops nothing.
    pub(crate) fn view(shared: Arc<Shared>) -> MockStripe {
        MockStripe {
            shared,
            api_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            control_addr: None,
            stop: None,
            server: None,
            control_stop: None,
            control_server: None,
            ticker: None,
        }
    }

    /// Validates `config` and starts the mock with the Stripe API on an ephemeral
    /// loopback port and no control API.
    pub async fn start(config: Config) -> Result<MockStripe, MockError> {
        MockStripe::start_with(config, StartOptions::default()).await
    }

    /// Validates `config` and starts the mock on the addresses in `options`.
    pub async fn start_with(
        config: Config,
        options: StartOptions,
    ) -> Result<MockStripe, MockError> {
        config.validate()?;
        let config_speed = config.clock.speed;
        let streams = RngStreams::new(config.seed);
        let start = SimTime::from_datetime(config.clock.start);
        // History (spec section 6.3): generation runs from `start - duration` with
        // scaled rates, before any destination exists, so none of it is delivered.
        let history_start = start.plus_ms(-config.history.duration.ms());
        let mut engine = Engine::new(history_start, &config, &streams);
        let mut generators = Generators::new(&config, &streams, history_start);
        // Initial accounts are connected at history start, so they get history too.
        generators.connect_initial(&mut engine, config.accounts.connected_accounts.initial);
        generators.scale_rates(config.history.scale, history_start);
        generators.run_until(&mut engine, start);
        generators.scale_rates(1.0, start);
        engine.add_config_destinations(&config);
        let sim = Sim {
            engine,
            generators,
            control_rng: streams.stream("control"),
            api_rng: streams.stream("api.request_ids"),
            api: ApiFaults::new(&config.api, &streams),
            journal: Vec::new(),
        };
        let shared = Arc::new(Shared {
            sim: Mutex::new(sim),
            config,
            step_lock: tokio::sync::Mutex::new(()),
            wall: Mutex::new(WallState {
                anchor_real: Instant::now(),
                anchor_sim: start,
                paused: false,
                speed: config_speed,
            }),
            // Stripe treats a redirect as a failed delivery, and receivers are local.
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()
                .map_err(|e| MockError::Config(format!("HTTP client: {e}")))?,
            shutdown: tokio::sync::watch::Sender::new(false),
            shutdown_requested: tokio::sync::watch::Sender::new(false),
        });
        let listener = match options.api_addr {
            Some(addr) => tokio::net::TcpListener::bind(addr).await?,
            None => tokio::net::TcpListener::bind("127.0.0.1:0").await?,
        };
        let api_addr = listener.local_addr()?;
        let control_listener = match options.control_addr {
            Some(addr) => Some(tokio::net::TcpListener::bind(addr).await?),
            None => None,
        };
        let control_addr = control_listener
            .as_ref()
            .map(tokio::net::TcpListener::local_addr)
            .transpose()?;
        let (stop, stopped) = oneshot::channel::<()>();
        let app = crate::api::router(shared.clone());
        let ticker = (shared.config.clock.mode == ClockMode::Wall).then(|| {
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(TICK);
                loop {
                    interval.tick().await;
                    shared.tick();
                }
            })
        });
        let server = serve(listener, app, stopped);
        let (control_stop, control_server) = match control_listener {
            Some(listener) => {
                let (stop, stopped) = oneshot::channel::<()>();
                let app = crate::control::http::router(shared.clone());
                (Some(stop), Some(serve(listener, app, stopped)))
            }
            None => (None, None),
        };
        Ok(MockStripe {
            shared,
            api_addr,
            control_addr,
            stop: Some(stop),
            server: Some(server),
            control_stop,
            control_server,
            ticker,
        })
    }

    /// Base URL of the Stripe-compatible API, e.g. `http://127.0.0.1:54321`.
    pub fn api_url(&self) -> String {
        format!("http://{}", self.api_addr)
    }

    /// Base URL of the control API, or `None` if no `control_addr` was given.
    pub fn control_url(&self) -> Option<String> {
        self.control_addr.map(|a| format!("http://{a}"))
    }

    /// Resolves once `POST /control/shutdown` has been called. It does not stop
    /// anything itself; the process owner calls [`MockStripe::shutdown`].
    pub fn shutdown_requested(&self) -> impl Future<Output = ()> + use<> {
        let mut requested = self.shared.shutdown_requested.subscribe();
        async move {
            let _ = requested.wait_for(|r| *r).await;
        }
    }

    /// Stops both HTTP servers (waiting until the ports are closed) and ends every subscription.
    pub async fn shutdown(mut self) {
        if let Some(ticker) = self.ticker.take() {
            ticker.abort();
        }
        self.shared.lock().engine.clear_subscribers();
        self.shared.shutdown.send_replace(true);
        for stop in [self.stop.take(), self.control_stop.take()]
            .into_iter()
            .flatten()
        {
            let _ = stop.send(());
        }
        for server in [self.server.take(), self.control_server.take()]
            .into_iter()
            .flatten()
        {
            let _ = server.await;
        }
    }

    /// Current simulated time.
    pub fn now(&self) -> SimTime {
        self.shared.lock().engine.now()
    }

    /// Advances virtual time by `d`, running every action due on the way.
    /// Concurrent calls are serialized.
    pub async fn step(&self, d: Duration) -> Result<(), MockError> {
        self.require_virtual()?;
        let _step = self.shared.step_lock.lock().await;
        let target = {
            let sim = self.shared.lock();
            let now = sim.engine.now();
            i64::try_from(d.as_millis())
                .ok()
                .and_then(|ms| now.0.checked_add(ms))
                .map(SimTime)
                .ok_or(MockError::TimeTravel {
                    requested: SimTime(i64::MAX),
                    now,
                })?
        };
        self.advance_locked(target).await?;
        self.record_reached();
        Ok(())
    }

    /// Advances virtual time to `t`. Equal to now is a no-op; earlier is an error.
    pub async fn advance_to(&self, t: SimTime) -> Result<(), MockError> {
        self.require_virtual()?;
        let _step = self.shared.step_lock.lock().await;
        self.advance_locked(t).await?;
        self.record_reached();
        Ok(())
    }

    /// Journals a finished `step` or `advance_to` as the time it reached.
    fn record_reached(&self) {
        let mut sim = self.shared.lock();
        let time = sim.engine.now().to_datetime();
        sim.record(ControlOp::AdvanceTo { time });
    }

    /// Replay: `advance_to` without journalling it.
    pub(crate) async fn advance_unjournalled(&self, t: SimTime) -> Result<(), MockError> {
        self.require_virtual()?;
        let _step = self.shared.step_lock.lock().await;
        self.advance_locked(t).await
    }

    /// Replay: a journalled pagination write, run directly in its scope.
    pub(crate) fn pagination_write(&self, scope: &Scope, t: ObjectType) -> Result<(), MockError> {
        let mut sim = self.shared.lock();
        sim.check_scope(scope)?;
        sim.record(ControlOp::PaginationWrite {
            account: scope.clone(),
            object_type: t,
        });
        let Sim {
            engine, generators, ..
        } = &mut *sim;
        generators.pagination_write(scope, t, engine);
        Ok(())
    }

    /// Wall mode: catches simulated time up with real time, so a control call
    /// applies after everything due by now (no-op in virtual mode).
    fn wall_tick(&self) {
        if self.shared.config.clock.mode == ClockMode::Wall {
            self.shared.tick();
        }
    }

    /// A journalled mutation: ticks in wall mode, then runs `f` and records its op
    /// under one sim-lock acquisition.
    fn journalled<R>(
        &self,
        f: impl FnOnce(&mut Sim) -> Result<(R, ControlOp), MockError>,
    ) -> Result<R, MockError> {
        self.wall_tick();
        self.shared.lock().journalled(f)
    }

    /// A journalled mutation in `scope`, which must be the platform or connected.
    fn journalled_in<R>(
        &self,
        scope: &Scope,
        op: ControlOp,
        f: impl FnOnce(&mut Engine, &mut SimRng, &mut Generators) -> Result<R, MockError>,
    ) -> Result<R, MockError> {
        self.journalled(|sim| {
            sim.check_scope(scope)?;
            let Sim {
                engine,
                control_rng,
                generators,
                ..
            } = sim;
            let r = engine.in_scope(scope.clone(), |e| f(e, control_rng, generators))?;
            Ok((r, op))
        })
    }

    /// The body of `advance_to`; the caller holds the step lock.
    ///
    /// Runs generator actions and delivery batches in time order. Each batch is sent
    /// with the sim lock released and recorded at its due time.
    async fn advance_locked(&self, t: SimTime) -> Result<(), MockError> {
        {
            let now = self.shared.lock().engine.now();
            if t < now {
                return Err(MockError::TimeTravel { requested: t, now });
            }
        }
        loop {
            let (due, batch) = {
                let mut sim = self.shared.lock();
                let Sim {
                    engine, generators, ..
                } = &mut *sim;
                let action = generators.next_due().filter(|g| *g <= t);
                let delivery = engine.next_delivery_due().filter(|d| *d <= t);
                match (action, delivery) {
                    (None, None) => {
                        generators.run_until(engine, t);
                        return Ok(());
                    }
                    // Actions first on ties, so a zero-delay event joins the batch.
                    (Some(g), d) if d.is_none_or(|d| g <= d) => {
                        generators.run_until(engine, g);
                        continue;
                    }
                    (_, Some(d)) => {
                        engine.set_now(d);
                        match engine.due_batch(d) {
                            Some(b) => b,
                            None => continue,
                        }
                    }
                    (Some(_), None) => unreachable!("covered by the action arm"),
                }
            };
            join(self.shared.spawn_batch(batch, Some(due))).await;
        }
    }

    /// Creates a customer now and returns its ID.
    pub fn create_customer(&self, p: NewCustomer) -> Result<String, MockError> {
        self.create_customer_in(&None, p)
    }

    pub(crate) fn create_customer_in(
        &self,
        scope: &Scope,
        p: NewCustomer,
    ) -> Result<String, MockError> {
        let op = ControlOp::CreateCustomer {
            account: scope.clone(),
            customer: p.clone(),
        };
        self.journalled_in(scope, op, |engine, control_rng, _| {
            let id = control_rng.id(ObjectType::Customer.id_prefix());
            let prefix = control_rng.alnum_upper(8);
            let state = new_customer_json(&id, engine.now().unix_seconds(), &prefix, &p);
            let cause = engine.begin_action("control.create_customer");
            engine.create(
                ObjectType::Customer,
                id.clone(),
                state,
                cause,
                &["customer.created"],
            );
            Ok(id)
        })
    }

    /// Updates a customer now. Returns the new version, or `None` if nothing changed.
    pub fn update_customer(&self, id: &str, u: CustomerUpdate) -> Result<Option<u64>, MockError> {
        self.update_customer_in(&None, id, u)
    }

    pub(crate) fn update_customer_in(
        &self,
        scope: &Scope,
        id: &str,
        u: CustomerUpdate,
    ) -> Result<Option<u64>, MockError> {
        let op = ControlOp::UpdateCustomer {
            account: scope.clone(),
            id: id.to_string(),
            update: u.clone(),
        };
        self.journalled_in(scope, op, |engine, _, _| {
            let cause = engine.begin_action("control.update_customer");
            let update = engine.update(
                ObjectType::Customer,
                id,
                |s| apply_update(s, &u),
                cause,
                ChangeKind::Updated,
                &["customer.updated"],
            )?;
            Ok(update.map(|u| u.version))
        })
    }

    /// Deletes a customer now.
    pub fn delete_customer(&self, id: &str) -> Result<(), MockError> {
        self.delete_customer_in(&None, id)
    }

    pub(crate) fn delete_customer_in(&self, scope: &Scope, id: &str) -> Result<(), MockError> {
        let op = ControlOp::DeleteCustomer {
            account: scope.clone(),
            id: id.to_string(),
        };
        self.journalled_in(scope, op, |engine, _, _| {
            let cause = engine.begin_action("control.delete_customer");
            delete_customer_cascade(engine, id, &cause)
        })
    }

    /// A control surface scoped to the connected account `id`. Its methods fail with
    /// `UnknownAccount` while the account is unknown or disconnected.
    pub fn account(&self, id: &str) -> AccountHandle<'_> {
        AccountHandle {
            mock: self,
            scope: Some(id.to_string()),
        }
    }

    /// Reads in `scope`, failing with `UnknownAccount` for an account that is
    /// unknown or disconnected.
    fn read_scoped<R>(&self, scope: &Scope, f: impl FnOnce(&Engine) -> R) -> Result<R, MockError> {
        let mut sim = self.shared.lock();
        sim.check_scope(scope)?;
        Ok(sim.engine.in_scope(scope.clone(), |e| f(e)))
    }

    /// Connects a new account through the given Accounts API now and returns its ID.
    /// The v1 type, or the v2 configurations, are drawn from the config mixes.
    pub fn connect_account(&self, api: AccountApi) -> Result<String, MockError> {
        self.require_connect()?;
        self.journalled(|sim| {
            let Sim {
                engine, generators, ..
            } = sim;
            let id = generators.connect_account(engine, api);
            Ok((id, ControlOp::ConnectAccount { api }))
        })
    }

    /// Disconnects a connected account now (the platform loses access to it).
    pub fn disconnect_account(&self, id: &str) -> Result<(), MockError> {
        self.require_connect()?;
        self.journalled(|sim| {
            let Sim {
                engine, generators, ..
            } = sim;
            generators.disconnect_account(engine, id)?;
            Ok(((), ControlOp::DisconnectAccount { id: id.to_string() }))
        })
    }

    /// Every account ever connected, in connection order.
    pub fn accounts(&self) -> Vec<AccountInfo> {
        self.shared
            .lock()
            .engine
            .account_metas()
            .iter()
            .map(|a| AccountInfo {
                id: a.id.clone(),
                api: a.api,
                kind: a.kind,
                configurations: a.configurations.clone(),
                connected: a.connected,
            })
            .collect()
    }

    fn require_connect(&self) -> Result<(), MockError> {
        if self.shared.config.accounts.connect {
            Ok(())
        } else {
            Err(MockError::ConnectDisabled)
        }
    }

    /// Runs a business action now, plus every follow-up step due by now
    /// (spec section 11, "Triggers"). Returns the action's cause.
    pub fn run_action(&self, a: Action) -> Result<Cause, MockError> {
        self.run_action_in(&None, a)
    }

    pub(crate) fn run_action_in(&self, scope: &Scope, a: Action) -> Result<Cause, MockError> {
        let op = ControlOp::RunAction {
            account: scope.clone(),
            action: a.clone(),
        };
        self.journalled_in(scope, op, |engine, _, generators| {
            generators.run_action(scope, engine, a)
        })
    }

    /// Subscribes to the latest state of every matching object (spec section 10).
    pub fn subscribe(&self, filter: SubscribeFilter) -> Subscription {
        self.subscribe_with(filter, SubscribeOptions::default())
    }

    /// Like [`MockStripe::subscribe`], with explicit options.
    pub fn subscribe_with(
        &self,
        filter: SubscribeFilter,
        options: SubscribeOptions,
    ) -> Subscription {
        self.shared.lock().engine.subscribe(filter, &options)
    }

    /// The latest truth-log entry for an object, deleted or not.
    pub fn get(&self, t: ObjectType, id: &str) -> Option<StateUpdate> {
        self.shared.lock().engine.latest(t, id).cloned()
    }

    /// Delivers everything now: turns generation off, makes every event visible,
    /// and attempts every pending delivery (retries included) until each succeeds
    /// or exhausts its schedule. Simulated time does not move.
    pub async fn quiesce(&self) -> Result<(), MockError> {
        let _step = self.shared.step_lock.lock().await;
        self.journalled(|sim| {
            let now = sim.engine.now();
            sim.generators.set_enabled(false, now);
            sim.engine.begin_quiesce();
            Ok(((), ControlOp::Quiesce))
        })?;
        let _quiescing = QuiesceGuard(&self.shared);
        loop {
            let next = {
                let mut sim = self.shared.lock();
                sim.engine.rush_deliveries();
                let now = sim.engine.now();
                match sim.engine.due_batch(now) {
                    Some(b) => Some(b),
                    None if sim.engine.has_pending_deliveries() => None,
                    None => break,
                }
            };
            let Some((_, batch)) = next else {
                // Attempts started elsewhere (the wall ticker, or a cancelled step)
                // are still in flight; their tasks always finish.
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            };
            join(self.shared.spawn_batch(batch, None)).await;
        }
        Ok(())
    }

    /// Turns generation back on after `quiesce`; next due times are redrawn from now.
    pub fn resume_generation(&self) {
        let resumed = self.journalled(|sim| {
            let now = sim.engine.now();
            sim.generators.set_enabled(true, now);
            Ok(((), ControlOp::ResumeGeneration))
        });
        resumed.expect("resuming generation cannot fail");
    }

    /// Whether generated traffic is running.
    pub fn generation_enabled(&self) -> bool {
        self.shared.lock().generators.enabled()
    }

    /// Changes webhook settings (validated); affects future scheduling and attempts only.
    pub fn update_webhooks_config(
        &self,
        f: impl FnOnce(&mut WebhooksConfig),
    ) -> Result<(), MockError> {
        self.journalled(|sim| {
            let mut cfg = sim.engine.webhooks_config().clone();
            f(&mut cfg);
            cfg.validate()?;
            sim.engine.set_webhooks_config(cfg.clone());
            Ok(((), ControlOp::SetWebhooksConfig { config: cfg }))
        })
    }

    /// Changes `[api]` settings (validated) for requests from now on. A changed
    /// `rate_limit` starts with a full bucket.
    pub fn update_api_config(&self, f: impl FnOnce(&mut ApiConfig)) -> Result<(), MockError> {
        self.journalled(|sim| {
            let mut cfg = sim.api.config().clone();
            f(&mut cfg);
            cfg.validate()?;
            sim.api.set_config(cfg.clone(), Instant::now());
            Ok(((), ControlOp::SetApiConfig { config: cfg }))
        })
    }

    /// A rate-limit storm: every Stripe API request gets 429 for `d` of wall time.
    pub fn force_rate_limit(&self, d: Duration) {
        self.shared.lock().api.storm(Instant::now() + d);
    }

    /// Changes `[actions]` settings (validated). Changed rates redraw their next
    /// due time from now; other settings apply to later draws.
    pub fn update_actions_config(
        &self,
        f: impl FnOnce(&mut ActionsConfig),
    ) -> Result<(), MockError> {
        self.journalled(|sim| {
            let mut cfg = sim.generators.actions_config().clone();
            f(&mut cfg);
            cfg.validate()?;
            sim.generators
                .set_actions_config(cfg.clone(), &mut sim.engine);
            Ok(((), ControlOp::SetActionsConfig { config: cfg }))
        })
    }

    /// Changes Events API settings for events created from now on.
    pub fn update_events_config(&self, f: impl FnOnce(&mut EventsConfig)) -> Result<(), MockError> {
        self.journalled(|sim| {
            let mut cfg = sim.engine.projector_mut().config.clone();
            f(&mut cfg);
            cfg.validate()?;
            sim.engine.projector_mut().config = cfg.clone();
            Ok(((), ControlOp::SetEventsConfig { config: cfg }))
        })
    }

    /// Adds a delivery target (validated like config destinations).
    pub fn add_destination(&self, d: DestinationConfig) -> Result<DestinationInfo, MockError> {
        self.journalled(|sim| {
            let now = sim.engine.now();
            let op = ControlOp::AddDestination {
                destination: d.clone(),
            };
            Ok((sim.engine.destinations_mut().add(d, now)?, op))
        })
    }

    /// Removes a delivery target.
    pub fn remove_destination(&self, id: &str) -> Result<(), MockError> {
        self.journalled(|sim| {
            sim.engine.remove_destination(id)?;
            Ok(((), ControlOp::RemoveDestination { id: id.to_string() }))
        })
    }

    /// Enables or disables a delivery target.
    pub fn set_destination_enabled(&self, id: &str, enabled: bool) -> Result<(), MockError> {
        self.journalled(|sim| {
            sim.engine.set_destination_enabled(id, enabled)?;
            let op = ControlOp::SetDestinationEnabled {
                id: id.to_string(),
                enabled,
            };
            Ok(((), op))
        })
    }

    /// Starts a simulated outage: attempts to the destination fail with `Outage`,
    /// without HTTP, until `duration` of simulated time has passed or `end_outage`.
    pub fn start_outage(&self, id: &str, duration: Duration) -> Result<(), MockError> {
        let ms = duration_ms(duration);
        self.journalled(|sim| {
            let until = sim.engine.now().plus_ms(ms);
            sim.engine.destinations_mut().get_mut(id)?.outage_until = Some(until);
            let op = ControlOp::StartOutage {
                id: id.to_string(),
                duration: DurationStr(ms),
            };
            Ok(((), op))
        })
    }

    /// Ends a simulated outage now.
    pub fn end_outage(&self, id: &str) -> Result<(), MockError> {
        self.journalled(|sim| {
            sim.engine.destinations_mut().get_mut(id)?.outage_until = None;
            Ok(((), ControlOp::EndOutage { id: id.to_string() }))
        })
    }

    /// Replaces a destination's secret; the old one keeps signing for `overlap`.
    pub fn rotate_secret(&self, id: &str, overlap: Duration) -> Result<String, MockError> {
        let ms = duration_ms(overlap);
        self.journalled(|sim| {
            let until = sim.engine.now().plus_ms(ms);
            let secret = sim.engine.destinations_mut().rotate_secret(id, until)?;
            let op = ControlOp::RotateSecret {
                id: id.to_string(),
                overlap: DurationStr(ms),
            };
            Ok((secret, op))
        })
    }

    /// All delivery targets, in creation order.
    pub fn destinations(&self) -> Vec<DestinationInfo> {
        self.shared
            .lock()
            .engine
            .destinations()
            .iter()
            .map(|d| d.info.clone())
            .collect()
    }

    /// The latest state of every live object matching `filter`:
    /// `{"at": time, "scopes": [{"account": null | "acct_…", "objects": {"<type>": [state, …]}}]}`.
    /// Deleted objects are omitted.
    pub fn dump_state(&self, filter: &SubscribeFilter) -> serde_json::Value {
        self.shared.lock().engine.dump_state(filter)
    }

    /// The start config with `[api]`, `[events]`, `[webhooks]` and `[actions]` at
    /// their current runtime values and `destinations` as they are now.
    pub fn effective_config(&self) -> Config {
        let sim = self.shared.lock();
        let mut config = self.shared.config.clone();
        config.api = sim.api.config().clone();
        config.events = sim.engine.projector().config.clone();
        config.webhooks = sim.engine.webhooks_config().clone();
        config.actions = sim.generators.actions_config().clone();
        config.destinations = sim
            .engine
            .destinations()
            .iter()
            .map(|d| {
                let i = &d.info;
                let thin = i.payload == PayloadFormat::Thin;
                DestinationConfig {
                    name: i.name.clone(),
                    url: i.url.clone(),
                    payload: i.payload,
                    enabled_events: i.enabled_events.clone(),
                    connect: i.connect,
                    events_from: thin.then(|| i.events_from.clone()),
                    secret: Some(i.secret.clone()),
                    enabled: i.enabled,
                }
            })
            .collect();
        config
    }

    /// Every event in creation order, with its hidden visibility time.
    pub fn event_timeline(&self) -> Vec<EventInfo> {
        self.shared
            .lock()
            .engine
            .events()
            .all()
            .iter()
            .map(EventInfo::from)
            .collect()
    }

    /// A copy of the whole delivery log.
    pub fn delivery_log(&self) -> Vec<DeliveryRecord> {
        self.shared.lock().engine.delivery_log().to_vec()
    }

    /// A copy of the whole truth log.
    pub fn truth_log(&self) -> Vec<StateUpdate> {
        self.shared.lock().engine.log().to_vec()
    }

    /// Wall mode: freezes simulated time.
    pub fn pause(&self) -> Result<(), MockError> {
        self.require_wall()?;
        self.shared.tick();
        self.shared.wall.lock().expect("wall lock poisoned").paused = true;
        Ok(())
    }

    /// Wall mode: changes the simulated seconds per real second. Simulated time
    /// does not jump; it continues from now at the new speed.
    pub fn set_speed(&self, speed: f64) -> Result<(), MockError> {
        self.require_wall()?;
        if !speed.is_finite() || speed <= 0.0 {
            return Err(MockError::Config(format!(
                "speed must be a finite number > 0, got {speed}"
            )));
        }
        // Catch up at the old speed first, then re-anchor like `resume`.
        self.shared.tick();
        let now = self.now();
        let mut wall = self.shared.wall.lock().expect("wall lock poisoned");
        wall.anchor_real = Instant::now();
        wall.anchor_sim = now;
        wall.speed = speed;
        Ok(())
    }

    /// Wall mode: continues from the paused simulated time, without a jump.
    pub fn resume(&self) -> Result<(), MockError> {
        self.require_wall()?;
        let now = self.now();
        let mut wall = self.shared.wall.lock().expect("wall lock poisoned");
        if wall.paused {
            wall.anchor_real = Instant::now();
            wall.anchor_sim = now;
            wall.paused = false;
        }
        Ok(())
    }

    fn require_wall(&self) -> Result<(), MockError> {
        match self.shared.config.clock.mode {
            ClockMode::Wall => Ok(()),
            ClockMode::Virtual => Err(MockError::ClockMode(
                "the clock is in virtual mode; pause and resume need wall mode".to_string(),
            )),
        }
    }

    fn require_virtual(&self) -> Result<(), MockError> {
        match self.shared.config.clock.mode {
            ClockMode::Virtual => Ok(()),
            ClockMode::Wall => Err(MockError::ClockMode(
                "the clock is in wall mode; step and advance_to need virtual mode".to_string(),
            )),
        }
    }
}

/// Whole milliseconds, as the journal stores durations.
fn duration_ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl Drop for MockStripe {
    fn drop(&mut self) {
        if let Some(ticker) = self.ticker.take() {
            ticker.abort();
        }
        for stop in [self.stop.take(), self.control_stop.take()]
            .into_iter()
            .flatten()
        {
            let _ = stop.send(());
        }
    }
}

/// A control surface scoped to one connected account, from [`MockStripe::account`].
/// Same operations as the platform's, acting on that account's objects only.
#[derive(Debug, Clone)]
pub struct AccountHandle<'a> {
    mock: &'a MockStripe,
    scope: Scope,
}

impl AccountHandle<'_> {
    /// Creates a customer in the account now and returns its ID.
    pub fn create_customer(&self, p: NewCustomer) -> Result<String, MockError> {
        self.mock.create_customer_in(&self.scope, p)
    }

    /// Updates a customer of the account. Returns the new version, or `None` if
    /// nothing changed.
    pub fn update_customer(&self, id: &str, u: CustomerUpdate) -> Result<Option<u64>, MockError> {
        self.mock.update_customer_in(&self.scope, id, u)
    }

    /// Deletes a customer of the account.
    pub fn delete_customer(&self, id: &str) -> Result<(), MockError> {
        self.mock.delete_customer_in(&self.scope, id)
    }

    /// Runs a business action in the account now, plus every follow-up step due.
    pub fn run_action(&self, a: Action) -> Result<Cause, MockError> {
        self.mock.run_action_in(&self.scope, a)
    }

    /// The latest truth-log entry for an object of the account, deleted or not.
    pub fn get(&self, t: ObjectType, id: &str) -> Result<Option<StateUpdate>, MockError> {
        self.mock
            .read_scoped(&self.scope, |engine| engine.latest(t, id).cloned())
    }
}
