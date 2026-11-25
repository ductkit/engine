//! Stateful mock of the Stripe API for testing ductkit sync.
//!
//! See `docs/stripe-mock-spec.md` in the repository.

mod config;
mod engine;
mod error;
mod generate;
mod handle;
mod model;
mod rng;
mod stream;
mod time;

pub use config::{
    ActionsConfig, ApiConfig, ChurnConfig, ClockConfig, ClockMode, Config, CustomerChurn,
    PINNED_API_VERSION,
};
pub use engine::{Cause, ChangeKind, EventRefs, StateUpdate};
pub use error::MockError;
pub use handle::MockStripe;
pub use model::ObjectType;
pub use model::customer::{CustomerUpdate, NewCustomer};
pub use stream::{SubscribeFilter, SubscribeOptions, Subscription, SubscriptionItem};
pub use time::SimTime;
