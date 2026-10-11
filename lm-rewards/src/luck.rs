//! EXPLORATORY — multi-class luck table (doc 12 §12.10). This is *not*
//! wired into any consensus path: adoption requires a doc 06 revision
//! (rev 1.1: freeze, reserve resizing, per-class gates) before this module
//! leaves exploration. Prototype comparisons use the top-64-bit draw
//! shortcut (bias ≤ 2⁻⁶⁴, fine for simulation); the consensus version
//! compares full 256-bit `out < floor(T/odds)` exactly like doc 06 §6.1.
//!
//! **Rarest-wins partitioning changes the conservation identity.** A draw
//! that clears several nested thresholds pays only the rarest class, so
//! probabilities partition: a class with odds `N` and next-rarer odds `P`
//! wins with probability `1/N − 1/P` — *not* `1/N`. The table is solved
//! against partition weights in "millis × per-100k-blocks":
//!
//! ```text
//! c_i = 100_000 / odds_i            (c_0 = 0)
//! w_i = c_i − c_{i−1}               (partition width)
//! Σᵢ bonus_millisᵢ × w_i  ==  50 × 100_000  ==  5_000_000
//! ```
//!
//! (50 millis/block = the 5% levy per block; `bonus_millis` 1000 = 1×R.)
//! All `odds` divide 100_000, so every term is integer-exact. Rarest class
//! first wins when a draw clears several thresholds.

/// One rarity class: `1/odds` chance per block, paying `base × bonus_millis/1000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LuckClass {
    pub name: &'static str,
    pub odds: u64,
    pub bonus_millis: u64,
}

/// Ten classes, rarest first. Partition-weights × bonuses sum to the full 5% levy.
pub const LUCK_TABLE: [LuckClass; 10] = [
    LuckClass { name: "mythic",  odds: 100_000, bonus_millis: 225_000 },
    LuckClass { name: "aurora",  odds: 10_000,  bonus_millis: 25_000 },
    LuckClass { name: "comet",   odds: 2_500,   bonus_millis: 12_500 },
    LuckClass { name: "beacon",  odds: 1_000,   bonus_millis: 6_250 },
    LuckClass { name: "bonfire", odds: 500,     bonus_millis: 5_000 },
    LuckClass { name: "cinder",  odds: 250,     bonus_millis: 2_500 },
    LuckClass { name: "ember",   odds: 100,     bonus_millis: 1_000 },
    LuckClass { name: "flare",   odds: 50,      bonus_millis: 500 },
    LuckClass { name: "glint",   odds: 25,      bonus_millis: 250 },
    LuckClass { name: "spark",   odds: 10,      bonus_millis: 200 },
];

/// Partition width of class `i` per 100,000 blocks (`c_i − c_{i−1}`).
#[must_use]
pub fn partition_weight(i: usize) -> u64 {
    let c = 100_000 / LUCK_TABLE[i].odds;
    let prev = if i == 0 { 0 } else { 100_000 / LUCK_TABLE[i - 1].odds };
    c - prev
}

/// Conservation identity: Σ bonus_millis × partition_weight. Must equal
/// `LEVY_PERMYRIAD/100 × 100_000` = 5,000,000 (integer-exact).
#[must_use]
pub fn table_conservation() -> u128 {
    LUCK_TABLE
        .iter()
        .enumerate()
        .map(|(i, c)| c.bonus_millis as u128 * partition_weight(i) as u128)
        .sum()
}

/// Classify a top-64-bit draw (0 = best). Rarest match wins; `None` when
/// the draw clears no threshold (the ~84% common case incl. non-wins).
#[must_use]
pub fn classify(draw_top64: u64) -> Option<&'static LuckClass> {
    LUCK_TABLE
        .iter()
        .find(|c| draw_top64 < u64::MAX / c.odds)
}

/// Bonus paid for a class at base reward `R`: floor(R × millis / 1000),
/// saturating. (Same floor discipline as the rest of the money paths.)
#[must_use]
pub fn bonus_for(base: u64, class: &LuckClass) -> u64 {
    base.saturating_mul(class.bonus_millis) / 1000
}

/// Expected bonus per block in millis of R, under rarest-wins partitioning:
/// Σ bonus_millis × partition_weight / 100_000. Exactly 50 (5% of R) for a
/// conserving table.
#[must_use]
pub fn expected_millis_per_block() -> f64 {
    table_conservation() as f64 / 100_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_exactly_conserving() {
        // 5,000,000 == 50 millis/block × 100,000 blocks. Integer-exact.
        assert_eq!(table_conservation(), 5_000_000);
        assert!((expected_millis_per_block() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn rarest_wins_and_thresholds_bite() {
        assert_eq!(classify(0).expect("any draw wins something").name, "mythic");
        // Just inside the spark threshold but not glint.
        let spark_edge = u64::MAX / 25; // < MAX/10, >= MAX/25
        let c = classify(spark_edge).expect("spark");
        assert_eq!(c.name, "spark");
        // Above the easiest threshold: nothing.
        assert!(classify(u64::MAX / 5).is_none());
        assert!(classify(u64::MAX).is_none());
    }

    #[test]
    fn odds_boundaries_are_strict_and_ordered() {
        // Sorted rarest-first: odds strictly descending.
        for w in LUCK_TABLE.windows(2) {
            assert!(w[0].odds > w[1].odds, "{} !> {}", w[0].name, w[1].name);
        }
        // A draw exactly at a class threshold must NOT trigger it (strict <).
        let just_under_mythic = u64::MAX / 100_000;
        assert_ne!(classify(just_under_mythic).map(|c| c.name), Some("mythic"));
        assert_eq!(
            classify(just_under_mythic - 1).map(|c| c.name),
            Some("mythic")
        );
    }

    #[test]
    fn bonuses_scale_and_floor() {
        let mythic = &LUCK_TABLE[0];
        let spark = &LUCK_TABLE[9];
        assert_eq!(bonus_for(1_000_000, spark), 200_000); // 0.2 × R
        assert_eq!(bonus_for(1_000_000, mythic), 225_000_000); // 225 × R
        assert_eq!(bonus_for(999, spark), 199); // floor
        // Saturating multiply, then floor divide: MAX × 225000 saturates to
        // MAX, then /1000 → MAX/1000 (not MAX).
        assert_eq!(bonus_for(u64::MAX, mythic), u64::MAX / 1000);
    }

    #[test]
    fn simulation_matches_expectation() {
        // Deterministic xorshift draws: total bonus should land near the
        // theoretical 5% levy, and per-class hit counts near 1/odds.
        let seed = 0x5EED_1234_ABCD_9876u64;
        let mut x = seed;
        let mut next = || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let blocks = 200_000u64;
        let base = 1_000_000u64;
        let mut total_bonus: u128 = 0;
        let mut hits = [0u64; 10];
        for _ in 0..blocks {
            if let Some(c) = classify(next()) {
                let idx = LUCK_TABLE.iter().position(|t| t.name == c.name).unwrap();
                hits[idx] += 1;
                total_bonus += bonus_for(base, c) as u128;
            }
        }
        let expected = blocks as f64 * 0.05 * base as f64;
        let ratio = total_bonus as f64 / expected;
        assert!((0.85..=1.15).contains(&ratio), "ratio {ratio} hits {hits:?}");
        // Robust per-class checks: classes expected ≥ 40 hits in 200k
        // (partition-weighted, since rarest-wins partitions the draws).
        for (i, c) in LUCK_TABLE.iter().enumerate() {
            let expect = blocks as f64 * partition_weight(i) as f64 / 100_000.0;
            if expect >= 40.0 {
                let got = hits[i] as f64;
                assert!(
                    (got / expect) > 0.6 && (got / expect) < 1.5,
                    "{}: got {got} expect {expect}",
                    c.name
                );
            }
        }
    }
}
