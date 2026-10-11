//! Block templates: the seam between pool and chain.
//!
//! [`TemplateProvider`] is the only interface the server needs from a chain:
//! poll the current template, submit a found block. The forked node (P1)
//! implements this over its RPC; until then [`MockProvider`] simulates a
//! deterministic mock chain with the *real* money rules — real EmberHash
//! verification, the real `T/100` jackpot trigger, and the real
//! levy/bonus/cap reserve machine — so the full loop is testable today.

use lm_pow::{is_jackpot, meets_target, pow_hash};
use lm_rewards::reserve::apply_block;
use primitive_types::U256;

/// Template the pool builds jobs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTemplate {
    pub header: Vec<u8>,
    pub height: u64,
    pub pool_target: [u8; 32],
    pub net_target: [u8; 32],
}

/// Result of handing a network-meeting share to the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockOutcome {
    /// Chain accepted the block (recomputed PoW meets the network target).
    pub accepted: bool,
    pub out: [u8; 32],
    pub jackpot: bool,
    /// Jackpot bonus paid on top of the normal payout (0 if not a jackpot
    /// or the reserve was empty — the documented warm-up).
    pub bonus: u64,
    /// Levy slice diverted to the league prize pool (cap overflow).
    pub league_overflow: u64,
    /// Reserve balance after this block.
    pub reserve: u64,
}

pub trait TemplateProvider: Send {
    /// Current template (called on every broadcast tick; height changes
    /// signal a new job with `clean = true`).
    fn current(&self) -> JobTemplate;
    /// Submit a candidate block. Implementations MUST re-verify the PoW —
    /// the pool's check is advisory, the chain's is authoritative.
    fn submit_block(&mut self, header: &[u8], height: u64, nonce: u64) -> BlockOutcome;
}

/// Deterministic mock chain for offline development and the e2e suite.
/// Each accepted block stamps `height` into header bytes `[0..8]` and
/// advances, so templates chain visibly. Construct with max targets for an
/// instant-block flow harness, or tight targets for rejection drills.
#[derive(Debug)]
pub struct MockProvider {
    height: u64,
    pool_target: [u8; 32],
    net_target: [u8; 32],
    base_reward: u64,
    fees: u64,
    reserve: u64,
    blocks: u64,
}

impl MockProvider {
    pub fn new(pool_target: [u8; 32], net_target: [u8; 32], base_reward: u64, fees: u64) -> Self {
        Self { height: 1, pool_target, net_target, base_reward, fees, reserve: 0, blocks: 0 }
    }

    /// Instant-block harness: every share is a block (both targets max).
    pub fn instant(base_reward: u64) -> Self {
        Self::new([0xffu8; 32], [0xffu8; 32], base_reward, 0)
    }

    fn header(&self) -> Vec<u8> {
        let mut h: Vec<u8> = (0u8..80).collect();
        h[..8].copy_from_slice(&self.height.to_le_bytes());
        h
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    pub fn reserve(&self) -> u64 {
        self.reserve
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }
}

impl TemplateProvider for MockProvider {
    fn current(&self) -> JobTemplate {
        JobTemplate {
            header: self.header(),
            height: self.height,
            pool_target: self.pool_target,
            net_target: self.net_target,
        }
    }

    fn submit_block(&mut self, header: &[u8], height: u64, nonce: u64) -> BlockOutcome {
        let out = pow_hash(header, nonce, height);
        let net = U256::from_big_endian(&self.net_target);
        if height != self.height || !meets_target(&out, &self.net_target) {
            return BlockOutcome {
                accepted: false,
                out,
                jackpot: false,
                bonus: 0,
                league_overflow: 0,
                reserve: self.reserve,
            };
        }
        let jackpot = is_jackpot(&out, net);
        let p = apply_block(self.reserve, self.base_reward, self.fees, jackpot);
        self.reserve = p.reserve;
        self.blocks += 1;
        self.height += 1;
        BlockOutcome {
            accepted: true,
            out,
            jackpot,
            bonus: p.bonus,
            league_overflow: p.to_league,
            reserve: p.reserve,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instant_chain_accepts_and_advances() {
        let mut m = MockProvider::instant(1_000_000);
        let t = m.current();
        assert_eq!(t.height, 1);
        // Nonce 0 under max targets: real PoW, real acceptance.
        let o = m.submit_block(&t.header, t.height, 0);
        assert!(o.accepted);
        assert_eq!(m.blocks(), 1);
        assert_eq!(m.height(), 2);
        // Same header at the old height is now stale-chain: rejected.
        let o2 = m.submit_block(&t.header, t.height, 0);
        assert!(!o2.accepted);
        // Levy accounting ran: 5% of 1M banked (or paid out on a jackpot).
        assert_eq!(o.reserve + o.bonus, m.reserve() + o.bonus);
        assert!(m.reserve() <= 50_000 + o.bonus);
    }

    #[test]
    fn tight_chain_rejects_weak_work() {
        let mut m = MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let t = m.current();
        let o = m.submit_block(&t.header, t.height, 0);
        assert!(!o.accepted);
        assert_eq!(m.blocks(), 0);
    }

    #[test]
    fn jackpot_trigger_fires_on_real_hashes() {
        // Grind real hashes against max targets until the 1% trigger fires.
        // Fully deterministic (fixed headers, fixed nonces) — the hit block
        // is pinned so trigger regressions show up as test failures.
        let mut m = MockProvider::instant(1_000_000);
        let mut hit_at = None;
        for n in 0..600u64 {
            let t = m.current();
            let o = m.submit_block(&t.header, t.height, n);
            assert!(o.accepted);
            if o.jackpot {
                assert!(o.bonus > 0, "every block levies first, so any jackpot pays");
                hit_at = Some(n);
                break;
            }
        }
        let hit = hit_at.expect("no jackpot in 600 blocks at 1% odds (p ≈ 0.2%)");
        assert_eq!(hit, 119);
    }
}
