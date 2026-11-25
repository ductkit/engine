/// Errors returned by the mock's Rust API.
#[derive(Debug, thiserror::Error)]
pub enum MockError {
    /// The configuration is invalid or uses something not implemented yet.
    #[error("invalid config: {0}")]
    Config(String),
    /// Asked to move simulated time backwards.
    #[error("cannot move time back to {requested}; current time is {now}")]
    TimeTravel {
        requested: crate::SimTime,
        now: crate::SimTime,
    },
    /// Reading a config file failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
