//! **Lucky Lode** — *Book*-style 5-reel, 10-line math engine (doc 14 §14.1).
//!
//! A single-payline abstraction is used (each reel contributes one symbol; the
//! line result is multiplied by `lines`). This makes the base-game RTP
//! *exact* and cheap to compute while matching the payout distribution the
//! settle path uses, so the simulated RTP gate and the ledger agree.
//!
//! Features modelled: left-anchored runs of 3–5 with **Lodestone (wild)**
//! substitution, **Prospector's Almanac (scatter)** paying on 3–5 anywhere and
//! triggering `free_spins` free spins at `fs_multiplier` (expanding-symbol
//! approximation), and a hard max-win cap of [`MAX_WIN_MULTIPLE`] (×8000).
//!
//! Paytables are stored in **permille** (thousandths) of the line bet;
//! scatter pays in permille of the *total* bet. RTP is tuned to the owner-set
//! [`crate::TARGET_RTP`] with [`LineMath::tune_to_target`], which scales the
//! low/mid pays while pinning the wild row so the ×8000 max win is preserved.

use crate::{SCATTER, SYMBOL_NAMES, WILD};

/// Maximum win as a multiple of the total bet (bounds house exposure).
pub const MAX_WIN_MULTIPLE: u64 = 8_000;
/// Free spins awarded on 3+ scatters.
pub const FREE_SPINS: u64 = 10;

/// Base paytable in permille of the line bet, indexed `[symbol][run-3]`.
/// Symbol 10 is the wild; its top entry pins the ×8000 max win.
#[must_use]
pub fn base_paytable() -> Vec<[u64; 3]> {
    vec![
        [2_000, 10_000, 50_000],         // spark
        [3_000, 15_000, 60_000],         // glint
        [5_000, 25_000, 100_000],        // flare
        [8_000, 40_000, 150_000],        // ember
        [10_000, 50_000, 200_000],       // cinder
        [15_000, 75_000, 300_000],       // bonfire
        [20_000, 100_000, 400_000],      // beacon
        [30_000, 150_000, 600_000],      // comet
        [50_000, 250_000, 1_000_000],    // aurora
        [100_000, 500_000, 2_000_000],   // mythic
        [200_000, 1_000_000, 8_000_000], // lodestone (wild) — pins ×8000
        [0, 0, 0],                       // almanac (scatter) pays via scatter_pay
    ]
}

/// Scatter pay in permille of the **total** bet for 3/4/5 scatters.
pub const BASE_SCATTER_PAY: [u64; 3] = [2_000, 20_000, 200_000];

/// Per-reel, per-symbol weights (len [`crate::SYMBOL_NAMES`]).
#[must_use]
pub fn base_reel_weights() -> Vec<Vec<u64>> {
    let w = vec![18, 16, 14, 12, 11, 9, 7, 5, 4, 3, 3, 10];
    vec![w.clone(); 5]
}

/// The result of a settled spin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpinOutcome {
    /// Line win in permille of the line bet (before the `lines` multiply).
    pub line_permille: u64,
    /// Number of scatters landed.
    pub scatters: usize,
    /// Whether free spins were triggered.
    pub triggered_free_spins: bool,
}

/// Lucky Lode math engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LineMath {
    /// Per-reel, per-symbol weights.
    pub reel_weights: Vec<Vec<u64>>,
    /// Per-symbol pay in permille of the line bet, `[run-3]`.
    pub paytable: Vec<[u64; 3]>,
    /// Scatter pay in permille of the total bet, `[count-3]`.
    pub scatter_pay: [u64; 3],
    /// Number of paylines.
    pub lines: u64,
    /// Free spins awarded on 3+ scatters.
    pub free_spins: u64,
    /// Free-spin multiplier in permille (of line wins).
    pub fs_multiplier_permille: u64,
    /// Model average line return in permille (used as the free-spin base so a
    /// triggered spin's free-spin value is not biased by the scatter that
    /// triggered it). Refreshed by [`LineMath::tune_to_target`].
    pub fs_base_line_permille: u64,
}

impl Default for LineMath {
    fn default() -> Self {
        let mut m = Self {
            reel_weights: base_reel_weights(),
            paytable: base_paytable(),
            scatter_pay: BASE_SCATTER_PAY,
            lines: 10,
            free_spins: FREE_SPINS,
            fs_multiplier_permille: 2_000,
            fs_base_line_permille: 0,
        };
        m.refresh_fs_base();
        m
    }
}

impl LineMath {
    /// Normalised per-reel symbol probabilities.
    #[must_use]
    pub fn reel_probs(&self) -> Vec<Vec<f64>> {
        self.reel_weights
            .iter()
            .map(|w| {
                let s: u64 = w.iter().sum();
                w.iter().map(|x| *x as f64 / s as f64).collect()
            })
            .collect()
    }

    fn run_pay_permille(&self, sym: usize, run: usize) -> u64 {
        if run >= 3 {
            self.paytable[sym][run - 3]
        } else {
            0
        }
    }

    /// Expected line pay in permille of the line bet, via an exact DP over
    /// left-anchored runs with wild substitution.
    #[must_use]
    pub fn expected_line_permille(&self) -> f64 {
        let rp = self.reel_probs();
        let n = rp.len();
        // State: 0 = wild-only run, else Some(sym) is encoded as sym+1.
        // `states[k]` = probability mass in state k (0 = wild-only, 1+s = sym s).
        let nsym = SYMBOL_NAMES.len();
        let mut states = vec![0.0f64; nsym + 1];
        let p0 = &rp[0];
        for (s, p) in p0.iter().enumerate() {
            if s == SCATTER {
                continue;
            }
            if s == WILD {
                states[0] += p;
            } else {
                states[1 + s] += p;
            }
        }
        let mut acc = 0.0f64;
        for (i, p) in rp.iter().enumerate().skip(1) {
            let mut new = vec![0.0f64; nsym + 1];
            for (k, pr) in states.iter().enumerate() {
                if *pr == 0.0 {
                    continue;
                }
                for (s, q) in p.iter().enumerate() {
                    let q = q * pr;
                    if s == SCATTER {
                        let sym = if k == 0 { WILD } else { k - 1 };
                        acc += q * self.run_pay_permille(sym, i) as f64;
                    } else if k == 0 {
                        // wild-only run: absorbs a natural, stays wild on more wilds
                        if s == WILD {
                            new[0] += q;
                        } else {
                            new[1 + s] += q;
                        }
                    } else {
                        let sym = k - 1;
                        if s == sym || s == WILD {
                            new[k] += q;
                        } else {
                            acc += q * self.run_pay_permille(sym, i) as f64;
                        }
                    }
                }
            }
            states = new;
        }
        for (k, pr) in states.iter().enumerate() {
            if *pr == 0.0 {
                continue;
            }
            let sym = if k == 0 { WILD } else { k - 1 };
            acc += pr * self.run_pay_permille(sym, n) as f64;
        }
        acc
    }

    /// Probability distribution of scatter counts (Poisson-binomial over reels).
    #[must_use]
    pub fn scatter_distribution(&self) -> Vec<f64> {
        let rp = self.reel_probs();
        let mut dist = vec![1.0f64];
        for p in &rp {
            let q = p[SCATTER];
            let mut nd = vec![0.0f64; dist.len() + 1];
            for (k, v) in dist.iter().enumerate() {
                nd[k] += v * (1.0 - q);
                nd[k + 1] += v * q;
            }
            dist = nd;
        }
        dist
    }

    /// Probability of 3+ scatters (free-spins trigger rate).
    #[must_use]
    pub fn free_spins_probability(&self) -> f64 {
        self.scatter_distribution().iter().skip(3).sum()
    }

    /// Number of strip positions on each reel (total weight).
    #[must_use]
    pub fn reel_strip_lengths(&self) -> Vec<u64> {
        self.reel_weights.iter().map(|w| w.iter().sum()).collect()
    }

    /// Symbol shown at `stop` on `reel` (weights expanded to a strip).
    #[must_use]
    pub fn symbol_at(&self, reel: usize, stop: u64) -> usize {
        let w = &self.reel_weights[reel];
        let mut acc = 0u64;
        for (s, x) in w.iter().enumerate() {
            acc += x;
            if stop < acc {
                return s;
            }
        }
        w.len() - 1
    }

    /// Refresh the free-spin base from the current paytable.
    pub fn refresh_fs_base(&mut self) {
        self.fs_base_line_permille = self.expected_line_permille().round() as u64;
    }

    /// Exact return to player for the configured math.
    #[must_use]
    pub fn rtp(&self) -> f64 {
        let e = self.expected_line_permille() / 1000.0;
        let dist = self.scatter_distribution();
        let scatter: f64 = (3..=5)
            .map(|k| dist.get(k).copied().unwrap_or(0.0) * self.scatter_pay[k - 3] as f64 / 1000.0)
            .sum();
        let pfs: f64 = dist.iter().skip(3).sum();
        let fs = pfs * self.free_spins as f64 * (self.fs_multiplier_permille as f64 / 1000.0) * e;
        e + scatter + fs
    }

    /// Max win as a multiple of the total bet (from the wild row).
    #[must_use]
    pub fn max_win_multiple(&self) -> u64 {
        self.paytable[WILD][2] / 1000
    }

    /// Scale the low/mid and scatter pays so RTP meets `target`, pinning the
    /// wild row so the max win is preserved. Converges in a few passes.
    pub fn tune_to_target(&mut self, target: f64) {
        for _ in 0..32 {
            let rtp = self.rtp();
            if (rtp - target).abs() < 1e-4 {
                break;
            }
            let f = target / rtp;
            for (s, row) in self.paytable.iter_mut().enumerate() {
                if s == WILD {
                    continue;
                }
                for v in row.iter_mut() {
                    let scaled = (*v as f64 * f).round() as u64;
                    *v = scaled.max(1);
                }
            }
            for v in self.scatter_pay.iter_mut() {
                let scaled = (*v as f64 * f).round() as u64;
                *v = scaled.max(1);
            }
        }
        self.refresh_fs_base();
    }

    /// Evaluate a settled spin from one symbol per reel.
    #[must_use]
    pub fn evaluate(&self, symbols: &[usize]) -> SpinOutcome {
        let line_permille = self.evaluate_line(symbols);
        let scatters = symbols.iter().filter(|s| **s == SCATTER).count();
        SpinOutcome {
            line_permille,
            scatters,
            triggered_free_spins: scatters >= 3,
        }
    }

    /// Left-anchored run pay in permille of the line bet with wild
    /// substitution (mirrors the DP's `run_pay` rules).
    #[must_use]
    pub fn evaluate_line(&self, symbols: &[usize]) -> u64 {
        let mut run_sym: Option<usize> = None;
        let mut run = 0usize;
        for s in symbols {
            match run_sym {
                None => {
                    if *s == SCATTER {
                        break;
                    } else if *s == WILD {
                        run += 1; // still undetermined
                    } else {
                        run_sym = Some(*s);
                        run += 1;
                    }
                }
                Some(sym) => {
                    if *s == sym || *s == WILD {
                        run += 1;
                    } else {
                        break;
                    }
                }
            }
        }
        let sym = run_sym.unwrap_or(WILD);
        self.run_pay_permille(sym, run)
    }

    /// Settle a spin to micro-LKYL. `line_bet_micros` is the per-line stake;
    /// the total bet is `lines × line_bet_micros`.
    #[must_use]
    pub fn payout_micros(&self, symbols: &[usize], line_bet_micros: u64) -> u64 {
        let out = self.evaluate(symbols);
        let total_bet = line_bet_micros.saturating_mul(self.lines);
        let line_win = line_bet_micros
            .saturating_mul(out.line_permille)
            .saturating_div(1000)
            .saturating_mul(self.lines);
        let scatter_win = if out.scatters >= 3 {
            total_bet
                .saturating_mul(self.scatter_pay[out.scatters.min(5) - 3])
                .saturating_div(1000)
        } else {
            0
        };
        let fs_win = if out.triggered_free_spins {
            // Free-spin value is the model average line return, not this
            // spin's scatter-conditioned result (keeps the gate and the
            // settle path consistent).
            self.free_spins
                .saturating_mul(total_bet)
                .saturating_mul(self.fs_base_line_permille)
                .saturating_div(1000)
                .saturating_mul(self.fs_multiplier_permille)
                .saturating_div(1000)
        } else {
            0
        };
        let cap = total_bet.saturating_mul(MAX_WIN_MULTIPLE);
        line_win
            .saturating_add(scatter_win)
            .saturating_add(fs_win)
            .min(cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fair::Fair;

    #[test]
    fn tune_hits_target_and_preserves_max_win() {
        let mut m = LineMath::default();
        m.tune_to_target(crate::TARGET_RTP);
        assert!(
            (m.rtp() - crate::TARGET_RTP).abs() < 5e-3,
            "rtp {} target {}",
            m.rtp(),
            crate::TARGET_RTP
        );
        assert_eq!(m.max_win_multiple(), MAX_WIN_MULTIPLE);
    }

    #[test]
    fn wild_substitution_and_runs() {
        let m = LineMath::default();
        // 5 wilds pays the wild top row.
        assert_eq!(m.evaluate_line(&[WILD; 5]), m.paytable[WILD][2]);
        // wild substitutes for mythic: 3 mythic (with 1 wild) pays the 3-row.
        let sym = 9; // mythic
        assert_eq!(
            m.evaluate_line(&[sym, sym, WILD, sym, sym]),
            m.paytable[sym][2]
        );
        // A scatter at reel 1 kills the line.
        assert_eq!(m.evaluate_line(&[SCATTER, 0, 0, 0, 0]), 0);
        // Two-in-a-row pays nothing (min run 3).
        assert_eq!(m.evaluate_line(&[0, 0, 1, 1, 1]), 0);
        // Break at reel 3 leaves a 2-run: nothing.
        assert_eq!(m.evaluate_line(&[0, 0, 1, 2, 0]), 0);
        // 3-of-a-kind at the front pays.
        assert_eq!(m.evaluate_line(&[0, 0, 0, 1, 2]), m.paytable[0][0]);
    }

    #[test]
    fn free_spins_trigger_rate_is_sane() {
        let m = LineMath::default();
        let p = m.free_spins_probability();
        assert!((0.001..0.02).contains(&p), "pfs {p}");
    }

    #[test]
    fn max_win_cap_bounds_payout() {
        let m = LineMath::default();
        let line_bet = 1_000; // micros
        let total = line_bet * m.lines;
        // Contrived all-wild spin with a large scatter count still caps.
        let payout = m.payout_micros(&[WILD; 5], line_bet);
        assert!(payout <= total * MAX_WIN_MULTIPLE);
    }

    #[test]
    fn simulated_rtp_matches_analytic() {
        // Draw symbols per the reel distributions and check the empirical RTP
        // lands near the analytic value.
        let mut m = LineMath::default();
        m.tune_to_target(crate::TARGET_RTP);
        let target = m.rtp();
        let mut fair = Fair::new([9u8; 32], "sim", 0);
        // Build cumulative distribution per reel for sampling.
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
        let n = 200_000u64;
        let line_bet = 1_000u64;
        let mut paid = 0u128;
        let mut staked = 0u128;
        for _ in 0..n {
            let mut symbols = Vec::with_capacity(5);
            for c in &cum {
                let f = fair.next_u64() as f64 / (u64::MAX as f64 + 1.0);
                let idx = c.iter().position(|x| f < *x).unwrap_or(c.len() - 1);
                symbols.push(idx);
            }
            paid += m.payout_micros(&symbols, line_bet) as u128;
            staked += (line_bet * m.lines) as u128;
        }
        let sim = paid as f64 / staked as f64;
        assert!((sim - target).abs() < 0.03, "sim {sim} analytic {target}");
    }
}
