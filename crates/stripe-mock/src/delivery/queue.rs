//! Delivery scheduling, the per-delivery state machine and the delivery log (spec sections 9, 10.5).

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;

use crate::delivery::Destinations;
use crate::engine::events::{EventFormat, EventRecord, EventStore, thin_notification};
use crate::rng::{RngStreams, SimRng};
use crate::{SimTime, WebhooksConfig};

/// One delivery-log entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeliveryRecord {
    /// Gap-free, starting at 1.
    pub seq: u64,
    pub at: SimTime,
    pub delivery_id: String,
    pub event_id: String,
    pub destination_id: String,
    pub kind: DeliveryRecordKind,
}

/// What a [`DeliveryRecord`] records.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeliveryRecordKind {
    /// A delivery was queued for its first attempt.
    Scheduled {
        attempt_at: SimTime,
        duplicate: bool,
        jitter_ms: i64,
        object_version: u64,
    },
    /// One HTTP attempt finished.
    Attempt {
        /// 1-based.
        attempt: u32,
        outcome: AttemptOutcome,
        elapsed_ms: u64,
        /// The object version the event carries.
        object_version: u64,
        /// The object's latest version when the attempt finished.
        latest_version: u64,
        signature: SignatureFault,
        next_retry_at: Option<SimTime>,
    },
    /// No further attempts will be made.
    Abandoned { reason: AbandonReason },
    /// The destination was disabled after failing for too long.
    DestinationDisabled,
}

/// The result of one attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// A 2xx response.
    Success { status: u16 },
    /// Any other response status.
    HttpError { status: u16 },
    /// No response within `response_timeout`.
    Timeout,
    /// The connection failed.
    ConnectError { message: String },
    /// A simulated outage; no request was sent.
    Outage,
}

/// A deliberate signature fault on one attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignatureFault {
    None,
    /// The timestamp is off by `secs`, outside the tolerance.
    Skewed {
        secs: i64,
    },
    /// Signed with a wrong secret.
    Invalid,
}

/// Why a delivery was abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AbandonReason {
    RetriesExhausted,
    DestinationDisabled,
    DestinationRemoved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Queued,
    InFlight,
    /// The last attempt failed and no retry is queued.
    Failed,
    Succeeded,
    Abandoned,
}

#[derive(Debug, Clone)]
struct Delivery {
    id: String,
    event_id: String,
    destination_id: String,
    duplicate: bool,
    attempts: u32,
    state: State,
    object_version: u64,
}

/// One attempt ready to send, built under the sim lock.
#[derive(Debug, Clone)]
pub(crate) struct PreparedAttempt {
    pub index: usize,
    pub delivery_id: String,
    pub event_id: String,
    pub destination_id: String,
    pub url: String,
    pub body: Vec<u8>,
    pub secrets: Vec<String>,
    pub signature: SignatureFault,
    pub attempt: u32,
    pub scheduled_at: SimTime,
    pub timeout_ms: u64,
    pub object_version: u64,
}

/// Every delivery, its queue and the log.
#[derive(Debug)]
pub(crate) struct Deliveries {
    pub config: WebhooksConfig,
    list: Vec<Delivery>,
    by_event: HashMap<String, Vec<usize>>,
    /// `(due, index)`; `index` breaks ties in scheduling order.
    queue: BTreeSet<(SimTime, usize)>,
    log: Vec<DeliveryRecord>,
    delay: SimRng,
    reorder: SimRng,
    duplicate: SimRng,
    faults: SimRng,
}

impl Deliveries {
    pub(crate) fn new(config: &WebhooksConfig, streams: &RngStreams) -> Self {
        Deliveries {
            config: config.clone(),
            list: Vec::new(),
            by_event: HashMap::new(),
            queue: BTreeSet::new(),
            log: Vec::new(),
            delay: streams.stream("webhooks.delay"),
            reorder: streams.stream("webhooks.reorder"),
            duplicate: streams.stream("webhooks.duplicate"),
            faults: streams.stream("webhooks.faults"),
        }
    }

    fn push_log(&mut self, at: SimTime, index: usize, kind: DeliveryRecordKind) {
        let d = &self.list[index];
        let record = DeliveryRecord {
            seq: self.log.len() as u64 + 1,
            at,
            delivery_id: d.id.clone(),
            event_id: d.event_id.clone(),
            destination_id: d.destination_id.clone(),
            kind,
        };
        self.log.push(record);
    }

    /// Queues a delivery of `ev` to every matching enabled destination, plus random duplicates.
    pub(crate) fn schedule_for_event(&mut self, ev: &EventRecord, dests: &Destinations) {
        for destination_id in dests.matching(ev.format, &ev.event_type) {
            self.schedule_one(ev, &destination_id, false);
            if self.duplicate.chance(self.config.duplicate_probability) {
                self.schedule_one(ev, &destination_id, true);
            }
        }
    }

    fn schedule_one(&mut self, ev: &EventRecord, destination_id: &str, duplicate: bool) {
        let delay = self.delay.sample_ms(&self.config.delivery_delay);
        let jitter = self.reorder.uniform_ms(0, self.config.reorder_window.ms());
        let attempt_at = ev.created.plus_ms(delay + jitter);
        let index = self.list.len();
        self.list.push(Delivery {
            id: format!("dlv_{}", index + 1),
            event_id: ev.id.clone(),
            destination_id: destination_id.to_string(),
            duplicate,
            attempts: 0,
            state: State::Queued,
            object_version: ev.object_version,
        });
        self.by_event.entry(ev.id.clone()).or_default().push(index);
        self.queue.insert((attempt_at, index));
        self.push_log(
            ev.created,
            index,
            DeliveryRecordKind::Scheduled {
                attempt_at,
                duplicate,
                jitter_ms: jitter,
                object_version: ev.object_version,
            },
        );
    }

    /// The earliest queued attempt time.
    pub(crate) fn next_due(&self) -> Option<SimTime> {
        self.queue.first().map(|(t, _)| *t)
    }

    /// Takes every attempt due at the earliest due time `<= upto` and marks them in flight.
    /// Deliveries whose destination is gone are abandoned instead.
    pub(crate) fn due_batch(
        &mut self,
        upto: SimTime,
        events: &EventStore,
        dests: &Destinations,
    ) -> Option<(SimTime, Vec<PreparedAttempt>)> {
        let due = self.next_due().filter(|d| *d <= upto)?;
        let mut batch = Vec::new();
        while let Some(&(t, index)) = self.queue.first() {
            if t != due {
                break;
            }
            self.queue.pop_first();
            let Some(dest) = dests.get(&self.list[index].destination_id) else {
                self.list[index].state = State::Abandoned;
                self.push_log(
                    due,
                    index,
                    DeliveryRecordKind::Abandoned {
                        reason: AbandonReason::DestinationRemoved,
                    },
                );
                continue;
            };
            let signature = self.draw_fault();
            let d = &mut self.list[index];
            d.attempts += 1;
            d.state = State::InFlight;
            let d = d.clone();
            let ev = events
                .get(&d.event_id)
                .expect("delivered events are stored");
            let body = match ev.format {
                EventFormat::Snapshot => {
                    let mut v = ev.payload.clone();
                    v["pending_webhooks"] = self.pending_for_event(&d.event_id).into();
                    v
                }
                EventFormat::Thin => thin_notification(&ev.payload),
            };
            batch.push(PreparedAttempt {
                index,
                delivery_id: d.id,
                event_id: d.event_id,
                destination_id: d.destination_id,
                url: dest.info.url.clone(),
                body: serde_json::to_vec(&body).expect("JSON serializes"),
                secrets: dest.signing_secrets(due),
                signature,
                attempt: d.attempts,
                scheduled_at: due,
                timeout_ms: self.config.response_timeout.ms().max(0) as u64,
                object_version: d.object_version,
            });
        }
        Some((due, batch))
    }

    fn draw_fault(&mut self) -> SignatureFault {
        let skew = &self.config.clock_skew;
        if self.faults.chance(skew.probability) {
            let negative = self.faults.below(2) == 0;
            let min = self.config.signature_tolerance.ms() + 1000;
            let secs = self.faults.uniform_ms(min, skew.max.ms()) / 1000;
            SignatureFault::Skewed {
                secs: if negative { -secs } else { secs },
            }
        } else if self
            .faults
            .chance(self.config.invalid_signature_probability)
        {
            SignatureFault::Invalid
        } else {
            SignatureFault::None
        }
    }

    /// Logs a finished attempt. Returns whether it succeeded.
    pub(crate) fn record_outcome(
        &mut self,
        a: &PreparedAttempt,
        outcome: AttemptOutcome,
        elapsed_ms: u64,
        now: SimTime,
        latest_version: u64,
    ) -> bool {
        let success = matches!(outcome, AttemptOutcome::Success { .. });
        self.list[a.index].state = if success {
            State::Succeeded
        } else {
            State::Failed
        };
        self.push_log(
            now,
            a.index,
            DeliveryRecordKind::Attempt {
                attempt: a.attempt,
                outcome,
                elapsed_ms,
                object_version: a.object_version,
                latest_version,
                signature: a.signature.clone(),
                next_retry_at: None,
            },
        );
        success
    }

    pub(crate) fn log(&self) -> &[DeliveryRecord] {
        &self.log
    }

    /// Non-duplicate deliveries of the event that have not succeeded.
    pub(crate) fn pending_for_event(&self, event_id: &str) -> usize {
        self.by_event.get(event_id).map_or(0, |v| {
            v.iter()
                .filter(|i| {
                    let d = &self.list[**i];
                    !d.duplicate && d.state != State::Succeeded
                })
                .count()
        })
    }
}
