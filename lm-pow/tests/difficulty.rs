//! Difficulty vectors — cross-checked against the Python integer oracle
//! (doc 05 §5.3 rev 1.1). Any divergence is a consensus-breaking bug.

use lm_pow::difficulty::*;
use primitive_types::U256;

fn u256_pow2(e: u32) -> U256 {
    U256::from(1u8) << e
}

#[test]
fn v1_steady_state_is_fixed_point() {
    let g = u256_pow2(220);
    let targets = vec![g; 60];
    let times: Vec<u64> = (0..=60).map(|i| i * 60).collect();
    assert_eq!(lwma_next_target(&targets, &times), Some(g));
}

#[test]
fn v2_slowdown_clamped_to_plus_third() {
    // 2x slow would double; clamp caps at last + last/3.
    let g = u256_pow2(220);
    let targets = vec![g; 60];
    let times: Vec<u64> = (0..=60).map(|i| i * 120).collect();
    let want = U256([
        0x5555_5555_5555_5555,
        0x5555_5555_5555_5555,
        0x5555_5555_5555_5555,
        0x0000_0000_1555_5555,
    ]);
    assert_eq!(lwma_next_target(&targets, &times), Some(want));
    assert_eq!(want, g + g / 3);
}

#[test]
fn v3_speedup_clamped_to_minus_third() {
    let g = u256_pow2(220);
    let targets = vec![g; 60];
    let times: Vec<u64> = (0..=60).map(|i| i * 30).collect();
    let want = U256([
        0xaaaa_aaaa_aaaa_aaab,
        0xaaaa_aaaa_aaaa_aaaa,
        0xaaaa_aaaa_aaaa_aaaa,
        0x0000_0000_0aaa_aaaa,
    ]);
    assert_eq!(lwma_next_target(&targets, &times), Some(want));
    assert_eq!(want, g - g / 3);
}

#[test]
fn v4_ragged_oracle_vector() {
    // targets[i] = 2^220 - i*2^212, times[i] = 60i + 13*(i mod 7).
    let targets: Vec<U256> = (0..60)
        .map(|i| u256_pow2(220) - U256::from(i) * u256_pow2(212))
        .collect();
    let times: Vec<u64> = (0..=60).map(|i| i * 60 + (i % 7) * 13).collect();
    let want = U256([
        0xbfdf_c4e9_0080_ec5b,
        0x5bfd_fc4e_9008_0ec5,
        0xc5bf_dfc4_e900_80ec,
        0x0000_0000_0eda_080e,
    ]);
    assert_eq!(lwma_next_target(&targets, &times), Some(want));
}

#[test]
fn zero_solvetimes_floor_at_minus_third() {
    // All-equal timestamps: raw computes 0, clamp holds 2/3 of last.
    let g = u256_pow2(220);
    let targets = vec![g; 60];
    let times = vec![1_000_000u64; 61];
    assert_eq!(lwma_next_target(&targets, &times), Some(g - g / 3));
}

#[test]
fn bad_lengths_rejected() {
    let g = u256_pow2(220);
    assert_eq!(lwma_next_target(&vec![g; 59], &[0u64; 61]), None);
    assert_eq!(lwma_next_target(&vec![g; 60], &[0u64; 60]), None);
}

#[test]
fn pow_limit_is_0x1f00ffff() {
    assert_eq!(compact_to_target(0x1f00ffff), Some(POW_LIMIT));
    assert_eq!(target_to_compact(POW_LIMIT), 0x1f00ffff);
}

#[test]
fn nbits_bitcoin_genesis_vector() {
    // Bitcoin genesis 0x1d00ffff -> 0x00000000FFFF00...0 (Bitcoin-exact).
    let decoded = compact_to_target(0x1d00ffff).expect("valid encoding");
    assert_eq!(decoded, U256::from(0xffffu64) << 208);
    assert_eq!(target_to_compact(decoded), 0x1d00ffff);
}

#[test]
fn nbits_rejects_negative_and_overflow() {
    assert_eq!(compact_to_target(0x1c800000), None); // sign bit
    assert_eq!(compact_to_target(0x02808000), None); // sign bit, small size
    assert_eq!(compact_to_target(0x2200ffff), None); // overflow past 256 bits
                                                     // Degenerate zero-size decodes to target 0 (Bitcoin-exact); target 0
                                                     // never validates any hash, so it is harmless.
    assert_eq!(compact_to_target(0x00123456), Some(U256::zero()));
}

#[test]
fn next_target_never_easier_than_limit() {
    // Absurdly easy history still clamps to POW_LIMIT.
    let easy = POW_LIMIT;
    let targets = vec![easy; 60];
    let times: Vec<u64> = (0..=60).map(|i| i * 360).collect(); // max-clamped st
    let next = lwma_next_target(&targets, &times).expect("valid lengths");
    assert!(next <= POW_LIMIT);
}

#[test]
fn jackpot_threshold_is_floor_target_over_100() {
    // Exact division anchor: 100 * q + r -> threshold q.
    let target = U256::from(10_000u64) + U256::from(99u64); // 10099 -> 100
    assert_eq!(jackpot_threshold(target), U256::from(100u64));
    assert_eq!(jackpot_threshold(U256::zero()), U256::zero());
    // POW_LIMIT threshold is exact floor division, not a float approx.
    assert_eq!(jackpot_threshold(POW_LIMIT), POW_LIMIT / 100);
}

#[test]
fn jackpot_trigger_is_strict_less_on_threshold() {
    let target = U256::from(100_000u64);
    let thr_be = jackpot_threshold(target).to_big_endian(); // = 1000
    assert_eq!(thr_be[30], 0x03);
    assert_eq!(thr_be[31], 0xE8);
    let mut below = [0u8; 32];
    below[31] = 0xE7; // 999 < 1000
    let mut at = [0u8; 32];
    at[30] = 0x03;
    at[31] = 0xE8; // exactly 1000
    assert!(is_jackpot(&below, target));
    assert!(!is_jackpot(&at, target), "equal to threshold is not a win");
    assert!(!is_jackpot(&[0xffu8; 32], target));
    // Zero target never triggers (threshold 0, nothing is strictly below).
    assert!(!is_jackpot(&[0u8; 32], U256::zero()));
    // Crate-root helper agrees with the difficulty helper.
    assert_eq!(
        lm_pow::is_jackpot(&below, target),
        is_jackpot(&below, target)
    );
}
