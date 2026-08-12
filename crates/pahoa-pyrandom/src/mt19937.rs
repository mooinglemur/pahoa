//! The Mersenne Twister core, matching CPython's `Modules/_randommodule.c`.
//!
//! This is the reference MT19937 with CPython's exact `init_by_array` seeding.
//! Every constant here is load-bearing; the golden vectors exist because a
//! single wrong shift produces plausible-looking output that diverges from
//! CPython immediately.

const N: usize = 624;
const M: usize = 397;
const MATRIX_A: u32 = 0x9908b0df;
const UPPER_MASK: u32 = 0x8000_0000;
const LOWER_MASK: u32 = 0x7fff_ffff;

#[derive(Clone)]
pub struct Mt19937 {
    state: [u32; N],
    /// Index into `state`. CPython persists this as the 625th element of
    /// `getstate()`, and `index == N` means "exhausted, regenerate on next draw".
    index: usize,
}

impl Mt19937 {
    /// `init_genrand`: Knuth's multiplicative seeding.
    pub fn from_u32(seed: u32) -> Self {
        let mut state = [0u32; N];
        state[0] = seed;
        for i in 1..N {
            state[i] = 1812433253u32
                .wrapping_mul(state[i - 1] ^ (state[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        Self { state, index: N }
    }

    /// `init_by_array`: how CPython actually seeds, given the key words derived
    /// from the seed integer.
    pub fn from_key(key: &[u32]) -> Self {
        let mut mt = Self::from_u32(19650218);
        let mut i: usize = 1;
        let mut j: usize = 0;

        // An empty key would make the loops below misbehave; CPython never
        // produces one (a zero seed yields the single word [0]).
        let key = if key.is_empty() { &[0][..] } else { key };

        let mut k = N.max(key.len());
        while k > 0 {
            let prev = mt.state[i - 1];
            mt.state[i] = (mt.state[i] ^ ((prev ^ (prev >> 30)).wrapping_mul(1664525)))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                mt.state[0] = mt.state[N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
            k -= 1;
        }

        let mut k = N - 1;
        while k > 0 {
            let prev = mt.state[i - 1];
            mt.state[i] = (mt.state[i] ^ ((prev ^ (prev >> 30)).wrapping_mul(1566083941)))
                .wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                mt.state[0] = mt.state[N - 1];
                i = 1;
            }
            k -= 1;
        }

        // Guarantees a non-zero initial state.
        mt.state[0] = 0x8000_0000;
        mt.index = N;
        mt
    }

    fn regenerate(&mut self) {
        for i in 0..N {
            let y = (self.state[i] & UPPER_MASK) | (self.state[(i + 1) % N] & LOWER_MASK);
            let mut next = self.state[(i + M) % N] ^ (y >> 1);
            if y & 1 != 0 {
                next ^= MATRIX_A;
            }
            self.state[i] = next;
        }
        self.index = 0;
    }

    /// `genrand_uint32`: one tempered 32-bit output.
    pub fn next_u32(&mut self) -> u32 {
        if self.index >= N {
            self.regenerate();
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// The 625 words CPython's `getstate()` exposes: the state array followed
    /// by the index. Saves persist this so a resumed room continues the same
    /// sequence.
    pub fn to_state(&self) -> [u32; N + 1] {
        let mut out = [0u32; N + 1];
        out[..N].copy_from_slice(&self.state);
        out[N] = self.index as u32;
        out
    }

    /// Inverse of [`Self::to_state`]. Rejects an out-of-range index rather than
    /// producing a generator that would panic or silently misbehave later.
    pub fn from_state(words: &[u32]) -> Option<Self> {
        if words.len() != N + 1 {
            return None;
        }
        let index = words[N] as usize;
        if index > N {
            return None;
        }
        let mut state = [0u32; N];
        state.copy_from_slice(&words[..N]);
        Some(Self { state, index })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// init_by_array with the classic {0x123, 0x234, 0x345, 0x456} key.
    ///
    /// Values captured from CPython 3.13 (seeding with the integer whose
    /// little-endian words are that key), not transcribed from the
    /// Matsumoto/Nishimura paper — CPython is the behavior we must match, and
    /// it is the only source that settles a disagreement.
    #[test]
    fn matches_cpython_init_by_array() {
        let mut mt = Mt19937::from_key(&[0x123, 0x234, 0x345, 0x456]);
        let expected = [
            1067595299u32,
            955945823,
            477289528,
            4107218783,
            4228976476,
            3344332714,
            3355579695,
            227628506,
            810200273,
            2591290167,
        ];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(mt.next_u32(), *want, "output {i}");
        }
    }

    #[test]
    fn state_round_trips() {
        let mut a = Mt19937::from_key(&[42]);
        for _ in 0..1000 {
            a.next_u32();
        }
        let mut b = Mt19937::from_state(&a.to_state()).expect("valid state");
        for i in 0..100 {
            assert_eq!(a.next_u32(), b.next_u32(), "draw {i} after restore");
        }
    }

    #[test]
    fn rejects_malformed_state() {
        assert!(Mt19937::from_state(&[0; 10]).is_none());
        let mut bad = [0u32; N + 1];
        bad[N] = (N + 1) as u32;
        assert!(Mt19937::from_state(&bad).is_none());
    }
}
