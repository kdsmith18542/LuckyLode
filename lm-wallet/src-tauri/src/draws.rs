//! Pre-committed draw math — mirrors `verus/src/consensus/lucky_draw.h`
//! byte-for-byte (doc 06 rev 1.2). Frozen in the DRAW KAT; this module is
//! the wallet-side recompute for the "next block is a jackpot" banner.

use sha2::{Digest, Sha256};

/// draw_digest = SHA256D(parent hash internal bytes || height LE32),
/// read as a big-endian 256-bit integer (the digest bytes as returned by
/// SHA256 in standard order).
#[must_use]
pub fn draw_digest(prev_internal: &[u8; 32], height: u64) -> [u8; 32] {
    let mut buf = [0u8; 36];
    buf[..32].copy_from_slice(prev_internal);
    buf[32..36].copy_from_slice(&(height as u32).to_le_bytes());
    let first = Sha256::digest(buf);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

/// floor((2^256 - 1) / divisor) as big-endian bytes (long division of an
/// all-ones dividend — exact match with arith_uint256 in the node).
#[must_use]
pub fn threshold_be(divisor: u64) -> [u8; 32] {
    let d = divisor.max(1) as u128;
    let mut rem: u128 = 0;
    let mut out = [0u8; 32];
    for byte in out.iter_mut() {
        let cur = (rem << 8) | 0xff;
        *byte = (cur / d) as u8;
        rem = cur % d;
    }
    out
}

/// Strict less-than, big-endian lexicographic (digest < threshold).
#[must_use]
pub fn below(digest: &[u8; 32], divisor: u64) -> bool {
    digest < &threshold_be(divisor)
}

/// tier = digest mod 100 (big-endian long mod).
#[must_use]
pub fn tier(digest: &[u8; 32]) -> u64 {
    let mut rem: u64 = 0;
    for b in digest {
        rem = (rem * 256 + u64::from(*b)) % 100;
    }
    rem
}

/// Display hex (as `getblockhash` returns) → internal byte order.
#[must_use]
pub fn display_hex_to_internal(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = u8::from_str_radix(&hex[i * 2..i * 2 + 1], 16).ok()?;
        let lo = u8::from_str_radix(&hex[i * 2 + 1..i * 2 + 2], 16).ok()?;
        out[31 - i] = (hi << 4) | lo;
    }
    Some(out)
}

/// The draw class of the NEXT height (the only one computable: its parent
/// is the current tip; beyond that parents are unknown until blocks land).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NextDraw {
    pub height: u64,
    pub jackpot: bool,
    pub pot_event: bool,
    pub tier: u64,
    /// Pot splash percent for this tier (10/25/50), computed from the
    /// frozen consensus fractions — the tier roll itself is NOT a percent.
    pub pot_pct: u64,
    pub digest: String,
}

#[must_use]
pub fn next_draw(
    tip_display_hex: &str,
    next_height: u64,
    jdiv: u64,
    pdiv: u64,
) -> Option<NextDraw> {
    let prev = display_hex_to_internal(tip_display_hex)?;
    let d = draw_digest(&prev, next_height);
    let t = tier(&d);
    let (num, den) = lm_rewards::pot::pot_fraction(t);
    Some(NextDraw {
        height: next_height,
        jackpot: below(&d, jdiv),
        pot_event: below(&d, pdiv),
        tier: t,
        pot_pct: num * 100 / den,
        digest: hex(&d),
    })
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_node_kat_vectors() {
        // DRAW KAT vectors (lucky_draw_kat.cpp).
        let mut prev = [0u8; 32];
        for (i, b) in prev.iter_mut().enumerate() {
            *b = i as u8;
        }
        let d = draw_digest(&prev, 12345);
        assert_eq!(
            hex(&d),
            "de233f46bbab5baf7e4e6183469038e219e360be104d018f4e0691ad0f70e3a7"
        );
        assert_eq!(tier(&d), 15);
        assert!(!below(&d, 100));

        let d1 = draw_digest(&[0x11u8; 32], 100);
        assert_eq!(
            hex(&d1),
            "3086b122f52f0c27064e330f31e1ebf04164cf07be2b1dcfc0ed70647b7bd63e"
        );
        assert_eq!(tier(&d1), 66);

        let d3 = draw_digest(&[0x22u8; 32], 108);
        assert!(below(&d3, 100));
        assert!(!below(&d3, 1000));
        assert_eq!(tier(&d3), 87);

        let d4 = draw_digest(&[0x22u8; 32], 440);
        assert!(below(&d4, 1000) && below(&d4, 100));
        assert_eq!(tier(&d4), 66);
    }

    #[test]
    fn threshold_matches_expected_boundaries() {
        let t100 = threshold_be(100);
        // floor((2^256-1)/100) starts with 0x028f5c28f5c28f5c…
        assert_eq!(t100[0], 0x02);
        assert_eq!(t100[1], 0x8f);
        // Boundary: t-1 passes, t does not.
        let mut below_t = t100;
        // decrement big-endian by 1
        for i in (0..32).rev() {
            if below_t[i] > 0 {
                below_t[i] -= 1;
                break;
            }
            below_t[i] = 0xff;
        }
        assert!(below(&below_t, 100));
        assert!(!below(&t100, 100));
        assert!(!below(&[0xffu8; 32], 100));
        assert!(below(&[0u8; 32], 100));
    }

    #[test]
    fn display_hex_roundtrip() {
        let internal = display_hex_to_internal(
            "000024a132b37b93939504f73f8b3d6477f256c072254bc9379dde44321f3cfa",
        )
        .expect("hex");
        // internal[0] = last display byte
        assert_eq!(internal[0], 0xfa);
        assert_eq!(internal[31], 0x00);
    }

    #[test]
    fn pot_pct_matches_the_frozen_tier_fractions() {
        // The tier roll is 0-99, not a percent: 10/25/50% bands from the
        // consensus fractions (a UI that prints the tier as a percent
        // overstates tier 87 as 87% instead of 25%).
        for t in 0..70u64 {
            assert_eq!(lm_rewards::pot::pot_fraction(t), (1, 10), "tier {t}");
        }
        for t in 70..95u64 {
            assert_eq!(lm_rewards::pot::pot_fraction(t), (1, 4), "tier {t}");
        }
        for t in 95..100u64 {
            assert_eq!(lm_rewards::pot::pot_fraction(t), (1, 2), "tier {t}");
        }
        let pct = |t: u64| {
            let (num, den) = lm_rewards::pot::pot_fraction(t);
            num * 100 / den
        };
        assert_eq!(pct(0), 10);
        assert_eq!(pct(69), 10);
        assert_eq!(pct(70), 25);
        assert_eq!(pct(87), 25);
        assert_eq!(pct(94), 25);
        assert_eq!(pct(95), 50);
        assert_eq!(pct(99), 50);
    }
}
