//! Mine-to-play / stake-to-play / serve-to-play participation tiers
//! (doc 14 §14.4).
//!
//! Verified PoW shares, PoS stake and active Luckynode service combine into a
//! single tier that grants rakeback, a free-spin allowance and tournament
//! eligibility. The score compresses each input (√ on shares/stake) so a
//! large farm cannot dominate by scale alone, and so a hobbyist staker or a
//! Luckynode operator meaningfully counts.

use crate::MICROS_PER_LKYL;

/// A player's verified participation inputs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Participation {
    /// Accepted PoW shares (pool-side, verified).
    pub pow_shares: u64,
    /// PoS stake in micro-LKYL.
    pub pos_stake_micros: u64,
    /// Continuous active Luckynode service days.
    pub luckynode_service_days: u64,
}

/// The resulting casino tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    /// Tier level, 0–5.
    pub level: u8,
    /// Rakeback in permille of the player's own edge (0–100 = 0–10%).
    pub rakeback_permille: u64,
    /// House-funded free spins granted per drop.
    pub bonus_free_spins: u64,
    /// Eligible for tournaments / lucky hours.
    pub tournaments: bool,
}

/// Lower score bounds for levels 0–5.
pub const TIER_THRESHOLDS: [u64; 6] = [0, 10, 30, 100, 300, 1_000];

/// Deterministic participation score.
#[must_use]
pub fn participation_score(p: &Participation) -> u64 {
    let pow = p.pow_shares.isqrt();
    let pos = (p.pos_stake_micros / MICROS_PER_LKYL).isqrt();
    let luckynode = p.luckynode_service_days.saturating_mul(5);
    pow.saturating_add(pos).saturating_add(luckynode)
}

/// Map participation to a tier.
#[must_use]
pub fn tier_for(p: &Participation) -> Tier {
    let score = participation_score(p);
    let reached = TIER_THRESHOLDS.iter().filter(|t| score >= **t).count();
    let level = (reached.saturating_sub(1)).min(5) as u8;
    Tier {
        level,
        rakeback_permille: level as u64 * 20,
        bonus_free_spins: level as u64 * 5,
        tournaments: level >= 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_participation_is_level_zero() {
        let t = tier_for(&Participation::default());
        assert_eq!(t.level, 0);
        assert_eq!(t.rakeback_permille, 0);
        assert!(!t.tournaments);
    }

    #[test]
    fn score_is_monotone_in_each_input() {
        let base = Participation::default();
        let pow = Participation {
            pow_shares: 10_000,
            ..base
        };
        let pos = Participation {
            pos_stake_micros: 1_000 * MICROS_PER_LKYL,
            ..base
        };
        let node = Participation {
            luckynode_service_days: 20,
            ..base
        };
        let s0 = participation_score(&base);
        assert!(participation_score(&pow) > s0);
        assert!(participation_score(&pos) > s0);
        assert!(participation_score(&node) > s0);
    }

    #[test]
    fn high_participation_reaches_top_tier() {
        let p = Participation {
            pow_shares: 10_000_000,
            pos_stake_micros: 1_000_000 * MICROS_PER_LKYL,
            luckynode_service_days: 400,
        };
        let t = tier_for(&p);
        assert_eq!(t.level, 5);
        assert_eq!(t.rakeback_permille, 100);
        assert!(t.tournaments);
    }

    #[test]
    fn tier_only_improves_as_score_rises() {
        let mut last = 0;
        for shares in [0u64, 100, 1_000, 10_000, 100_000, 1_000_000, 50_000_000] {
            let t = tier_for(&Participation {
                pow_shares: shares,
                ..Participation::default()
            });
            assert!(t.level >= last, "level regressed at {shares}");
            last = t.level;
        }
    }
}
