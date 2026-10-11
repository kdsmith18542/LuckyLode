//! Pool-side fun-layer accounting (Lucky Lode docs 03, 06).
//!
//! - [`streak`]: uptime-streak (+1%/day to +30%), comeback (+10% × 7 days),
//!   overnight (+5%) share weights — doc 03 §3.4.
//! - [`league`]: device-class leagues from observed rate — doc 03 §3.3.
//! - [`ledger`]: PPLNS share log, effective-share aggregation, jackpot
//!   payouts (via `lm-rewards` sqrt/cap/dust), league-pool splits,
//!   seasonal leaderboard.
//!
//! Nothing here is consensus: all of it runs pool-side and replays
//! deterministically from the share log plus an explicit timestamp.

pub mod league;
pub mod ledger;
pub mod streak;
