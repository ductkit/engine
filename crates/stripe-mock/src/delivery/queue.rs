//! Delivery scheduling, the per-delivery state machine and the delivery log (spec sections 9, 10.5).

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;

use crate::delivery::{Destinations, retry_delays};
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
    /// When the queued attempt is due (its queue key).
    next_at: Option<SimTime>,
    /// When the next attempt is nominally due on the retry schedule; equal to
    /// `next_at` except while quiescing.
    timeline_at: SimTime,
    first_attempt_at: Option<SimTime>,
}

/// One attempt ready to send, built under the sim lock.
#[derive(Debug, Clone)]
pub(crate) struct PreparedAttempt {
    pub index: usize,
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
    /// The destination is in a simulated outage: record `Outage` without sending.
    pub outage: bool,
}

/// Every delivery, its queue and the log.
#[derive(Debug)]
pub(crate) struct Deliveries {
    pub config: WebhooksConfig,
    /// Retries are queued at `now` instead of after their backoff.
    pub quiescing: bool,
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
            quiescing: false,
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
        for destination_id in dests.matching(ev.format, &ev.event_type, ev.account.as_deref()) {
            self.schedule_one(ev, &destination_id, false);
            if self.duplicate.chance(self.config.duplicate_probability) {
                self.schedule_one(ev, &destination_id, true);
            }
        }
    }

    fn schedule_one(&mut self, ev: &EventRecord, destination_id: &str, duplicate: bool) {
        let delay = self.delay.sample_ms(&self.config.delivery_delay);
        let jitter = self.reorder.uniform_ms(0, self.config.reorder_window.ms());
        let attempt_at = ev.created.plus_ms(delay.saturating_add(jitter));
        let index = self.list.len();
        self.list.push(Delivery {
            id: format!("dlv_{}", index + 1),
            event_id: ev.id.clone(),
            destination_id: destination_id.to_string(),
            duplicate,
            attempts: 0,
            state: State::Queued,
            object_version: ev.object_version,
            next_at: Some(attempt_at),
            timeline_at: attempt_at,
            first_attempt_at: None,
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
            self.list[index].next_at = None;
            let dest = match dests.get(&self.list[index].destination_id) {
                None => {
                    self.abandon(index, AbandonReason::DestinationRemoved, due);
                    continue;
                }
                Some(d) if !d.info.enabled => {
                    self.abandon(index, AbandonReason::DestinationDisabled, due);
                    continue;
                }
                Some(d) => d,
            };
            let signature = self.draw_fault();
            let d = &mut self.list[index];
            d.attempts += 1;
            d.state = State::InFlight;
            // Nominal time: quiesce may send early, but the retry window follows the schedule.
            d.first_attempt_at.get_or_insert(d.timeline_at);
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
                event_id: d.event_id,
                destination_id: d.destination_id,
                url: dest.info.url.clone(),
                body: serde_json::to_vec(&body).expect("JSON serializes"),
                secrets: dest.signing_secrets(due),
                signature,
                attempt: d.attempts,
                scheduled_at: d.timeline_at,
                timeout_ms: self.config.response_timeout.ms().max(0) as u64,
                object_version: d.object_version,
                outage: dest.outage_until.is_some_and(|until| until > due),
            });
        }
        Some((due, batch))
    }

    fn draw_fault(&mut self) -> SignatureFault {
        let skew = &self.config.clock_skew;
        if self.faults.chance(skew.probability) {
            let negative = self.faults.below(2) == 0;
            let min = self.config.signature_tolerance.ms().saturating_add(1000);
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

    fn abandon(&mut self, index: usize, reason: AbandonReason, now: SimTime) {
        let d = &mut self.list[index];
        if let Some(t) = d.next_at.take() {
            self.queue.remove(&(t, index));
        }
        d.state = State::Abandoned;
        self.push_log(now, index, DeliveryRecordKind::Abandoned { reason });
    }

    /// Abandons every queued delivery to `destination_id`. In-flight ones are
    /// abandoned when their result arrives.
    pub(crate) fn abandon_for_destination(
        &mut self,
        destination_id: &str,
        reason: AbandonReason,
        now: SimTime,
    ) {
        let queued: Vec<usize> = (0..self.list.len())
            .filter(|i| {
                let d = &self.list[*i];
                d.destination_id == destination_id && d.state == State::Queued
            })
            .collect();
        for index in queued {
            self.abandon(index, reason, now);
        }
    }

    /// Logs a finished attempt at `now` and decides what follows: done, a retry,
    /// abandonment, or disabling the destination.
    pub(crate) fn record_outcome(
        &mut self,
        a: &PreparedAttempt,
        outcome: AttemptOutcome,
        elapsed_ms: u64,
        now: SimTime,
        latest_version: u64,
        dests: &mut Destinations,
    ) {
        let success = matches!(outcome, AttemptOutcome::Success { .. });
        // While quiescing, `now` stands still; failures count on the nominal timeline.
        let effective = now.max(a.scheduled_at);
        let mut next_retry_at = None;
        let mut abandon = None;
        let mut disable = false;
        match dests.get_mut(&a.destination_id).ok() {
            Some(d) if success => {
                self.list[a.index].state = State::Succeeded;
                d.first_failure_since_success = None;
            }
            _ if success => self.list[a.index].state = State::Succeeded,
            None => abandon = Some(AbandonReason::DestinationRemoved),
            Some(d) if !d.info.enabled => abandon = Some(AbandonReason::DestinationDisabled),
            Some(d) => {
                let since = *d.first_failure_since_success.get_or_insert(effective);
                if effective.0.saturating_sub(since.0) >= self.config.disable_after_failures.ms() {
                    d.info.enabled = false;
                    disable = true;
                } else {
                    let first = self.list[a.index]
                        .first_attempt_at
                        .unwrap_or(a.scheduled_at);
                    let delay = retry_delays(&self.config.retry_schedule)
                        .nth(a.attempt.saturating_sub(1) as usize);
                    match delay {
                        Some(ms)
                            if a.scheduled_at.0.saturating_add(ms).saturating_sub(first.0)
                                <= self.config.max_retry_duration.ms() =>
                        {
                            let nominal = a.scheduled_at.plus_ms(ms);
                            self.list[a.index].timeline_at = nominal;
                            next_retry_at = Some(if self.quiescing { now } else { nominal });
                        }
                        _ => abandon = Some(AbandonReason::RetriesExhausted),
                    }
                }
            }
        }
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
                next_retry_at,
            },
        );
        if let Some(t) = next_retry_at {
            let d = &mut self.list[a.index];
            d.state = State::Queued;
            d.next_at = Some(t);
            self.queue.insert((t, a.index));
        }
        if let Some(reason) = abandon {
            self.abandon(a.index, reason, now);
        }
        if disable {
            self.push_log(now, a.index, DeliveryRecordKind::DestinationDisabled);
            self.abandon(a.index, AbandonReason::DestinationDisabled, now);
            self.abandon_for_destination(
                &a.destination_id,
                AbandonReason::DestinationDisabled,
                now,
            );
        }
    }

    /// Moves every queued attempt due after `now` to `now`.
    pub(crate) fn rush_all(&mut self, now: SimTime) {
        let later: Vec<(SimTime, usize)> = self
            .queue
            .iter()
            .copied()
            .filter(|(t, _)| *t > now)
            .collect();
        for key in later {
            self.queue.remove(&key);
            self.queue.insert((now, key.1));
            self.list[key.1].next_at = Some(now);
        }
    }

    /// Whether any delivery is queued or in flight.
    pub(crate) fn has_pending(&self) -> bool {
        self.list
            .iter()
            .any(|d| matches!(d.state, State::Queued | State::InFlight))
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
