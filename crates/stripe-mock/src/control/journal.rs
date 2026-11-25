//! The run journal and its export format.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::control::ControlOp;
use crate::{Config, MockError, SimTime};

/// The `format` of a [`RunExport`].
pub const RUN_FORMAT: &str = "ductkit-stripe-mock/run@1";

/// One journalled op, at the simulated time it was applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub at: SimTime,
    pub op: ControlOp,
}

/// A run as a replayable script: the start config and every journalled op.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunExport {
    /// Always [`RUN_FORMAT`].
    pub format: String,
    pub seed: u64,
    /// The config the run started with.
    pub config: Config,
    /// The simulated time when the export was taken.
    pub ended_at: SimTime,
    pub journal: Vec<JournalEntry>,
}

impl RunExport {
    /// Writes the export as pretty JSON.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Reads an export written by [`RunExport::save`].
    pub fn load(path: impl AsRef<Path>) -> Result<RunExport, MockError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| MockError::Config(format!("run file {}: {e}", path.display())))?;
        check_format(value.get("format").and_then(|f| f.as_str()))?;
        serde_json::from_value(value)
            .map_err(|e| MockError::Config(format!("run file {}: {e}", path.display())))
    }

    pub(crate) fn check_format(&self) -> Result<(), MockError> {
        check_format(Some(&self.format))
    }
}

fn check_format(format: Option<&str>) -> Result<(), MockError> {
    match format {
        Some(RUN_FORMAT) => Ok(()),
        Some(other) => Err(MockError::Config(format!(
            "unsupported run format `{other}` (expected `{RUN_FORMAT}`)"
        ))),
        None => Err(MockError::Config(format!(
            "unsupported run format: no `format` key (expected `{RUN_FORMAT}`)"
        ))),
    }
}
