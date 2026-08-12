//! Arbitrary-precision integers, for the Python ints that do not fit in `i64`.
//!
//! These are rare but real. A live multidata carries
//! `slot_data[n]["seed_name"] == 56979137468180783661` — larger than `u64`, let
//! alone `i64`. `slot_data` is opaque and forwarded verbatim to clients in
//! `Connected`, so the value has to survive a decode/encode round trip exactly;
//! saturating or truncating it would silently corrupt a world's own state.
//!
//! Only what that requires is implemented: construction from pickle's LONG1
//! payload (little-endian two's complement) and exact decimal rendering
//! matching CPython's `str(int)`. No arithmetic, so no dependency.

use std::fmt;

/// Sign-magnitude, little-endian base 2^32, normalized (no trailing zero limbs).
/// Zero is `negative: false` with an empty magnitude.
#[derive(Clone, PartialEq, Eq)]
pub struct BigInt {
    negative: bool,
    magnitude: Vec<u32>,
}

impl BigInt {
    /// Decode pickle's LONG1/LONG4 payload: little-endian two's complement,
    /// variable width, empty meaning zero.
    pub fn from_le_twos_complement(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self {
                negative: false,
                magnitude: Vec::new(),
            };
        }
        let negative = bytes[bytes.len() - 1] & 0x80 != 0;

        // For a negative value take the two's complement to get the magnitude:
        // invert every byte, then add one.
        let mut bytes = bytes.to_vec();
        if negative {
            for b in &mut bytes {
                *b = !*b;
            }
            let mut carry = 1u16;
            for b in &mut bytes {
                let v = *b as u16 + carry;
                *b = v as u8;
                carry = v >> 8;
                if carry == 0 {
                    break;
                }
            }
        }

        let mut magnitude = Vec::with_capacity(bytes.len().div_ceil(4));
        for chunk in bytes.chunks(4) {
            let mut limb = [0u8; 4];
            limb[..chunk.len()].copy_from_slice(chunk);
            magnitude.push(u32::from_le_bytes(limb));
        }
        while magnitude.last() == Some(&0) {
            magnitude.pop();
        }
        if magnitude.is_empty() {
            return Self {
                negative: false,
                magnitude,
            };
        }
        Self {
            negative,
            magnitude,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.magnitude.is_empty()
    }

    /// `Some` when the value fits, so callers can keep the fast path in `i64`.
    pub fn to_i64(&self) -> Option<i64> {
        match self.magnitude.len() {
            0 => Some(0),
            1 => {
                let v = self.magnitude[0] as i64;
                Some(if self.negative { -v } else { v })
            }
            2 => {
                let v = ((self.magnitude[1] as u64) << 32 | self.magnitude[0] as u64) as u128;
                if self.negative {
                    // i64::MIN has no positive counterpart, so check magnitude.
                    (v <= i64::MAX as u128 + 1).then(|| (v as i128).wrapping_neg() as i64)
                } else {
                    (v <= i64::MAX as u128).then_some(v as i64)
                }
            }
            _ => None,
        }
    }
}

impl fmt::Display for BigInt {
    /// Exact decimal, by repeated division of the magnitude by 10^9.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_zero() {
            return f.write_str("0");
        }

        const CHUNK: u64 = 1_000_000_000;
        let mut limbs = self.magnitude.clone();
        let mut chunks: Vec<u32> = Vec::new();

        while !limbs.is_empty() {
            let mut rem: u64 = 0;
            for limb in limbs.iter_mut().rev() {
                let cur = (rem << 32) | *limb as u64;
                *limb = (cur / CHUNK) as u32;
                rem = cur % CHUNK;
            }
            chunks.push(rem as u32);
            while limbs.last() == Some(&0) {
                limbs.pop();
            }
        }

        if self.negative {
            f.write_str("-")?;
        }
        // Most significant chunk unpadded, the rest zero-padded to 9 digits.
        let mut it = chunks.iter().rev();
        write!(f, "{}", it.next().unwrap())?;
        for c in it {
            write!(f, "{c:09}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(bytes: &[u8]) -> String {
        BigInt::from_le_twos_complement(bytes).to_string()
    }

    #[test]
    fn renders_small_values() {
        assert_eq!(dec(&[]), "0");
        assert_eq!(dec(&[0x01]), "1");
        assert_eq!(dec(&[0xff]), "-1");
        assert_eq!(dec(&[0x80]), "-128");
        assert_eq!(dec(&[0x7f]), "127");
        assert_eq!(dec(&[0x00, 0x01]), "256");
    }

    #[test]
    fn renders_the_value_found_in_real_multidata() {
        // slot_data[..]["seed_name"] == 56979137468180783661, which exceeds u64.
        let v: u128 = 56979137468180783661;
        let mut bytes = v.to_le_bytes().to_vec();
        // Trim to pickle's minimal signed encoding.
        while bytes.len() > 1 && bytes[bytes.len() - 1] == 0 && bytes[bytes.len() - 2] & 0x80 == 0 {
            bytes.pop();
        }
        assert_eq!(dec(&bytes), "56979137468180783661");
    }

    #[test]
    fn renders_powers_of_two_exactly() {
        for exp in [63u32, 64, 100, 127, 200] {
            let expected = {
                // Build 2^exp as a decimal string without a bignum library:
                // repeated doubling of a digit vector.
                let mut digits = vec![1u8];
                for _ in 0..exp {
                    let mut carry = 0;
                    for d in digits.iter_mut() {
                        let v = *d * 2 + carry;
                        *d = v % 10;
                        carry = v / 10;
                    }
                    while carry > 0 {
                        digits.push(carry % 10);
                        carry /= 10;
                    }
                }
                digits
                    .iter()
                    .rev()
                    .map(|d| (b'0' + d) as char)
                    .collect::<String>()
            };

            let mut bytes = vec![0u8; (exp / 8) as usize];
            bytes.push(1 << (exp % 8));
            // Ensure the sign bit is clear so it reads as positive.
            if bytes[bytes.len() - 1] & 0x80 != 0 {
                bytes.push(0);
            }
            assert_eq!(dec(&bytes), expected, "2^{exp}");
        }
    }

    #[test]
    fn narrows_to_i64_when_it_fits() {
        assert_eq!(BigInt::from_le_twos_complement(&[0x01]).to_i64(), Some(1));
        assert_eq!(BigInt::from_le_twos_complement(&[0xff]).to_i64(), Some(-1));

        let max = i64::MAX.to_le_bytes();
        assert_eq!(
            BigInt::from_le_twos_complement(&max).to_i64(),
            Some(i64::MAX)
        );

        let min = i64::MIN.to_le_bytes();
        assert_eq!(
            BigInt::from_le_twos_complement(&min).to_i64(),
            Some(i64::MIN)
        );

        // 2^64 does not fit.
        let mut big = vec![0u8; 8];
        big.push(1);
        assert_eq!(BigInt::from_le_twos_complement(&big).to_i64(), None);
    }

    #[test]
    fn sign_extension_does_not_change_the_value() {
        assert_eq!(dec(&[0xff]), dec(&[0xff, 0xff]));
        assert_eq!(dec(&[0x01]), dec(&[0x01, 0x00]));
    }
}
