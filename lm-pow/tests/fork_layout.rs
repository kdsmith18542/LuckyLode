//! Fork-layout KAT (doc 05 Addendum A, vector A1): pins the canonical
//! 109-byte fork header form. The C++ `ember_hash` port must reproduce
//! these bytes; any mismatch is a consensus-breaking interface bug.

use lm_pow::pow_full;

fn fork_header() -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(&0x0001_0004u32.to_le_bytes()); // nVersion VERUS_V2
    h.extend_from_slice(&[0x11u8; 32]); // hashPrevBlock
    h.extend_from_slice(&[0x22u8; 32]); // hashMerkleRoot
    h.extend_from_slice(&[0x33u8; 32]); // hashFinalSaplingRoot
    h.extend_from_slice(&42u32.to_le_bytes()); // nTime
    h.extend_from_slice(&0x1d00_ffffu32.to_le_bytes()); // nBits
    h.push(0x00); // CompactSize(len(nSolution)) = 0
    h
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn fork_layout_is_109_bytes_minimal_form() {
    assert_eq!(fork_header().len(), 109);
    assert_eq!(
        fork_header()[..4],
        [0x04, 0x00, 0x01, 0x00],
        "nVersion little-endian"
    );
}

#[test]
fn vector_a1_frozen_outputs() {
    let f = pow_full(&fork_header(), 7, 100);
    assert_eq!(
        f.seed[..],
        hex("df2f1c48bdc8f58fb4f7247b5c4fe2257011c8842e3859fc245cd654a8030619")[..]
    );
    assert_eq!(
        f.mem[..],
        hex("d68a955d887a8e4d4c40d34ec2bc4fbdf8e8f4135e0e856dd6d6a2f957b8110d")[..]
    );
    assert_eq!(
        f.out[..],
        hex("e491f172e0d0adcb161fb264413292bfca631fff8c390218b6cabdac6f452131")[..]
    );
}

#[test]
fn solution_bytes_change_the_digest() {
    // Binding check: appending one solution byte must produce a different
    // digest (guards against accidentally excluding nSolution from input).
    let mut h = fork_header();
    h[108] = 0x01; // length 1
    h.push(0xaa); // one solution byte
    let f = pow_full(&h, 7, 100);
    assert_ne!(
        f.out[..],
        pow_full(&fork_header(), 7, 100).out[..],
        "solution must be part of header_bytes"
    );
}
