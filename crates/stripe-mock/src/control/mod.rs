//! Control ops: one serializable form of every mutating control call, applied by
//! [`MockStripe::apply`], run as scripts, journalled, exported and replayed.

pub(crate) mod http;
pub(crate) mod journal;

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use journal::{JournalEntry, RUN_FORMAT, RunExport};

use crate::delivery::DestinationInfo;
use crate::engine::Cause;
use crate::generate::Action;
use crate::model::customer::{CustomerUpdate, NewCustomer};
use crate::{
    AccountApi, ActionsConfig, ApiConfig, ClockMode, DestinationConfig, DurationStr, EventsConfig,
    MockError, MockStripe, ObjectType, SimTime, WebhooksConfig,
};

/// A mutating control call. `account: None` is the platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ControlOp {
    CreateCustomer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        customer: NewCustomer,
    },
    UpdateCustomer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        id: String,
        update: CustomerUpdate,
    },
    DeleteCustomer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        id: String,
    },
    RunAction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        action: Action,
    },
    ConnectAccount {
        api: AccountApi,
    },
    DisconnectAccount {
        id: String,
    },
    SetApiConfig {
        config: ApiConfig,
    },
    SetEventsConfig {
        config: EventsConfig,
    },
    SetWebhooksConfig {
        config: WebhooksConfig,
    },
    SetActionsConfig {
        config: ActionsConfig,
    },
    AddDestination {
        destination: DestinationConfig,
    },
    RemoveDestination {
        id: String,
    },
    SetDestinationEnabled {
        id: String,
        enabled: bool,
    },
    StartOutage {
        id: String,
        duration: DurationStr,
    },
    EndOutage {
        id: String,
    },
    RotateSecret {
        id: String,
        overlap: DurationStr,
    },
    Step {
        duration: DurationStr,
    },
    AdvanceTo {
        time: DateTime<Utc>,
    },
    Quiesce,
    ResumeGeneration,
    /// Journal only: a write an API list request fired between pages.
    PaginationWrite {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        object_type: ObjectType,
    },
}

impl ControlOp {
    /// Ops on destinations, which replay skips with `drop_destinations`.
    fn is_destination_op(&self) -> bool {
        matches!(
            self,
            ControlOp::AddDestination { .. }
                | ControlOp::RemoveDestination { .. }
                | ControlOp::SetDestinationEnabled { .. }
                | ControlOp::StartOutage { .. }
                | ControlOp::EndOutage { .. }
                | ControlOp::RotateSecret { .. }
        )
    }
}

/// What an applied [`ControlOp`] returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum OpResult {
    Done,
    Customer { id: String },
    Version { version: Option<u64> },
    Cause { cause: Cause },
    Account { id: String },
    Destination { destination: DestinationInfo },
    Secret { secret: String },
}

/// A script stopped at its first failing op.
#[derive(Debug, thiserror::Error)]
#[error("script failed after {completed} completed steps: {error}")]
pub struct ScriptError {
    /// Ops applied before the failing one.
    pub completed: usize,
    #[source]
    pub error: MockError,
}

/// How [`MockStripe::replay`] treats the export.
#[derive(Debug, Clone, Default)]
pub struct ReplayOptions {
    /// Starts without the export's destinations and skips every destination op,
    /// so the replay sends no webhooks.
    pub drop_destinations: bool,
}

pub(crate) fn duration(d: DurationStr) -> Result<Duration, MockError> {
    u64::try_from(d.ms())
        .map(Duration::from_millis)
        .map_err(|_| MockError::Config(format!("duration must not be negative, got {} ms", d.ms())))
}

impl MockStripe {
    /// Applies one op through the matching control method, which journals it.
    pub async fn apply(&self, op: ControlOp) -> Result<OpResult, MockError> {
        use ControlOp as O;
        Ok(match op {
            O::CreateCustomer { account, customer } => OpResult::Customer {
                id: self.create_customer_in(&account, customer)?,
            },
            O::UpdateCustomer {
                account,
                id,
                update,
            } => OpResult::Version {
                version: self.update_customer_in(&account, &id, update)?,
            },
            O::DeleteCustomer { account, id } => {
                self.delete_customer_in(&account, &id)?;
                OpResult::Done
            }
            O::RunAction { account, action } => OpResult::Cause {
                cause: self.run_action_in(&account, action)?,
            },
            O::ConnectAccount { api } => OpResult::Account {
                id: self.connect_account(api)?,
            },
            O::DisconnectAccount { id } => {
                self.disconnect_account(&id)?;
                OpResult::Done
            }
            O::SetApiConfig { config } => {
                self.update_api_config(|c| *c = config)?;
                OpResult::Done
            }
            O::SetEventsConfig { config } => {
                self.update_events_config(|c| *c = config)?;
                OpResult::Done
            }
            O::SetWebhooksConfig { config } => {
                self.update_webhooks_config(|c| *c = config)?;
                OpResult::Done
            }
            O::SetActionsConfig { config } => {
                self.update_actions_config(|c| *c = config)?;
                OpResult::Done
            }
            O::AddDestination { destination } => OpResult::Destination {
                destination: self.add_destination(destination)?,
            },
            O::RemoveDestination { id } => {
                self.remove_destination(&id)?;
                OpResult::Done
            }
            O::SetDestinationEnabled { id, enabled } => {
                self.set_destination_enabled(&id, enabled)?;
                OpResult::Done
            }
            O::StartOutage { id, duration: d } => {
                self.start_outage(&id, duration(d)?)?;
                OpResult::Done
            }
            O::EndOutage { id } => {
                self.end_outage(&id)?;
                OpResult::Done
            }
            O::RotateSecret { id, overlap } => OpResult::Secret {
                secret: self.rotate_secret(&id, duration(overlap)?)?,
            },
            O::Step { duration: d } => {
                self.step(duration(d)?).await?;
                OpResult::Done
            }
            O::AdvanceTo { time } => {
                self.advance_to(SimTime::from_datetime(time)).await?;
                OpResult::Done
            }
            O::Quiesce => {
                self.quiesce().await?;
                OpResult::Done
            }
            O::ResumeGeneration => {
                self.resume_generation();
                OpResult::Done
            }
            O::PaginationWrite { .. } => {
                return Err(MockError::InvalidAction(
                    "pagination_write is recorded by the mock, not applied by clients".to_string(),
                ));
            }
        })
    }

    /// Applies `steps` in order, stopping at the first error.
    pub async fn run_script(&self, steps: &[ControlOp]) -> Result<Vec<OpResult>, ScriptError> {
        let mut results = Vec::with_capacity(steps.len());
        for (completed, op) in steps.iter().enumerate() {
            match self.apply(op.clone()).await {
                Ok(r) => results.push(r),
                Err(error) => return Err(ScriptError { completed, error }),
            }
        }
        Ok(results)
    }

    /// The run so far: the start config and the journal.
    pub fn export_run(&self) -> RunExport {
        let (ended_at, journal) = {
            // Also used while unwinding (`ReplayGuard`), so a poisoned lock is read.
            let sim = self
                .shared
                .sim
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (sim.engine.now(), sim.journal.clone())
        };
        RunExport {
            format: RUN_FORMAT.to_string(),
            seed: self.shared.config.seed,
            config: self.shared.config.clone(),
            ended_at,
            journal,
        }
    }

    /// Re-runs an export on a new virtual-clock mock: each op at its time, then up
    /// to `ended_at`. The truth log and event timeline match the original run's.
    pub async fn replay(
        export: RunExport,
        options: ReplayOptions,
    ) -> Result<MockStripe, MockError> {
        export.check_format()?;
        let mut config = export.config;
        config.seed = export.seed;
        config.clock.mode = ClockMode::Virtual;
        if options.drop_destinations {
            config.destinations.clear();
        }
        let m = MockStripe::start(config).await?;
        for entry in export.journal {
            if options.drop_destinations && entry.op.is_destination_op() {
                continue;
            }
            m.advance_unjournalled(entry.at).await?;
            match entry.op {
                ControlOp::PaginationWrite {
                    account,
                    object_type,
                } => m.pagination_write(&account, object_type)?,
                op => {
                    m.apply(op).await?;
                }
            }
        }
        m.advance_unjournalled(export.ended_at).await?;
        Ok(m)
    }

    /// Exports the run to `dir` if dropped while panicking, e.g. by a failing test.
    pub fn replay_guard(&self, dir: impl AsRef<Path>) -> ReplayGuard<'_> {
        ReplayGuard {
            mock: self,
            dir: dir.as_ref().to_path_buf(),
        }
    }
}

/// From [`MockStripe::replay_guard`]. On drop during a panic it writes
/// `<dir>/run-<seed>-<ended_at ms>.json` and prints the replay command to stderr.
#[derive(Debug)]
pub struct ReplayGuard<'a> {
    mock: &'a MockStripe,
    dir: PathBuf,
}

impl Drop for ReplayGuard<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let export = self.mock.export_run();
        let path = self
            .dir
            .join(format!("run-{}-{}.json", export.seed, export.ended_at.0));
        let saved = std::fs::create_dir_all(&self.dir).and_then(|()| export.save(&path));
        match saved {
            Ok(()) => eprintln!(
                "stripe-mock: run exported to {p}; replay with: ductkit-stripe-mock replay --run {p}",
                p = path.display()
            ),
            Err(e) => eprintln!(
                "stripe-mock: could not export the run to {}: {e}",
                path.display()
            ),
        }
    }
}
