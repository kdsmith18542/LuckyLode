//! Pre-committable draw math (doc 06 rev 1.2) — the pool-side mirror of
//! the node's `lucky_draw` (frozen in the DRAW KAT). Lives in lm-pow so
//! consensus-adjacent math sits beside the frozen vectors.

use primitive_types::U256;
use sha2::{Digest, Sha256};

/// Pre-committable draw digest: SHA256D over the parent hash's internal
/// (serialized) bytes || height LE32, read as a big-endian 256-bit
/// integer — byte-for-byte the node's `lucky_draw::Digest`.
#[must_use]
pub fn precommitted_digest(prev_internal: &[u8; 32], height: u64) -> U256 {
    let mut buf = [0u8; 36];
    buf[..32].copy_from_slice(prev_internal);
    buf[32..36].copy_from_slice(&(height as u32).to_le_bytes());
    let first = Sha256::digest(buf);
    let second = Sha256::digest(first);
    U256::from_big_endian(&second)
}

/// Draw class of a height, from the pre-committed digest: jackpot band
/// `< 2^256/jackpot_divisor`, pot band `< 2^256/pot_divisor`, tier =
/// digest mod 100. Divisors default to the frozen odds (100 / 1000);
/// regtest pools mirror the node's overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawClass {
    pub jackpot: bool,
    pub pot_event: bool,
    pub tier: u64,
}

#[must_use]
pub fn draw_class(digest: &U256, jackpot_divisor: u64, pot_divisor: u64) -> DrawClass {
    let max = U256::MAX;
    let jt = max / U256::from(jackpot_divisor.max(1));
    let pt = max / U256::from(pot_divisor.max(1));
    DrawClass {
        jackpot: *digest < jt,
        pot_event: *digest < pt,
        tier: (*digest % U256::from(100u64)).low_u64(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_of(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn precommitted_digest_matches_the_node_kat() {
        // Frozen vector from the DRAW KAT (lucky_draw_kat.cpp): prev =
        // internal bytes 0..31, height 12345 -> digest de233f46...
        let mut prev = [0u8; 32];
        for (i, b) in prev.iter_mut().enumerate() {
            *b = i as u8;
        }
        let d = precommitted_digest(&prev, 12345);
        assert_eq!(
            hex_of(&d.to_big_endian()),
            "de233f46bbab5baf7e4e6183469038e219e360be104d018f4e0691ad0f70e3a7"
        );
        // Vector 1 (symmetric prev 0x11*32, height 100).
        let d1 = precommitted_digest(&[0x11u8; 32], 100);
        assert_eq!(
            hex_of(&d1.to_big_endian()),
            "3086b122f52f0c27064e330f31e1ebf04164cf07be2b1dcfc0ed70647b7bd63e"
        );
        // Jackpot-height vector (0x22*32, 108) — found by search in the KAT.
        let d3 = precommitted_digest(&[0x22u8; 32], 108);
        assert!(draw_class(&d3, 100, 1000).jackpot);
        assert!(!draw_class(&d3, 100, 1000).pot_event);
        assert_eq!(draw_class(&d3, 100, 1000).tier, 87);
        // Pot vector (0x22*32, 440) — nested inside the jackpot band.
        let d4 = precommitted_digest(&[0x22u8; 32], 440);
        let c4 = draw_class(&d4, 100, 1000);
        assert!(c4.pot_event && c4.jackpot);
        assert_eq!(c4.tier, 66);
    }

    #[test]
    fn draw_class_boundaries_and_divisors() {
        let max = U256::MAX;
        let t100 = max / U256::from(100u64);
        let t1000 = max / U256::from(1000u64);
        // Strict less-than at the threshold.
        assert!(draw_class(&(t100 - U256::from(1u64)), 100, 1000).jackpot);
        assert!(!draw_class(&t100, 100, 1000).jackpot);
        assert!(draw_class(&(t1000 - U256::from(1u64)), 100, 1000).pot_event);
        assert!(!draw_class(&t1000, 100, 1000).pot_event);
        // Regtest divisor 1: every height draws.
        let c = draw_class(&U256::from(5u64), 1, 1);
        assert!(c.jackpot && c.pot_event);
        // Tier = digest mod 100.
        assert_eq!(draw_class(&U256::from(1075u64), 100, 1000).tier, 75);
        assert_eq!(draw_class(&U256::from(1095u64), 100, 1000).tier, 95);
    }
}
