//! The public control handle (spec section 11, in-process).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::delivery::sender::send;
use crate::delivery::{DeliveryRecord, DestinationInfo, PreparedAttempt};
use crate::engine::events::EventInfo;
use crate::engine::{Engine, StateUpdate};
use crate::generate::Generator;
use crate::model::customer::{
    CustomerUpdate, NewCustomer, apply_update, deleted_stub, new_customer_json,
};
use crate::rng::{RngStreams, SimRng};
use crate::stream::{SubscribeFilter, SubscribeOptions, Subscription};
use crate::{ClockMode, Config, DestinationConfig, EventsConfig, MockError, ObjectType, SimTime};

/// Everything behind the lock.
#[derive(Debug)]
pub(crate) struct Sim {
    pub engine: Engine,
    pub generator: Generator,
    pub control_rng: SimRng,
    /// Draws `Request-Id` values.
    pub api_rng: SimRng,
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
}

/// Maps real time to simulated time in wall mode.
#[derive(Debug)]
pub(crate) struct WallState {
    anchor_real: Instant,
    anchor_sim: SimTime,
    paused: bool,
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
        let target = wall
            .anchor_sim
            .plus_ms((elapsed_ms * self.config.clock.speed) as i64);
        let mut sim = self.lock();
        if target > sim.engine.now() {
            let Sim {
                engine, generator, ..
            } = &mut *sim;
            generator.run_until(engine, target);
        }
        let now = sim.engine.now();
        while let Some((_, batch)) = sim.engine.due_batch(now) {
            for a in batch {
                let shared = self.clone();
                tokio::spawn(async move {
                    let (outcome, elapsed) = send(&shared.client, &a).await;
                    let mut sim = shared.lock();
                    let now = sim.engine.now();
                    sim.engine.record_outcome(&a, outcome, elapsed, now);
                });
            }
        }
    }
}

impl Shared {
    pub(crate) fn lock(&self) -> MutexGuard<'_, Sim> {
        self.sim.lock().expect("sim lock poisoned")
    }
}

/// A running mock. Drop it or call `shutdown` to stop it.
#[derive(Debug)]
pub struct MockStripe {
    shared: Arc<Shared>,
    api_addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<()>>,
    ticker: Option<JoinHandle<()>>,
}

impl MockStripe {
    /// Validates `config` and starts the mock.
    pub async fn start(config: Config) -> Result<MockStripe, MockError> {
        config.validate()?;
        let streams = RngStreams::new(config.seed);
        let start = SimTime::from_datetime(config.clock.start);
        let sim = Sim {
            engine: Engine::new(start, &config, &streams),
            generator: Generator::new(&config, &streams, start),
            control_rng: streams.stream("control"),
            api_rng: streams.stream("api.request_ids"),
        };
        let shared = Arc::new(Shared {
            sim: Mutex::new(sim),
            config,
            step_lock: tokio::sync::Mutex::new(()),
            wall: Mutex::new(WallState {
                anchor_real: Instant::now(),
                anchor_sim: start,
                paused: false,
            }),
            client: reqwest::Client::new(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let api_addr = listener.local_addr()?;
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
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await;
        });
        Ok(MockStripe {
            shared,
            api_addr,
            stop: Some(stop),
            server: Some(server),
            ticker,
        })
    }

    /// Base URL of the Stripe-compatible API, e.g. `http://127.0.0.1:54321`.
    pub fn api_url(&self) -> String {
        format!("http://{}", self.api_addr)
    }

    /// Stops the HTTP server (waiting until the port is closed) and ends every subscription.
    pub async fn shutdown(mut self) {
        if let Some(ticker) = self.ticker.take() {
            ticker.abort();
        }
        self.shared.lock().engine.clear_subscribers();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
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
        self.advance_locked(target).await
    }

    /// Advances virtual time to `t`. Equal to now is a no-op; earlier is an error.
    pub async fn advance_to(&self, t: SimTime) -> Result<(), MockError> {
        self.require_virtual()?;
        let _step = self.shared.step_lock.lock().await;
        self.advance_locked(t).await
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
                    engine, generator, ..
                } = &mut *sim;
                let action = generator.next_due().filter(|g| *g <= t);
                let delivery = engine.next_delivery_due().filter(|d| *d <= t);
                match (action, delivery) {
                    (None, None) => {
                        generator.run_until(engine, t);
                        return Ok(());
                    }
                    // Actions first on ties, so a zero-delay event joins the batch.
                    (Some(g), d) if d.is_none_or(|d| g <= d) => {
                        generator.run_until(engine, g);
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
            let results = self.send_batch(&batch).await;
            let mut sim = self.shared.lock();
            for (a, (outcome, elapsed)) in batch.iter().zip(results) {
                sim.engine.record_outcome(a, outcome, elapsed, due);
            }
        }
    }

    async fn send_batch(&self, batch: &[PreparedAttempt]) -> Vec<(crate::AttemptOutcome, u64)> {
        let client = &self.shared.client;
        futures_util::future::join_all(batch.iter().map(|a| send(client, a))).await
    }

    /// Creates a customer now and returns its ID.
    pub fn create_customer(&self, p: NewCustomer) -> Result<String, MockError> {
        let mut sim = self.shared.lock();
        let Sim {
            engine,
            control_rng,
            ..
        } = &mut *sim;
        let id = control_rng.id(ObjectType::Customer.id_prefix());
        let prefix = control_rng.alnum_upper(8);
        let state = new_customer_json(&id, engine.now().unix_seconds(), &prefix, &p);
        let cause = engine.begin_action("control.create_customer");
        engine.create(ObjectType::Customer, id.clone(), state, cause);
        Ok(id)
    }

    /// Updates a customer now. Returns the new version, or `None` if nothing changed.
    pub fn update_customer(&self, id: &str, u: CustomerUpdate) -> Result<Option<u64>, MockError> {
        let mut sim = self.shared.lock();
        let cause = sim.engine.begin_action("control.update_customer");
        let update = sim
            .engine
            .update(ObjectType::Customer, id, |s| apply_update(s, &u), cause)?;
        Ok(update.map(|u| u.version))
    }

    /// Deletes a customer now.
    pub fn delete_customer(&self, id: &str) -> Result<(), MockError> {
        let mut sim = self.shared.lock();
        let cause = sim.engine.begin_action("control.delete_customer");
        sim.engine
            .delete(ObjectType::Customer, id, deleted_stub(id), cause)?;
        Ok(())
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

    /// Changes Events API settings for events created from now on.
    pub fn update_events_config(&self, f: impl FnOnce(&mut EventsConfig)) -> Result<(), MockError> {
        let mut sim = self.shared.lock();
        let mut cfg = sim.engine.projector_mut().config.clone();
        f(&mut cfg);
        cfg.validate()?;
        sim.engine.projector_mut().config = cfg;
        Ok(())
    }

    /// Adds a delivery target (validated like config destinations).
    pub fn add_destination(&self, d: DestinationConfig) -> Result<DestinationInfo, MockError> {
        let mut sim = self.shared.lock();
        let now = sim.engine.now();
        sim.engine.destinations_mut().add(d, now)
    }

    /// Removes a delivery target.
    pub fn remove_destination(&self, id: &str) -> Result<(), MockError> {
        self.shared.lock().engine.remove_destination(id)
    }

    /// Enables or disables a delivery target.
    pub fn set_destination_enabled(&self, id: &str, enabled: bool) -> Result<(), MockError> {
        self.shared
            .lock()
            .engine
            .set_destination_enabled(id, enabled)
    }

    /// Starts a simulated outage: attempts to the destination fail with `Outage`,
    /// without HTTP, until `duration` of simulated time has passed or `end_outage`.
    pub fn start_outage(&self, id: &str, duration: Duration) -> Result<(), MockError> {
        let mut sim = self.shared.lock();
        let until = sim.engine.now().plus_ms(duration.as_millis() as i64);
        sim.engine.destinations_mut().get_mut(id)?.outage_until = Some(until);
        Ok(())
    }

    /// Ends a simulated outage now.
    pub fn end_outage(&self, id: &str) -> Result<(), MockError> {
        let mut sim = self.shared.lock();
        sim.engine.destinations_mut().get_mut(id)?.outage_until = None;
        Ok(())
    }

    /// Replaces a destination's secret; the old one keeps signing for `overlap`.
    pub fn rotate_secret(&self, id: &str, overlap: Duration) -> Result<String, MockError> {
        let mut sim = self.shared.lock();
        let until = sim.engine.now().plus_ms(overlap.as_millis() as i64);
        sim.engine.destinations_mut().rotate_secret(id, until)
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

impl Drop for MockStripe {
    fn drop(&mut self) {
        if let Some(ticker) = self.ticker.take() {
            ticker.abort();
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
