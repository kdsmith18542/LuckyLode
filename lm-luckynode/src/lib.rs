//! Luckynodes: consensus-draw participation over node RPC (doc 06 §6.7).
//!
//! Registration is a LuckyID `ln-` name (payout = the primary address),
//! liveness is an identity update transaction landing inside the epoch
//! window, tenure is first registration, and the sybil cost is the
//! identity fee schedule. No pool, no ed25519 side-identities, no
//! collateral bonds: the chain is the registry, and winners are paid by
//! directed coinbase claims that any block builder attaches.
//!
//! [`identity`] holds the shared node-RPC orchestration used by the
//! `luckynode` daemon and the wallet's embedded service so the
//! sequences never drift.

pub mod identity;

pub use identity::{
    eligible_at, ensure_identity, heartbeat_if_due, identity_status, is_ln_name,
    resolve_epoch_len, EnsureOutcome, IdentityState, NodeRpc,
};
