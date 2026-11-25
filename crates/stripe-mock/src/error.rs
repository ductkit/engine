/// Errors returned by the mock's Rust API.
#[derive(Debug, thiserror::Error)]
pub enum MockError {
    /// The configuration is invalid or uses something not implemented yet.
    #[error("invalid config: {0}")]
    Config(String),
    /// The operation needs the other clock mode.
    #[error("{0}")]
    ClockMode(String),
    /// Asked to move simulated time backwards.
    #[error("cannot move time back to {requested}; current time is {now}")]
    TimeTravel {
        requested: crate::SimTime,
        now: crate::SimTime,
    },
    /// No object with this ID exists.
    #[error("no such {object_type}: {id}")]
    NoSuchObject {
        object_type: crate::ObjectType,
        id: String,
    },
    /// The object exists but has been deleted.
    #[error("{object_type} {id} is deleted")]
    Deleted {
        object_type: crate::ObjectType,
        id: String,
    },
    /// No destination with this ID exists.
    #[error("no such destination: {0}")]
    NoSuchDestination(String),
    /// Reading a config file failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
