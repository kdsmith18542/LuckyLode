//! Lucky Lode first-party casino (doc 14) — executable spec.
//!
//! This crate is the deterministic, testable core of `luckylode.cc/casino`:
//!
//! - [`fair`] — provably-fair commit-reveal RNG (HMAC-SHA256 keystream, reel
//!   stop mapping, and a player-side verifier). No house trust required.
//! - [`line`] — **Lucky Lode** (*Book*-style, 5×3, 10 lines) exact-RTP math
//!   engine, tunable to the owner-set 91% RTP.
//! - [`ways`] — **Lucky Lode Ways** (ways-style, 6 reels × 2–7 rows, up to
//!   117,649 ways) exact-RTP math engine, also tuned to 91%.
//! - [`ledger`] — house ledger: deposits/withdrawals, bets/wins, bankroll
//!   limits and automatic throttling, with exact money conservation.
//! - **FUN** (doc 14 §14.9) — free play money: claimable on a cooldown,
//!   played with the same provably-fair RNG and math, and completely
//!   isolated from the house ledger (never on-chain, never convertible).
//! - [`edge`] — the ~9% house edge split (dev fund / Lucky Pot / miner
//!   airdrop / game services), largest-remainder exact.
//! - [`tier`] — mine-to-play / stake-to-play / serve-to-play participation
//!   tiers (PoW shares, PoS stake, Luckynode service).
//! - [`bonus`] — owner/manager bonus engine: rule-based and manual exclusive
//!   bonuses for top miners / top spenders, with an audit log.
//!
//! Money is integer micro-LKYL (`MICROS_PER_LKYL` = 1_000_000). Nothing here
//! touches consensus. Real-value wagering is gated on licensing (doc 14 §14.5).

pub mod bonus;
pub mod custody;
pub mod edge;
pub mod engine;
pub mod fair;
pub mod http;
pub mod ledger;
pub mod line;
pub mod oracle;
pub mod rpc;
pub mod tier;
pub mod ways;

/// Micro-units per LKYL (same fixed-point discipline as the rest of the
/// money paths in this repo).
pub const MICROS_PER_LKYL: u64 = 1_000_000;

/// Owner-set return to player for both launch titles (91% → ~9% edge).
pub const TARGET_RTP: f64 = 0.91;

/// The ten luck classes (`lm-rewards::luck`) plus the two special symbols,
/// used as the shared symbol ladder across both games — the casino and the
/// chain speak one visual language.
pub const SYMBOL_NAMES: [&str; 12] = [
    "spark",
    "glint",
    "flare",
    "ember",
    "cinder",
    "bonfire",
    "beacon",
    "comet",
    "aurora",
    "mythic",
    "lodestone",
    "almanac",
];

/// Index of the wild symbol ("Lodestone").
pub const WILD: usize = 10;
/// Index of the scatter symbol ("Prospector's Almanac").
pub const SCATTER: usize = 11;
