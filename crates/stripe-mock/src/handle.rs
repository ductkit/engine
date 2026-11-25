//! The public control handle (spec section 11, in-process).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::engine::events::EventInfo;
use crate::engine::{Engine, StateUpdate};
use crate::generate::Generator;
use crate::model::customer::{
    CustomerUpdate, NewCustomer, apply_update, deleted_stub, new_customer_json,
};
use crate::rng::{RngStreams, SimRng};
use crate::stream::{SubscribeFilter, SubscribeOptions, Subscription};
use crate::{ClockMode, Config, MockError, ObjectType, SimTime};

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
    /// Wall-clock pacing; unused in virtual mode.
    pub wall: Mutex<WallState>,
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
    /// Wall mode: advances simulated time to match real time (no-op while paused).
    fn tick(&self) {
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
            wall: Mutex::new(WallState {
                anchor_real: Instant::now(),
                anchor_sim: start,
                paused: false,
            }),
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
    pub fn step(&self, d: Duration) -> Result<(), MockError> {
        let target = self.now().plus_ms(d.as_millis() as i64);
        self.advance_to(target)
    }

    /// Advances virtual time to `t`. Equal to now is a no-op; earlier is an error.
    pub fn advance_to(&self, t: SimTime) -> Result<(), MockError> {
        self.require_virtual()?;
        let mut sim = self.shared.lock();
        let now = sim.engine.now();
        if t < now {
            return Err(MockError::TimeTravel { requested: t, now });
        }
        let Sim {
            engine, generator, ..
        } = &mut *sim;
        generator.run_until(engine, t);
        Ok(())
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
