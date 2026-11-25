//! Simulated time.

use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};

/// A point in simulated time: milliseconds since the Unix epoch.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SimTime(pub i64);

impl SimTime {
    /// Converts from a UTC datetime, truncating to milliseconds.
    pub fn from_datetime(dt: DateTime<Utc>) -> Self {
        SimTime(dt.timestamp_millis())
    }

    /// Converts to a UTC datetime.
    pub fn to_datetime(self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.0).expect("SimTime out of chrono range")
    }

    /// Unix seconds, rounded down (Stripe `created` values).
    pub fn unix_seconds(self) -> i64 {
        self.0.div_euclid(1000)
    }

    /// This time plus `ms` milliseconds.
    pub fn plus_ms(self, ms: i64) -> Self {
        SimTime(self.0 + ms)
    }
}

impl std::fmt::Display for SimTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_datetime().format("%Y-%m-%dT%H:%M:%S%.3fZ"))
    }
}

impl Serialize for SimTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_ms() {
        let t = SimTime::from_datetime("2026-01-01T00:12:03.120Z".parse().unwrap());
        assert_eq!(serde_json::to_value(t).unwrap(), "2026-01-01T00:12:03.120Z");
        assert_eq!(t.unix_seconds(), 1767226323);
        assert_eq!(SimTime(-1).unix_seconds(), -1);
    }
}
