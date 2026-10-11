//! Emits the canonical fork-shaped header vector (doc 05 Addendum A).
//!
//! The C++ port must reproduce these values byte-for-byte. Regenerate with:
//!
//! ```sh
//! cargo run -p lm-pow --example fork_vector
//! ```
//!
//! Layout (109 bytes, empty solution vector):
//!   nVersion u32 LE = 0x00010004 (CPOSNonce::VERUS_V2)
//!   hashPrevBlock   = 0x11 * 32
//!   hashMerkleRoot  = 0x22 * 32
//!   hashFinalSaplingRoot = 0x33 * 32
//!   nTime u32 LE    = 42
//!   nBits u32 LE    = 0x1d00ffff
//!   solution CompactSize = 0x00 (empty)
//! Then `pow_hash(header, nonce=7, height=100)`.

use lm_pow::pow_full;

fn main() {
    let mut header = Vec::new();
    header.extend_from_slice(&0x0001_0004u32.to_le_bytes()); // nVersion (VERUS_V2)
    header.extend_from_slice(&[0x11u8; 32]); // hashPrevBlock
    header.extend_from_slice(&[0x22u8; 32]); // hashMerkleRoot
    header.extend_from_slice(&[0x33u8; 32]); // hashFinalSaplingRoot
    header.extend_from_slice(&42u32.to_le_bytes()); // nTime
    header.extend_from_slice(&0x1d00_ffffu32.to_le_bytes()); // nBits
    header.push(0x00); // CompactSize(len(nSolution)) = 0
    assert_eq!(header.len(), 109);

    let f = pow_full(&header, 7, 100);
    println!("header_len = {}", header.len());
    println!("header     = {}", hex::encode(&header));
    println!("nonce      = 7");
    println!("height     = 100");
    println!("seed       = {}", hex::encode(f.seed));
    println!("mem        = {}", hex::encode(f.mem));
    println!("out        = {}", hex::encode(f.out));
}
