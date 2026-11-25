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
        let mut s = String::with_capacity(prefix.len() + 15);
        s.push_str(prefix);
        s.push('_');
        for _ in 0..14 {
            s.push(ALNUM[self.below(62) as usize] as char);
        }
        s
    }

    /// `len` characters from `[0-9A-Z]`.
    pub(crate) fn alnum_upper(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| ALNUM_UPPER[self.below(36) as usize] as char)
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
    fn below_in_range() {
        let mut r = RngStreams::new(5).stream("b");
        assert!((0..10_000).all(|_| r.below(62) < 62));
    }
}
