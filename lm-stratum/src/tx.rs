//! Minimal transaction parse/serialize for the J1 bonus template.
//!
//! The pool claims a jackpot by appending one output to the node's
//! coinbase (the node's ceiling allows `normal + bonus` on jackpot blocks,
//! and the coinbase is the only output the pool controls before the merkle
//! is fixed). That needs exactly two operations: locate the vout section
//! and insert an output. Strictness is deliberate — anything malformed is
//! refused rather than half-parsed, because these bytes become a block.
//!
//! Scope: legacy transparent transactions only (v1, no overwinter/sapling
//! flags) — which is what the node's coinbase is. Anything with the
//! overwinter flag bit set is rejected.

/// A parsed coinbase: offsets into the original bytes, enough to splice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoinbaseLayout {
    /// Offset of the vout count CompactSize byte.
    pub vout_count_at: usize,
    /// Offset just past the last vout (where locktime begins).
    pub vouts_end: usize,
    /// The declared vout count.
    pub vout_count: u64,
    /// Script of the last output (the payout destination we mirror).
    pub last_script: Vec<u8>,
    /// Value of the last output.
    pub last_value: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TxError {
    Truncated,
    UnsupportedFlags,
    BadCompactSize,
    TrailingBytes,
}

fn read_compact(b: &[u8], p: &mut usize) -> Result<u64, TxError> {
    let v = *b.get(*p).ok_or(TxError::Truncated)?;
    *p += 1;
    match v {
        0..=0xfc => Ok(v as u64),
        0xfd => {
            let s = b.get(*p..*p + 2).ok_or(TxError::Truncated)?;
            *p += 2;
            Ok(u16::from_le_bytes([s[0], s[1]]) as u64)
        }
        0xfe => {
            let s = b.get(*p..*p + 4).ok_or(TxError::Truncated)?;
            *p += 4;
            Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as u64)
        }
        0xff => {
            let s = b.get(*p..*p + 8).ok_or(TxError::Truncated)?;
            *p += 8;
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            Ok(u64::from_le_bytes(a))
        }
    }
}

fn write_compact(n: u64, out: &mut Vec<u8>) {
    if n < 0xfd {
        out.push(n as u8);
    } else if n <= 0xffff {
        out.push(0xfd);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= 0xffff_ffff {
        out.push(0xfe);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    } else {
        out.push(0xff);
        out.extend_from_slice(&n.to_le_bytes());
    }
}

/// Parse a legacy transparent transaction far enough to find its vout
/// section. Rejects flag-bearing (overwinter/sapling) transactions: the
/// pool must never splice into bytes it cannot fully account for.
pub fn parse_coinbase_layout(tx: &[u8]) -> Result<CoinbaseLayout, TxError> {
    let mut p = 0usize;
    let ver = tx.get(p..p + 4).ok_or(TxError::Truncated)?;
    p += 4;
    let version = u32::from_le_bytes([ver[0], ver[1], ver[2], ver[3]]);
    if version & 0x8000_0000 != 0 {
        return Err(TxError::UnsupportedFlags);
    }
    let vin_count = read_compact(tx, &mut p)?;
    for _ in 0..vin_count {
        p += 36;
        if p > tx.len() {
            return Err(TxError::Truncated);
        }
        let script_len = read_compact(tx, &mut p)? as usize;
        p = p.checked_add(script_len).ok_or(TxError::Truncated)?;
        p += 4; // sequence
        if p > tx.len() {
            return Err(TxError::Truncated);
        }
    }
    let vout_count_at = p;
    let vout_count = read_compact(tx, &mut p)?;
    let mut last_script = Vec::new();
    let mut last_value = 0i64;
    for _ in 0..vout_count {
        let v = tx.get(p..p + 8).ok_or(TxError::Truncated)?;
        last_value = i64::from_le_bytes(v.try_into().expect("8"));
        p += 8;
        let script_len = read_compact(tx, &mut p)? as usize;
        let script = tx.get(p..p + script_len).ok_or(TxError::Truncated)?;
        last_script = script.to_vec();
        p += script_len;
    }
    let vouts_end = p;
    // locktime must complete the tx exactly
    let lock = tx.get(p..p + 4).ok_or(TxError::Truncated)?;
    let _ = u32::from_le_bytes(lock.try_into().expect("4"));
    if p + 4 != tx.len() {
        return Err(TxError::TrailingBytes);
    }
    Ok(CoinbaseLayout {
        vout_count_at,
        vouts_end,
        vout_count,
        last_script,
        last_value,
    })
}

/// Sum of all output values (u64, saturating). The pool uses it to learn
/// the block's net value from the node's own template bytes — what the
/// pool's receipt will be — without trusting any advisory field.
pub fn coinbase_outputs_total(tx: &[u8]) -> Result<u64, TxError> {
    let layout = parse_coinbase_layout(tx)?;
    let mut p = layout.vout_count_at;
    let _ = read_compact(tx, &mut p)?;
    let mut total = 0u64;
    for _ in 0..layout.vout_count {
        let v = tx.get(p..p + 8).ok_or(TxError::Truncated)?;
        let value = u64::from_le_bytes(v.try_into().expect("8"));
        total = total.saturating_add(value);
        p += 8;
        let script_len = read_compact(tx, &mut p)? as usize;
        p = p.checked_add(script_len).ok_or(TxError::Truncated)?;
        if p > tx.len() {
            return Err(TxError::Truncated);
        }
    }
    Ok(total)
}

/// All output scripts in vout order (for the prize-claim collision
/// check: the directed-sum rule counts every output paying the winner's
/// script, so the pool must not append a claim when the script is
/// already paid — same builder rule as the internal miner).
pub fn output_scripts(tx: &[u8]) -> Result<Vec<Vec<u8>>, TxError> {
    let layout = parse_coinbase_layout(tx)?;
    let mut p = layout.vout_count_at;
    let n = read_compact(tx, &mut p)?;
    debug_assert_eq!(n, layout.vout_count);
    let mut out = Vec::with_capacity(layout.vout_count as usize);
    for _ in 0..layout.vout_count {
        let v = tx.get(p..p + 8).ok_or(TxError::Truncated)?;
        let _ = u64::from_le_bytes(v.try_into().map_err(|_| TxError::Truncated)?);
        p += 8;
        let script_len = read_compact(tx, &mut p)? as usize;
        let script = tx.get(p..p + script_len).ok_or(TxError::Truncated)?;
        out.push(script.to_vec());
        p += script_len;
    }
    Ok(out)
}

/// Append one output (`value`, same script as the last output) and fix the
/// count. Only valid for counts < 0xfd (same CompactSize width); the
/// coinbase never approaches that, and we refuse rather than mis-encode.
pub fn append_output(tx: &[u8], value: i64, script: &[u8]) -> Result<Vec<u8>, TxError> {
    let layout = parse_coinbase_layout(tx)?;
    if layout.vout_count >= 0xfd {
        return Err(TxError::BadCompactSize);
    }
    let mut out = Vec::with_capacity(tx.len() + 8 + 1 + script.len() + 1);
    out.extend_from_slice(&tx[..layout.vout_count_at]);
    write_compact(layout.vout_count + 1, &mut out);
    out.extend_from_slice(&tx[layout.vout_count_at + 1..layout.vouts_end]);
    out.extend_from_slice(&value.to_le_bytes());
    write_compact(script.len() as u64, &mut out);
    out.extend_from_slice(script);
    out.extend_from_slice(&tx[layout.vouts_end..]);
    // Re-parse to prove the splice is self-consistent.
    let check = parse_coinbase_layout(&out)?;
    debug_assert_eq!(check.vout_count, layout.vout_count + 1);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real regtest coinbase from the frozen GBT fixture (98 bytes).
    const CB_HEX: &str = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff03550101ffffffff01c3a4220100000000232103f9c81821efd89ad66e9fc56e0a3d870aa8ce9a424396cb5c856fb6a609977666ac67d7cb6a";

    fn hexd(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn parses_the_real_coinbase() {
        let cb = hexd(CB_HEX);
        let l = parse_coinbase_layout(&cb).expect("parse");
        assert_eq!(l.vout_count, 1);
        assert_eq!(l.vouts_end + 4, cb.len(), "locktime ends the tx");
        assert_eq!(l.last_value, 0x01_22a4c3);
        assert_eq!(l.last_script.len(), 35); // P2PK: PUSH33 || pubkey || CHECKSIG
    }

    #[test]
    fn outputs_total_sums_all_vouts() {
        let cb = hexd(CB_HEX);
        // Single vout: total == last value (0x01_22a4c3).
        assert_eq!(coinbase_outputs_total(&cb).expect("sum"), 0x01_22a4c3);
        // Two vouts: fixture value + spliced bonus, exactly.
        let l = parse_coinbase_layout(&cb).expect("parse");
        let with_bonus = append_output(&cb, 1_142_856, &l.last_script).expect("append");
        assert_eq!(
            coinbase_outputs_total(&with_bonus).expect("sum2"),
            0x01_22a4c3 + 1_142_856
        );
    }

    #[test]
    fn append_output_splices_exactly() {
        let cb = hexd(CB_HEX);
        let l1 = parse_coinbase_layout(&cb).unwrap();
        let bonus_script = l1.last_script.clone();
        let with_bonus = append_output(&cb, 1_142_856, &bonus_script).expect("append");
        // count went 1 -> 2
        let l2 = parse_coinbase_layout(&with_bonus).expect("reparse");
        assert_eq!(l2.vout_count, 2);
        assert_eq!(l2.last_value, 1_142_856);
        assert_eq!(l2.last_script, bonus_script);
        // everything before the count byte is byte-identical (splice only
        // touches the count and the tail)
        assert_eq!(&with_bonus[..l2.vout_count_at], &cb[..l1.vout_count_at]);
        // the first output is preserved verbatim between count and bonus
        assert_eq!(
            &with_bonus[l2.vout_count_at + 1
                ..l2.vout_count_at + 1 + (l1.vouts_end - l1.vout_count_at - 1)],
            &cb[l1.vout_count_at + 1..l1.vouts_end]
        );
        assert_eq!(&with_bonus[l2.vouts_end..], &cb[l1.vouts_end..]);
        // size delta = 8 (value) + 1 (script len) + script
        assert_eq!(with_bonus.len(), cb.len() + 8 + 1 + bonus_script.len());
        // txid changes (it must — the merkle depends on it)
        assert_ne!(
            crate::node_provider::txid(&cb),
            crate::node_provider::txid(&with_bonus)
        );
    }

    #[test]
    fn rejects_flagged_and_truncated() {
        let mut cb = hexd(CB_HEX);
        cb[3] = 0x80; // overwinter flag bit
        assert_eq!(parse_coinbase_layout(&cb), Err(TxError::UnsupportedFlags));
        let cb = hexd(CB_HEX);
        assert_eq!(parse_coinbase_layout(&cb[..50]), Err(TxError::Truncated));
        let mut cb = hexd(CB_HEX);
        cb.push(0x00); // trailing byte
        assert_eq!(parse_coinbase_layout(&cb), Err(TxError::TrailingBytes));
    }
}
