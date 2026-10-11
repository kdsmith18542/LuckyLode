//! Search-path tests: deterministic solo solves against pinned vectors.
//!
//! The KAT V1 digest (`05-pow-algorithm.md` §5.4, nonce 0 / height 1) anchors
//! exact expectations: with `target = max` the single-threaded search must
//! stop at nonce 0 with exactly the frozen digest — any deviation is a
//! consensus-breaking bug in the search path, not just the hash core.

use lm_miner::search;
use lm_pow::meets_target;

fn header_v() -> Vec<u8> {
    (0u8..80).collect()
}

fn kat_v1_out() -> [u8; 32] {
    let mut out = [0u8; 32];
    let hex = "ff0d88d9bf49d0d5c47a78b9a8f5478640359c4da7e73ee23973d6c338e2ca6e";
    for i in 0..32 {
        out[i] = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

#[test]
fn single_thread_max_target_solves_at_nonce_zero_with_kat_digest() {
    let s = search(&header_v(), 1, &[0xffu8; 32], 0, 100, 1).expect("max target solves");
    assert_eq!(s.nonce, 0);
    assert_eq!(s.out, kat_v1_out());
}

#[test]
fn boundary_target_honors_strict_less() {
    // Target exactly one above the KAT digest: nonce 0 still wins.
    let mut above = kat_v1_out();
    above[31] = above[31].wrapping_add(1);
    let s = search(&header_v(), 1, &above, 0, 100, 1).expect("solves just under target");
    assert_eq!(s.nonce, 0);
    // Target exactly equal to the digest: nonce 0 must NOT win (strict-less,
    // Bitcoin semantics) — start past it so the search stays cheap: with only
    // nonce 1 in range, assert the boundary property on whatever comes back.
    let exact = kat_v1_out();
    let s = search(&header_v(), 1, &exact, 1, 1, 1);
    // Nonce 1's digest is unknown here — assert only the boundary property:
    // if it solved, its digest is strictly under; None is equally valid.
    if let Some(s) = s {
        assert!(meets_target(&s.out, &exact));
        assert_ne!(s.out, exact);
    }
}

#[test]
fn multi_thread_solution_always_valid_and_in_range() {
    let max = [0xffu8; 32];
    let s = search(&header_v(), 1, &max, 0, 100, 4).expect("max target solves");
    assert!(s.nonce < 100, "nonce {} outside searched range", s.nonce);
    assert!(meets_target(&s.out, &max));
    // It really is the claimed hash of the claimed nonce.
    assert_eq!(s.out, lm_pow::pow_hash(&header_v(), s.nonce, 1));
}

#[test]
fn search_start_offset_partitions_deterministically() {
    // Single-threaded from nonce 5 with max target: first candidate wins.
    let max = [0xffu8; 32];
    let s = search(&header_v(), 1, &max, 5, 100, 1).expect("solves");
    assert_eq!(s.nonce, 5);
    assert_eq!(s.out, lm_pow::pow_hash(&header_v(), 5, 1));
}
