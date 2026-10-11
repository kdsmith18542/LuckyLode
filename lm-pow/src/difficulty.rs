//! Difficulty: exact integer LWMA retarget + Bitcoin-compatible `nBits`
//! (lowmine doc 05 §5.3 rev 1.1). No floats anywhere — byte-identical
//! results on every architecture are a consensus requirement.

use primitive_types::{U256, U512};

/// LWMA window: last 60 blocks.
pub const LWMA_N: usize = 60;
/// Target block time in seconds.
pub const LWMA_T: u64 = 60;
/// `N(N+1)/2` weight normalizer.
pub const LWMA_K: u64 = LWMA_N as u64 * (LWMA_N as u64 + 1) / 2; // 1830
/// Per-solvetime clamp: `±6T` (shock absorber, Zawy-style).
pub const SOLVETIME_CLAMP: u64 = 6 * LWMA_T; // 360 s
/// Per-block target adjustment clamp: `±1/3` of the previous target.
pub const ADJUST_DENOM: u64 = 3;
/// Easiest allowed target (`0x1f00ffff` expansion — doc 05 §5.3).
pub const POW_LIMIT: U256 = U256([
    0x0000_0000_0000_0000,
    0x0000_0000_0000_0000,
    0x0000_0000_0000_0000,
    0x0000_ffff_0000_0000,
]);

/// Decode Bitcoin-style compact `nBits` to a target.
/// `None` on negative, zero-mantissa-with-size, or overflowing encodings
/// (Bitcoin-exact rules). Targets above [`POW_LIMIT`] decode fine — the
/// limit is enforced by [`lwma_next_target`] and block validation, not here.
pub fn compact_to_target(nbits: u32) -> Option<U256> {
    let size = (nbits >> 24) as usize;
    let word = nbits & 0x007f_ffff;
    if nbits & 0x0080_0000 != 0 {
        return None; // negative — always invalid
    }
    if word == 0 {
        return None; // zero mantissa carries no difficulty information
    }
    if size > 34 || (word > 0xff && size > 33) || (word > 0xffff && size > 32) {
        return None; // overflow — exceeds 256 bits
    }
    let target = if size <= 3 {
        U256::from(word) >> (8 * (3 - size) as u32)
    } else {
        U256::from(word) << (8 * (size - 3) as u32)
    };
    Some(target)
}

/// Encode a target to compact `nBits` (Bitcoin-exact, including the
/// sign-bit shift-up rule). Assumes `target <= U256::MAX` (always true).
pub fn target_to_compact(target: U256) -> u32 {
    if target.is_zero() {
        return 0;
    }
    // Byte length of the target.
    let mut size = 32;
    while size > 1 && target >> ((size - 1) * 8) == U256::zero() {
        size -= 1;
    }
    let mut word = if size <= 3 {
        (target.low_u64() << (8 * (3 - size))) as u32
    } else {
        ((target >> (8 * (size - 3))) & U256::from(0xffff_ffffu64)).low_u32()
    };
    // Sign-bit rule: shift down so the mantissa stays positive.
    let mut size = size as u32;
    if word & 0x0080_0000 != 0 {
        word >>= 8;
        size += 1;
    }
    debug_assert!(word <= 0x007f_ffff);
    (size << 24) | word
}

/// Jackpot odds divisor: a block is a jackpot iff `out < floor(T / 100)`
/// (lowmine doc 06 §6.1 — ~1% of blocks, the PoW hash is the ticket).
pub const JACKPOT_ODDS: u64 = 100;

/// Jackpot threshold for a block target: exact integer `floor(T / 100)`.
/// Verifiers recompute identically; odds never drift with hashrate.
#[must_use]
pub fn jackpot_threshold(target: U256) -> U256 {
    target / JACKPOT_ODDS
}

/// Jackpot check on a big-endian PoW digest: `out < floor(T / 100)`.
/// Same strict-less semantics as [`crate::meets_target`]; equal is not a win.
#[must_use]
pub fn is_jackpot(out_be: &[u8; 32], target: U256) -> bool {
    let thr_be = jackpot_threshold(target).to_big_endian();
    out_be < &thr_be
}

/// LWMA next target (doc 05 §5.3 rev 1.1, exact integer algorithm):
///
/// ```text
/// acc  = Σ clamp(times[i]-times[i-1], 0, 6T) * i,  i = 1..=60
///        (i = 1 oldest … i = 60 newest: recent blocks weigh most)
/// avg  = Σ targets / 60                          (floor)
/// raw  = avg * acc / (T * K)                      (floor, U512 intermediate,
///                                                 saturate to U256::MAX)
/// next = clamp(raw, last - last/3, last + last/3) (floor legs)
/// next = min(next, POW_LIMIT)
/// ```
///
/// `targets` oldest→newest (60 entries), `times` 61 unix stamps
/// (`times[0]` parents the first block). `None` on wrong lengths.
pub fn lwma_next_target(targets: &[U256], times: &[u64]) -> Option<U256> {
    if targets.len() != LWMA_N || times.len() != LWMA_N + 1 {
        return None;
    }
    let mut acc: u64 = 0;
    for i in 1..=LWMA_N {
        let st = times[i].saturating_sub(times[i - 1]).min(SOLVETIME_CLAMP);
        acc += st * i as u64;
    }
    let sum: U512 = targets
        .iter()
        .fold(U512::zero(), |acc, t| acc + U512::from(*t));
    let avg = sum / LWMA_N;
    let raw512 = avg * acc / (LWMA_T * LWMA_K);
    let raw = if raw512 > U512::from(U256::MAX) {
        U256::MAX
    } else {
        raw512.try_into().expect("bounded above")
    };
    let last = targets[LWMA_N - 1];
    let lo = last - last / ADJUST_DENOM;
    let hi = last + last / ADJUST_DENOM;
    let next = raw.max(lo).min(hi);
    Some(next.min(POW_LIMIT))
}
