//! Locked emission schedule (docs/04 §4.1, LOCKED 2026-10-13).
//!
//! The node's `komodo_ac_block_subsidy` is the authority; this module is
//! the single Rust mirror used by the explorer and the wallet. Values are
//! pinned against on-chain coinbases in the tests.
//!
//! Schedule (60 s blocks, 1,440/day):
//! - **week 1** (blocks 1..=10,080): linear ramp 0 → 16 LKYL
//! - **years 1–8**: 16 LKYL halving every 1,051,920 blocks → 8 / 4 / 2
//! - **year 9+**: fixed 1 LKYL/block forever (anti-fade tail)

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
        // The node already applies the 4th halving at exactly ERA2_END.
        assert_eq!(subsidy(ERA2_END), TAIL_REWARD);
        assert_eq!(subsidy(u64::MAX), TAIL_REWARD);
    }
}
