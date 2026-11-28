//! Seeded, named random streams (spec section 12).
//!
//! The algorithms here are fixed so recorded runs replay identically.

use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};

const ALNUM: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const ALNUM_UPPER: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// 64-bit FNV-1a hash.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Factory for independent named streams derived from one seed.
#[derive(Debug, Clone)]
pub(crate) struct RngStreams {
    seed: u64,
}

impl RngStreams {
    pub(crate) fn new(seed: u64) -> Self {
        RngStreams { seed }
    }

    /// The stream called `name`. Same seed and name always give the same stream.
    pub(crate) fn stream(&self, name: &str) -> SimRng {
        let mut rng = ChaCha8Rng::seed_from_u64(self.seed);
        rng.set_stream(fnv1a64(name.as_bytes()));
        SimRng(rng)
    }
}

/// One deterministic random stream.
#[derive(Debug, Clone)]
pub(crate) struct SimRng(ChaCha8Rng);

impl SimRng {
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    /// Uniform in `[0, 1)`.
    pub(crate) fn unit_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * 2f64.powi(-53)
    }

    /// Uniform in `0..n` (`n > 0`), by rejection sampling.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0)");
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let x = self.next_u64();
            if x < zone {
                return x % n;
            }
        }
    }

    /// Exponential inter-arrival time in ms for a rate per hour; `None` if the rate is 0.
    pub(crate) fn exp_interval_ms(&mut self, rate_per_hour: f64) -> Option<i64> {
        if rate_per_hour == 0.0 {
            return None;
        }
        let u = self.unit_f64();
        let ms = (-(1.0 - u).ln() / rate_per_hour * 3_600_000.0).ceil();
        Some((ms as i64).max(1))
    }

    /// A Stripe-style ID: `prefix_` plus 14 alphanumerics.
    pub(crate) fn id(&mut self, prefix: &str) -> String {
        self.id_len(prefix, 14)
    }

    /// `prefix_` plus `len` alphanumerics.
    pub(crate) fn id_len(&mut self, prefix: &str, len: usize) -> String {
        let mut s = String::with_capacity(prefix.len() + len + 1);
        s.push_str(prefix);
        s.push('_');
        for _ in 0..len {
            s.push(ALNUM[self.below(62) as usize] as char);
        }
        s
    }

    /// True with probability `p`. Draws nothing when `p == 0`.
    pub(crate) fn chance(&mut self, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        self.unit_f64() < p
    }

    /// Standard normal via Box–Muller (draws `u1` then `u2`).
    pub(crate) fn normal(&mut self) -> f64 {
        let u1 = self.unit_f64();
        let u2 = self.unit_f64();
        (-2.0 * (1.0 - u1).ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    /// A delay in ms from `d`. `Fixed` draws nothing.
    pub(crate) fn sample_ms(&mut self, d: &crate::Distribution) -> i64 {
        match *d {
            crate::Distribution::Fixed { ms } => ms,
            crate::Distribution::Lognormal { median_ms, p99_ms } => {
                let mu = median_ms.ln();
                let sigma = (p99_ms.ln() - mu) / 2.326348;
                // A tail draw can far exceed `p99_ms`; cap it at the config limit.
                ((mu + sigma * self.normal()).exp().round() as i64)
                    .clamp(0, crate::config::MAX_DURATION_MS)
            }
        }
    }

    /// Uniform in `min..=max` (ms). Draws nothing when `min == max`.
    pub(crate) fn uniform_ms(&mut self, min: i64, max: i64) -> i64 {
        if max <= min {
            return min;
        }
        min + self.below((max - min + 1) as u64) as i64
    }

    /// `len` characters from `[0-9A-Z]`.
    pub(crate) fn alnum_upper(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| ALNUM_UPPER[self.below(36) as usize] as char)
            .collect()
    }

    /// `len` characters from `[0-9A-Za-z]`, as in a coupon ID.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "coupons arrive in phase 9 Task 3")
    )]
    pub(crate) fn code(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| ALNUM[self.below(62) as usize] as char)
            .collect()
    }

    /// A uniformly chosen element of a non-empty slice.
    pub(crate) fn choose<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: u64 = 18323390549929048544;

    #[test]
    fn fnv_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn streams_are_deterministic_and_independent() {
        let s = RngStreams::new(7);
        let take = |mut r: SimRng| (0..100).map(|_| r.next_u64()).collect::<Vec<_>>();
        let a = take(s.stream("x"));
        let b = take(s.stream("x"));
        let c = take(s.stream("y"));
        let d = take(RngStreams::new(8).stream("x"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn golden_first_value() {
        // Fails if a dependency upgrade changes the output, which would break replays.
        assert_eq!(
            RngStreams::new(42)
                .stream("generate.customer.create")
                .next_u64(),
            GOLDEN
        );
    }

    #[test]
    fn id_format() {
        let id = RngStreams::new(1).stream("ids").id("cus");
        assert!(id.starts_with("cus_"));
        assert_eq!(id.len(), 18);
        assert!(id[4..].bytes().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn exp_intervals() {
        let mut r = RngStreams::new(3).stream("exp");
        assert_eq!(r.exp_interval_ms(0.0), None);
        let n = 20_000;
        let mean = (0..n)
            .map(|_| r.exp_interval_ms(60.0).unwrap())
            .sum::<i64>() as f64
            / n as f64;
        assert!((57_000.0..63_000.0).contains(&mean), "{mean}");
        assert!((0..1000).all(|_| r.exp_interval_ms(1e12).unwrap() >= 1));
    }

    #[test]
    fn id_len_and_golden_unchanged() {
        let mut r = RngStreams::new(1).stream("ids");
        let id = r.id_len("evt", 24);
        assert!(id.starts_with("evt_") && id.len() == 28);
        assert_eq!(
            RngStreams::new(42)
                .stream("generate.customer.create")
                .next_u64(),
            GOLDEN
        );
    }

    #[test]
    fn chance_zero_does_not_draw() {
        let s = RngStreams::new(2);
        let (mut a, mut b) = (s.stream("c"), s.stream("c"));
        assert!(!a.chance(0.0));
        assert_eq!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn lognormal_median_and_p99() {
        let mut r = RngStreams::new(3).stream("ln");
        let d = crate::Distribution::Lognormal {
            median_ms: 200.0,
            p99_ms: 5000.0,
        };
        let mut v: Vec<i64> = (0..20_000).map(|_| r.sample_ms(&d)).collect();
        v.sort();
        let (median, p99) = (v[10_000], v[19_800]);
        assert!((180..=220).contains(&median), "{median}");
        assert!((4000..=6200).contains(&p99), "{p99}");
        assert_eq!(r.sample_ms(&crate::Distribution::Fixed { ms: 7 }), 7);
    }

    #[test]
    fn uniform_bounds() {
        let mut r = RngStreams::new(4).stream("u");
        assert!((0..1000).all(|_| (5..=9).contains(&r.uniform_ms(5, 9))));
        assert_eq!(r.uniform_ms(3, 3), 3);
    }

    #[test]
    fn code_draws_alphanumerics() {
        let mut r = RngStreams::new(6).stream("code");
        let a = r.code(8);
        let b = r.code(8);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn below_in_range() {
        let mut r = RngStreams::new(5).stream("b");
        assert!((0..10_000).all(|_| r.below(62) < 62));
    }
}
