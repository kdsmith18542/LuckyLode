//! Self-custody core for the Lucky Lode wallet: keys, addresses, and
//! (next) transparent transaction building + signing.
//!
//! Formats match the fork's C++ exactly (see module docs for the pinned
//! source lines); BIP39/BIP32 behavior is pinned to the official vectors.

pub mod address;
pub mod chain;
pub mod hd;
pub mod mnemonic;
pub mod sign;
pub mod tx;
pub mod zip243;

pub use sign::{sign_message, verify_message};
