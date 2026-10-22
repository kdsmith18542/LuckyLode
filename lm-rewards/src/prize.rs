//! Prize Fund + Luckynode Draw money rules (doc 06 §6.7, rev 1.3) —
//! the Rust reference mirror of the node's `lucky_prize.h`. The prize
//! fund is the third consensus fund: the jackpot-levy slice that
//! overflows the reserve cap is credited to it every block (unspendable
//! bookkeeping before rev 1.3), and only Luckynode Draw claims spend it.
//! All amounts are base units, all division floors, intermediates
//! saturate. KAT-pinned against the C++ header (frozen vectors in both
//! test sets must stay byte-identical).

/// Genesis seed: 1,000 LKYL (mainnet value finalized at the ceremony).
pub const PRIZE_SEED: u64 = 100_000_000_000;
/// Draw budget: slice of the trailing epoch's prize inflow, per-myriad
/// (1000 = 10%, frozen — the pool-era v1 default).
pub const PRIZE_SLICE_BPS: u64 = 1_000;
const PRIZE_SLICE_DENOM: u64 = 10_000;

/// Credit one block's overflow into the fund (saturating).
#[must_use]
pub fn credit(balance: u64, overflow: u64) -> u64 {
    balance.saturating_add(overflow)
}

/// Prize inflow over a trailing window: the fund moved from
/// `balance_then` to `balance_now` while `claims` left it, so the
/// inflow is the signed delta plus what was drained — floored at zero
/// (a window that starts after a large drain under-counts, which only
/// makes the budget smaller: fail-safe).
#[must_use]
pub fn trailing_inflow(balance_now: u64, balance_then: u64, claims: u64) -> u64 {
    (i128::from(balance_now) - i128::from(balance_then) + i128::from(claims)).max(0) as u64
}

/// Seed release for one epoch: seed / 1000 (1 LKYL at the frozen
/// seed — the fund's front-load, draining over ~1,000 epochs; without
/// it a pure inflow-slice budget would recycle organic inflow forever
/// while the seed sat dead).
pub const PRIZE_SEED_RELEASE_DIVISOR: u64 = 1_000;

/// Seed release for one epoch (0 when the seed is 0).
#[must_use]
pub fn seed_release(seed: u64) -> u64 {
    seed / PRIZE_SEED_RELEASE_DIVISOR
}

/// Draw budget: the slice of the trailing inflow plus the fixed seed
/// release, capped at the fund balance (a draw may only spend what the
/// fund holds). Zero inflow still pays the seed release while organic
/// inflow scales the budget with hashrate.
#[must_use]
pub fn budget(inflow: u64, balance: u64, slice_bps: u64, seed: u64) -> u64 {
    let slice = u128::from(inflow) * u128::from(slice_bps) / u128::from(PRIZE_SLICE_DENOM);
    let budget = slice + u128::from(seed_release(seed));
    budget.min(u128::from(balance)) as u64
}

/// Deterministic tenure-weighted winner over the entrant set:
/// `r = u64(seed[..8] LE) % total-weight`, walked id-ascending — the
/// byte-for-byte mirror of the pool-era v1 `draw_winner`
/// (`lm-luckynode`), kept identical so the promotion changes no
/// selection math. `None` for empty entrants or zero total weight.
#[must_use]
pub fn draw_winner<'a>(entrants: &'a [(&'a str, u64)], seed: &[u8; 32]) -> Option<&'a str> {
    let total: u128 = entrants.iter().map(|(_, w)| u128::from(*w)).sum();
    if total == 0 || entrants.is_empty() {
        return None;
    }
    let mut sorted: Vec<(&str, u64)> = entrants.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut r = u128::from(u64::from_le_bytes(seed[..8].try_into().expect("seed len")));
    r %= total;
    for (id, w) in &sorted {
        let w = u128::from(*w);
        if r < w {
            return Some(id);
        }
        r -= w;
    }
    sorted.last().map(|(id, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Frozen vectors — byte-identical to the C++ KAT (lucky_prize_kat.cpp).

    #[test]
    fn constants_are_the_frozen_spec() {
        assert_eq!(PRIZE_SEED, 100_000_000_000);
        assert_eq!(PRIZE_SLICE_BPS, 1_000);
    }

    #[test]
    fn credit_saturates() {
        assert_eq!(credit(0, 0), 0);
        assert_eq!(credit(100, 25), 125);
        assert_eq!(credit(u64::MAX - 1, 5), u64::MAX);
    }

    #[test]
    fn trailing_inflow_is_signed_delta_plus_claims() {
        assert_eq!(trailing_inflow(150, 100, 0), 50);
        assert_eq!(trailing_inflow(100, 150, 60), 10);
        assert_eq!(trailing_inflow(100, 150, 200), 150);
        assert_eq!(trailing_inflow(90, 100, 5), 0); // floored at zero
        assert_eq!(trailing_inflow(u64::MAX, 0, 0), u64::MAX);
    }

    #[test]
    fn budget_slices_releases_and_caps() {
        assert_eq!(seed_release(PRIZE_SEED), 100_000_000);
        assert_eq!(seed_release(0), 0);
        assert_eq!(budget(0, PRIZE_SEED, PRIZE_SLICE_BPS, 0), 0);
        assert_eq!(budget(0, PRIZE_SEED, PRIZE_SLICE_BPS, PRIZE_SEED), 100_000_000);
        assert_eq!(budget(1000, PRIZE_SEED, PRIZE_SLICE_BPS, PRIZE_SEED), 100_000_100);
        assert_eq!(budget(999, PRIZE_SEED, PRIZE_SLICE_BPS, PRIZE_SEED), 100_000_099);
        assert_eq!(budget(100, 5, PRIZE_SLICE_BPS, PRIZE_SEED), 5); // capped at balance
        // Draw 51 live vector (v1 shape, seed 0): inflow 2,952,381 -> floor 10%.
        assert_eq!(budget(2_952_381, 2_952_381, PRIZE_SLICE_BPS, 0), 295_238);
    }

    #[test]
    fn winner_selection_matches_the_cpp_kat() {
        let seed1 = [7u8; 32];
        let seed2 = [9u8; 32];
        assert_eq!(draw_winner(&[], &seed1), None);
        assert_eq!(draw_winner(&[("x", 0)], &seed1), None);
        assert_eq!(draw_winner(&[("solo", 1000)], &seed2), Some("solo"));
        // Determinism.
        let e = [("alice", 1000u64), ("bob", 2000u64)];
        assert_eq!(draw_winner(&e, &seed1), draw_winner(&e, &seed1));
        // Proportionality: bob (2x) wins ~2/3 over the same 3000-seed
        // sweep the C++ KAT runs (identical seed construction).
        let (mut alice, mut bob) = (0u32, 0u32);
        for i in 0u32..3000 {
            let mut s = [0u8; 32];
            s[..4].copy_from_slice(&i.to_le_bytes());
            s[4..8].copy_from_slice(&0x9e3779b9u32.to_le_bytes());
            match draw_winner(&e, &s) {
                Some("alice") => alice += 1,
                Some("bob") => bob += 1,
                _ => panic!("unexpected"),
            }
        }
        let frac = f64::from(bob) / f64::from(alice + bob);
        assert!((frac - 2.0 / 3.0).abs() < 0.03, "bob frac {frac}");
    }
}
