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

// The schedule and its constants live in `lm_rewards::emission` (one
// Rust mirror shared with the explorer). `subsidy`/`net_subsidy` are
// re-exported so existing wallet call sites keep their names.
pub use lm_rewards::emission::{
    net_subsidy, subsidy, ERA2_END, ERA2_REWARD, HALVING, LEVY_PERMYRIAD, RAMP_END, TAIL_REWARD,
};

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
