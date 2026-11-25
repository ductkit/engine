/// Errors returned by the mock's Rust API.
#[derive(Debug, thiserror::Error)]
pub enum MockError {
    /// The configuration is invalid or uses something not implemented yet.
    #[error("invalid config: {0}")]
    Config(String),
    /// Reading a config file failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
