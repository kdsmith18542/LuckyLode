//! Base-unit money helpers: one definition of a coin and one formatting
//! rule for Rust-side display (the JS UIs keep their own formatters, but
//! they consume the same base-unit API).

/// Base units per coin (1 LKYL = 100,000,000).
pub const COIN_BASE: u64 = 100_000_000;

/// Base units → exact decimal string with 8 decimals (no trailing-zero
/// trimming: `format_sats` is the canonical wire form).
#[must_use]
pub fn format_sats(v: u64) -> String {
    format!("{}.{:08}", v / COIN_BASE, v % COIN_BASE)
}

/// Signed variant that keeps the minus for sub-coin negatives (display
/// paths: address flows, fee deltas).
#[must_use]
pub fn format_sats_i64(v: i64) -> String {
    let sign = if v < 0 { "-" } else { "" };
    let a = v.unsigned_abs();
    format!("{sign}{}.{:08}", a / COIN_BASE, a % COIN_BASE)
}

/// Parse a decimal coin amount (`"1"`, `"0.5"`, `"1.23456789"`) into base
/// units. Returns `None` for junk, negatives, or more than 8 decimals —
/// callers treat that as a usage error rather than silently clearing.
#[must_use]
pub fn parse_coins(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if frac.len() > 8 {
        return None;
    }
    let whole: u64 = whole.parse().ok()?;
    let mut base = whole.checked_mul(COIN_BASE)?;
    if !frac.is_empty() {
        let padded = format!("{frac:0<8}");
        base = base.checked_add(padded.parse::<u64>().ok()?)?;
    }
    Some(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_exact_and_sign_safe() {
        assert_eq!(format_sats(0), "0.00000000");
        assert_eq!(format_sats(COIN_BASE), "1.00000000");
        assert_eq!(format_sats(50_000_000), "0.50000000");
        assert_eq!(format_sats_i64(-50_000_000), "-0.50000000");
        assert_eq!(format_sats_i64(-1), "-0.00000001");
        assert_eq!(format_sats_i64(100_000_000), "1.00000000");
    }

    #[test]
    fn parse_roundtrips_and_rejects_junk() {
        for s in ["0", "0.5", "1", "1.23456789", "16.00000001", "  2.25  "] {
            let v = parse_coins(s).unwrap_or_else(|| panic!("parse {s}"));
            assert!(format_sats(v).parse::<f64>().unwrap() >= 0.0);
        }
        assert_eq!(parse_coins("0.5"), Some(50_000_000));
        assert_eq!(parse_coins("1.00000001"), Some(100_000_001));
        assert_eq!(parse_coins("2.25"), Some(225_000_000));
        for bad in ["", "x", "-1", "1.123456789", "1,5", "1e3", "NaN"] {
            assert_eq!(parse_coins(bad), None, "{bad}");
        }
        assert_eq!(parse_coins("18446744073709551616"), None, "overflow");
    }
}
