//! Reference pool wire layer (Lucky Lode): Stratum V1 framing with the
//! Lucky Lode job profile, a mock-chain template provider, a threaded TCP pool
//! server, and a blocking client.
//!
//! - [`v1`]: protocol types, job registry, share evaluation, JSON-RPC shapes.
//! - [`v2`]: Stratum V2 framing + core message codec (the framing
//!   milestone; V1 remains the live wire until Noise/V2 negotiation lands).
//! - [`provider`]: [`provider::TemplateProvider`] seam + [`provider::MockProvider`].
//! - [`server`]: [`server::spawn_server`] — testnet-in-a-box reference pool.
//! - [`client`]: blocking client for tooling and the e2e suite.
//!
//! Production path (doc 02 §2.3): Stratum V2 replaces `v1` framing; the
//! provider seam, ledger wiring, and money rules carry over unchanged.

pub mod client;
pub mod monitor;
pub mod node_provider;
pub mod provider;
pub mod rpc;
pub mod server;
pub mod tls;
pub mod tx;
pub mod v1;
pub mod v2;
pub mod vardiff;
pub mod workerstats;
