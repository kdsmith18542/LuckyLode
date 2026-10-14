//! `lm-pow`: reference implementation of **LuckyHash v1**, the Lucky Lode
//! proof-of-work (frozen spec: `../05-pow-algorithm.md` rev 1.0).
//!
//! Construction:
//!
//! ```text
//! seed = BLAKE3(header_bytes || nonce_le64)
//! salt = "LMv1" || height_le64
//! mem  = Argon2id(seed, salt, AD="lowmine-pow/v1", m=1024, t=3, p=1)
//! out  = BLAKE3(mem || seed)          // compared against target, big-endian
//! ```
//!
//! This crate is the normative implementation: any disagreement with the
//! frozen test vectors (doc 05 §5.4) is a bug here, not in the spec.

use argon2::{Algorithm, Argon2, AssociatedData, ParamsBuilder, Version};
use primitive_types::U256;
use std::sync::OnceLock;

pub mod difficulty;

/// Argon2id memory cost in KiB (1 MiB — phone L2/L3 friendly).
pub const M_COST: u32 = 1024;
/// Argon2id time cost (passes).
pub const T_COST: u32 = 3;
/// Argon2id parallelism (one lane per instance; miners scale across threads).
pub const P_COST: u32 = 1;
/// Salt prefix; full salt is `SALT_PREFIX || height_le64`.
pub const SALT_PREFIX: &[u8; 4] = b"LMv1";
/// Argon2 associated data (domain separation).
pub const ASSOCIATED_DATA: &[u8] = b"lowmine-pow/v1";
/// Digest length in bytes.
pub const OUT_LEN: usize = 32;

/// All three stages of one evaluation (test-vector and debugging surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullHash {
    /// `BLAKE3(header || nonce_le64)`.
    pub seed: [u8; 32],
    /// `Argon2id(seed, salt, AD, m=1024, t=3, p=1)`.
    pub mem: [u8; 32],
    /// `BLAKE3(mem || seed)` — the value compared against target.
    pub out: [u8; 32],
}

fn argon2_ctx() -> &'static Argon2<'static> {
    static CTX: OnceLock<Argon2<'static>> = OnceLock::new();
    CTX.get_or_init(|| {
        let params = ParamsBuilder::new()
            .m_cost(M_COST)
            .t_cost(T_COST)
            .p_cost(P_COST)
            .output_len(OUT_LEN)
            .data(AssociatedData::new(ASSOCIATED_DATA).expect("AD within 32 B limit"))
            .build()
            .expect("LuckyHash v1 params satisfy Argon2 limits");
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
    })
}

/// Seed stage alone: `BLAKE3(header || nonce_le64)`. Exposed for
/// alternative Argon2id backends (the OpenCL fill path, doc 15) that
/// reproduce [`pow_full`]'s stages on different hardware.
#[must_use]
pub fn seed(header: &[u8], nonce: u64) -> [u8; OUT_LEN] {
    let mut pre = Vec::with_capacity(header.len() + 8);
    pre.extend_from_slice(header);
    pre.extend_from_slice(&nonce.to_le_bytes());
    *blake3::hash(&pre).as_bytes()
}

/// Digest stage alone: `BLAKE3(mem || seed)`. Exposed for the same
/// reason as [`seed`].
#[must_use]
pub fn final_digest(seed: &[u8; OUT_LEN], mem: &[u8; OUT_LEN]) -> [u8; OUT_LEN] {
    let mut post = Vec::with_capacity(OUT_LEN * 2);
    post.extend_from_slice(mem);
    post.extend_from_slice(seed);
    *blake3::hash(&post).as_bytes()
}

/// Full three-stage evaluation.
pub fn pow_full(header: &[u8], nonce: u64, height: u64) -> FullHash {
    let seed = seed(header, nonce);

    let mut salt = Vec::with_capacity(SALT_PREFIX.len() + 8);
    salt.extend_from_slice(SALT_PREFIX);
    salt.extend_from_slice(&height.to_le_bytes());

    let mut mem = [0u8; OUT_LEN];
    argon2_ctx()
        .hash_password_into(&seed, &salt, &mut mem)
        .expect("fixed params; inputs always valid");

    FullHash { seed, mem, out: final_digest(&seed, &mem) }
}

/// Final digest only (the hot path for miners and verifiers).
pub fn pow_hash(header: &[u8], nonce: u64, height: u64) -> [u8; 32] {
    pow_full(header, nonce, height).out
}

/// Target check: `out < target`, both 256-bit big-endian (Bitcoin convention).
/// Equal is *not* valid (matches Bitcoin semantics).
#[must_use]
pub fn meets_target(out: &[u8; 32], target: &[u8; 32]) -> bool {
    out < target
}

/// Jackpot check (doc 06 §6.1): `out < floor(T / 100)`, digest big-endian,
/// target as [`U256`]. Same helper family as [`meets_target`] — see
/// [`difficulty::jackpot_threshold`] for the exact threshold.
#[must_use]
pub fn is_jackpot(out: &[u8; 32], target: U256) -> bool {
    difficulty::is_jackpot(out, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_compare_is_big_endian_strict() {
        let lo = [0x00u8; 32];
        let mut hi = [0x00u8; 32];
        hi[31] = 0x01;
        assert!(meets_target(&lo, &hi));
        assert!(!meets_target(&hi, &hi), "equal must not validate");
        assert!(!meets_target(&hi, &lo));
        // Leading byte dominates trailing bytes.
        let mut a = [0x00u8; 32];
        a[0] = 0x01;
        let mut b = [0x00u8; 32];
        b[31] = 0xff;
        assert!(!meets_target(&a, &b));
        assert!(meets_target(&b, &a));
    }

    #[test]
    fn max_target_accepts_anything_nonmax() {
        let max = [0xffu8; 32];
        let header: Vec<u8> = (0u8..80).collect();
        let out = pow_hash(&header, 0, 1);
        assert_ne!(out, max, "sanity: test hash is not all-0xff");
        assert!(meets_target(&out, &max));
    }

    #[test]
    fn height_binding_diverges() {
        let header: Vec<u8> = (0u8..80).collect();
        assert_ne!(
            pow_hash(&header, 0, 0),
            pow_hash(&header, 0, 1),
            "same nonce at different heights must differ"
        );
    }

    #[test]
    fn exposed_stages_compose_to_pow_full() {
        let header: Vec<u8> = (0u8..80).collect();
        let f = pow_full(&header, 7, 3);
        assert_eq!(seed(&header, 7), f.seed);
        assert_eq!(final_digest(&f.seed, &f.mem), f.out);
        assert_eq!(pow_hash(&header, 7, 3), f.out);
    }

    #[test]
    fn empty_header_accepted() {
        // Degenerate input must not panic; output still well-formed.
        let out = pow_hash(&[], 0, 0);
        assert_eq!(out.len(), OUT_LEN);
    }
}
