//! Simulated time.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

    /// This time plus `ms` milliseconds, saturating at the edges of chrono's date
    /// range so the result can always be displayed. It never panics, whatever
    /// `ms` is; use [`SimTime::checked_plus_ms`] to detect the edge instead.
    pub fn plus_ms(self, ms: i64) -> Self {
        let min = DateTime::<Utc>::MIN_UTC.timestamp_millis();
        let max = DateTime::<Utc>::MAX_UTC.timestamp_millis();
        SimTime(self.0.saturating_add(ms).clamp(min, max))
    }

    /// This time plus `ms` milliseconds, or `None` on overflow or when the result
    /// is outside chrono's date range (so it could not be displayed). Use it for
    /// durations that come from users.
    pub fn checked_plus_ms(self, ms: i64) -> Option<Self> {
        self.0
            .checked_add(ms)
            .filter(|&t| DateTime::from_timestamp_millis(t).is_some())
            .map(SimTime)
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

/// Any RFC 3339 time, truncated to milliseconds.
impl<'de> Deserialize<'de> for SimTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        DateTime::<Utc>::deserialize(d).map(SimTime::from_datetime)
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

    #[test]
    fn checked_plus_ms_overflows_to_none() {
        assert_eq!(SimTime(1).checked_plus_ms(2), Some(SimTime(3)));
        assert_eq!(SimTime(1).checked_plus_ms(i64::MAX), None);
        assert_eq!(
            SimTime(0).checked_plus_ms(i64::MAX / 2),
            None,
            "beyond chrono"
        );
    }
}
