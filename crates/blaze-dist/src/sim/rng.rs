//! A seeded random stream: SplitMix64, the same on every machine and in every build.

/// A seeded random stream.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// The stream of `seed`.
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x6A09_E667_F3BC_C909)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n`, or 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// A number in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    /// True `per_million` times in a million.
    pub fn chance(&mut self, per_million: u32) -> bool {
        self.below(1_000_000) < u64::from(per_million)
    }

    /// A stream of its own, so one part's draws do not shift another's.
    pub fn fork(&mut self) -> Rng {
        Rng(self.next_u64())
    }

    /// 32 random bytes.
    pub fn digest(&mut self) -> [u8; 32] {
        let mut out = [0u8; 32];
        for chunk in out.chunks_mut(8) {
            chunk.copy_from_slice(&self.next_u64().to_le_bytes());
        }
        out
    }
}
