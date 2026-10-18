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

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PayoutLedger {
    owed: HashMap<String, u64>,
    paid: HashMap<String, u64>,
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
        weights: &[(String, u64)],
        fee_permyriad: u16,
    ) {
        let fee = total as u128 * u64::from(fee_permyriad) as u128 / FEE_DENOM_PERMYRIAD as u128;
        self.fees = self.fees.saturating_add(fee as u64);
        let distributable = total.saturating_sub(fee as u64);
        let mut rows: Vec<(String, u64)> = weights.to_vec();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let weight_total: u128 = rows.iter().map(|(_, w)| *w as u128).sum();
        if rows.is_empty() || weight_total == 0 || distributable == 0 {
            self.fees = self.fees.saturating_add(distributable);
            return;
        }
        let mut pays = vec![0u64; rows.len()];
        let mut fracs: Vec<(u128, String)> = Vec::new();
        let mut assigned = 0u64;
        for (i, (_, w)) in rows.iter().enumerate() {
            let num = distributable as u128 * *w as u128;
            pays[i] = (num / weight_total) as u64;
            assigned += pays[i];
            fracs.push((num % weight_total, rows[i].0.clone()));
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
            let e = self.owed.entry(id.clone()).or_default();
            *e = e.saturating_add(pay);
        }
    }

    /// Credit an already-split distribution verbatim (jackpot/pot/league
    /// splits from the pool's own rules).
    pub fn credit_split(&mut self, payouts: &[(String, u64)]) {
        for (id, amount) in payouts {
            let e = self.owed.entry(id.clone()).or_default();
            *e = e.saturating_add(*amount);
        }
    }

    /// Move up to `amount` from the operator fee pot to a miner's owed
    /// balance (the reference-pool dev-fee slice, docs/04 §4.2). Capped by
    /// what's actually in the pot — a zero pool fee means zero dev accrual,
    /// never a silent miner haircut. Returns the amount moved.
    pub fn allocate_dev(&mut self, dev_id: &str, amount: u64) -> u64 {
        let take = amount.min(self.fees);
        if take > 0 {
            self.fees -= take;
            let e = self.owed.entry(dev_id.to_string()).or_default();
            *e = e.saturating_add(take);
        }
        take
    }

    /// Currently owed to a miner.
    #[must_use]
    pub fn owed(&self, miner: &str) -> u64 {
        self.owed.get(miner).copied().unwrap_or(0)
    }

    /// Lifetime paid out to a miner (via [`PayoutLedger::record_paid`]).
    #[must_use]
    pub fn paid(&self, miner: &str) -> u64 {
        self.paid.get(miner).copied().unwrap_or(0)
    }

    /// Lifetime pool-fee take.
    #[must_use]
    pub fn fees(&self) -> u64 {
        self.fees
    }

    /// `(owed_total, paid_total, fees)` across every miner — the
    /// reconciliation triple: block credits == owed_total + paid_total +
    /// fees. The per-miner `paid` map and the capped `/api/stats`
    /// history cannot reconstruct this (a fully-paid miner drops out of
    /// the owed map), so expose the sums directly.
    #[must_use]
    pub fn totals(&self) -> (u64, u64, u64) {
        (
            self.owed.values().copied().sum(),
            self.paid.values().copied().sum(),
            self.fees,
        )
    }

    /// Balances at/above `threshold`, miner id ascending (payout candidates).
    #[must_use]
    pub fn select_due(&self, threshold: u64) -> Vec<(String, u64)> {
        let mut due: Vec<(String, u64)> = self
            .owed
            .iter()
            .filter(|(_, &v)| v >= threshold)
            .map(|(id, &v)| (id.clone(), v))
            .collect();
        due.sort_by(|a, b| a.0.cmp(&b.0));
        due
    }

    /// Debit after a successful on-chain send (nothing debits on dry runs
    /// or send failures — balances only move when money does).
    pub fn record_paid(&mut self, miner: &str, amount: u64) {
        let owed = self.owed.get(miner).copied().unwrap_or(0);
        let pay = amount.min(owed);
        self.owed.insert(miner.to_string(), owed - pay);
        let e = self.paid.entry(miner.to_string()).or_default();
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
/// byte is chain-specific — 'R' (Verus mainnets), 'L' (Lucky Lode
/// mainnet), 'm'/'n'/'2' (Lucky Lode testnet, Bitcoin-style 111/196) — so
/// all are accepted. Shape check only — the node is the final arbiter at
/// send time (it checks the network params and returns an error the
/// payout loop treats as a failed send).
#[must_use]
pub fn valid_payout_address(s: &str) -> bool {
    s.len() == 34
        && s.starts_with(['R', 'L', 'm', 'n', '2'])
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
        p.credit_block(1001, &[("m1".to_string(), 400), ("m2".to_string(), 200)], 0);
        assert_eq!(p.owed("m1"), 667);
        assert_eq!(p.owed("m2"), 334);
        assert_eq!(p.owed("m1") + p.owed("m2") + p.fees(), 1001);
        // A second block accumulates; miner order is id-sorted.
        p.credit_block(300, &[("m2".to_string(), 1)], 0);
        assert_eq!(p.owed("m2"), 634);
        // Conservation against the conservation sweep below covers totals.
    }

    #[test]
    fn fee_skimmed_first_floored() {
        let mut p = PayoutLedger::default();
        p.credit_block(10_000, &[("m1".to_string(), 1)], 100); // 1% fee
        assert_eq!(p.fees(), 100);
        assert_eq!(p.owed("m1"), 9_900);
        // Empty weights / zero value: everything lands in fees, nothing lost.
        p.credit_block(500, &[], 0);
        assert_eq!(p.fees(), 600);
        p.credit_block(0, &[("m1".to_string(), 1)], 0);
        assert_eq!(p.owed("m1"), 9_900);
    }

    #[test]
    fn ledger_survives_a_json_round_trip() {
        // Pool restart persistence (--state-file): the exact ledger must
        // come back byte-for-byte.
        let mut p = PayoutLedger::default();
        p.credit_block(7_000_000, &[("m1".to_string(), 3), ("m2".to_string(), 1)], 500);
        p.record_paid("m1", 1_000_000);
        let json = serde_json::to_string(&p).expect("serialize");
        let back: PayoutLedger = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.owed("m1"), p.owed("m1"));
        assert_eq!(back.owed("m2"), p.owed("m2"));
        assert_eq!(back.paid("m1"), p.paid("m1"));
        assert_eq!(back.fees(), p.fees());
        assert_eq!(back.totals(), p.totals());
    }

    #[test]
    fn totals_reconcile_credits_owed_paid_and_fees() {
        let mut p = PayoutLedger::default();
        p.credit_block(1_000_000, &[("m1".to_string(), 1), ("m2".to_string(), 1)], 0);
        p.credit_block(500_000, &[("m1".to_string(), 1)], 1_000); // 10% fee
        p.record_paid("m1", 600_000);
        let (owed, paid, fees) = p.totals();
        assert_eq!(paid, 600_000);
        assert_eq!(fees, 50_000);
        assert_eq!(owed + paid + fees, 1_500_000, "every credited unit is owed, paid, or fee");
        // A fully paid-out miner still counts in the totals; overpay
        // attempts are capped at what is owed (2 only had 500k).
        p.record_paid("m2", 1_000_000);
        let (owed2, paid2, _) = p.totals();
        assert_eq!(paid2, 1_100_000);
        assert_eq!(owed2, 350_000);
        assert_eq!(owed2 + paid2 + 50_000, 1_500_000);
    }

    #[test]
    fn threshold_selects_and_paid_debits() {
        let mut p = PayoutLedger::default();
        p.credit_split(&[("m1".to_string(), 5_000_000), ("m2".to_string(), 500_000)]);
        assert_eq!(p.select_due(1_000_000), vec![("m1".to_string(), 5_000_000)]);
        // Partial payment debits only what moved.
        p.record_paid("m1", 2_000_000);
        assert_eq!(p.owed("m1"), 3_000_000);
        assert_eq!(p.paid("m1"), 2_000_000);
        assert_eq!(p.select_due(1_000_000), vec![("m1".to_string(), 3_000_000)]);
        // Overpayment clamps to the owed balance (no negative money).
        p.record_paid("m2", 999_999_999);
        assert_eq!((p.owed("m2"), p.paid("m2")), (0, 500_000));
    }

    #[test]
    fn block_credits_conserve_exactly() {
        for &(total, fee) in &[(1u64, 0u16), (7, 100), (999, 333), (80_000_000, 50), (u64::MAX / 2, 0)] {
            let mut p = PayoutLedger::default();
            p.credit_block(total, &[("m1".to_string(), 3), ("m2".to_string(), 2), ("m3".to_string(), 1)], fee);
            let owed: u64 = ["m1", "m2", "m3"].iter().map(|m| p.owed(m)).sum();
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
        assert!(valid_payout_address("n4EijXmFTNf7tMDGYSQB7rZytCCmRttZb6")); // live testnet addr
        assert!(valid_payout_address(&addr('m', 'a', 33))); // testnet prefix
        assert!(valid_payout_address(&addr('n', 'a', 33))); // testnet prefix
        assert!(!valid_payout_address("t1short"));
        assert!(!valid_payout_address("")); // empty
        assert!(!valid_payout_address(&addr('R', '0', 33))); // 0 not base58
        assert!(!valid_payout_address(&addr('R', 'O', 33))); // O not base58
        assert!(!valid_payout_address(&addr('Z', 'a', 33))); // wrong prefix
        assert!(!valid_payout_address(&addr('R', 'a', 32))); // short
        assert!(!valid_payout_address(&addr('R', 'a', 34))); // long
    }
}

    #[test]
    fn dev_slice_moves_from_fees_not_miners() {
        let mut p = PayoutLedger::default();
        p.credit_block(10_000, &[("m1".to_string(), 1)], 200); // 2% pool fee = 200
        assert_eq!(p.fees(), 200);
        assert_eq!(p.owed("m1"), 9_800);
        // Dev takes 50 (0.5pp of the block) out of the operator's 200.
        assert_eq!(p.allocate_dev("m99", 50), 50);
        assert_eq!(p.fees(), 150);
        assert_eq!(p.owed("m99"), 50);
        assert_eq!(p.owed("m1"), 9_800); // miners untouched
        assert_eq!(p.owed("m1") + p.owed("m99") + p.fees(), 10_000);
        // Capped by the pot: asking for more moves only what's there.
        assert_eq!(p.allocate_dev("m99", 500), 150);
        assert_eq!(p.fees(), 0);
        assert_eq!(p.owed("m99"), 200);
    }

    #[test]
    fn dev_slice_empty_when_pool_fee_zero() {
        let mut p = PayoutLedger::default();
        p.credit_block(10_000, &[("m1".to_string(), 1)], 0);
        assert_eq!(p.allocate_dev("m99", 50), 0);
        assert_eq!(p.owed("m99"), 0);
        assert_eq!(p.owed("m1"), 10_000);
    }
