//! Seed-funded operational reserves (internal/20): the faucet and the dev
//! fund start from the first block like the jackpot/pot seeds, as virtual
//! consensus balances spendable only through capped per-block claims to
//! pinned scripts. Exact mirror of the C++ `lucky_seed.h` (same constants,
//! same math) — the pool runs this to decide claims, the node enforces it.
//!
//! All amounts are base units (u64); every op saturates, never wraps.

/// Genesis seed: faucet working capital (testnet).
pub const FAUCET_SEED: u64 = 10_000_000_000_000;
/// Genesis seed: dev starter capital (testnet; the 1% + 0.5pp fee streams
/// are ongoing income, this is the starter).
pub const DEV_SEED: u64 = 5_000_000_000_000;
/// Max faucet claim per block (a ceiling, not a schedule).
pub const FAUCET_CAP: u64 = 100_000_000_000;
/// Dev claims land every DEV_EPOCH-th block ("salary" cadence).
pub const DEV_EPOCH: u64 = 100;
/// Max dev claim per dev-epoch block.
pub const DEV_CAP: u64 = 10_000_000_000;

/// Seed balances before the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SeedState {
    pub faucet: u64,
    pub dev: u64,
}

/// Faucet allowance this block: capped and bounded by the reserve.
/// Unpinned (mainnet until ceremony) ⇒ 0.
#[must_use]
pub fn faucet_allowance(reserve: u64, pinned: bool) -> u64 {
    if !pinned || reserve == 0 {
        return 0;
    }
    reserve.min(FAUCET_CAP)
}

/// Dev allowance this block: only on epoch heights, capped, bounded.
/// `epoch` defaults to the frozen spec; consensus passes its effective
/// epoch (regtest override parity).
#[must_use]
pub fn dev_allowance(reserve: u64, height: u64, pinned: bool, epoch: u64) -> u64 {
    if !pinned || reserve == 0 || epoch == 0 || !height.is_multiple_of(epoch) {
        return 0;
    }
    reserve.min(DEV_CAP)
}

/// One block's seed-claim outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedPayout {
    pub faucet_claimed: u64,
    pub dev_claimed: u64,
    pub faucet: u64,
    pub dev: u64,
    pub ok: bool,
}

/// Apply one block's seed claims (`faucet_sum`/`dev_sum` = summed coinbase
/// outputs paying the pinned scripts). Saturating, never wraps; rejects
/// instead of clamping an over-allowance claim.
#[must_use]
pub fn apply_seed_block(
    reserves: SeedState,
    height: u64,
    faucet_sum: u64,
    dev_sum: u64,
    faucet_pinned: bool,
    dev_pinned: bool,
    dev_epoch: u64,
) -> SeedPayout {
    let faucet_allow = faucet_allowance(reserves.faucet, faucet_pinned);
    let dev_allow = dev_allowance(reserves.dev, height, dev_pinned, dev_epoch);
    let ok = faucet_sum <= faucet_allow && dev_sum <= dev_allow;
    let faucet_claimed = if ok { faucet_sum } else { 0 };
    let dev_claimed = if ok { dev_sum } else { 0 };
    SeedPayout {
        faucet_claimed,
        dev_claimed,
        faucet: reserves.faucet.saturating_sub(faucet_claimed),
        dev: reserves.dev.saturating_sub(dev_claimed),
        ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors of the C++ KAT vectors (lucky_seed_kat.cpp).
    #[test]
    fn seeds_and_caps_match_the_frozen_spec() {
        assert_eq!(FAUCET_SEED, 10_000_000_000_000);
        assert_eq!(DEV_SEED, 5_000_000_000_000);
        assert_eq!(FAUCET_CAP, 100_000_000_000);
        assert_eq!(DEV_CAP, 10_000_000_000);
        assert_eq!(DEV_EPOCH, 100);
    }

    #[test]
    fn allowances_gate_on_pins_reserves_and_epoch() {
        assert_eq!(faucet_allowance(FAUCET_SEED, true), FAUCET_CAP);
        assert_eq!(faucet_allowance(500, true), 500);
        assert_eq!(faucet_allowance(FAUCET_SEED, false), 0);
        assert_eq!(faucet_allowance(0, true), 0);
        assert_eq!(dev_allowance(DEV_SEED, 100, true, DEV_EPOCH), DEV_CAP);
        assert_eq!(dev_allowance(DEV_SEED, 200, true, DEV_EPOCH), DEV_CAP);
        assert_eq!(dev_allowance(DEV_SEED, 99, true, DEV_EPOCH), 0);
        assert_eq!(dev_allowance(DEV_SEED, 101, true, DEV_EPOCH), 0);
        assert_eq!(dev_allowance(50, 300, true, DEV_EPOCH), 50);
        assert_eq!(dev_allowance(DEV_SEED, 100, false, DEV_EPOCH), 0);
        assert_eq!(dev_allowance(DEV_SEED, 100, true, DEV_EPOCH), DEV_CAP);
        // Explicit epoch threading (regtest override parity).
        assert_eq!(dev_allowance(DEV_SEED, 6, true, 5), 0);
        assert_eq!(dev_allowance(DEV_SEED, 10, true, 5), DEV_CAP);
    }

    #[test]
    fn claims_debit_actuals_and_conserve() {
        let r = SeedState {
            faucet: FAUCET_SEED,
            dev: DEV_SEED,
        };
        let p = apply_seed_block(r, 100, 1_000, 50, true, true, DEV_EPOCH);
        assert!(p.ok);
        assert_eq!((p.faucet_claimed, p.dev_claimed), (1_000, 50));
        assert_eq!(p.faucet + 1_000, FAUCET_SEED);
        assert_eq!(p.dev + 50, DEV_SEED);
        // Zero claims leave full reserves (the common block).
        let q = apply_seed_block(r, 101, 0, 0, true, true, DEV_EPOCH);
        assert!(q.ok && q.faucet == FAUCET_SEED && q.dev == DEV_SEED);
    }

    #[test]
    fn over_allowance_claims_fail_closed() {
        let r = SeedState {
            faucet: 1_000,
            dev: 1_000,
        };
        let p = apply_seed_block(r, 200, 1_001, 0, true, true, DEV_EPOCH);
        assert!(!p.ok && p.faucet == 1_000 && p.dev == 1_000);
        let q = apply_seed_block(r, 200, 0, 1_001, true, true, DEV_EPOCH);
        assert!(!q.ok);
        // Unpinned legs cannot pay even with full reserves.
        let s = apply_seed_block(
            SeedState {
                faucet: FAUCET_SEED,
                dev: DEV_SEED,
            },
            300,
            1,
            1,
            false,
            false,
            DEV_EPOCH,
        );
        assert!(!s.ok && s.faucet == FAUCET_SEED && s.dev == DEV_SEED);
        // Dev claims off-epoch fail even within cap.
        let t = apply_seed_block(
            SeedState {
                faucet: FAUCET_SEED,
                dev: DEV_SEED,
            },
            301,
            0,
            10,
            true,
            true,
            DEV_EPOCH,
        );
        assert!(!t.ok && t.dev == DEV_SEED);
    }
}
