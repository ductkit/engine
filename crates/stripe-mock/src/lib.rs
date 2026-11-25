//! Stateful mock of the Stripe API for testing ductkit sync.
//!
//! See `docs/stripe-mock-spec.md` in the repository.

mod config;
mod error;
// Used from Task 5 on; remove this allow once the generator uses every helper.
#[allow(dead_code)]
mod rng;
mod time;

pub use config::{
    ActionsConfig, ApiConfig, ChurnConfig, ClockConfig, ClockMode, Config, CustomerChurn,
    PINNED_API_VERSION,
};
pub use error::MockError;
pub use time::SimTime;
