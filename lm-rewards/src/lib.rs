//! Reference implementation of the jackpot-bonus split (Lucky Lode doc 06
//! §6.3): sub-linear (sqrt) weights, per-miner cap, largest-remainder dust.
//!
//! Rules (frozen alongside doc 06 rev 1.0):
//!
//! - Miners with zero shares are excluded. Empty input → no payouts (bonus
//!   stays in reserve — caller decides).
//! - One miner takes everything (solo-equivalent; the on-chain rule already
//!   pays solo finders the whole jackpot).
//! - Weights `w_i = isqrt(shares_i)`; uncapped split is pro-rata by weight.
//! - Cap: no miner takes more than 250/1000 of the bonus **when 5+ miners
//!   share** (with ≤4 miners a 25% cap is unsatisfiable, so it lifts).
//!   Applied iteratively to a fixpoint (water-filling — order-independent).
//! - Rounding: floor each payout; leftover dust goes largest-remainder
//!   (fraction desc, miner id asc — fully deterministic).
//! - Conservation: payouts always sum to exactly the bonus.
//!
//! Reserve accounting lives in [`reserve`] (per-block levy, bonus, cap,
//! league-pool overflow — doc 06 §6.2); the Lucky Pot (second stochastic
//! sink: frequent small splashes beside rare full-drain jackpots) lives in
//! [`pot`] (internal/16, promotes to doc 06 on sign-off); multi-class luck
//! exploration lives in [`luck`] (doc 12 §12.10, not consensus-wired).

pub mod luck;
pub mod pot;
pub mod reserve;

/// Per-mille cap denominator base.
pub const CAP_DENOM: u64 = 1000;
/// Per-mille cap when it binds (25%).
pub const CAP_PERMILLE: u64 = 250;
/// Minimum sharers for the cap to bind.
pub const CAP_MIN_MINERS: usize = 5;

/// One miner's PPLNS-window shares. `miner` is the pool-side worker identity
/// — the FULL name string (review fix 2026-10-17: money must never be keyed
/// by a non-crypto 64-bit hash; FNV worker ids were forgeable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub miner: String,
    pub shares: u64,
}

/// Split `bonus` micros over `shares`. Returns `(miner, payout)` sorted by
/// miner id ascending.
pub fn split_jackpot(shares: &[Share], bonus: u64) -> Vec<(String, u64)> {
    // Sum duplicate miner rows first (a duplicate id used to double-pay).
    let mut miners: Vec<(String, u64)> = Vec::new();
    for s in shares.iter().filter(|s| s.shares > 0) {
        match miners.iter_mut().find(|(id, _)| *id == s.miner) {
            Some(row) => row.1 = row.1.saturating_add(s.shares),
            None => miners.push((s.miner.clone(), s.shares)),
        }
    }
    if miners.is_empty() || bonus == 0 {
        return Vec::new();
    }
    miners.sort_by(|a, b| a.0.cmp(&b.0));

    if miners.len() == 1 {
        return vec![(miners[0].0.clone(), bonus)];
    }

    let weights: Vec<u64> = miners.iter().map(|(_, s)| s.isqrt()).collect();
    // Floor of 1 micro keeps the water-fill live on dust bonuses (bonus < 4
    // would otherwise round the cap to 0 and lock everyone out of dust).
    let cap_each = if miners.len() >= CAP_MIN_MINERS {
        ((bonus as u128 * CAP_PERMILLE as u128 / CAP_DENOM as u128) as u64).max(1)
    } else {
        bonus // cap lifted: unsatisfiable below 5 sharers
    };

    // Water-filling fixpoint: cap violators lock at cap, remainder flows on.
    let n = miners.len();
    let mut locked = vec![false; n];
    let mut remaining_bonus = bonus;
    let mut remaining_weight: u64 = weights.iter().sum();
    loop {
        let mut progress = false;
        for i in 0..n {
            if locked[i] {
                continue;
            }
            let fair = remaining_bonus as u128 * weights[i] as u128 / remaining_weight as u128;
            if fair >= cap_each as u128 {
                locked[i] = true;
                remaining_bonus -= cap_each;
                remaining_weight -= weights[i];
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    // Dust-scale fallback: the cap floor of 1 micro can over-lock when the
    // whole bonus is dust (at scale this is impossible — n×25% > 100% for
    // n ≥ 5 — so the cap always leaves someone unlocked). If everyone
    // locked, lift the cap and split pure pro-rata: conservation of money
    // outranks the cap at dust scale.
    if locked.iter().all(|l| *l) {
        locked = vec![false; n];
        remaining_bonus = bonus;
        remaining_weight = weights.iter().sum();
    }

    // Floor each unlocked payout against the remaining pool.
    let mut payouts = vec![0u64; n];
    let mut remainders: Vec<(u128, String)> = Vec::new(); // (fractional numerator, miner id)
    let mut assigned = 0u64;
    for i in 0..n {
        if locked[i] {
            payouts[i] = cap_each;
            assigned += cap_each;
            continue;
        }
        let num = remaining_bonus as u128 * weights[i] as u128;
        let den = remaining_weight as u128;
        payouts[i] = (num / den) as u64;
        assigned += payouts[i];
        remainders.push((num % den, miners[i].0.clone()));
    }
    // Largest-remainder dust, miner-id tiebreak (miners already id-sorted).
    let mut dust = bonus - assigned;
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, id) in remainders {
        if dust == 0 {
            break;
        }
        let idx = miners
            .iter()
            .position(|(m, _)| *m == id)
            .expect("id present");
        payouts[idx] += 1;
        dust -= 1;
    }
    // Locked miners hold exactly cap_each and take no dust.
    debug_assert_eq!(payouts.iter().sum::<u64>(), bonus);
    miners
        .iter()
        .zip(payouts)
        .map(|((id, _), pay)| (id.clone(), pay))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(payouts: &[(String, u64)]) -> u64 {
        payouts.iter().map(|(_, p)| p).sum()
    }

    #[test]
    fn empty_and_zero_bonus_pay_nothing() {
        assert!(split_jackpot(&[], 1_000_000).is_empty());
        assert!(split_jackpot(
            &[Share {
                miner: "m1".to_string(),
                shares: 100
            }],
            0
        )
        .is_empty());
        // Zero-share miners are excluded, not paid.
        let out = split_jackpot(
            &[
                Share {
                    miner: "m1".to_string(),
                    shares: 0,
                },
                Share {
                    miner: "m2".to_string(),
                    shares: 100,
                },
            ],
            1000,
        );
        assert_eq!(out, vec![("m2".to_string(), 1000)]);
    }

    #[test]
    fn single_miner_takes_all() {
        let out = split_jackpot(
            &[Share {
                miner: "m7".to_string(),
                shares: 5,
            }],
            999_999,
        );
        assert_eq!(out, vec![("m7".to_string(), 999_999)]);
    }

    #[test]
    fn dominant_miner_capped_rest_pro_rata() {
        // A: w=1000, B..E: w=10 each. A uncapped = 96.15% -> capped 25%.
        let shares = vec![
            Share {
                miner: "m1".to_string(),
                shares: 1_000_000,
            },
            Share {
                miner: "m2".to_string(),
                shares: 100,
            },
            Share {
                miner: "m3".to_string(),
                shares: 100,
            },
            Share {
                miner: "m4".to_string(),
                shares: 100,
            },
            Share {
                miner: "m5".to_string(),
                shares: 100,
            },
        ];
        let out = split_jackpot(&shares, 1_000_000);
        assert_eq!(
            out,
            vec![
                ("m1".to_string(), 250_000),
                ("m2".to_string(), 187_500),
                ("m3".to_string(), 187_500),
                ("m4".to_string(), 187_500),
                ("m5".to_string(), 187_500)
            ]
        );
    }

    #[test]
    fn flat_five_way_split() {
        let shares: Vec<Share> = (1..=5)
            .map(|m| Share {
                miner: format!("m{m}"),
                shares: 400,
            })
            .collect();
        let out = split_jackpot(&shares, 1_000_000);
        assert_eq!(
            out,
            vec![
                ("m1".to_string(), 200_000),
                ("m2".to_string(), 200_000),
                ("m3".to_string(), 200_000),
                ("m4".to_string(), 200_000),
                ("m5".to_string(), 200_000)
            ]
        );
    }

    #[test]
    fn dust_conserves_bonus_with_id_tiebreak() {
        let shares = vec![
            Share {
                miner: "m1".to_string(),
                shares: 1_000_000,
            },
            Share {
                miner: "m2".to_string(),
                shares: 100,
            },
            Share {
                miner: "m3".to_string(),
                shares: 100,
            },
            Share {
                miner: "m4".to_string(),
                shares: 100,
            },
            Share {
                miner: "m5".to_string(),
                shares: 100,
            },
        ];
        // Cap = 1_000_003*250/1000 = 250000; remainder 750003/4 = 187500 r3:
        // dust +1 to miners 2,3,4 (tied remainders, lowest ids first).
        let out = split_jackpot(&shares, 1_000_003);
        assert_eq!(
            out,
            vec![
                ("m1".to_string(), 250_000),
                ("m2".to_string(), 187_501),
                ("m3".to_string(), 187_501),
                ("m4".to_string(), 187_501),
                ("m5".to_string(), 187_500)
            ]
        );
        assert_eq!(total(&out), 1_000_003);
    }

    #[test]
    fn all_locked_fallback_splits_pro_rata() {
        // Dust-scale bonus where the cap floor would lock everyone:
        // falls back to pure pro-rata. 7 micros over 5 equal miners ->
        // floors of 1 plus dust 2 to the lowest ids.
        let shares: Vec<Share> = (1..=5)
            .map(|m| Share {
                miner: format!("m{m}"),
                shares: 400,
            })
            .collect();
        let out = split_jackpot(&shares, 7);
        assert_eq!(out, vec![("m1".to_string(), 2), ("m2".to_string(), 2), ("m3".to_string(), 1), ("m4".to_string(), 1), ("m5".to_string(), 1)]);
    }

    #[test]
    fn small_pool_no_cap_few_miners() {
        // 3 miners: cap lifted, pure sqrt pro-rata. w = 100,50,10 (total 160),
        // bonus 160_000 -> exact 100k/50k/10k.
        let shares = vec![
            Share {
                miner: "m1".to_string(),
                shares: 10_000,
            },
            Share {
                miner: "m2".to_string(),
                shares: 2_500,
            },
            Share {
                miner: "m3".to_string(),
                shares: 100,
            },
        ];
        let out = split_jackpot(&shares, 160_000);
        assert_eq!(out, vec![("m1".to_string(), 100_000), ("m2".to_string(), 50_000), ("m3".to_string(), 10_000)]);
    }

    #[test]
    fn conservation_fuzzish_sweep() {
        // Deterministic sweep: payouts must always sum to the bonus.
        for bonus in [1u64, 7, 999, 1_000_000, 1_000_003, u64::MAX / 4] {
            for n in 1..=8usize {
                let shares: Vec<Share> = (0..n)
                    .map(|i| Share {
                        miner: format!("m{i}"),
                        shares: 1000 * (i as u64 + 1) * (i as u64 + 1),
                    })
                    .collect();
                let out = split_jackpot(&shares, bonus);
                assert_eq!(total(&out), bonus, "bonus={bonus} n={n}");
            }
        }
    }
}
