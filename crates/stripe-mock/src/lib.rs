//! Stateful mock of the Stripe API for testing ductkit sync.
//!
//! See `docs/stripe-mock-spec.md` in the repository.

mod api;
pub mod cli;
mod config;
mod control;
mod delivery;
mod engine;
mod error;
mod faults;
mod generate;
mod handle;
mod model;
mod rng;
mod stream;
pub mod stripe;
mod time;

pub use config::{
    AccountApiMix, AccountTypeMix, AccountsConfig, ActionsConfig, ApiConfig, ApiErrors,
    ApplicationFeeConfig, CatalogChurn, ChargePattern, ChurnConfig, ClockConfig, ClockMode,
    ClockSkew, Config, ConnectedAccounts, CustomerChurn, DestinationConfig, DisableAfter,
    Distribution, DurationStr, EventRetentionConfig, EventsConfig, ExternalAccountChurn,
    HistoryConfig, LateTail, MAX_DURATION_MS, MAX_SPEED, PINNED_API_VERSION, PaginationWrites,
    PayloadFormat, PayloadsConfig, PaymentMethodChurn, PayoutMode, PersonChurn,
    PlatformPaymentsMix, RateLimit, RenewMode, RetrySchedule, StaleRead, UpdateChurn,
    WebhooksConfig,
};
pub use control::{
    ControlOp, JournalEntry, OpResult, RUN_FORMAT, ReplayGuard, ReplayOptions, RunExport,
    ScriptError,
};
pub use delivery::signing::verify_signature;
pub use delivery::{
    AbandonReason, AttemptOutcome, DeliveryRecord, DeliveryRecordKind, DestinationInfo,
    SignatureFault,
};
pub use engine::events::{EventFormat, EventInfo};
pub use engine::{Cause, ChangeKind, EventRefs, Scope, StateUpdate};
pub use error::MockError;
pub use generate::{Action, CheckoutMode, DisputeOutcome, ExternalAccountKind, InvoiceOutcome};
pub use handle::{AccountHandle, AccountInfo, MockStripe, StartOptions};
pub use model::ObjectType;
pub use model::account::{AccountApi, AccountKind, Configuration};
pub use model::customer::{CustomerUpdate, NewCustomer};
pub use stream::{SubscribeFilter, SubscribeOptions, Subscription, SubscriptionItem};
pub use time::SimTime;

/// The `expand[]` paths a retrieve of `t` allows (lists take them after `data.`). For tests
/// that cover every path; not a stable API.
#[doc(hidden)]
pub fn expand_paths(t: ObjectType) -> &'static [&'static str] {
    api::expand::allowed(t)
}
