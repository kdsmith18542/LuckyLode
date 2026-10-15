//! Emission schedule mirror (docs/04 §4.1, LOCKED 2026-10-13) — the wallet
//! shows the live phase; the node's `komodo_ac_block_subsidy` is the
//! authority. Values here are pinned against on-chain coinbases in tests.
//!
//! Schedule (60 s blocks, 1,440/day):
//! - **week 1** (blocks 1..=10,080): linear ramp 0 → 16 LKYL
//! - **years 1–8**: 16 LKYL halving every 1,051,920 blocks → 8 / 4 / 2
//! - **year 9+**: fixed 1 LKYL/block forever (anti-fade tail)
//!
//! A 5% per-block levy is withheld from the miner and split 4% jackpot
//! reserve / 1% Lucky Pot (doc 06 §6.6); `net` below is what the finder
//! keeps before fees.

pub const RAMP_END: u64 = 10_080; // week 1 at 60 s blocks
pub const ERA2_REWARD: u64 = 1_600_000_000; // 16 LKYL
pub const HALVING: u64 = 1_051_920; // 2 years at 60 s blocks
pub const ERA2_END: u64 = 4_217_760; // 4 halvings (16/8/4/2)
pub const TAIL_REWARD: u64 = 100_000_000; // 1 LKYL forever
pub const LEVY_PERMYRIAD: u64 = 500; // 5% jackpot/pot levy

/// Gross block subsidy at `height` (base units) — floor division, exactly
/// the node's era walk.
#[must_use]
pub fn subsidy(height: u64) -> u64 {
    if height == 0 {
        return 0;
    }
    if height <= RAMP_END {
        // Era 1: linear interpolation 0 -> era-2's 16 LKYL.
        return (ERA2_REWARD * height) / RAMP_END;
    }
    if height < ERA2_END {
        let halvings = (height - RAMP_END) / HALVING;
        return ERA2_REWARD >> halvings.min(63);
    }
    TAIL_REWARD
}

/// Net coinbase before fees: the subsidy minus the 5% levy.
#[must_use]
pub fn net_subsidy(height: u64) -> u64 {
    let s = subsidy(height);
    s - (s * LEVY_PERMYRIAD) / 10_000
}

/// Live emission phase for display.
#[derive(Debug, Clone, PartialEq)]
pub struct Emission {
    pub height: u64,
    pub era: u8,
    pub phase: &'static str,
    /// Gross subsidy (base units).
    pub reward: u64,
    /// Subsidy minus the 5% levy (what the finder keeps, before fees).
    pub net: u64,
    /// Next schedule change: (height, label).
    pub next_milestone: Option<(u64, &'static str)>,
    /// Fraction through the current phase (0..=1).
    pub progress: f64,
}

/// Describe the schedule at `height`.
#[must_use]
pub fn emission(height: u64) -> Emission {
    if height <= RAMP_END {
        let progress = height as f64 / RAMP_END as f64;
        return Emission {
            height,
            era: 1,
            phase: "week-1 ramp",
            reward: subsidy(height),
            net: net_subsidy(height),
            next_milestone: Some((RAMP_END, "full rate 16 LKYL")),
            progress,
        };
    }
    if height < ERA2_END {
        let since = height - RAMP_END;
        let halvings = since / HALVING;
        let next = RAMP_END + (halvings + 1) * HALVING;
        let (milestone, label) = if next < ERA2_END {
            (next, "next halving")
        } else {
            (ERA2_END, "tail 1 LKYL/block")
        };
        return Emission {
            height,
            era: 2,
            phase: "full rate",
            reward: subsidy(height),
            net: net_subsidy(height),
            next_milestone: Some((milestone, label)),
            progress: (since % HALVING) as f64 / HALVING as f64,
        };
    }
    Emission {
        height,
        era: 3,
        phase: "tail",
        reward: TAIL_REWARD,
        net: net_subsidy(height),
        next_milestone: None,
        progress: 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_matches_on_chain_coinbases() {
        // Pinned against the live testnet chain (2026-10-14/15): block 8
        // coinbase 1,206,349 = 1,269,841 gross - 5% levy.
        assert_eq!(subsidy(0), 0);
        assert_eq!(subsidy(1), 158_730);
        assert_eq!(subsidy(8), 1_269_841);
        assert_eq!(net_subsidy(8), 1_206_349);
        assert_eq!(subsidy(10), 1_587_301);
        assert_eq!(net_subsidy(10), 1_507_936);
        assert_eq!(subsidy(RAMP_END), ERA2_REWARD);
        assert_eq!(net_subsidy(RAMP_END), 1_520_000_000);
    }

    #[test]
    fn ramp_is_monotonic_and_bounded() {
        let mut prev = 0;
        for h in 1..=RAMP_END {
            let s = subsidy(h);
            assert!(s >= prev, "ramp must not fall at {h}");
            assert!(s <= ERA2_REWARD);
            prev = s;
        }
    }

    #[test]
    fn halvings_and_tail() {
        assert_eq!(subsidy(RAMP_END + HALVING - 1), ERA2_REWARD);
        assert_eq!(subsidy(RAMP_END + HALVING), ERA2_REWARD / 2);
        assert_eq!(subsidy(RAMP_END + 2 * HALVING), ERA2_REWARD / 4);
        assert_eq!(subsidy(RAMP_END + 3 * HALVING), ERA2_REWARD / 8);
        assert_eq!(subsidy(ERA2_END - 1), ERA2_REWARD / 8);
        assert_eq!(subsidy(ERA2_END), TAIL_REWARD);
        assert_eq!(subsidy(ERA2_END + 100_000_000), TAIL_REWARD);
    }

    #[test]
    fn emission_phases_and_milestones() {
        let e = emission(5_000);
        assert_eq!(e.era, 1);
        assert_eq!(e.phase, "week-1 ramp");
        assert_eq!(e.next_milestone, Some((RAMP_END, "full rate 16 LKYL")));
        assert!((e.progress - 5_000.0 / RAMP_END as f64).abs() < 1e-9);

        let e = emission(RAMP_END + 10);
        assert_eq!(e.era, 2);
        assert_eq!(e.phase, "full rate");
        assert_eq!(e.next_milestone, Some((RAMP_END + HALVING, "next halving")));

        // Last halving era: the milestone is the tail.
        let e = emission(RAMP_END + 3 * HALVING + 5);
        assert_eq!(e.next_milestone, Some((ERA2_END, "tail 1 LKYL/block")));

        let e = emission(ERA2_END + 1);
        assert_eq!(e.era, 3);
        assert_eq!(e.phase, "tail");
        assert_eq!(e.next_milestone, None);
        assert_eq!(e.reward, TAIL_REWARD);
    }

    #[test]
    fn net_never_exceeds_gross() {
        for h in [1u64, 1_000, RAMP_END, RAMP_END + HALVING, ERA2_END, ERA2_END + 999] {
            let (g, n) = (subsidy(h), net_subsidy(h));
            assert!(n < g || h == 0, "levy must be withheld at {h}");
            assert_eq!(n, g - g * 5 / 100);
        }
    }
}
