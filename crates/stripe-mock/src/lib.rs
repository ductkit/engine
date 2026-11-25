//! Stateful mock of the Stripe API for testing ductkit sync.
//!
//! See `docs/stripe-mock-spec.md` in the repository.

mod api;
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
    ActionsConfig, ApiConfig, ChurnConfig, ClockConfig, ClockMode, ClockSkew, Config,
    CustomerChurn, DestinationConfig, DisableAfter, Distribution, DurationStr,
    EventRetentionConfig, EventsConfig, LateTail, PINNED_API_VERSION, PayloadFormat, RetrySchedule,
    WebhooksConfig,
};
pub use engine::events::{EventFormat, EventInfo};
pub use engine::{Cause, ChangeKind, EventRefs, StateUpdate};
pub use error::MockError;
pub use handle::MockStripe;
pub use model::ObjectType;
pub use model::customer::{CustomerUpdate, NewCustomer};
pub use stream::{SubscribeFilter, SubscribeOptions, Subscription, SubscriptionItem};
pub use time::SimTime;
