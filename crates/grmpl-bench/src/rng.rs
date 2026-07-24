//! A tiny deterministic PRNG so workloads are reproducible without pulling in
//! `rand`. SplitMix64 (Steele et al.) — one multiply-xorshift per draw, good
//! enough to shuffle keys and pick contention targets; nothing here is
//! security- or statistics-sensitive.

/// A seedable SplitMix64 generator. `Copy` so a per-thread clone is trivial.
#[derive(Clone, Copy)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A draw in `[0, n)` (`n > 0`), via the classic modulo — the slight bias is
    /// irrelevant for workload generation.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}
