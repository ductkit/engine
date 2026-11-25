//! Stateful mock of the Stripe API for testing ductkit sync.
//!
//! See `docs/stripe-mock-spec.md` in the repository.

mod api;
mod config;
mod delivery;
mod engine;
mod error;
mod faults;
mod generate;
mod handle;
mod model;
mod rng;
mod stream;
mod time;

pub use config::{
    ActionsConfig, ApiConfig, ApiErrors, CatalogChurn, ChurnConfig, ClockConfig, ClockMode,
    ClockSkew, Config, CustomerChurn, DestinationConfig, DisableAfter, Distribution, DurationStr,
    EventRetentionConfig, EventsConfig, HistoryConfig, LateTail, PINNED_API_VERSION, PayloadFormat,
    PayloadsConfig, RateLimit, RenewMode, RetrySchedule, UpdateChurn, WebhooksConfig,
};
pub use delivery::signing::verify_signature;
pub use delivery::{
    AbandonReason, AttemptOutcome, DeliveryRecord, DeliveryRecordKind, DestinationInfo,
    SignatureFault,
};
pub use engine::events::{EventFormat, EventInfo};
pub use engine::{Cause, ChangeKind, EventRefs, StateUpdate};
pub use error::MockError;
pub use generate::{Action, InvoiceOutcome};
pub use handle::MockStripe;
pub use model::ObjectType;
pub use model::customer::{CustomerUpdate, NewCustomer};
pub use stream::{SubscribeFilter, SubscribeOptions, Subscription, SubscriptionItem};
pub use time::SimTime;
