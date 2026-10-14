//! **Lucky Lode Ways** — ways-style 6-reel math engine (doc 14 §14.1).
//!
//! Up to 117,649 ways (6 reels × 2–7 rows). RTP is computed *analytically*
//! under the standard independence assumptions:
//!
//! - Each reel shows `h ∼ Uniform{2..=7}` cells, each cell drawn i.i.d. from
//!   that reel's symbol distribution.
//! - A symbol wins on the longest left-anchored run of length ≥ 3; the ways
//!   count is the product of that symbol's per-reel cell counts.
//! - Expected win for symbol `s` at run length `L`:
//!   `pay[s][L] × Π_{r≤L} E[count_s(r)] × P(count_s(L+1)=0)`.
//! - Cascading reels and the free-spin multiplier are folded into a single
//!   `feature_multiplier` (a documented approximation refined during real
//!   math-model work).
//!
//! Tuned to the owner-set [`crate::TARGET_RTP`], same discipline as [`crate::line`].

/// Maximum win as a multiple of the total bet for the Ways title.
pub const MAX_WIN_MULTIPLE_WAYS: u64 = 10_000;
/// Paying symbols are the ten luck classes (no wild/scatter in this model).
pub const WAYS_SYMBOLS: usize = 10;
/// Reel count.
pub const REELS: usize = 6;
/// Uniform row-count support (2..=7).
const MIN_ROWS: u64 = 2;
const MAX_ROWS: u64 = 7;

/// Base ways paytable in permille of the total bet **per way**, `[run-3]`.
#[must_use]
pub fn base_paytable_ways() -> Vec<[u64; 4]> {
    vec![
        [50, 100, 200, 400],           // spark
        [60, 120, 240, 480],           // glint
        [75, 150, 300, 600],           // flare
        [100, 200, 400, 800],          // ember
        [150, 300, 600, 1_200],        // cinder
        [200, 400, 800, 1_600],        // bonfire
        [300, 600, 1_200, 2_400],      // beacon
        [500, 1_000, 2_000, 4_000],    // comet
        [800, 1_600, 3_200, 6_400],    // aurora
        [1_500, 3_000, 6_000, 12_000], // mythic
    ]
}

/// Per-reel, per-symbol weights for the six reels.
#[must_use]
pub fn base_reel_weights_ways() -> Vec<Vec<u64>> {
    let w = vec![16, 15, 14, 13, 12, 11, 9, 7, 5, 3];
    vec![w; REELS]
}

/// Lucky Lode Ways math engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WaysMath {
    /// Per-reel, per-symbol weights (len [`WAYS_SYMBOLS`]).
    pub reel_weights: Vec<Vec<u64>>,
    /// Per-symbol pay in permille of the total bet per way, `[run-3]`.
    pub paytable: Vec<[u64; 4]>,
    /// Average cascades + free-spin multiplier, in permille.
    pub feature_multiplier_permille: u64,
}

impl Default for WaysMath {
    fn default() -> Self {
        Self {
            reel_weights: base_reel_weights_ways(),
            paytable: base_paytable_ways(),
            feature_multiplier_permille: 6_000,
        }
    }
}

impl WaysMath {
    fn reel_probs(&self) -> Vec<Vec<f64>> {
        self.reel_weights
            .iter()
            .map(|w| {
                let s: u64 = w.iter().sum();
                w.iter().map(|x| *x as f64 / s as f64).collect()
            })
            .collect()
    }

    /// `E[count_s]` and `P(count_s = 0)` for a cell count `h ∼ U{2..=7}`
    /// and per-cell symbol probability `p`.
    fn cell_moments(p: f64) -> (f64, f64) {
        let heights = (MAX_ROWS - MIN_ROWS + 1) as f64;
        let mut e = 0.0;
        let mut p0 = 0.0;
        for h in MIN_ROWS..=MAX_ROWS {
            e += h as f64 * p;
            p0 += (1.0 - p).powi(h as i32);
        }
        (e / heights, p0 / heights)
    }

    /// Analytic ways RTP contribution of one symbol (total-bet units),
    /// before the feature multiplier.
    fn symbol_rtp(&self, probs: &[Vec<f64>], s: usize) -> f64 {
        let n = probs.len();
        // `prefix` = Π_{r<l} E[count_s(r)] once reel l-1 has been folded in.
        let mut prefix = 1.0;
        let mut total = 0.0;
        for l in 1..=n {
            let (e, _) = Self::cell_moments(probs[l - 1][s]);
            prefix *= e;
            if l < 3 {
                continue;
            }
            let terminator = if l < n {
                let (_, p0) = Self::cell_moments(probs[l][s]);
                p0
            } else {
                1.0
            };
            let pay = self.paytable[s][l - 3] as f64 / 1000.0;
            total += pay * prefix * terminator;
        }
        total
    }

    /// Exact (modelled) RTP.
    #[must_use]
    pub fn rtp(&self) -> f64 {
        let probs = self.reel_probs();
        let base: f64 = (0..WAYS_SYMBOLS).map(|s| self.symbol_rtp(&probs, s)).sum();
        base * (self.feature_multiplier_permille as f64 / 1000.0)
    }

    /// Max win multiple of the total bet (nominal cap).
    #[must_use]
    pub fn max_win_multiple(&self) -> u64 {
        MAX_WIN_MULTIPLE_WAYS
    }

    /// Symbols per reel (the fair-draw modulo per reel).
    #[must_use]
    pub fn reel_symbol_counts(&self) -> Vec<u64> {
        self.reel_weights.iter().map(|w| w.len() as u64).collect()
    }

    /// The game's maximum ways surface: `MAX_ROWS ^ REELS` (= 117,649).
    #[must_use]
    pub fn max_ways(&self) -> u64 {
        MAX_ROWS.pow(REELS as u32)
    }

    /// Evaluate a ways spin: `reels[r]` are the visible symbols on reel `r`.
    /// Returns the win in permille of the total bet (per-way pays × ways),
    /// before the feature multiplier.
    #[must_use]
    pub fn evaluate_permille(&self, reels: &[Vec<usize>]) -> u64 {
        let mut total = 0u64;
        for s in 0..WAYS_SYMBOLS {
            let mut ways = 1u64;
            let mut run = 0usize;
            for reel in reels {
                let c = reel.iter().filter(|x| **x == s).count() as u64;
                if c == 0 {
                    break;
                }
                ways = ways.saturating_mul(c);
                run += 1;
            }
            if run >= 3 {
                total = total.saturating_add(self.paytable[s][run - 3].saturating_mul(ways));
            }
        }
        total
    }

    /// Settle a ways spin to micro-LKYL, applying the feature multiplier and
    /// the max-win cap.
    #[must_use]
    pub fn payout_micros(&self, reels: &[Vec<usize>], bet_micros: u64) -> u64 {
        let permille = self.evaluate_permille(reels);
        let win = bet_micros
            .saturating_mul(permille)
            .saturating_div(1000)
            .saturating_mul(self.feature_multiplier_permille)
            .saturating_div(1000);
        win.min(bet_micros.saturating_mul(MAX_WIN_MULTIPLE_WAYS))
    }

    /// Scale the paytable so RTP meets `target` (converges in a few passes).
    pub fn tune_to_target(&mut self, target: f64) {
        for _ in 0..32 {
            let rtp = self.rtp();
            if (rtp - target).abs() < 1e-4 {
                break;
            }
            let f = target / rtp;
            for row in self.paytable.iter_mut() {
                for v in row.iter_mut() {
                    let scaled = (*v as f64 * f).round() as u64;
                    *v = scaled.max(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunes_to_target() {
        let mut m = WaysMath::default();
        m.tune_to_target(crate::TARGET_RTP);
        assert!(
            (m.rtp() - crate::TARGET_RTP).abs() < 5e-3,
            "rtp {} target {}",
            m.rtp(),
            crate::TARGET_RTP
        );
        assert_eq!(m.max_win_multiple(), MAX_WIN_MULTIPLE_WAYS);
    }

    #[test]
    fn rtp_is_monotone_in_feature_multiplier() {
        let mut m = WaysMath::default();
        let before = m.rtp();
        m.feature_multiplier_permille *= 2;
        assert!(m.rtp() > before);
    }

    #[test]
    fn symbol_rtp_increases_with_probability() {
        let m = WaysMath::default();
        let low = vec![vec![0.02; WAYS_SYMBOLS]; REELS];
        let high = vec![vec![0.15; WAYS_SYMBOLS]; REELS];
        assert!(m.symbol_rtp(&high, 9) > m.symbol_rtp(&low, 9));
    }

    #[test]
    fn ways_surface_and_evaluation() {
        let mut m = WaysMath::default();
        m.tune_to_target(crate::TARGET_RTP);
        assert_eq!(m.max_ways(), 117_649);
        assert_eq!(m.reel_symbol_counts(), vec![10; REELS]);
        // Six mythic symbols, one per reel -> run of 6, ways = 1.
        let reels: Vec<Vec<usize>> = (0..REELS).map(|_| vec![9]).collect();
        assert_eq!(m.evaluate_permille(&reels), m.paytable[9][3]);
        // Two mythic on each reel -> ways = 2^6 = 64.
        let reels: Vec<Vec<usize>> = (0..REELS).map(|_| vec![9, 9]).collect();
        assert_eq!(m.evaluate_permille(&reels), m.paytable[9][3] * 64);
        // A gap on reel 2 leaves only a 1-run: nothing.
        let mut broke: Vec<Vec<usize>> = (0..REELS).map(|_| vec![9]).collect();
        broke[1] = vec![0];
        assert_eq!(m.evaluate_permille(&broke), 0);
        // Payout respects the cap.
        let bet = 1_000u64;
        assert!(
            m.payout_micros(
                &(0..REELS)
                    .map(|_| vec![9, 9, 9, 9, 9, 9, 9])
                    .collect::<Vec<_>>(),
                bet
            ) <= bet * MAX_WIN_MULTIPLE_WAYS
        );
    }

    #[test]
    fn simulated_rtp_matches_analytic() {
        use crate::fair::Fair;
        let mut m = WaysMath::default();
        m.tune_to_target(crate::TARGET_RTP);
        let target = m.rtp();
        let probs = m.reel_probs();
        let cum: Vec<Vec<f64>> = probs
            .iter()
            .map(|p| {
                let mut acc = 0.0;
                p.iter()
                    .map(|x| {
                        acc += x;
                        acc
                    })
                    .collect()
            })
            .collect();
        let mut fair = Fair::new([5u8; 32], "ways-sim", 0);
        let n = 100_000u64;
        let bet = 1_000u64;
        let mut paid = 0u128;
        let mut staked = 0u128;
        for _ in 0..n {
            let mut reels: Vec<Vec<usize>> = Vec::with_capacity(REELS);
            for c in &cum {
                let rows = MIN_ROWS + fair.next_index(MAX_ROWS - MIN_ROWS + 1);
                let mut reel = Vec::with_capacity(rows as usize);
                for _ in 0..rows {
                    let f = fair.next_u64() as f64 / (u64::MAX as f64 + 1.0);
                    let idx = c.iter().position(|x| f < *x).unwrap_or(c.len() - 1);
                    reel.push(idx);
                }
                reels.push(reel);
            }
            paid += m.payout_micros(&reels, bet) as u128;
            staked += bet as u128;
        }
        let sim = paid as f64 / staked as f64;
        assert!((sim - target).abs() < 0.05, "sim {sim} analytic {target}");
    }
}
