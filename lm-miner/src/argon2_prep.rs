//! Argon2id preparation + CPU fill mirror for the GPU pipeline.
//!
//! The GPU kernel runs ONLY the memory-hard fill (doc 15 §15.1). Everything
//! else — the BLAKE2b H0 pre-hash, the initial two blocks per lane, and the
//! variable-length finalization — happens on the CPU here. This module also
//! contains a **CPU mirror of the fill** with the exact structure of the
//! OpenCL kernel, so the pipeline is fully testable without a GPU:
//!
//!   prep (this file) + fill mirror  ==  `lm-pow`'s argon2id mem  (KAT)
//!   GPU kernel                      ==  fill mirror              (diff test)
//!
//! Params are the frozen EmberHash ones: p=1, m=1024 KiB, t=3, v=0x13,
//! Argon2id, tag 32. Transcribed from the `argon2` 0.6 crate internals
//! (fill_blocks / compress / blake2b_long) — the same code that produced
//! the frozen vectors, so the mirror is authoritative rather than
//! reinvented.
//!
//! NOTE: written but not yet compiled (grind window). Verify with
//! `cargo test -p lm-miner argon2_prep` before GPU work starts.

use blake2::digest::{Update, VariableOutput};
use blake2::{Blake2b512, Blake2bVar, Digest};

/// Argon2 parameters, frozen by doc 05.
pub const M_COST: u32 = 1024; // KiB
pub const T_COST: u32 = 3;
pub const P_COST: u32 = 1;
pub const VERSION: u32 = 0x13;
pub const ALGORITHM_ID: u32 = 2; // Argon2id
pub const TAG_LEN: u32 = 32;

const BLOCK_WORDS: usize = 128; // 1 KiB blocks as u64 words
const LANE_LENGTH: usize = M_COST as usize; // p=1: one lane, 1024 blocks
const SEGMENT_LENGTH: usize = LANE_LENGTH / 4; // 256
const PASSES: usize = T_COST as usize;

/// `blake2b_long` (Argon2's H'): variable-length BLAKE2b with the LE32
/// length prefix, exactly as the reference implements it.
pub fn blake2b_long(inputs: &[&[u8]], out_len: usize) -> Vec<u8> {
    assert!(out_len > 0);
    let len_bytes = (out_len as u32).to_le_bytes();

    if out_len <= 64 {
        let mut h = Blake2bVar::new(out_len).expect("valid len");
        Update::update(&mut h, &len_bytes);
        for &input in inputs {
            Update::update(&mut h, input);
        }
        let mut out = vec![0u8; out_len];
        h.finalize_variable(&mut out).expect("valid len");
        return out;
    }

    let mut h = Blake2b512::new();
    Digest::update(&mut h, len_bytes);
    for &input in inputs {
        Digest::update(&mut h, input);
    }
    let mut last = h.finalize().to_vec();

    let mut out = Vec::with_capacity(out_len);
    out.extend_from_slice(&last[..32]);
    while out_len - out.len() > 64 {
        last = Blake2b512::digest(&last).to_vec();
        out.extend_from_slice(&last[..32]);
    }
    let remaining = out_len - out.len();
    let mut hv = Blake2bVar::new(remaining).expect("<=64");
    Update::update(&mut hv, &last);
    let mut tail = vec![0u8; remaining];
    hv.finalize_variable(&mut tail).expect("<=64");
    out.extend_from_slice(&tail);
    out
}

/// H0: the parameter/password/salt/AD pre-hash (BLAKE2b-512), exact field
/// order from the reference `initial_hash`.
pub fn h0(seed: &[u8], salt: &[u8], ad: &[u8]) -> [u8; 64] {
    let mut d = Blake2b512::new();
    Digest::update(&mut d, P_COST.to_le_bytes());
    Digest::update(&mut d, TAG_LEN.to_le_bytes());
    Digest::update(&mut d, M_COST.to_le_bytes());
    Digest::update(&mut d, T_COST.to_le_bytes());
    Digest::update(&mut d, VERSION.to_le_bytes());
    Digest::update(&mut d, ALGORITHM_ID.to_le_bytes());
    Digest::update(&mut d, (seed.len() as u32).to_le_bytes());
    Digest::update(&mut d, seed);
    Digest::update(&mut d, (salt.len() as u32).to_le_bytes());
    Digest::update(&mut d, salt);
    Digest::update(&mut d, 0u32.to_le_bytes()); // no secret
    Digest::update(&mut d, (ad.len() as u32).to_le_bytes());
    Digest::update(&mut d, ad);
    let mut out = [0u8; 64];
    out.copy_from_slice(&d.finalize());
    out
}

/// The two initial blocks per lane: `blake2b_long(H0 || LE32(i) || LE32(l))`.
pub fn initial_blocks(h0: &[u8; 64]) -> [[u64; BLOCK_WORDS]; 2] {
    let mut out = [[0u64; BLOCK_WORDS]; 2];
    for (i, block) in out.iter_mut().enumerate() {
        let i_le = (i as u32).to_le_bytes();
        let l_le = 0u32.to_le_bytes(); // p=1: lane 0
        let bytes = blake2b_long(&[h0, &i_le, &l_le], BLOCK_WORDS * 8);
        for (w, chunk) in bytes.chunks_exact(8).enumerate() {
            block[w] = u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
        }
    }
    out
}

// ---- the fill: CPU mirror of the OpenCL kernel (same structure) ----

#[inline(always)]
fn blamka_step(a: &mut u64, b: &mut u64, c: &mut u64, d: &mut u64) {
    let trunc = 0xffff_ffffu64;
    *a = a
        .wrapping_add(*b)
        .wrapping_add(2u64.wrapping_mul((*a & trunc).wrapping_mul(*b & trunc)));
    *d = (*d ^ *a).rotate_right(32);
    *c = c
        .wrapping_add(*d)
        .wrapping_add(2u64.wrapping_mul((*c & trunc).wrapping_mul(*d & trunc)));
    *b = (*b ^ *c).rotate_right(24);
    *a = a
        .wrapping_add(*b)
        .wrapping_add(2u64.wrapping_mul((*a & trunc).wrapping_mul(*b & trunc)));
    *d = (*d ^ *a).rotate_right(16);
    *c = c
        .wrapping_add(*d)
        .wrapping_add(2u64.wrapping_mul((*c & trunc).wrapping_mul(*d & trunc)));
    *b = (*b ^ *c).rotate_right(63);
}

/// Permute 16 words. Takes and returns the array by value: Rust's borrow
/// checker rejects simultaneous `&mut v[i]` borrows (even at constant
/// indices), so the words are moved into 16 named locals, permuted, and
/// moved back — the compile-safe equivalent of the kernel's pointer
/// indexing, bit-identical by construction.
fn permute16(mut v: [u64; 16]) -> [u64; 16] {
    let (mut v0, mut v1, mut v2, mut v3) = (v[0], v[1], v[2], v[3]);
    let (mut v4, mut v5, mut v6, mut v7) = (v[4], v[5], v[6], v[7]);
    let (mut v8, mut v9, mut v10, mut v11) = (v[8], v[9], v[10], v[11]);
    let (mut v12, mut v13, mut v14, mut v15) = (v[12], v[13], v[14], v[15]);
    blamka_step(&mut v0, &mut v4, &mut v8, &mut v12);
    blamka_step(&mut v1, &mut v5, &mut v9, &mut v13);
    blamka_step(&mut v2, &mut v6, &mut v10, &mut v14);
    blamka_step(&mut v3, &mut v7, &mut v11, &mut v15);
    blamka_step(&mut v0, &mut v5, &mut v10, &mut v15);
    blamka_step(&mut v1, &mut v6, &mut v11, &mut v12);
    blamka_step(&mut v2, &mut v7, &mut v8, &mut v13);
    blamka_step(&mut v3, &mut v4, &mut v9, &mut v14);
    v[0] = v0;
    v[1] = v1;
    v[2] = v2;
    v[3] = v3;
    v[4] = v4;
    v[5] = v5;
    v[6] = v6;
    v[7] = v7;
    v[8] = v8;
    v[9] = v9;
    v[10] = v10;
    v[11] = v11;
    v[12] = v12;
    v[13] = v13;
    v[14] = v14;
    v[15] = v15;
    v
}

/// Argon2 compression G: `P(prev ^ ref) ^ (prev ^ ref)`.
///
/// The 16 words of each round are copied into locals before permuting:
/// Rust's borrow checker rejects 16 simultaneous `&mut q[i]` borrows, and
/// the locals pattern is the compile-safe equivalent of the kernel's
/// pointer indexing (bit-identical result).
fn compress(prev: &[u64; BLOCK_WORDS], reference: &[u64; BLOCK_WORDS]) -> [u64; BLOCK_WORDS] {
    let mut r = [0u64; BLOCK_WORDS];
    for i in 0..BLOCK_WORDS {
        r[i] = prev[i] ^ reference[i];
    }
    let mut q = r;
    for row in 0..8 {
        let b = row * 16;
        let mut v = [0u64; 16];
        v.copy_from_slice(&q[b..b + 16]);
        let v = permute16(v);
        q[b..b + 16].copy_from_slice(&v);
    }
    for col in 0..8 {
        let b = col * 2;
        let mut v = [0u64; 16];
        let idx = [
            b,
            b + 1,
            b + 16,
            b + 17,
            b + 32,
            b + 33,
            b + 48,
            b + 49,
            b + 64,
            b + 65,
            b + 80,
            b + 81,
            b + 96,
            b + 97,
            b + 112,
            b + 113,
        ];
        for (k, &i) in idx.iter().enumerate() {
            v[k] = q[i];
        }
        let v = permute16(v);
        for (k, &i) in idx.iter().enumerate() {
            q[i] = v[k];
        }
    }
    for i in 0..BLOCK_WORDS {
        q[i] ^= r[i];
    }
    q
}

/// Address-block generation for Argon2id's data-independent half
/// (pass 0, slices 0-1): `addr = G(zero, G(zero, input_with_counter))`.
fn next_address_block(input_block: &mut [u64; BLOCK_WORDS]) -> [u64; BLOCK_WORDS] {
    input_block[6] += 1;
    let zero = [0u64; BLOCK_WORDS];
    let first = compress(&zero, input_block);
    compress(&zero, &first)
}

/// The full fill for p=1: 3 passes × 4 slices × 256 blocks. Mirror of the
/// kernel `argon2id_fill` (same order, same arithmetic).
pub fn fill(initial: &[[u64; BLOCK_WORDS]; 2]) -> [u64; BLOCK_WORDS] {
    let mut mem = vec![[0u64; BLOCK_WORDS]; LANE_LENGTH];
    mem[0] = initial[0];
    mem[1] = initial[1];

    for pass in 0..PASSES {
        for slice in 0..4usize {
            let data_independent = pass == 0 && slice < 2;
            let mut input_block = [0u64; BLOCK_WORDS];
            let mut address_block = [0u64; BLOCK_WORDS];
            if data_independent {
                input_block[0] = pass as u64;
                input_block[1] = 0; // lane
                input_block[2] = slice as u64;
                input_block[3] = LANE_LENGTH as u64;
                input_block[4] = PASSES as u64;
                input_block[5] = ALGORITHM_ID as u64;
            }
            let first_block = if pass == 0 && slice == 0 {
                if data_independent {
                    address_block = next_address_block(&mut input_block);
                }
                2
            } else {
                0
            };

            // The mirror tracks cur/prev explicitly (same shape as the
            // kernel); clippy's iterator suggestion would obscure that.
            #[allow(clippy::explicit_counter_loop)]
            let mut cur_index = slice * SEGMENT_LENGTH + first_block;
            let mut prev_index = if slice == 0 && first_block == 0 {
                cur_index + LANE_LENGTH - 1
            } else {
                cur_index - 1
            };

            #[allow(clippy::explicit_counter_loop)]
            for block in first_block..SEGMENT_LENGTH {
                let rand = if data_independent {
                    let address_index = block % 128;
                    if address_index == 0 {
                        address_block = next_address_block(&mut input_block);
                    }
                    address_block[address_index]
                } else {
                    mem[prev_index][0]
                };

                // p=1: ref_lane is always 0 (same lane).
                let reference_area_size = if pass == 0 {
                    if slice == 0 {
                        block - 1
                    } else {
                        slice * SEGMENT_LENGTH + block - 1
                    }
                } else {
                    LANE_LENGTH - SEGMENT_LENGTH + block - 1
                };

                let mut map = rand & 0xffff_ffff;
                map = (map.wrapping_mul(map)) >> 32;
                let relative_position =
                    reference_area_size - 1 - ((reference_area_size as u64 * map) >> 32) as usize;
                let start_position = if pass != 0 && slice != 3 {
                    (slice + 1) * SEGMENT_LENGTH
                } else {
                    0
                };
                let ref_index = (start_position + relative_position) % LANE_LENGTH;

                let result = compress(&mem[prev_index], &mem[ref_index]);
                if pass == 0 {
                    mem[cur_index] = result;
                } else {
                    for i in 0..BLOCK_WORDS {
                        mem[cur_index][i] ^= result[i];
                    }
                }
                prev_index = cur_index;
                cur_index += 1;
            }
        }
    }
    mem[LANE_LENGTH - 1]
}

/// Full Argon2id mem output for the EmberHash parameters: `blake2b_long`
/// of the last block, 32 bytes.
pub fn mem(seed: &[u8], salt: &[u8], ad: &[u8]) -> [u8; 32] {
    let h0 = h0(seed, salt, ad);
    let initial = initial_blocks(&h0);
    let last = fill(&initial);
    let mut last_bytes = [0u8; BLOCK_WORDS * 8];
    for (i, w) in last.iter().enumerate() {
        last_bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    let tag = blake2b_long(&[&last_bytes], 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&tag);
    out
}

/// EmberHash salt for a height: `"LMv1" || height_le64` (frozen, doc 05).
pub fn ember_salt(height: u64) -> [u8; 12] {
    let mut salt = [0u8; 12];
    salt[..4].copy_from_slice(b"LMv1");
    salt[4..].copy_from_slice(&height.to_le_bytes());
    salt
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE gate: prep + CPU fill must equal `lm-pow`'s argon2id mem for
    /// the frozen vectors (the same values the GPU kernel will be held to).
    #[test]
    fn cpu_pipeline_matches_lm_pow_mem() {
        let header: Vec<u8> = (0u8..80).collect();
        let cases: [(u64, u64, &str); 3] = [
            (0, 1, "446765c0245c7d3da7185456343055971444c45dd90b015d4fd7b3c6262a2b99"),
            (1, 1, "3924a6a0bfefe126c881fc7764794d6f45b992f5f22d33b2fd339af5bf405dbe"),
            (u64::MAX, 12345, "48063374526971416c8c7d8f7b4e4ab815d4e4c6a4956024d2c3b459817a2791"),
        ];
        for (nonce, height, want_hex) in cases {
            let f = lm_pow::pow_full(&header, nonce, height);
            let mine = mem(&f.seed, &ember_salt(height), b"lowmine-pow/v1");
            assert_eq!(
                mine.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                want_hex,
                "nonce={nonce} height={height}"
            );
            // And the full out stage matches too (mem feeds BLAKE3).
            assert_eq!(f.mem, mine);
        }
    }

    #[test]
    fn salt_is_frozen_layout() {
        let s = ember_salt(0x0102_0304_0506_0708);
        assert_eq!(&s[..4], b"LMv1");
        assert_eq!(&s[4..], &0x0102_0304_0506_0708u64.to_le_bytes());
    }

    #[test]
    fn blake2b_long_short_path_is_variable_output() {
        // 32-byte path must equal BLAKE2bVar (not truncated BLAKE2b-512).
        let a = blake2b_long(&[b"abc"], 32);
        let mut h = Blake2bVar::new(32).unwrap();
        Update::update(&mut h, &32u32.to_le_bytes());
        Update::update(&mut h, b"abc");
        let mut b = vec![0u8; 32];
        h.finalize_variable(&mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn blake2b_long_long_path_chains() {
        // 1024-byte path: length check + chaining structure sanity.
        let out = blake2b_long(&[b"seed", &[0u8, 0, 0, 0], &[0u8, 0, 0, 0]], 1024);
        assert_eq!(out.len(), 1024);
        // Deterministic.
        assert_eq!(out, blake2b_long(&[b"seed", &[0u8, 0, 0, 0], &[0u8, 0, 0, 0]], 1024));
        // Different inputs diverge.
        assert_ne!(out, blake2b_long(&[b"seed", &[1u8, 0, 0, 0], &[0u8, 0, 0, 0]], 1024));
    }
}
