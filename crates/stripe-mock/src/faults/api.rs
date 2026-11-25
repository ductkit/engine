//! Faults on the Stripe-compatible API (spec section 7.4): rate limiting,
//! random errors, timeouts, connection resets and latency.

use std::time::Instant;

use crate::ApiConfig;
use crate::rng::{RngStreams, SimRng};

/// A token bucket on wall time.
#[derive(Debug, Clone)]
pub(crate) struct TokenBucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub(crate) fn new(rate: f64, burst: u32, now: Instant) -> Self {
        TokenBucket {
            rate,
            burst: f64::from(burst),
            tokens: f64::from(burst),
            last: now,
        }
    }

    /// Takes one token if there is one.
    pub(crate) fn try_take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// What an injected fault does to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// 429 `rate_limit`.
    RateLimited,
    /// 500 `api_error`.
    ServerError,
    /// 503 `api_error`.
    Unavailable,
    /// No response until the client gives up.
    Timeout,
    /// The connection is dropped without a complete response.
    Reset,
}

/// Runtime `[api]` settings and the state their faults need.
#[derive(Debug)]
pub(crate) struct ApiFaults {
    config: ApiConfig,
    bucket: Option<TokenBucket>,
    storm_until: Option<Instant>,
    errors: SimRng,
    latency: SimRng,
    stale: SimRng,
    pagination_writes: SimRng,
}

impl ApiFaults {
    pub(crate) fn new(config: &ApiConfig, streams: &RngStreams) -> Self {
        let mut f = ApiFaults {
            config: config.clone(),
            bucket: None,
            storm_until: None,
            errors: streams.stream("api.errors"),
            latency: streams.stream("api.latency"),
            stale: streams.stream("api.stale_reads"),
            pagination_writes: streams.stream("api.pagination_writes"),
        };
        f.set_config(config.clone(), Instant::now());
        f
    }

    pub(crate) fn config(&self) -> &ApiConfig {
        &self.config
    }

    /// Replaces the settings; the rate-limit bucket starts full.
    pub(crate) fn set_config(&mut self, c: ApiConfig, now: Instant) {
        self.bucket = c
            .rate_limit
            .as_ref()
            .map(|r| TokenBucket::new(r.requests_per_second, r.burst, now));
        self.config = c;
    }

    /// Every request is rate limited until `until`.
    pub(crate) fn storm(&mut self, until: Instant) {
        self.storm_until = Some(until);
    }

    /// For one object retrieve or list: the stale lag in ms, if this read is stale.
    pub(crate) fn stale_lag(&mut self) -> Option<i64> {
        let s = &self.config.stale_read;
        self.stale
            .chance(s.probability)
            .then(|| self.stale.uniform_ms(1, s.max_lag.ms()))
    }

    /// For one object list request with a cursor: whether a write runs first.
    pub(crate) fn pagination_write(&mut self) -> bool {
        self.pagination_writes
            .chance(self.config.pagination_writes.probability)
    }

    /// The fault for one request that passed the gate (if any) and its latency in ms.
    /// Always draws once from `"api.errors"` and once from `"api.latency"`.
    pub(crate) fn decide(&mut self, now: Instant) -> (Option<Fault>, u64) {
        let u = self.errors.unit_f64();
        let latency = self.latency.sample_ms(&self.config.latency).max(0) as u64;
        if self.storm_until.is_some_and(|t| now < t) {
            return (Some(Fault::RateLimited), latency);
        }
        if let Some(b) = &mut self.bucket
            && !b.try_take(now)
        {
            return (Some(Fault::RateLimited), latency);
        }
        let e = &self.config.errors;
        let mut edge = 0.0;
        for (p, fault) in [
            (e.e500, Fault::ServerError),
            (e.e503, Fault::Unavailable),
            (e.timeout, Fault::Timeout),
            (e.reset, Fault::Reset),
        ] {
            edge += p;
            if u < edge {
                return (Some(fault), latency);
            }
        }
        (None, latency)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn bucket_refills() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(10.0, 2, t0);
        assert!(b.try_take(t0));
        assert!(b.try_take(t0));
        assert!(!b.try_take(t0));
        assert!(b.try_take(t0 + Duration::from_millis(100)));
    }

    #[test]
    fn error_ranges() {
        let mut config = ApiConfig::default();
        config.errors.e500 = 0.1;
        config.errors.reset = 0.1;
        let mut f = ApiFaults::new(&config, &RngStreams::new(1));
        let now = Instant::now();
        let (mut server, mut reset, mut ok) = (0i32, 0i32, 0i32);
        for _ in 0..10_000 {
            match f.decide(now).0 {
                Some(Fault::ServerError) => server += 1,
                Some(Fault::Reset) => reset += 1,
                None => ok += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        for (got, want) in [(server, 1000), (reset, 1000), (ok, 8000)] {
            assert!((got - want).abs() <= 200, "{got} vs {want}");
        }
    }
}
