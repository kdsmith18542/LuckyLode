//! Per-miner value accounting: who is owed what (internal/17 §17.4).
//!
//! The pool's node wallet receives block coinbases; this ledger tracks
//! what each miner earned — normal block value pro-rata by PPLNS weight,
//! plus already-split distributions (jackpot/pot/league) credited
//! verbatim — minus a pool fee, until a payout threshold trips and the
//! payout loop sends via `sendtoaddress` and calls [`PayoutLedger::record_paid`].
//!
//! All money math is `u64` integer with floored pro-rata and
//! largest-remainder dust (fraction desc, miner id asc — the same
//! determinism posture as the jackpot split). Nothing here touches
//! consensus, persistence, or the network: pure ledger, fully testable.

use std::collections::HashMap;

/// Fee denominator (per-myriad): fee = `total * fee_permyriad / 10_000`.
pub const FEE_DENOM_PERMYRIAD: u64 = 10_000;
/// Coins per whole coin (base units) for the `sendtoaddress` decimal form.
pub const COIN_BASE: u64 = 100_000_000;

#[derive(Debug, Clone, Default)]
pub struct PayoutLedger {
    owed: HashMap<u64, u64>,
    paid: HashMap<u64, u64>,
    fees: u64,
}

impl PayoutLedger {
    /// Credit one block's net value (what the pool's wallet received)
    /// pro-rata by `(miner, weight)` — the caller's PPLNS-window weights.
    /// The pool fee is skimmed first (floored); dust goes
    /// largest-remainder. Empty weights or a zero distributable leave
    /// everything in the operator fee pot (documented, never silent).
    pub fn credit_block(
        &mut self,
        total: u64,
        weights: &[(u64, u64)],
        fee_permyriad: u16,
    ) {
        let fee = total as u128 * u64::from(fee_permyriad) as u128 / FEE_DENOM_PERMYRIAD as u128;
        self.fees = self.fees.saturating_add(fee as u64);
        let distributable = total.saturating_sub(fee as u64);
        let mut rows: Vec<(u64, u64)> = weights.to_vec();
        rows.sort_by_key(|(id, _)| *id);
        let weight_total: u128 = rows.iter().map(|(_, w)| *w as u128).sum();
        if rows.is_empty() || weight_total == 0 || distributable == 0 {
            self.fees = self.fees.saturating_add(distributable);
            return;
        }
        let mut pays = vec![0u64; rows.len()];
        let mut fracs: Vec<(u128, u64)> = Vec::new();
        let mut assigned = 0u64;
        for (i, (_, w)) in rows.iter().enumerate() {
            let num = distributable as u128 * *w as u128;
            pays[i] = (num / weight_total) as u64;
            assigned += pays[i];
            fracs.push((num % weight_total, rows[i].0));
        }
        fracs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut dust = distributable - assigned;
        for (_, id) in fracs {
            if dust == 0 {
                break;
            }
            let idx = rows.iter().position(|(m, _)| *m == id).expect("id present");
            pays[idx] += 1;
            dust -= 1;
        }
        for ((id, _), pay) in rows.iter().zip(pays) {
            let e = self.owed.entry(*id).or_default();
            *e = e.saturating_add(pay);
        }
    }

    /// Credit an already-split distribution verbatim (jackpot/pot/league
    /// splits from the pool's own rules).
    pub fn credit_split(&mut self, payouts: &[(u64, u64)]) {
        for (id, amount) in payouts {
            let e = self.owed.entry(*id).or_default();
            *e = e.saturating_add(*amount);
        }
    }

    /// Currently owed to a miner.
    #[must_use]
    pub fn owed(&self, miner: u64) -> u64 {
        self.owed.get(&miner).copied().unwrap_or(0)
    }

    /// Lifetime paid out to a miner (via [`PayoutLedger::record_paid`]).
    #[must_use]
    pub fn paid(&self, miner: u64) -> u64 {
        self.paid.get(&miner).copied().unwrap_or(0)
    }

    /// Lifetime pool-fee take.
    #[must_use]
    pub fn fees(&self) -> u64 {
        self.fees
    }

    /// Balances at/above `threshold`, miner id ascending (payout candidates).
    #[must_use]
    pub fn select_due(&self, threshold: u64) -> Vec<(u64, u64)> {
        let mut due: Vec<(u64, u64)> = self
            .owed
            .iter()
            .filter(|(_, &v)| v >= threshold)
            .map(|(&id, &v)| (id, v))
            .collect();
        due.sort_by_key(|(id, _)| *id);
        due
    }

    /// Debit after a successful on-chain send (nothing debits on dry runs
    /// or send failures — balances only move when money does).
    pub fn record_paid(&mut self, miner: u64, amount: u64) {
        let owed = self.owed.get(&miner).copied().unwrap_or(0);
        let pay = amount.min(owed);
        self.owed.insert(miner, owed - pay);
        let e = self.paid.entry(miner).or_default();
        *e = e.saturating_add(pay);
    }
}

/// Base units → decimal coins string for `sendtoaddress`. Integer math
/// only — floats never touch money: `76_000_000 → "0.76000000"`.
#[must_use]
pub fn format_coins(v: u64) -> String {
    format!("{}.{:08}", v / COIN_BASE, v % COIN_BASE)
}

/// Transparent payout address shape: 34-char base58. The leading version
/// byte is chain-specific ('R' on Verus mainnets, 'L' on Lucky Lode —
/// distinct address version bytes are part of the rebrand checklist), so
/// both are accepted. Shape check only — the node is the final arbiter at
/// send time (it checks the network params and returns an error the
/// payout loop treats as a failed send).
#[must_use]
pub fn valid_payout_address(s: &str) -> bool {
    s.len() == 34
        && s.starts_with(['R', 'L'])
        && s.bytes().all(|b| {
            matches!(b, b'1'..=b'9' | b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'Z' | b'a'..=b'k' | b'm'..=b'z')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pro_rata_splits_with_dust_and_conserves() {
        let mut p = PayoutLedger::default();
        // 1:400, 2:200 → floors 667/333, dust 1 to the largest
        // remainder (miner 2: 400 vs miner 1: 200).
        p.credit_block(1001, &[(1, 400), (2, 200)], 0);
        assert_eq!(p.owed(1), 667);
        assert_eq!(p.owed(2), 334);
        assert_eq!(p.owed(1) + p.owed(2) + p.fees(), 1001);
        // A second block accumulates; miner order is id-sorted.
        p.credit_block(300, &[(2, 1)], 0);
        assert_eq!(p.owed(2), 634);
        // Conservation against the conservation sweep below covers totals.
    }

    #[test]
    fn fee_skimmed_first_floored() {
        let mut p = PayoutLedger::default();
        p.credit_block(10_000, &[(1, 1)], 100); // 1% fee
        assert_eq!(p.fees(), 100);
        assert_eq!(p.owed(1), 9_900);
        // Empty weights / zero value: everything lands in fees, nothing lost.
        p.credit_block(500, &[], 0);
        assert_eq!(p.fees(), 600);
        p.credit_block(0, &[(1, 1)], 0);
        assert_eq!(p.owed(1), 9_900);
    }

    #[test]
    fn threshold_selects_and_paid_debits() {
        let mut p = PayoutLedger::default();
        p.credit_split(&[(1, 5_000_000), (2, 500_000)]);
        assert_eq!(p.select_due(1_000_000), vec![(1, 5_000_000)]);
        // Partial payment debits only what moved.
        p.record_paid(1, 2_000_000);
        assert_eq!(p.owed(1), 3_000_000);
        assert_eq!(p.paid(1), 2_000_000);
        assert_eq!(p.select_due(1_000_000), vec![(1, 3_000_000)]);
        // Overpayment clamps to the owed balance (no negative money).
        p.record_paid(2, 999_999_999);
        assert_eq!((p.owed(2), p.paid(2)), (0, 500_000));
    }

    #[test]
    fn block_credits_conserve_exactly() {
        for &(total, fee) in &[(1u64, 0u16), (7, 100), (999, 333), (80_000_000, 50), (u64::MAX / 2, 0)] {
            let mut p = PayoutLedger::default();
            p.credit_block(total, &[(1, 3), (2, 2), (3, 1)], fee);
            let owed: u64 = [1, 2, 3].iter().map(|m| p.owed(*m)).sum();
            assert_eq!(owed + p.fees(), total, "total={total} fee={fee}");
            assert_eq!(p.select_due(0).iter().map(|(_, v)| v).sum::<u64>(), owed);
        }
    }

    #[test]
    fn coins_format_is_integer_exact() {
        assert_eq!(format_coins(0), "0.00000000");
        assert_eq!(format_coins(1), "0.00000001");
        assert_eq!(format_coins(76_000_000), "0.76000000");
        assert_eq!(format_coins(100_000_000), "1.00000000");
        assert_eq!(format_coins(12_345_678_901_234), "123456.78901234");
        assert_eq!(format_coins(u64::MAX), "184467440737.09551615");
    }

    #[test]
    fn address_shape_check() {
        let addr = |first: char, rest: char, n: usize| -> String {
            format!("{}{}", first, &rest.to_string().repeat(n))
        };
        assert!(valid_payout_address(&addr('R', 'a', 33)));
        assert!(valid_payout_address(&addr('L', 'a', 33))); // Lucky Lode prefix
        assert!(valid_payout_address("LYPJ8Vy8pEHMqMBksWzgxnZx6VsfuFXxtf")); // live regtest addr
        assert!(!valid_payout_address("t1short"));
        assert!(!valid_payout_address("")); // empty
        assert!(!valid_payout_address(&addr('R', '0', 33))); // 0 not base58
        assert!(!valid_payout_address(&addr('R', 'O', 33))); // O not base58
        assert!(!valid_payout_address(&addr('Z', 'a', 33))); // wrong prefix
        assert!(!valid_payout_address(&addr('R', 'a', 32))); // short
        assert!(!valid_payout_address(&addr('R', 'a', 34))); // long
    }
}
