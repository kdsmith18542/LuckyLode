//! Jackpot reserve accounting (Lucky Lode doc 06 §6.2, frozen rev 1.0).
//!
//! Pure, reorg-friendly state transitions: [`apply_block`] takes the reserve
//! balance before the block and returns the balance after, so a reorg simply
//! drops trailing states — no special casing (doc 06 §6.2 "Reorgs").
//!
//! Order of operations (deterministic, documented here because the spec
//! states the two legs separately):
//!
//! 1. Compute `levy = floor((R + F) * 500 / 10000)`.
//! 2. Credit the levy to the reserve up to the cap (`50 * R` at the block's
//!    own `R`); the excess flows to the **league prize pool** instead of
//!    burning (doc 06 §6.2 "Overflow").
//! 3. On a jackpot block, pay `min(5 * R, reserve_after_levy)` from the
//!    reserve on top of the normal payout.
//!
//! All math is `u64` integer with saturating cap/bonus intermediates —
//! byte-identical on every architecture. Monetary units are opaque base
//! units (satoshis/micros); callers keep the denomination consistent.
//!
//! Expectation-neutrality: per block, expected levy inflow `0.05 * (R + F)`
//! balances expected bonus outflow `0.01 * 5R = 0.05R` at steady state
//! (fees make it slightly positive-sum — by design, fees feed the spectacle).

/// Per-myriad levy: 500 = 5% of `R + F` into the jackpot reserve.
pub const LEVY_PERMYRIAD: u64 = 500;
/// Levy denominator (per-myriad).
pub const LEVY_DENOM: u64 = 10_000;
/// Jackpot bonus multiple: bonus = `5 * R`, paid from the reserve.
pub const JACKPOT_BONUS_MULT: u64 = 5;
/// Reserve cap multiple: cap = `50 * R` at the block's current `R`.
pub const RESERVE_CAP_MULT: u64 = 50;

/// Per-block levy: `floor((base + fees) * 500 / 10000)` (saturating sum).
#[must_use]
pub fn levy_amount(base: u64, fees: u64) -> u64 {
    (base.saturating_add(fees) as u128 * LEVY_PERMYRIAD as u128 / LEVY_DENOM as u128) as u64
}

/// Reserve cap at the block's current base reward: `50 * R` (saturating).
#[must_use]
pub fn reserve_cap(base: u64) -> u64 {
    base.saturating_mul(RESERVE_CAP_MULT)
}

/// Jackpot bonus available from a reserve balance: `min(5 * R, reserve)`.
/// Reserve starts at 0, so early jackpots pay thin by design (warm-up).
#[must_use]
pub fn jackpot_bonus(base: u64, reserve: u64) -> u64 {
    base.saturating_mul(JACKPOT_BONUS_MULT).min(reserve)
}

/// Result of applying one block to the reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPayout {
    /// Total paid to the block finder (coinbase): normal payout plus any bonus.
    pub miner: u64,
    /// Jackpot bonus included in `miner` (0 on normal blocks).
    pub bonus: u64,
    /// Levy slice credited to the reserve this block.
    pub to_reserve: u64,
    /// Levy slice diverted to the league prize pool (cap overflow).
    pub to_league: u64,
    /// Reserve balance after this block.
    pub reserve: u64,
}

/// Apply one block's money rules — the PRE-SPLIT rule (doc 06 §6.2 as of
/// rev 1.1: the whole 5% levy credited the jackpot reserve).
///
/// **Deprecated:** the consensus rule since doc 06 rev 1.2 is the 4/1
/// levy split, whose exact executable mirror is
/// [`crate::pot::apply_pot_block`] (gated by the pot test suite).
/// Kept because the helpers it shares with the pot path
/// ([`levy_amount`], [`reserve_cap`], [`jackpot_bonus`]) are the live
/// jackpot leg, and the tests below pin the legacy contract.
///
/// - Normal block: `miner = R + F − levy`; `reserve += levy` (capped).
/// - Jackpot block: `miner = (R + F − levy) + min(5R, reserve_after_levy)`.
/// - Conservation: `miner + (reserve − reserve_before) + to_league == R + F`.
#[must_use]
#[deprecated(since = "0.2.0", note = "pre-split rule; use pot::apply_pot_block")]
pub fn apply_block(reserve: u64, base: u64, fees: u64, is_jackpot: bool) -> BlockPayout {
    let levy = levy_amount(base, fees);
    let normal = base.saturating_add(fees).saturating_sub(levy);
    let cap = reserve_cap(base);
    let to_reserve = levy.min(cap.saturating_sub(reserve));
    let to_league = levy - to_reserve;
    // Invariant: to_reserve <= cap - reserve, so the credit cannot
    // overflow (cap is a saturating multiple of base).
    let credited = reserve
        .checked_add(to_reserve)
        .expect("invariant: reserve credit stays under the cap");
    if !is_jackpot {
        return BlockPayout {
            miner: normal,
            bonus: 0,
            to_reserve,
            to_league,
            reserve: credited,
        };
    }
    let bonus = jackpot_bonus(base, credited);
    BlockPayout {
        miner: normal.saturating_add(bonus),
        bonus,
        to_reserve,
        to_league,
        // Invariant: bonus <= min(5R, credited) — the debit cannot
        // underflow.
        reserve: credited
            .checked_sub(bonus)
            .expect("invariant: bonus debit covered by the credit"),
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    #[test]
    fn levy_is_five_percent_floor() {
        // Doc §6.2 worked example (R=100, F=2): levy 5.1 in decimals, 5 floored.
        assert_eq!(levy_amount(100, 2), 5);
        assert_eq!(levy_amount(0, 0), 0);
        assert_eq!(levy_amount(200, 0), 10);
        // Scale where the doc's decimals are exact: R=100_000_000 micros.
        assert_eq!(levy_amount(100_000_000, 2_000_000), 5_100_000);
    }

    #[test]
    fn normal_block_grows_reserve_and_conserves() {
        let p = apply_block(0, 100_000_000, 2_000_000, false);
        assert_eq!(p.miner, 102_000_000 - 5_100_000);
        assert_eq!(p.bonus, 0);
        assert_eq!(p.to_reserve, 5_100_000);
        assert_eq!(p.to_league, 0);
        assert_eq!(p.reserve, 5_100_000);
        // Conservation: miner + Δreserve + league == R + F.
        assert_eq!(p.miner + p.reserve + p.to_league, 102_000_000);
    }

    #[test]
    fn warmup_jackpot_pays_thin_then_recovers() {
        // Reserve starts at 0: the first jackpot can only pay the levy
        // credited by its own block (documented warm-up, not a bug).
        let p = apply_block(0, 100, 2, true);
        assert_eq!(p.to_reserve, 5);
        assert_eq!(p.bonus, 5); // min(500, 5)
        assert_eq!(p.miner, (102 - 5) + 5);
        assert_eq!(p.reserve, 0);
        // After 200 normal blocks the reserve holds 1000; a jackpot pays it.
        let mut reserve = 0u64;
        for _ in 0..200 {
            reserve = apply_block(reserve, 100, 2, false).reserve;
        }
        assert_eq!(reserve, 1000);
        let j = apply_block(reserve, 100, 2, true);
        assert_eq!(j.bonus, 500); // min(500, 1005) — full bonus restored
        assert_eq!(j.reserve, 1005 - 500);
    }

    #[test]
    fn steady_state_jackpot_pays_full_bonus() {
        // Reserve at the cap: jackpot pays exactly 5R.
        let p = apply_block(5_000, 100, 0, true);
        assert_eq!(p.bonus, 500);
        assert_eq!(p.to_reserve, 0); // reserve full: levy flows to league pool
        assert_eq!(p.to_league, 5);
        assert_eq!(p.miner, (100 - 5) + 500);
        assert_eq!(p.reserve, 5_000 - 500);
        // Conservation: miner + Δreserve + league == R + F.
        assert_eq!(p.miner as i128 + (p.reserve as i128 - 5_000) + p.to_league as i128, 100);
    }

    #[test]
    fn cap_overflow_feeds_league_pool() {
        let cap = reserve_cap(100); // 5000
        // Levy that fits entirely under the cap stays in the reserve.
        let p = apply_block(cap - 5, 100, 2, false);
        assert_eq!(p.to_reserve, 5);
        assert_eq!(p.to_league, 0);
        assert_eq!(p.reserve, cap);
        // Levy that would breach the cap splits: reserve fills, rest to league.
        let p = apply_block(cap - 2, 100, 2, false);
        assert_eq!(p.to_reserve, 2);
        assert_eq!(p.to_league, 3);
        assert_eq!(p.reserve, cap);
        // At a full reserve the whole levy flows to the league pool.
        let p = apply_block(cap, 100, 2, true);
        assert_eq!(p.to_reserve, 0);
        assert_eq!(p.to_league, 5);
        assert_eq!(p.bonus, 500);
        assert_eq!(p.reserve, cap - 500);
    }

    #[test]
    fn cap_tracks_current_base_reward() {
        // Halving-style drop: the cap follows the block's own R.
        let full_cap = reserve_cap(200); // 10_000
        let p = apply_block(full_cap, 100, 0, false);
        // Reserve 10_000 already exceeds the new cap 5000: no room for levy.
        assert_eq!(p.to_reserve, 0);
        assert_eq!(p.to_league, levy_amount(100, 0));
        assert_eq!(p.reserve, full_cap);
    }

    #[test]
    fn zero_and_saturating_edges() {
        // Zero base with fees: levy on fees alone, never a bonus.
        let p = apply_block(0, 0, 1000, true);
        assert_eq!(p.miner, 1000 - 50);
        assert_eq!(p.bonus, 0);
        // u64 extremes never wrap (saturating math). Conservation is only
        // defined below saturation: at u64::MAX the miner payout itself
        // saturates, destroying value instead of wrapping — acceptable
        // because real supplies never approach 2^64 base units. Here we
        // assert finiteness and structural sanity, not conservation.
        let p = apply_block(u64::MAX, u64::MAX, u64::MAX, true);
        assert_eq!(p.to_reserve, 0); // reserve already at/over cap
        assert_eq!(p.to_league, levy_amount(u64::MAX, u64::MAX));
        assert_eq!(p.bonus, u64::MAX);
        assert_eq!(p.reserve, 0);
        assert_eq!(p.miner, u64::MAX); // (MAX − levy) + MAX saturates
        // Large-but-sane values still conserve exactly.
        let p = apply_block(1_000_000, 12_500_000_000, 500_000, true);
        assert_eq!(
            p.miner as u128 + p.reserve as u128 + p.to_league as u128 - 1_000_000u128,
            12_500_000_000u128 + 500_000
        );
        let big = apply_block(0, u64::MAX / 2, 0, false);
        assert_eq!(big.reserve, levy_amount(u64::MAX / 2, 0));
        assert_eq!(reserve_cap(u64::MAX), u64::MAX);
        assert_eq!(jackpot_bonus(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn every_block_conserves_money() {
        // miner + Δreserve + league == R + F for jackpot and normal blocks,
        // across reserve states (empty, mid, capped) and reward levels.
        for &(reserve, base, fees) in &[
            (0u64, 100u64, 2u64),
            (7u64, 100, 2),
            (5000u64, 100, 2),
            (0u64, 0, 1000),
            (1_000_000u64, 12_500_000_000, 500_000),
        ] {
            for jackpot in [false, true] {
                let p = apply_block(reserve, base, fees, jackpot);
                let inflow = base as u128 + fees as u128;
                let outflow =
                    p.miner as u128 + p.reserve as u128 + p.to_league as u128 - reserve as u128;
                assert_eq!(outflow, inflow, "reserve={reserve} base={base} jackpot={jackpot}");
            }
        }
    }
}
