//! The public control handle (spec section 11, in-process).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

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
}

/// State shared by the handle, the HTTP server and the wall-clock ticker.
#[derive(Debug)]
pub(crate) struct Shared {
    pub sim: Mutex<Sim>,
    pub config: Config,
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
}

impl MockStripe {
    /// Validates `config` and starts the mock.
    pub async fn start(config: Config) -> Result<MockStripe, MockError> {
        config.validate()?;
        let streams = RngStreams::new(config.seed);
        let start = SimTime::from_datetime(config.clock.start);
        let sim = Sim {
            engine: Engine::new(start),
            generator: Generator::new(&config, &streams, start),
            control_rng: streams.stream("control"),
        };
        let shared = Arc::new(Shared {
            sim: Mutex::new(sim),
            config,
        });
        Ok(MockStripe { shared })
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

    /// A copy of the whole truth log.
    pub fn truth_log(&self) -> Vec<StateUpdate> {
        self.shared.lock().engine.log().to_vec()
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
