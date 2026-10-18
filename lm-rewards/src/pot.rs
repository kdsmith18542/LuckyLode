//! Lucky Pot accounting, promotes to doc 06 on sign-off).
//!
//! Second stochastic sink beside the jackpot reserve: rare + full-drain
//! (jackpot) vs frequent + small (pot) — two dopamine loops, one levy.
//! Pre-seeded at genesis with a game-locked fund;
//! thereafter purely organic.
//!
//! Rules (proposed, frozen on sign-off):
//!
//! 1. The 5% levy ([`crate::reserve::levy_amount`], unchanged total) is
//!    split 4/5 jackpot reserve, 1/5 pot — `pot_levy = floor(levy / 5)`,
//!    `jackpot_levy = levy − pot_levy`, so the split is exact.
//! 2. The pot slice funds the pot up to its cap (`200 * R`); the excess
//!    **burns** (unspendable coinbase output) — the automatic deflation
//!    valve, no votes, no standing burn.
//! 3. Pot draw, every PoW block: event iff `digest < T/1000` (~0.1% of
//!    blocks, ~1–2/day, auto-calibrates with difficulty). Tier from
//!    `digest mod 100`: 0–69 → 10% of pot; 70–94 → 25%; 95–99 → 50%.
//!    Floor 10%, never the whole fund. (Digest math lives in `lm-pow`;
//!    this module takes the decided draw like
//!    [`crate::reserve::apply_block`] takes `is_jackpot`.)
//! 4. **Pity timer (drought cap):** after [`POT_DROUGHT_LIMIT`]
//!    consecutive blocks with no pot event, the next block pays a forced
//!    tier-A (10%) splash. At 0.1% draws a 3-day dry spell is ~6%
//!    likely; the cap bounds the worst case while leaving 99%+ of events
//!    genuinely random. Ritual/scheduled events live in the fun layer
//!    (league pool), not consensus — scheduled consensus payouts are
//!    ambushable by rented hashrate pointed at a known height.
//! 5. Jackpot leg is untouched ([`crate::reserve`] formulas, fed with the
//!    jackpot slice of the levy).
//!
//! Conservation (single rule, asserted in tests):
//! `miner + (reserve − reserve_before) + (pot − pot_before) + to_league
//! + to_burn == R + F`.
//!
//! All math is `u64` integer with saturating cap/bonus intermediates —
//! byte-identical on every architecture. Reorgs drop trailing states, no
//! special casing (same posture as [`crate::reserve`]).

use crate::reserve::{jackpot_bonus, levy_amount, reserve_cap};

/// Pot share of the levy: 1/5 (the other 4/5 stays jackpot reserve).
pub const POT_SPLIT_DEN: u64 = 5;
/// Pot cap multiple: cap = `200 * R` at the block's current `R`.
pub const POT_CAP_MULT: u64 = 200;
/// Tier roll boundary: rolls `< 70` pay a tenth of the pot.
pub const TIER_A_MAX: u64 = 70;
/// Tier roll boundary: rolls `< 95` pay a quarter; the rest pay a half.
pub const TIER_B_MAX: u64 = 95;
/// Drought cap: this many event-less blocks force a tier-A splash
/// (4,320 blocks ≈ 3 days at 60 s blocks).
pub const POT_DROUGHT_LIMIT: u64 = 4_320;

/// Pot state carried in the block index (reorg-friendly: drop trailing
/// states, no special casing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PotState {
    /// Pot balance before the block.
    pub pot: u64,
    /// Consecutive blocks before this one with no pot event.
    pub drought: u64,
}

/// The block's pre-decided draws from the digest (`lm-pow` decides).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockDraws {
    /// `digest < T/100` — jackpot.
    pub jackpot: bool,
    /// `digest < T/1000` — pot event.
    pub pot_event: bool,
    /// `digest mod 100` — pot tier roll.
    pub tier: u64,
}

/// Pot cap at the block's current base reward: `200 * R` (saturating).
#[must_use]
pub fn pot_cap(base: u64) -> u64 {
    base.saturating_mul(POT_CAP_MULT)
}

/// Pot payout fraction for a tier roll in `0..100`: `(numerator,
/// denominator)` — (1,10), (1,4), or (1,2). Rolls `>= 100` are caller
/// error territory; they pay the top tier (never panic in consensus).
#[must_use]
pub fn pot_fraction(tier: u64) -> (u64, u64) {
    if tier < TIER_A_MAX {
        (1, 10)
    } else if tier < TIER_B_MAX {
        (1, 4)
    } else {
        (1, 2)
    }
}

/// Result of applying one block's full money rules (jackpot leg + pot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PotPayout {
    /// Total paid to the block finder: normal + jackpot bonus + pot splash.
    pub miner: u64,
    /// Jackpot bonus included in `miner` (0 on normal blocks).
    pub jackpot_bonus: u64,
    /// Pot splash included in `miner` (0 when no pot event).
    pub pot_splash: u64,
    /// Jackpot-levy slice credited to the jackpot reserve this block.
    pub to_reserve: u64,
    /// Pot-levy slice credited to the pot this block.
    pub to_pot: u64,
    /// Pot-levy slice burned (pot cap overflow).
    pub to_burn: u64,
    /// Jackpot-levy slice diverted to the league prize pool (cap overflow).
    pub to_league: u64,
    /// Jackpot reserve balance after this block.
    pub reserve: u64,
    /// Pot balance after this block.
    pub pot: u64,
    /// Consecutive event-less blocks after this block (0 after an event).
    pub drought: u64,
    /// A pot event happened (digest or pity).
    pub pot_event: bool,
    /// The event was forced by the pity timer (not the digest).
    pub pity: bool,
}

/// Apply one block's money rules with the Lucky Pot.
///
/// The digest decides `draws` (`lm-pow`); this function applies the pity
/// timer, the 4/1 levy split, both caps, the tiers, and exact
/// conservation.
#[must_use]
pub fn apply_pot_block(
    reserve: u64,
    state: PotState,
    base: u64,
    fees: u64,
    draws: BlockDraws,
) -> PotPayout {
    let levy = levy_amount(base, fees);
    let normal = base.saturating_add(fees).saturating_sub(levy);
    // Exact split: pot takes the floor fifth, jackpot keeps the rest.
    let pot_levy = levy / POT_SPLIT_DEN;
    let jackpot_levy = levy - pot_levy;

    // Jackpot leg (reserve formulas, jackpot slice of the levy).
    let cap = reserve_cap(base);
    let to_reserve = jackpot_levy.min(cap.saturating_sub(reserve));
    let to_league = jackpot_levy - to_reserve;
    let credited_reserve = reserve + to_reserve;
    let jackpot = if draws.jackpot { jackpot_bonus(base, credited_reserve) } else { 0 };

    // Pot leg (cap-overflow burns instead of feeding the league pool).
    let pcap = pot_cap(base);
    let to_pot = pot_levy.min(pcap.saturating_sub(state.pot));
    let to_burn = pot_levy - to_pot;
    let credited_pot = state.pot + to_pot;
    // Pity: force a tier-A splash after a full drought; the digest event
    // itself never forces (and a digest event at any tier resets the run).
    let pity = !draws.pot_event && state.drought >= POT_DROUGHT_LIMIT;
    let pot_event = draws.pot_event || pity;
    let splash = if pot_event {
        let tier = if pity { 0 } else { draws.tier };
        let (num, den) = pot_fraction(tier);
        (credited_pot as u128 * num as u128 / den as u128) as u64
    } else {
        0
    };

    PotPayout {
        miner: normal.saturating_add(jackpot).saturating_add(splash),
        jackpot_bonus: jackpot,
        pot_splash: splash,
        to_reserve,
        to_pot,
        to_burn,
        to_league,
        reserve: credited_reserve - jackpot,
        pot: credited_pot - splash,
        drought: if pot_event { 0 } else { state.drought.saturating_add(1) },
        pot_event,
        pity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draws(pot_event: bool, tier: u64) -> BlockDraws {
        BlockDraws { jackpot: false, pot_event, tier }
    }

    #[test]
    fn levy_split_is_exact_fifths() {
        // Total levy unchanged; pot takes floor(levy/5), jackpot the rest.
        for levy in [0u64, 1, 4, 5, 7, 100, 4_000_000] {
            let p = apply_pot_block(0, PotState::default(), levy * 20, 0, draws(false, 0));
            assert_eq!(p.to_pot + p.to_burn, levy / 5, "levy={levy}");
            assert_eq!(p.to_reserve + p.to_league, levy - levy / 5, "levy={levy}");
        }
    }

    #[test]
    fn tiers_pay_ten_quarter_half() {
        let pot = 1_000_000u64;
        let state = PotState { pot, drought: 0 };
        // No levy noise: base 0 with fees 0 — tiers apply to the carried pot.
        for tier in [0u64, 1, 69] {
            let p = apply_pot_block(0, state, 0, 0, draws(true, tier));
            assert_eq!(p.pot_splash, 100_000, "tier={tier}");
            assert_eq!(p.pot, 900_000);
        }
        for tier in [70u64, 94] {
            let p = apply_pot_block(0, state, 0, 0, draws(true, tier));
            assert_eq!(p.pot_splash, 250_000, "tier={tier}");
        }
        for tier in [95u64, 99, 100, 999] {
            let p = apply_pot_block(0, state, 0, 0, draws(true, tier));
            assert_eq!(p.pot_splash, 500_000, "tier={tier}");
        }
        // No event, any tier: nothing moves.
        let p = apply_pot_block(0, state, 0, 0, draws(false, 99));
        assert_eq!((p.pot_splash, p.pot), (0, pot));
    }

    #[test]
    fn cap_overflow_burns_instead_of_hoarding() {
        let cap = pot_cap(100); // 20_000
        // Slice that fits: all credited, nothing burned.
        let state = PotState { pot: cap - 3, drought: 0 };
        let p = apply_pot_block(0, state, 100, 0, draws(false, 0));
        assert_eq!(p.to_pot, 1); // levy 5 → pot slice 1
        assert_eq!(p.to_burn, 0);
        assert_eq!(p.pot, cap - 3 + 1);
        // Slice that breaches: pot fills, rest burns.
        let state = PotState { pot: cap, drought: 0 };
        let p = apply_pot_block(0, state, 100, 20, draws(false, 0));
        assert_eq!(p.to_pot, 0);
        assert_eq!(p.to_burn, 1); // levy 6 → pot slice 1, no room
        assert_eq!(p.pot, cap);
    }

    #[test]
    fn pity_forces_tier_a_after_the_drought_limit() {
        let pot = 1_000_000u64;
        let cap = pot_cap(80_000_000); // far above the pot: no burn
        // One under the limit: nothing forced, drought advances.
        let state = PotState { pot, drought: POT_DROUGHT_LIMIT - 1 };
        let p = apply_pot_block(0, state, 80_000_000, 0, draws(false, 99));
        assert!(!p.pot_event && !p.pity);
        assert_eq!(p.drought, POT_DROUGHT_LIMIT);
        assert_eq!(p.pot_splash, 0);
        // At the limit: tier-A splash, drought resets.
        let state = PotState { pot: p.pot, drought: p.drought };
        let p2 = apply_pot_block(0, state, 80_000_000, 0, draws(false, 99));
        assert!(p2.pot_event && p2.pity);
        assert_eq!(p2.drought, 0);
        // 10% of (pot + this block's slice) — slice is 800k at R+F=80M.
        assert_eq!(p2.pot_splash, (p.pot + 800_000) / 10);
        // A digest event resets the run without pity.
        let state = PotState { pot, drought: POT_DROUGHT_LIMIT };
        let p3 = apply_pot_block(0, state, 80_000_000, 0, draws(true, 95));
        assert!(p3.pot_event && !p3.pity);
        assert_eq!(p3.drought, 0);
        assert_eq!(p3.pot_splash, (pot + 800_000) / 2);
        let _ = cap;
    }

    #[test]
    fn block_21_numbers_under_split_rules() {
        // The live J1 jackpot (reserve 39,999,990, R+F = 80M) replayed
        // under the 4/1 split: same block pays a smaller bonus because
        // 800k of the levy now seeds the pot instead of the reserve.
        let p = apply_pot_block(
            39_999_990,
            PotState::default(),
            80_000_000,
            0,
            BlockDraws { jackpot: true, pot_event: false, tier: 0 },
        );
        assert_eq!(p.to_reserve, 3_200_000);
        assert_eq!(p.to_pot, 800_000);
        assert_eq!(p.jackpot_bonus, 43_199_990);
        assert_eq!(p.reserve, 0);
        assert_eq!(p.pot, 800_000);
        assert_eq!(p.miner, 76_000_000 + 43_199_990);
    }

    #[test]
    fn seeded_start_pays_from_day_one() {
        // Genesis game-fund seed: a seeded pot pays a
        // splash on the first event — no warm-up, unlike the reserve.
        // The block's own levy slice (800k at R+F=80M) credits first, then
        // the tier fraction applies to the credited pot.
        let seed = 4_200_000_000u64; // 1%-of-year-one reference size
        let state = PotState { pot: seed, drought: 0 };
        let credited = seed + 800_000;
        let p = apply_pot_block(0, state, 80_000_000, 0, draws(true, 99));
        assert_eq!(p.to_pot, 800_000);
        assert_eq!(p.pot_splash, credited / 2);
        assert_eq!(p.pot, credited - credited / 2);
    }

    #[test]
    fn every_block_conserves_money() {
        // miner + Δreserve + Δpot + league + burn == R + F, across states,
        // tiers, jackpots and pot events.
        let states = [
            (0u64, 0u64, 0u64, 100u64, 2u64),
            (7, 13, 0, 100, 2),
            (5_000, 20_000, POT_DROUGHT_LIMIT, 100, 2),
            (0, 0, 0, 0, 1000),
            (39_999_990, 0, 0, 80_000_000, 0),
            (1_000_000, 4_200_000_000, 4_000, 12_500_000_000, 500_000),
        ];
        for &(reserve, pot, drought, base, fees) in &states {
            for jackpot in [false, true] {
                for (event, tier) in [(false, 0u64), (true, 0), (true, 69), (true, 70), (true, 95)] {
                    let state = PotState { pot, drought };
                    let d = BlockDraws { jackpot, pot_event: event, tier };
                    let p = apply_pot_block(reserve, state, base, fees, d);
                    let inflow = base as u128 + fees as u128;
                    let outflow = p.miner as u128
                        + p.reserve as u128
                        + p.pot as u128
                        + p.to_league as u128
                        + p.to_burn as u128
                        - reserve as u128
                        - pot as u128;
                    assert_eq!(
                        outflow, inflow,
                        "r={reserve} pot={pot} b={base} j={jackpot} e={event} t={tier}"
                    );
                }
            }
        }
    }

    #[test]
    fn ten_thousand_blocks_stay_bounded_and_exact() {
        // Deterministic season: pot event every 1000th block, tiers cycle
        // 0/70/95, jackpot every 500th. Conservation every step, pot and
        // reserve bounded by their caps.
        let mut reserve = 0u64;
        let mut state = PotState { pot: 4_200_000_000, drought: 0 };
        let mut splashes = 0u64;
        let mut jackpots = 0u64;
        for h in 1..=10_000u64 {
            let d = BlockDraws {
                jackpot: h % 500 == 0,
                pot_event: h % 1000 == 0,
                tier: [0u64, 70, 95][(h / 1000 % 3) as usize],
            };
            let p = apply_pot_block(reserve, state, 80_000_000, 0, d);
            let inflow = 80_000_000u128;
            let outflow = p.miner as u128 + p.reserve as u128 + p.pot as u128
                + p.to_league as u128
                + p.to_burn as u128
                - reserve as u128
                - state.pot as u128;
            assert_eq!(outflow, inflow, "height={h}");
            assert!(p.reserve <= reserve_cap(80_000_000), "height={h}");
            assert!(p.pot <= pot_cap(80_000_000), "height={h}");
            assert!(p.drought < POT_DROUGHT_LIMIT, "no pity in this pattern");
            if d.pot_event {
                assert_eq!(p.drought, 0, "an event resets the run");
                assert!(p.pot_splash > 0, "seeded pot pays from day one");
                splashes += 1;
            }
            if d.jackpot {
                jackpots += 1;
            }
            reserve = p.reserve;
            state = PotState { pot: p.pot, drought: p.drought };
        }
        assert_eq!((splashes, jackpots), (10, 20));
    }

    #[test]
    fn drought_run_never_exceeds_limit() {
        // No digest events at all: the pity timer must fire on exactly the
        // limit-th block, every time.
        let mut state = PotState { pot: 1_000_000, drought: 0 };
        let mut forced_at = Vec::new();
        for h in 1..=40_000u64 {
            let p = apply_pot_block(0, state, 0, 0, draws(false, 0));
            assert!(p.drought <= POT_DROUGHT_LIMIT, "height={h}");
            if p.pity {
                forced_at.push(h);
                assert_eq!(p.pot_splash, state.pot / 10, "height={h}");
                assert_eq!(p.drought, 0);
            }
            state = PotState { pot: p.pot, drought: p.drought };
        }
        let expected: Vec<u64> = (1..).map(|k| k * (POT_DROUGHT_LIMIT + 1)).take_while(|&x| x <= 40_000).collect();
        assert_eq!(forced_at, expected);
    }
}
