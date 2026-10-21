//! Shared by the random-program checkers (`explain-check`, `ra-fuzz`, `ra-simplify`,
//! `sched-alias-check`, `sched-rule-check`).

/// xorshift64: the checkers' generator (seeded runs are reproducible).
pub struct Rng(u64);

impl Rng {
    /// The generator for run `seed`.
    pub fn seeded(seed: u64) -> Rng {
        Rng(0x9E3779B97F4A7C15 ^ seed.wrapping_mul(0x2545F4914F6CDD1D))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform-ish in `0..n` (`0` for `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// True `pct` percent of the time.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}
