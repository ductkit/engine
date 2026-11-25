//! Stateful mock of the Stripe API for testing ductkit sync.
//!
//! See `docs/stripe-mock-spec.md` in the repository.

mod config;
mod error;

pub use config::{
    ActionsConfig, ApiConfig, ChurnConfig, ClockConfig, ClockMode, Config, CustomerChurn,
    PINNED_API_VERSION,
};
pub use error::MockError;
