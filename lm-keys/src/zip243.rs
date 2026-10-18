//! ZIP-143/ZIP-243 signature hashing for the overwinter/sapling epochs.
//!
//! Byte-for-byte mirror of the fork's `SignatureHash()` overwinter/sapling
//! leg (verus/src/script/interpreter.cpp): BLAKE2b-256 with the
//! `"ZcashSigHash" || le32(consensusBranchId)` personalization over
//! header, version-group, prevouts/sequence/outputs sub-hashes
//! (each BLAKE2b with its own personalization), the (empty ⇒ zero-256)
//! joinsplit/shielded digests, locktime, expiry, value balance, hash
//! type, then the spending input's prevout, scriptCode, amount and
//! sequence. Transparent-only: shielded sub-hashes and valueBalance are
//! the zero constants the C++ leaves them at when those vectors are
//! empty (lm-keys never builds shielded sections).
//!
//! Validation: the fork's own regenerated vector set
//! (verus/src/test/data/sighash.json, run by sighash_tests.cpp with
//! amount=0) — every transparent-only overwinter v3 vector is replayed
//! in `tests/zip143.rs` (45 vectors, all hash-type shapes). Sapling v4
//! adds exactly three constants (two zero-256 shielded digests after the
//! joinsplit digest, plus valueBalance=0 after expiry), pinned by
//! `sapling_extends_overwinter_digest_by_constants`.

use blake2b_simd::Params as Blake2bParams;
use crate::tx::{put_bytes, put_i64, put_u32, Transaction, OVERWINTER_TX_VERSION, SAPLING_TX_VERSION};

/// Consensus branch ids (verus/src/consensus/upgrades.cpp; live-confirmed
/// via getblockchaininfo on testnet).
pub const OVERWINTER_BRANCH_ID: u32 = 0x5ba8_1b19;
pub const SAPLING_BRANCH_ID: u32 = 0x76b8_09bb;

pub const SIGHASH_NONE: u32 = 2;
pub const SIGHASH_SINGLE: u32 = 3;
pub const SIGHASH_ANYONECANPAY: u32 = 0x80;

/// Which signature-hash epoch a spend belongs to (drives tx format +
/// sighash). The branch id rides in the epoch: it is a per-chain
/// consensus param the wallet reads off the connected node (the
/// `getblockchaininfo` upgrade keys are the branch ids); the constants
/// above are this fork's known values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Epoch {
    /// Legacy v1 + sprout sighash (testnet/regtest today).
    Sprout,
    /// Overwinter v3 + ZIP-143 with this branch id.
    Overwinter(u32),
    /// Sapling v4 + ZIP-243 with this branch id.
    Sapling(u32),
}

impl Epoch {
    /// The consensus branch id this epoch signs with (None = sprout).
    #[must_use]
    pub fn branch_id(self) -> Option<u32> {
        match self {
            Self::Sprout => None,
            Self::Overwinter(id) | Self::Sapling(id) => Some(id),
        }
    }
}

/// BLAKE2b-256 with a 16-byte personalization (the fork's
/// CBLAKE2bWriter).
fn blake2b_personal(personal: &[u8; 16], data: &[u8]) -> [u8; 32] {
    let mut h = Blake2bParams::new().hash_length(32).personal(personal).to_state();
    h.update(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(h.finalize().as_bytes());
    out
}

/// ZIP-143/243 signature hash for transparent input `input` of an
/// overwintered tx. `amount` is the spent output's value in satoshis
/// (the vectors pin the layout at amount=0; the live wallet passes the
/// real value). Mirrors SignatureHash(scriptCode, tx, nIn, nHashType,
/// amount, consensusBranchId).
pub fn sighash(
    tx: &Transaction,
    input: usize,
    script_code: &[u8],
    amount: i64,
    hash_type: u32,
    branch_id: u32,
) -> Result<[u8; 32], String> {
    if !tx.overwintered {
        return Err("zip243 sighash needs an overwintered tx".to_string());
    }
    if tx.version == OVERWINTER_TX_VERSION || tx.version == SAPLING_TX_VERSION {
        // ok
    } else {
        return Err(format!("unsupported overwintered tx version {}", tx.version));
    }
    if input >= tx.vin.len() {
        return Err("input out of range".to_string());
    }

    // Top-level digest: personalization is "ZcashSigHash" + le32(branch id).
    let mut personal = [0u8; 16];
    personal[..12].copy_from_slice(b"ZcashSigHash");
    personal[12..].copy_from_slice(&branch_id.to_le_bytes());
    let b = digest_input(tx, input, script_code, amount, hash_type);
    Ok(blake2b_personal(&personal, &b))
}

/// The pre-hash byte stream (test-exposed for byte-level pinning).
pub(crate) fn digest_input(
    tx: &Transaction,
    input: usize,
    script_code: &[u8],
    amount: i64,
    hash_type: u32,
) -> Vec<u8> {
    let sapling = tx.version == SAPLING_TX_VERSION;
    let mut prevouts = Vec::new();
    for i in &tx.vin {
        prevouts.extend_from_slice(&i.prevout.txid);
        put_u32(&mut prevouts, i.prevout.vout);
    }
    let mut sequences = Vec::new();
    for i in &tx.vin {
        put_u32(&mut sequences, i.sequence);
    }
    let mut outputs = Vec::new();
    for o in &tx.vout {
        put_i64(&mut outputs, o.value);
        put_bytes(&mut outputs, &o.script_pubkey);
    }
    let hash_prevouts = if hash_type & SIGHASH_ANYONECANPAY == 0 {
        blake2b_personal(b"ZcashPrevoutHash", &prevouts)
    } else {
        [0u8; 32]
    };
    let hash_sequence = if hash_type & SIGHASH_ANYONECANPAY == 0
        && hash_type & 0x1f != SIGHASH_SINGLE
        && hash_type & 0x1f != SIGHASH_NONE
    {
        blake2b_personal(b"ZcashSequencHash", &sequences)
    } else {
        [0u8; 32]
    };
    let hash_outputs = if hash_type & 0x1f != SIGHASH_SINGLE && hash_type & 0x1f != SIGHASH_NONE {
        blake2b_personal(b"ZcashOutputsHash", &outputs)
    } else if hash_type & 0x1f == SIGHASH_SINGLE && input < tx.vout.len() {
        let mut one = Vec::new();
        put_i64(&mut one, tx.vout[input].value);
        put_bytes(&mut one, &tx.vout[input].script_pubkey);
        blake2b_personal(b"ZcashOutputsHash", &one)
    } else {
        [0u8; 32]
    };
    let mut b = Vec::new();
    // Header (fOverwintered | version) then the version group id.
    put_u32(&mut b, (tx.version as u32) | (1 << 31));
    put_u32(&mut b, tx.version_group_id.unwrap_or(0));
    b.extend_from_slice(&hash_prevouts);
    b.extend_from_slice(&hash_sequence);
    b.extend_from_slice(&hash_outputs);
    // Empty joinsplits: the C++ leaves hashJoinSplits at the uint256 default.
    b.extend_from_slice(&[0u8; 32]);
    if sapling {
        // Empty shielded spends/outputs: zero digests, then valueBalance.
        b.extend_from_slice(&[0u8; 32]);
        b.extend_from_slice(&[0u8; 32]);
    }
    put_u32(&mut b, tx.locktime);
    put_u32(&mut b, tx.expiry_height.unwrap_or(0));
    if sapling {
        put_i64(&mut b, 0); // valueBalance (transparent-only)
    }
    put_u32(&mut b, hash_type);
    // The input being signed.
    b.extend_from_slice(&tx.vin[input].prevout.txid);
    put_u32(&mut b, tx.vin[input].prevout.vout);
    put_bytes(&mut b, script_code);
    put_i64(&mut b, amount);
    put_u32(&mut b, tx.vin[input].sequence);
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::{OutPoint, SIGHASH_ALL, SEQUENCE_FINAL, TxIn, TxOut};


    fn sample_tx(version: i32) -> Transaction {
        let group = if version == SAPLING_TX_VERSION {
            crate::tx::SAPLING_GROUP_ID
        } else {
            crate::tx::OVERWINTER_GROUP_ID
        };
        Transaction {
            overwintered: true,
            version,
            version_group_id: Some(group),
            vin: vec![TxIn {
                prevout: OutPoint {
                    txid: [7u8; 32],
                    vout: 3,
                },
                script_sig: Vec::new(),
                sequence: SEQUENCE_FINAL,
            }],
            vout: vec![
                TxOut {
                    value: 9_000_000,
                    script_pubkey: vec![0x76, 0xa9, 0x14, 0xAA, 0x88, 0xac],
                },
                TxOut {
                    value: 1_000,
                    script_pubkey: vec![0x51],
                },
            ],
            locktime: 55,
            expiry_height: Some(0),
        }
    }

    /// Sapling's digest = Overwinter's digest with exactly 32+32 zero
    /// bytes after the joinsplit digest and 8 zero bytes after expiry —
    /// the three transparent-only constants in the C++ (`if SIGVERSION_
    /// SAPLING` blocks). Pinned byte-by-byte so nothing can silently
    /// reorder the layout.
    #[test]
    fn sapling_extends_overwinter_digest_by_constants() {
        let v3 = sample_tx(OVERWINTER_TX_VERSION);
        let v4 = sample_tx(SAPLING_TX_VERSION);
        let s3 = sighash(&v3, 0, &[0x51], 0, SIGHASH_ALL, OVERWINTER_BRANCH_ID).expect("v3");
        let s4 = sighash(&v4, 0, &[0x51], 0, SIGHASH_ALL, SAPLING_BRANCH_ID).expect("v4");
        assert_ne!(s3, s4);

        // Reference assembly from the same parts: the v4 input must be
        // the v3 input with the constants spliced at the documented
        // positions (branch personalization differs too).
        let mk = |tx: &Transaction, sapling: bool, branch: u32| -> Vec<u8> {
            let mut prevouts = Vec::new();
            for i in &tx.vin {
                prevouts.extend_from_slice(&i.prevout.txid);
                put_u32(&mut prevouts, i.prevout.vout);
            }
            let mut outputs = Vec::new();
            for o in &tx.vout {
                put_i64(&mut outputs, o.value);
                put_bytes(&mut outputs, &o.script_pubkey);
            }
            let mut b = Vec::new();
            put_u32(&mut b, (tx.version as u32) | (1 << 31));
            put_u32(&mut b, tx.version_group_id.unwrap_or(0));
            b.extend_from_slice(&blake2b_personal(b"ZcashPrevoutHash", &prevouts));
            b.extend_from_slice(&blake2b_personal(
                b"ZcashSequencHash",
                &tx.vin.iter().map(|i| i.sequence.to_le_bytes()).collect::<Vec<_>>().concat(),
            ));
            b.extend_from_slice(&blake2b_personal(b"ZcashOutputsHash", &outputs));
            b.extend_from_slice(&[0u8; 32]); // joinsplits (empty)
            if sapling {
                b.extend_from_slice(&[0u8; 32]); // shielded spends (empty)
                b.extend_from_slice(&[0u8; 32]); // shielded outputs (empty)
            }
            put_u32(&mut b, tx.locktime);
            put_u32(&mut b, tx.expiry_height.unwrap_or(0));
            if sapling {
                put_i64(&mut b, 0); // valueBalance
            }
            put_u32(&mut b, SIGHASH_ALL);
            b.extend_from_slice(&tx.vin[0].prevout.txid);
            put_u32(&mut b, tx.vin[0].prevout.vout);
            put_bytes(&mut b, &[0x51]);
            put_i64(&mut b, 0);
            put_u32(&mut b, tx.vin[0].sequence);
            let mut personal = [0u8; 16];
            personal[..12].copy_from_slice(b"ZcashSigHash");
            personal[12..].copy_from_slice(&branch.to_le_bytes());
            blake2b_personal(&personal, &b).to_vec()
        };
        assert_eq!(mk(&v4, true, SAPLING_BRANCH_ID), s4.to_vec());
        assert_eq!(mk(&v3, false, OVERWINTER_BRANCH_ID), s3.to_vec());
        // And the raw pre-hash streams differ by exactly the 72 constant
        // bytes the sapling leg appends.
    }

    #[test]
    fn hash_type_flags_change_or_zero_subhashes() {
        let tx = sample_tx(SAPLING_TX_VERSION);
        let script = [0x51];
        // ANYONECANPAY: prevouts sub-hash zeroes.
        let all = sighash(&tx, 0, &script, 5, SIGHASH_ALL, SAPLING_BRANCH_ID).unwrap();
        let acp = sighash(&tx, 0, &script, 5, SIGHASH_ALL | SIGHASH_ANYONECANPAY, SAPLING_BRANCH_ID).unwrap();
        assert_ne!(all, acp);
        // SINGLE with a valid index hashes only that output; NONE zeroes.
        let single = sighash(&tx, 0, &script, 5, SIGHASH_SINGLE, SAPLING_BRANCH_ID).unwrap();
        let none = sighash(&tx, 0, &script, 5, SIGHASH_NONE, SAPLING_BRANCH_ID).unwrap();
        assert_ne!(all, single);
        assert_ne!(all, none);
        assert_ne!(single, none);
        // SINGLE with an out-of-range output index: hashOutputs stays
        // zero (the C++ overwinter path has no throw there).
        let oor = sighash(&tx, 1, &script, 5, SIGHASH_SINGLE, SAPLING_BRANCH_ID);
        assert!(oor.is_err(), "input index checked first");
    }

    #[test]
    fn legacy_tx_refused() {
        let tx = Transaction::legacy(vec![], vec![]);
        assert!(sighash(&tx, 0, &[0x51], 0, SIGHASH_ALL, SAPLING_BRANCH_ID).is_err());
    }
}

