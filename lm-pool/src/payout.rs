//! Per-miner value accounting: who is owed what.
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
        // Row index by id once (the dust walk was O(n) per unit).
        let row_index: std::collections::HashMap<&str, usize> = rows
            .iter()
            .enumerate()
            .map(|(i, (m, _))| (m.as_str(), i))
            .collect();
        for (_, id) in fracs {
            if dust == 0 {
                break;
            }
            pays[*row_index.get(id.as_str()).expect("id present")] += 1;
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
/// Base58check decode: 25 bytes (version byte, 20-byte hash160,
/// 4-byte sha256d checksum), checksum-verified. `b58check_ok` is this
/// plus the length check.
fn b58check_decode(s: &str) -> Option<[u8; 25]> {
    const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut num: Vec<u8> = Vec::with_capacity(25);
    for c in s.bytes() {
        let idx = ALPHABET.iter().position(|&a| a == c)?;
        let mut carry = idx as u32;
        for byte in num.iter_mut() {
            carry += u32::from(*byte) * 58;
            *byte = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            num.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let leading = s.bytes().take_while(|&b| b == b'1').count();
    let mut full = vec![0u8; leading];
    full.extend(num.iter().rev());
    if full.len() != 25 {
        return None;
    }
    use sha2::{Digest, Sha256};
    let checksum = Sha256::digest(Sha256::digest(&full[..21]));
    if full[21..] != checksum[..4] {
        return None;
    }
    let mut out = [0u8; 25];
    out.copy_from_slice(&full);
    Some(out)
}

/// Base58check validity: decodes to 25 bytes (a version byte, a 20-byte
/// hash160, and a 4-byte sha256d checksum) and verifies the checksum.
/// Shape alone let a typo hold a balance forever; the checksum is what
/// catches it.
///
/// The version byte is NOT constrained here — the prefix filter above
/// is the chain policy; this only proves the checksum.
fn b58check_ok(s: &str) -> bool {
    b58check_decode(s).is_some()
}

/// Expected locking script (hex) for one of our payout addresses, so the
/// pool can verify a designated bond UTXO actually pays the registered
/// address (and not some unrelated output). P2PKH for R/L/m/n prefixes,
/// P2SH for the `2...` multisig prefix — the same prefix set as
/// [`valid_payout_address`]. Returns None for anything else (fail closed).
#[must_use]
pub fn payout_script_pubkey(address: &str) -> Option<String> {
    if !valid_payout_address(address) {
        return None;
    }
    let raw = b58check_decode(address)?;
    let hash160: String = raw[1..21].iter().map(|b| format!("{b:02x}")).collect();
    match address.as_bytes().first() {
        Some(b'2') => Some(format!("a914{hash160}87")),
        _ => Some(format!("76a914{hash160}88ac")),
    }
}

/// Bond-UTXO binding check: the designated output must pay the registered
/// address (script match), hold at least the bond (satoshis), and be
/// unspent. Pure function over node-verified fields so the tick and the
/// registration path share one rule; both fail closed on any mismatch.
#[must_use]
pub fn bond_utxo_matches(expected_script_hex: &str, utxo_script_hex: &str, value_sats: u64, bond_sats: u64) -> bool {
    !expected_script_hex.is_empty()
        && expected_script_hex.eq_ignore_ascii_case(utxo_script_hex)
        && value_sats >= bond_sats
}

/// Payout-address validity: our chains' prefixes, then FULL base58check.
/// Lengths 33-35 all occur (the leading-zero pattern of the version byte
/// decides); the checksum is the real gate.
pub fn valid_payout_address(s: &str) -> bool {
    (33..=35).contains(&s.len())
        && s.starts_with(['R', 'L', 'm', 'n', '2'])
        && s.bytes().all(|b| {
            matches!(b, b'1'..=b'9' | b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'Z' | b'a'..=b'k' | b'm'..=b'z')
        })
        && b58check_ok(s)
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
        // Real base58check vectors (live chain addresses + generated).
        assert!(valid_payout_address("LYPJ8Vy8pEHMqMBksWzgxnZx6VsfuFXxtf")); // regtest L (v48)
        assert!(valid_payout_address("n4EijXmFTNf7tMDGYSQB7rZytCCmRttZb6")); // live testnet
        assert!(valid_payout_address("miN8TqxhnviDF21vd9LgubFgcZxwbcznuK")); // live testnet
        assert!(valid_payout_address("mx5tDPWsWxhC7EVkJXGCR14dUoMdps8fLy")); // live testnet
        assert!(valid_payout_address("mfWyW5fc9NUj75YAnFgoRLrjxgLDn2MMth")); // generated, v111
        assert!(valid_payout_address("2MstP8uJfYBsk1HfmCU9MW3qqKgvrw3ESd1")); // '2' P2SH, 35 chars
        // The checksum is the gate: same shape, wrong checksum — this is
        // the typo that used to hold a balance forever.
        assert!(!valid_payout_address("mfWyW5fc9NUj75YAnFgoRLrjxgLDmNrbUb"));
        assert!(!valid_payout_address("LYPJ8Vy8pEHMqMBksWzgxnZx6VsfuFXxtg")); // last char broken
        assert!(!valid_payout_address("t1short"));
        assert!(!valid_payout_address("")); // empty
        assert!(!valid_payout_address(&format!("R{}", "a".repeat(33)))); // shape-only, no checksum
        assert!(!valid_payout_address(&format!("R{}", "0".repeat(33)))); // 0 not base58
        assert!(!valid_payout_address(&format!("Z{}", "a".repeat(33)))); // wrong prefix
        assert!(!valid_payout_address(&format!("R{}", "a".repeat(31)))); // too short
        assert!(!valid_payout_address(&format!("R{}", "a".repeat(36)))); // too long
    }

    #[test]
    fn payout_script_matches_chain_vectors() {
        // Frozen vectors, decoded independently (Python hashlib): the pool
        // must derive the exact locking script the node will check, or the
        // bond-UTXO binding compares against the wrong bytes.
        assert_eq!(
            payout_script_pubkey("mxVc37AiDBhP4FcydkCu5AEZqgfMYy8hC9"),
            Some("76a914ba38cfe49e436bb2a43898083a98becc148c204488ac".to_string())
        );
        assert_eq!(
            payout_script_pubkey("2NFdM6aq2Rtr444aGVrE9CZXLzZ2JF8dadE"),
            Some("a914f581da35b024c755572850ad3a9d6f5bd8b1afcc87".to_string())
        );
        assert_eq!(
            payout_script_pubkey("2MstP8uJfYBsk1HfmCU9MW3qqKgvrw3ESd1").map(|s| s[..4].to_string()),
            Some("a914".to_string())
        );
        // Junk never yields a script (fail closed, never a match).
        assert_eq!(payout_script_pubkey("t1short"), None);
        assert_eq!(payout_script_pubkey(""), None);
        assert_eq!(payout_script_pubkey("mfWyW5fc9NUj75YAnFgoRLrjxgLDmNrbUb"), None);
    }

    #[test]
    fn bond_utxo_binding_needs_script_and_value() {
        let script = "76a914ba38cfe49e436bb2a43898083a98becc148c204488ac";
        // Exact match at/above the bond passes.
        assert!(bond_utxo_matches(script, script, 40_000_000_000, 40_000_000_000));
        assert!(bond_utxo_matches(script, script, 40_000_000_001, 40_000_000_000));
        // Case-insensitive hex (node returns lowercase; be liberal here).
        assert!(bond_utxo_matches(&script.to_uppercase(), script, 40_000_000_000, 40_000_000_000));
        // Dust designated as "locked" fails: value below the bond.
        assert!(!bond_utxo_matches(script, script, 1, 40_000_000_000));
        // Someone else's UTXO fails: script pays a different address.
        assert!(!bond_utxo_matches(script, "76a914000000000000000000000000000000000000000088ac", 50_000_000_000, 40_000_000_000));
        // Empty script never matches.
        assert!(!bond_utxo_matches("", script, 50_000_000_000, 40_000_000_000));
        assert!(!bond_utxo_matches(script, "", 50_000_000_000, 40_000_000_000));
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
}
