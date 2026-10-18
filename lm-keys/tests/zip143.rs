//! ZIP-143 KAT: replay every transparent-only overwinter v3 vector from
//! the fork's own regenerated test set
//! (the node repository's src/test/data/sighash.json, run by sighash_tests.cpp via
//! `SignatureHash(scriptCode, tx, nIn, nHashType, /*amount=*/0,
//! consensusBranchId)`). These 45 vectors cover every hash-type shape
//! (ALL / SINGLE / NONE / ANYONECANPAY and the odd bits the fuzz
//! generator produced). If the Rust sighash and the node's C++ ever
//! disagree, this test fails before anything signs real money.
//!
//! Sapling v4 has no transparent-only vector in either this fork's or
//! upstream zcashd's set (every v4 vector carries joinsplits); its
//! delta from v3 is the three transparent-only constants pinned in
//! `zip243.rs::tests::sapling_extends_overwinter_digest_by_constants`.
//! Live end-to-end proof lands when sapling activates on testnet
//! (height 207500 as of 2026-10-17).

use lm_keys::tx::Transaction;
use lm_keys::zip243::sighash;

#[derive(serde::Deserialize)]
struct Vector {
    raw: String,
    script: String,
    n_in: usize,
    hash_type: u32,
    branch_id: u32,
    sig_hash: String,
}

#[test]
fn fork_zip143_vectors_replay_exactly() {
    let data = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/zip143_vectors.json"))
        .expect("fixture");
    let vectors: Vec<Vector> = serde_json::from_str(&data).expect("fixture json");
    assert!(vectors.len() >= 40, "expected the full curated set, got {}", vectors.len());

    let mut checked = 0usize;
    for (idx, v) in vectors.iter().enumerate() {
        let raw = hex::decode(&v.raw).expect("raw hex");
        let (tx, used) = Transaction::deserialize(&raw).expect("vector tx parses");
        assert_eq!(used, raw.len(), "vector {idx}: whole tx consumed");
        assert!(tx.overwintered, "vector {idx}: overwintered");
        let script = hex::decode(&v.script).expect("script hex");
        let sh = sighash(
            &tx,
            v.n_in,
            &script,
            0, // the C++ vector runner passes amount = 0
            v.hash_type,
            v.branch_id,
        )
        .unwrap_or_else(|e| panic!("vector {idx}: sighash error {e}"));
        // The fixture hashes are display order (explorer-style hex).
        assert_eq!(
            lm_keys::tx::txid_display(&sh),
            v.sig_hash,
            "vector {idx}: sighash mismatch"
        );
        checked += 1;
    }
    assert_eq!(checked, vectors.len());
}
