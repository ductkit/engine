//! Delivery targets and webhook delivery (spec section 9).

pub(crate) mod queue;
pub(crate) mod sender;
pub(crate) mod signing;

pub use queue::{
    AbandonReason, AttemptOutcome, DeliveryRecord, DeliveryRecordKind, SignatureFault,
};
pub(crate) use queue::{Deliveries, PreparedAttempt};

use crate::engine::events::EventFormat;
use crate::rng::{RngStreams, SimRng};
use crate::{DestinationConfig, MockError, PayloadFormat, RetrySchedule, SimTime};

/// Public view of a destination, including its current signing secret.
#[derive(Debug, Clone, PartialEq)]
pub struct DestinationInfo {
    pub id: String,
    pub name: String,
    pub url: String,
    pub payload: PayloadFormat,
    pub enabled_events: Vec<String>,
    pub enabled: bool,
    pub secret: String,
    pub created: SimTime,
}

/// One delivery target with its runtime state.
#[derive(Debug, Clone)]
pub(crate) struct Destination {
    pub info: DestinationInfo,
    /// Previous secrets that still sign until their expiry.
    pub old_secrets: Vec<(String, SimTime)>,
    pub outage_until: Option<SimTime>,
    pub first_failure_since_success: Option<SimTime>,
}

impl Destination {
    /// Secrets that sign at `now`: the current one first, then unexpired old ones.
    pub(crate) fn signing_secrets(&self, now: SimTime) -> Vec<String> {
        let mut v = vec![self.info.secret.clone()];
        v.extend(
            self.old_secrets
                .iter()
                .filter(|(_, until)| *until > now)
                .map(|(s, _)| s.clone()),
        );
        v
    }

    pub(crate) fn format(&self) -> EventFormat {
        match self.info.payload {
            PayloadFormat::Snapshot => EventFormat::Snapshot,
            PayloadFormat::Thin => EventFormat::Thin,
        }
    }
}

/// Delays (ms) after attempts 1, 2, ...; the end of the iterator means no more retries.
pub(crate) fn retry_delays(schedule: &RetrySchedule) -> Box<dyn Iterator<Item = i64> + '_> {
    match schedule {
        RetrySchedule::StripeLike => Box::new(
            [60_000, 300_000, 1_800_000, 7_200_000, 18_000_000]
                .into_iter()
                .chain(std::iter::repeat(36_000_000)),
        ),
        RetrySchedule::Explicit(list) => Box::new(list.iter().map(|d| d.ms())),
    }
}

/// `pattern` is `*`, an exact event type, or a `prefix.*` wildcard.
pub(crate) fn event_matches(pattern: &str, event_type: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => event_type.starts_with(prefix),
        None => pattern == event_type,
    }
}

/// All destinations, in creation order.
#[derive(Debug)]
pub(crate) struct Destinations {
    list: Vec<Destination>,
    ids: SimRng,
    secrets: SimRng,
}

impl Destinations {
    pub(crate) fn new(streams: &RngStreams) -> Self {
        Destinations {
            list: Vec::new(),
            ids: streams.stream("destinations.ids"),
            secrets: streams.stream("destinations.secrets"),
        }
    }

    fn new_secret(&mut self) -> String {
        self.secrets.id_len("whsec", 32)
    }

    /// Validates and adds a destination.
    pub(crate) fn add(
        &mut self,
        d: DestinationConfig,
        now: SimTime,
    ) -> Result<DestinationInfo, MockError> {
        d.validate()?;
        if self.list.iter().any(|x| x.info.name == d.name) {
            return Err(MockError::Config(format!(
                "destination name `{}` is used twice",
                d.name
            )));
        }
        let id = match d.payload {
            PayloadFormat::Snapshot => self.ids.id_len("we", 24),
            PayloadFormat::Thin => self.ids.id_len("ed_test", 24),
        };
        let secret = match d.secret {
            Some(s) => s,
            None => self.new_secret(),
        };
        let info = DestinationInfo {
            id,
            name: d.name,
            url: d.url,
            payload: d.payload,
            enabled_events: d.enabled_events,
            enabled: true,
            secret,
            created: now,
        };
        self.list.push(Destination {
            info: info.clone(),
            old_secrets: Vec::new(),
            outage_until: None,
            first_failure_since_success: None,
        });
        Ok(info)
    }

    pub(crate) fn remove(&mut self, id: &str) -> Result<Destination, MockError> {
        let i = self
            .list
            .iter()
            .position(|d| d.info.id == id)
            .ok_or_else(|| MockError::NoSuchDestination(id.to_string()))?;
        Ok(self.list.remove(i))
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Destination> {
        self.list.iter().find(|d| d.info.id == id)
    }

    pub(crate) fn get_mut(&mut self, id: &str) -> Result<&mut Destination, MockError> {
        self.list
            .iter_mut()
            .find(|d| d.info.id == id)
            .ok_or_else(|| MockError::NoSuchDestination(id.to_string()))
    }

    /// Replaces the secret; the old one keeps signing until `until`.
    pub(crate) fn rotate_secret(&mut self, id: &str, until: SimTime) -> Result<String, MockError> {
        let secret = self.new_secret();
        let d = self.get_mut(id)?;
        let old = std::mem::replace(&mut d.info.secret, secret.clone());
        d.old_secrets.push((old, until));
        Ok(secret)
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, Destination> {
        self.list.iter()
    }

    /// IDs of enabled destinations of `format` subscribed to `event_type`.
    pub(crate) fn matching(&self, format: EventFormat, event_type: &str) -> Vec<String> {
        self.list
            .iter()
            .filter(|d| {
                d.info.enabled
                    && d.format() == format
                    && d.info
                        .enabled_events
                        .iter()
                        .any(|p| event_matches(p, event_type))
            })
            .map(|d| d.info.id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_schedules() {
        let stripe: Vec<i64> = retry_delays(&RetrySchedule::StripeLike).take(7).collect();
        assert_eq!(
            stripe,
            [
                60_000, 300_000, 1_800_000, 7_200_000, 18_000_000, 36_000_000, 36_000_000
            ]
        );
        let explicit = RetrySchedule::Explicit(vec![crate::DurationStr(5), crate::DurationStr(7)]);
        assert_eq!(retry_delays(&explicit).collect::<Vec<_>>(), [5, 7]);
    }

    #[test]
    fn patterns() {
        assert!(event_matches("*", "customer.created"));
        assert!(event_matches("customer.*", "customer.deleted"));
        assert!(event_matches("v1.customer.*", "v1.customer.updated"));
        assert!(!event_matches("customer.*", "v1.customer.updated"));
        assert!(!event_matches("customer.created", "customer.updated"));
    }
}
