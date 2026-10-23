//! Testnet-gate simulations for doc 06 §6.5 (offline mirror).
//!
//! The real gates run on testnet with real PoW hashes (≥20,000 blocks). These
//! tests mirror the gate *logic* deterministically offline:
//!
//! - Gate 1 (rate): uniform-64 draws against `u64::MAX / 100` — the `T/100`
//!   rule at the easiest target, where the top-64-bits comparison approximates
//!   the full 256-bit ticket to within 2^-64. Count must land in [0.5%, 2%].
//! - Gate 2 (neutrality): exact every-100th-block schedule pins the rate at
//!   precisely 1% so the money math is checked without lottery noise; plus a
//!   seeded-RNG run asserting conservation and bounded drift.
//! - Gate 3 (warm-up): reserve rebooted to 0 mid-run; early jackpots pay thin,
//!   full bonus recovers, no consensus faults (every block conserves).
//! - Gate 4 (split drill): [`lm_rewards::split_jackpot`] checked against an
//!   independent float-based reference implementation on a fixed case.
//!
//! RNG is xorshift64* with pinned seeds — no external crates, fully
//! deterministic. A failing gate here fails the same way on testnet.
//!
//! Gates 1-3 exercise the frozen pre-split reserve rule deliberately
//! (doc 06 rev 1.1 history); the pot split is gated separately in pot.rs.
#![allow(deprecated)]

use lm_rewards::reserve::{apply_block, levy_amount};
use lm_rewards::{split_jackpot, Share};

/// Test-side decode of the String miner ids ("mN") back to the numeric ids
/// the independent float reference works in.
fn decode(v: Vec<(String, u64)>) -> Vec<(u64, u64)> {
    v.into_iter()
        .map(|(id, pay)| {
            let n: u64 = id
                .strip_prefix('m')
                .expect("id shape")
                .parse()
                .expect("id num");
            (n, pay)
        })
        .collect()
}

const BLOCKS: usize = 20_000;
const BASE: u64 = 100_000_000;
const FEES: u64 = 2_000_000;
/// 1% threshold on uniform u64 draws (mirrors `floor(T/100)` at max target).
const DRAW_THRESHOLD: u64 = u64::MAX / 100;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64* (deterministic, no dependencies).
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[test]
fn gate1_jackpot_rate_within_half_to_two_percent() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let mut jackpots = 0usize;
    for _ in 0..BLOCKS {
        if rng.next() < DRAW_THRESHOLD {
            jackpots += 1;
        }
    }
    let rate = jackpots as f64 / BLOCKS as f64;
    assert!(
        (0.005..=0.02).contains(&rate),
        "jackpot rate {rate:.4} ({jackpots}/{BLOCKS}) outside [0.5%, 2%]"
    );
}

#[test]
fn gate2_neutrality_exact_schedule() {
    // Every 100th block is a jackpot: rate pinned at exactly 1%, fees zero so
    // the neutral expectation (0.05R/block) is exact, not approximate.
    let base = BASE;
    let levy = levy_amount(base, 0);
    assert_eq!(levy, 5_000_000);
    let mut reserve = 0u64;
    let mut bonus_total = 0u128;
    let mut levied_total = 0u128;
    for i in 1..=BLOCKS {
        let jackpot = i % 100 == 0;
        let before = reserve;
        let p = apply_block(reserve, base, 0, jackpot);
        reserve = p.reserve;
        levied_total += levy as u128;
        bonus_total += p.bonus as u128;
        // Conservation holds on every single block.
        assert_eq!(
            p.miner as u128 + p.reserve as u128 + p.to_prize as u128 - before as u128,
            base as u128
        );
    }
    // Exactly 200 jackpots × 5R, reserve fully drained each cycle: the money
    // is expectation-neutral to the unit (no drift at all on an exact 1%).
    let expected = 200u128 * 5 * base as u128;
    assert_eq!(bonus_total, expected);
    assert_eq!(levied_total, expected + reserve as u128);
    assert!(
        reserve <= 50 * base,
        "reserve {reserve} breached the 50R cap"
    );
}

#[test]
fn gate2_neutrality_seeded_run_with_fees() {
    // Seeded lottery + real fee-bearing rewards through the full state
    // machine: conservation exact, bonus within [0.5×, 2×] the neutral
    // expectation (the same band as the rate gate — count noise, not leakage).
    let mut rng = Rng(0xC0FF_EE11_2345_6789);
    let mut reserve = 0u64;
    let mut bonus_total = 0u128;
    let mut levied_total = 0u128;
    let mut prize_total = 0u128;
    let mut jackpots = 0usize;
    for _ in 0..BLOCKS {
        let jackpot = rng.next() < DRAW_THRESHOLD;
        jackpots += jackpot as usize;
        let before = reserve;
        let p = apply_block(reserve, BASE, FEES, jackpot);
        reserve = p.reserve;
        levied_total += levy_amount(BASE, FEES) as u128;
        bonus_total += p.bonus as u128;
        prize_total += p.to_prize as u128;
        assert_eq!(
            p.miner as u128 + p.reserve as u128 + p.to_prize as u128 - before as u128,
            BASE as u128 + FEES as u128,
            "conservation violated"
        );
    }
    // Exact money reconciliation: every levied unit is bonus, reserve, or prize.
    assert_eq!(levied_total, bonus_total + reserve as u128 + prize_total);
    let neutral = 0.05 * BASE as f64 * BLOCKS as f64;
    let ratio = bonus_total as f64 / neutral;
    assert!(
        (0.5..=2.0).contains(&ratio),
        "jackpots={jackpots} bonus_total={bonus_total} ratio={ratio:.3} vs neutral={neutral:.0}"
    );
}

#[test]
fn gate3_warmup_reboot_pays_thin_then_recovers() {
    // Mid-testnet reserve reboot to 0: forced early jackpots pay thin, the
    // full 5R bonus recovers after refill, and no block faults.
    let mut reserve = 0u64;
    // Fill, then reboot.
    for _ in 0..2000 {
        reserve = apply_block(reserve, BASE, FEES, false).reserve;
    }
    assert!(reserve > 0);
    reserve = 0; // reboot drill
    let thin = apply_block(reserve, BASE, FEES, true);
    assert_eq!(thin.bonus, levy_amount(BASE, FEES)); // min(5R, one levy)
    assert_eq!(thin.reserve, 0);
    assert_eq!(
        thin.miner,
        BASE + FEES - levy_amount(BASE, FEES) + thin.bonus
    );
    // Refill past 5R and confirm the full bonus lands with conservation.
    reserve = thin.reserve;
    for _ in 0..2000 {
        let before = reserve;
        let p = apply_block(reserve, BASE, FEES, false);
        reserve = p.reserve;
        assert_eq!(
            p.miner as u128 + p.reserve as u128 + p.to_prize as u128 - before as u128,
            BASE as u128 + FEES as u128
        );
    }
    let full = apply_block(reserve, BASE, FEES, true);
    assert_eq!(full.bonus, 5 * BASE);
    assert_eq!(full.miner, BASE + FEES - levy_amount(BASE, FEES) + 5 * BASE);
}

/// Independent float-based reference for the §6.3 split (gate 4): sqrt
/// weights, 25% cap for 5+ miners, floor + largest-remainder dust. Written
/// against the spec text, not against the implementation.
fn reference_split(miners: &[(u64, u64)], bonus: u64) -> Vec<(u64, u64)> {
    let mut ids: Vec<u64> = miners.iter().map(|(id, _)| *id).collect();
    ids.sort_unstable();
    if ids.is_empty() || bonus == 0 {
        return Vec::new();
    }
    if ids.len() == 1 {
        return vec![(ids[0], bonus)];
    }
    let shares_of = |id: u64| miners.iter().find(|(m, _)| *m == id).unwrap().1;
    let weights: Vec<f64> = ids
        .iter()
        .map(|&id| (shares_of(id) as f64).sqrt())
        .collect();
    let cap = if ids.len() >= 5 {
        (bonus as f64 * 0.25).floor().max(1.0) as u64
    } else {
        bonus
    };
    let wsum: f64 = weights.iter().sum();
    let mut locked = vec![false; ids.len()];
    let mut rem_bonus = bonus;
    let mut rem_w = wsum;
    loop {
        let mut hit = false;
        for i in 0..ids.len() {
            if locked[i] {
                continue;
            }
            if rem_bonus as f64 * weights[i] / rem_w >= cap as f64 {
                locked[i] = true;
                rem_bonus -= cap;
                rem_w -= weights[i];
                hit = true;
            }
        }
        if !hit {
            break;
        }
    }
    if locked.iter().all(|l| *l) {
        locked = vec![false; ids.len()];
        rem_bonus = bonus;
        rem_w = wsum;
    }
    let mut pays = vec![0u64; ids.len()];
    let mut fracs: Vec<(f64, u64)> = Vec::new();
    let mut assigned = 0u64;
    for i in 0..ids.len() {
        if locked[i] {
            pays[i] = cap;
            assigned += cap;
            continue;
        }
        let exact = rem_bonus as f64 * weights[i] / rem_w;
        pays[i] = exact.floor() as u64;
        assigned += pays[i];
        fracs.push((exact - exact.floor(), ids[i]));
    }
    fracs.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(a.1.cmp(&b.1)));
    let mut dust = bonus - assigned;
    for (_, id) in fracs {
        if dust == 0 {
            break;
        }
        pays[ids.iter().position(|m| *m == id).unwrap()] += 1;
        dust -= 1;
    }
    ids.into_iter().zip(pays).collect()
}

#[test]
fn gate4_pool_split_matches_independent_reference() {
    let miners = vec![(1u64, 1_000_000u64), (2, 100), (3, 100), (4, 100), (5, 100)];
    let shares: Vec<Share> = miners
        .iter()
        .map(|&(m, s)| Share {
            miner: format!("m{m}"),
            shares: s,
        })
        .collect();
    for bonus in [1_000_000u64, 1_000_003, 7, 999, 160_000, 87_654_321] {
        assert_eq!(
            decode(split_jackpot(&shares, bonus)),
            reference_split(&miners, bonus),
            "bonus={bonus}"
        );
    }
    // Small-pool (cap lifted) and flat cases too.
    let small = vec![(1u64, 10_000u64), (2, 2_500), (3, 100)];
    let small_shares: Vec<Share> = small
        .iter()
        .map(|&(m, s)| Share {
            miner: format!("m{m}"),
            shares: s,
        })
        .collect();
    assert_eq!(
        decode(split_jackpot(&small_shares, 160_000)),
        reference_split(&small, 160_000)
    );
    let flat: Vec<(u64, u64)> = (1..=5).map(|m| (m, 400u64)).collect();
    let flat_shares: Vec<Share> = flat
        .iter()
        .map(|&(m, s)| Share {
            miner: format!("m{m}"),
            shares: s,
        })
        .collect();
    assert_eq!(
        decode(split_jackpot(&flat_shares, 1_000_000)),
        reference_split(&flat, 1_000_000)
    );
}
