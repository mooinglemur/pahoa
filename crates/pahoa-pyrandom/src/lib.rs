//! A bit-exact reimplementation of CPython's `random.Random`.
//!
//! Archipelago seeds its hint PRNG from the multiworld's seed name and persists
//! the generator state in the save file, so hint ordering is reproducible across
//! restarts and directly visible to players
//! (`MultiServer.py:536`, `:671`, `:703`, `:1774`). Reproducing that ordering
//! exactly is what lets hint order be a *checked* signal in the differential
//! harness rather than permanent noise.
//!
//! Scope is only what Archipelago's server actually uses: seeding from a string,
//! `getrandbits`, `_randbelow`, `shuffle`, and state round-tripping. There is no
//! `random()`, `gauss()`, or the rest of the module.
//!
//! ```
//! use pahoa_pyrandom::PyRandom;
//! let mut r = PyRandom::seed_str("TestSeed12345");
//! assert_eq!(r.getrandbits_u64(32), 3640632534);
//! ```

mod mt19937;

pub use mt19937::Mt19937;

use sha2::{Digest, Sha512};

#[derive(Clone)]
pub struct PyRandom {
    mt: Mt19937,
}

impl PyRandom {
    /// `Random.seed(s)` for a string, version 2.
    ///
    /// CPython computes `int.from_bytes(utf8 + sha512(utf8).digest(), "big")`
    /// and seeds from that integer (`Lib/random.py`). Verified against CPython
    /// 3.13 rather than taken from the source by eye.
    pub fn seed_str(s: &str) -> Self {
        let bytes = s.as_bytes();
        let mut buf = Vec::with_capacity(bytes.len() + 64);
        buf.extend_from_slice(bytes);
        buf.extend_from_slice(&Sha512::digest(bytes));
        Self::seed_be_bytes(&buf)
    }

    /// Seed from a big-endian, non-negative integer given as bytes.
    ///
    /// `random_seed` in `Modules/_randommodule.c` takes `abs(a)` and reads it
    /// out as little-endian 32-bit words, so this reverses the byte order,
    /// drops the (now trailing) high-order zeros, and chunks.
    pub fn seed_be_bytes(be: &[u8]) -> Self {
        let mut le: Vec<u8> = be.iter().rev().copied().collect();
        while le.last() == Some(&0) {
            le.pop();
        }
        let key: Vec<u32> = le
            .chunks(4)
            .map(|c| {
                let mut w = [0u8; 4];
                w[..c.len()].copy_from_slice(c);
                u32::from_le_bytes(w)
            })
            .collect();
        // A zero seed yields the single key word [0], matching CPython's
        // `keyused = 1` floor for a zero-bit-length integer.
        Self::seed_key(if key.is_empty() { &[0] } else { &key })
    }

    /// Seed directly from the little-endian 32-bit key words.
    pub fn seed_key(key: &[u32]) -> Self {
        Self {
            mt: Mt19937::from_key(key),
        }
    }

    pub fn from_state(words: &[u32]) -> Option<Self> {
        Mt19937::from_state(words).map(|mt| Self { mt })
    }

    pub fn to_state(&self) -> [u32; 625] {
        self.mt.to_state()
    }

    /// `getrandbits(k)` as little-endian 32-bit words.
    ///
    /// The subtlety is the *most significant* word: CPython fills words from
    /// least to most significant, and the final one is shifted down when `k` is
    /// not a multiple of 32. Getting that backwards produces output that looks
    /// random and is wrong.
    pub fn getrandbits_words(&mut self, k: u32) -> Vec<u32> {
        if k == 0 {
            return vec![0];
        }
        if k <= 32 {
            return vec![self.mt.next_u32() >> (32 - k)];
        }
        let words = ((k - 1) / 32 + 1) as usize;
        let mut out = Vec::with_capacity(words);
        let mut remaining = k;
        for _ in 0..words {
            let mut r = self.mt.next_u32();
            if remaining < 32 {
                r >>= 32 - remaining;
            }
            out.push(r);
            remaining = remaining.wrapping_sub(32);
        }
        out
    }

    /// `getrandbits(k)` for `k <= 64`, which covers every use in the server.
    ///
    /// # Panics
    /// If `k > 64`; use [`Self::getrandbits_words`] for wider draws.
    pub fn getrandbits_u64(&mut self, k: u32) -> u64 {
        assert!(
            k <= 64,
            "getrandbits_u64 called with k={k}; use getrandbits_words"
        );
        let words = self.getrandbits_words(k);
        let lo = words[0] as u64;
        let hi = words.get(1).copied().unwrap_or(0) as u64;
        (hi << 32) | lo
    }

    /// `Random._randbelow_with_getrandbits(n)`: rejection sampling on
    /// `n.bit_length()` bits. The loop matters — a modulo shortcut would consume
    /// a different number of draws and desynchronize everything after it.
    pub fn randbelow(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let k = 64 - n.leading_zeros();
        loop {
            let r = self.getrandbits_u64(k);
            if r < n {
                return r;
            }
        }
    }

    /// `Random.shuffle(x)`: Fisher-Yates walking down from the end.
    pub fn shuffle<T>(&mut self, x: &mut [T]) {
        if x.len() < 2 {
            return;
        }
        for i in (1..x.len()).rev() {
            let j = self.randbelow(i as u64 + 1) as usize;
            x.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_str_matches_cpython_first_draws() {
        let mut r = PyRandom::seed_str("TestSeed12345");
        let got: Vec<u64> = (0..5).map(|_| r.getrandbits_u64(32)).collect();
        assert_eq!(
            got,
            [3640632534, 2509922890, 3181089231, 2733124716, 4261827827]
        );
    }

    #[test]
    fn empty_seed_is_not_a_zero_seed() {
        // "" hashes to a full sha512 digest, so it must not collapse to seed 0.
        let mut a = PyRandom::seed_str("");
        let mut b = PyRandom::seed_key(&[0]);
        assert_ne!(a.getrandbits_u64(32), b.getrandbits_u64(32));
    }

    #[test]
    fn randbelow_one_rejects_until_it_draws_zero() {
        // n=1 has bit_length 1, so CPython draws a single bit and *retries*
        // while r >= 1 — meaning it loops until it happens to draw 0. The draw
        // count is therefore variable, which matters: a modulo shortcut would
        // consume a different number of draws and desynchronize the stream.
        for seed in ["x", "y", "z", "seed-1", "seed-2"] {
            let mut r = PyRandom::seed_str(seed);
            assert_eq!(r.randbelow(1), 0);
        }

        // Equivalence with the explicit loop, on the same stream.
        let mut a = PyRandom::seed_str("x");
        let mut b = PyRandom::seed_str("x");
        assert_eq!(a.randbelow(1), 0);
        while b.getrandbits_u64(1) >= 1 {}
        assert_eq!(a.getrandbits_u64(32), b.getrandbits_u64(32));
    }

    #[test]
    fn randbelow_zero_consumes_no_draws() {
        // CPython returns early for n == 0 without touching the generator.
        let mut a = PyRandom::seed_str("x");
        let expected = a.clone().getrandbits_u64(32);
        assert_eq!(a.randbelow(0), 0);
        assert_eq!(a.getrandbits_u64(32), expected);
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut r = PyRandom::seed_str("perm");
        let mut deck: Vec<u32> = (0..500).collect();
        r.shuffle(&mut deck);
        let mut sorted = deck.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..500).collect::<Vec<_>>());
        assert_ne!(deck, sorted, "a 500-element shuffle should not be identity");
    }

    #[test]
    fn shuffle_of_short_slices_is_a_noop() {
        let mut r = PyRandom::seed_str("x");
        let before = r.clone().getrandbits_u64(32);
        let mut one = [1];
        r.shuffle(&mut one);
        // No draws consumed, matching CPython's `reversed(range(1, 1))`.
        assert_eq!(r.getrandbits_u64(32), before);
    }

    #[test]
    fn getrandbits_u64_rejects_oversized_k() {
        let mut r = PyRandom::seed_str("x");
        assert!(std::panic::catch_unwind(move || r.getrandbits_u64(65)).is_err());
    }
}
