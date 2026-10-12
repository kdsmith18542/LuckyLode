//! Share ledger: PPLNS windows, effective-share aggregation, jackpot and
//! league-pool payouts, seasonal leaderboard.
//!
//! Layering: this crate never touches consensus. It reads verified shares,
//! applies the pool-side fun-layer weights (doc 03 §3.3–§3.4), and calls
//! `lm-rewards` for the actual money splits (doc 06 §6.3). Every query is a
//! pure function of the ledger plus an explicit `now_ts` — replays and
//! audits recompute identically.
//!
//! Work units: each share carries `work`, the expected-hash count the pool
//! credits for it (`share_difficulty × 2^32`, Bitcoin convention). Rate
//! estimates divide trailing-window work by window seconds.
//!
//! Effective-share formula (pool policy, applied identically by every
//! reference pool):
//!
//! ```text
//! per in-window share:  work × (1000 + overnight?50) × (1000 + comeback_w) / 1_000_000
//! per miner total:      Σ shares × (1000 + streak‰) / 1000          (all floored)
//! ```
//!
//! Streaks evaluate over full history (a 30-day streak survives a 3-day
//! PPLNS window); comeback state replays the full ordered stream so windows
//! never cut a 7-day boost mid-flight.

use crate::league::{classify, DeviceClass};
use crate::streak::{
    current_streak, day_index, streak_bonus_per_mille, Comeback, DAY_SECS, OVERNIGHT_PERMILLE,
};
use lm_rewards::{split_jackpot, Share};
use std::collections::{HashMap, VecDeque};

/// Recommended PPLNS window (doc 06 §6.3): trailing ~3 days of shares.
pub const PPLNS_WINDOW_SECS: u64 = 3 * DAY_SECS;
/// Trailing window for hashrate (hence league) estimation.
pub const CLASS_WINDOW_SECS: u64 = DAY_SECS;
/// Raw-share retention (review finding G): covers the longest supported
/// query window (30-day season views) with slack. Older history is rolled
/// up into per-miner day lists and comeback state (kept forever, O(1) per
/// miner), so streaks/comeback survive pruning while the raw share log
/// stays bounded at any share rate. Queries must not use windows longer
/// than this; the reference pool's real windows are 1–3 days.
pub const SHARE_RETENTION_SECS: u64 = 32 * DAY_SECS;

/// One verified share as the pool recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareRec {
    /// Opaque pool-side miner id (also the split tiebreak domain).
    pub miner: u64,
    /// Unix timestamp of the share.
    pub ts: u64,
    /// Expected-hash credit (`share_difficulty × 2^32`).
    pub work: u64,
    /// True when mined charge-only inside the `lm-sched` overnight window.
    pub overnight: bool,
}

/// Per-miner aggregation over a window (see [`Ledger::aggregate`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinerAgg {
    pub miner: u64,
    pub raw_work: u64,
    pub eff_work: u64,
    pub days_active: u32,
    pub streak_days: u32,
    pub streak_per_mille: u64,
    pub class: DeviceClass,
}

/// One seasonal leaderboard row, ranked by effective work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardEntry {
    pub miner: u64,
    pub class: DeviceClass,
    pub eff_work: u64,
    pub days_active: u32,
    pub streak_days: u32,
}

/// The pool's share log plus its league prize-pool balance (fed by
/// `lm-rewards` reserve overflow, doc 06 §6.2).
///
/// Bounded by construction (review finding G): raw shares live for
/// [`SHARE_RETENTION_SECS`]; per-miner UTC active days and comeback state
/// are kept for the full history (O(1) each per miner), so streaks and
/// comeback weights survive pruning. The comeback weight of each raw
/// share is resolved once at submit, which is what lets old records go
/// without changing what queries compute.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    shares: VecDeque<StoredShare>,
    /// Per-miner UTC active days (sorted, unique) — the compacted rollup.
    days: HashMap<u64, Vec<u64>>,
    /// Per-miner latest share timestamp (streak reset check).
    last_seen: HashMap<u64, u64>,
    /// Per-miner comeback state machine, advanced at submit time.
    comeback: HashMap<u64, Comeback>,
    league_pool: u64,
}

/// One retained share plus its submit-time comeback weight (per-mille).
#[derive(Debug, Clone, Copy)]
struct StoredShare {
    rec: ShareRec,
    comeback_pm: u64,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one verified share. Resolves the comeback weight now (so
    /// queries never need pre-retention records), rolls the share's day
    /// into the miner's activity list, and prunes raw shares older than
    /// [`SHARE_RETENTION_SECS`] relative to the newest submission.
    pub fn submit(&mut self, rec: ShareRec) {
        let cb = self.comeback.entry(rec.miner).or_default().weight_for(rec.ts);
        let day = day_index(rec.ts);
        let days = self.days.entry(rec.miner).or_default();
        match days.last().copied() {
            Some(last) if last == day => {}
            Some(last) if last < day => days.push(day),
            _ => {
                if let Err(pos) = days.binary_search(&day) {
                    days.insert(pos, day);
                }
            }
        }
        let seen = self.last_seen.entry(rec.miner).or_default();
        if rec.ts > *seen {
            *seen = rec.ts;
        }
        self.shares.push_back(StoredShare { rec, comeback_pm: cb });
        // Append order ≈ time order, so the oldest records sit at the
        // front; prune against the newest submission.
        let cutoff = rec.ts.saturating_sub(SHARE_RETENTION_SECS);
        while let Some(front) = self.shares.front() {
            if front.rec.ts < cutoff {
                self.shares.pop_front();
            } else {
                break;
            }
        }
    }

    /// Retained raw-share count (bounded; dashboard/diagnostics).
    #[must_use]
    pub fn raw_share_count(&self) -> usize {
        self.shares.len()
    }

    /// League prize-pool balance (base units).
    #[must_use]
    pub fn league_balance(&self) -> u64 {
        self.league_pool
    }

    /// Credit reserve overflow into the league prize pool (doc 06 §6.2).
    pub fn credit_league(&mut self, amount: u64) {
        self.league_pool = self.league_pool.saturating_add(amount);
    }

    fn in_window(ts: u64, now_ts: u64, window_secs: u64) -> bool {
        ts <= now_ts && ts > now_ts.saturating_sub(window_secs)
    }

    /// Aggregate every miner with ≥1 in-window share. Result sorted by miner
    /// id ascending. Miners with zero effective work are included here (the
    /// jackpot split itself excludes them) so leaderboards stay complete.
    /// `window_secs` must not exceed [`SHARE_RETENTION_SECS`] (longer
    /// windows see the retained tail only).
    pub fn aggregate(&self, window_secs: u64, now_ts: u64) -> Vec<MinerAgg> {
        struct Acc {
            raw_work: u64,
            eff_sum: u128,
            window_days: Vec<u64>,
            class_work: u64,
        }
        let mut acc: HashMap<u64, Acc> = HashMap::new();
        for stored in &self.shares {
            let r = &stored.rec;
            if !Self::in_window(r.ts, now_ts, window_secs) {
                continue;
            }
            let e = acc.entry(r.miner).or_insert_with(|| Acc {
                raw_work: 0,
                eff_sum: 0,
                window_days: Vec::new(),
                class_work: 0,
            });
            e.raw_work = e.raw_work.saturating_add(r.work);
            e.window_days.push(day_index(r.ts));
            if Self::in_window(r.ts, now_ts, CLASS_WINDOW_SECS) {
                e.class_work = e.class_work.saturating_add(r.work);
            }
            let ov = if r.overnight { OVERNIGHT_PERMILLE } else { 0 };
            e.eff_sum += r.work as u128 * (1000 + ov) as u128
                * (1000 + stored.comeback_pm) as u128
                / 1_000_000;
        }
        let empty: Vec<u64> = Vec::new();
        let mut out = Vec::with_capacity(acc.len());
        for (miner, e) in acc {
            let all_days = self.days.get(&miner).unwrap_or(&empty);
            let last_ts = self.last_seen.get(&miner).copied().unwrap_or(0);
            let streak_days = current_streak(all_days, last_ts, now_ts);
            let streak_pm = streak_bonus_per_mille(streak_days);

            let mut window_days = e.window_days;
            window_days.sort_unstable();
            window_days.dedup();
            let eff_work = (e.eff_sum * (1000 + streak_pm) as u128 / 1000).min(u64::MAX as u128) as u64;
            // Sustained trailing-24h rate; a silent day classifies as Phone
            // (harmless: class only gates prizes, and payouts ignore class).
            let hps = e.class_work / CLASS_WINDOW_SECS.max(1);
            out.push(MinerAgg {
                miner,
                raw_work: e.raw_work,
                eff_work,
                days_active: window_days.len() as u32,
                streak_days,
                streak_per_mille: streak_pm,
                class: classify(hps),
            });
        }
        out.sort_by_key(|a| a.miner);
        out
    }

    /// Jackpot distribution over a PPLNS window (doc 06 §6.3): effective
    /// shares feed the reference sqrt/cap/dust split. Returns
    /// `(miner, payout)` sorted by miner id. Empty when nobody has effective
    /// work (bonus stays in reserve — caller decides).
    pub fn jackpot_payout(&self, bonus: u64, window_secs: u64, now_ts: u64) -> Vec<(u64, u64)> {
        let shares: Vec<Share> = self
            .aggregate(window_secs, now_ts)
            .into_iter()
            .map(|a| Share { miner: a.miner, shares: a.eff_work })
            .collect();
        split_jackpot(&shares, bonus)
    }

    /// Seasonal board over a window: effective work desc, miner id asc.
    /// Farm-class rows are included but flagged via [`DeviceClass`] — callers
    /// filter prizes with [`DeviceClass::prize_eligible`].
    pub fn leaderboard(&self, window_secs: u64, now_ts: u64) -> Vec<BoardEntry> {
        let mut rows: Vec<BoardEntry> = self
            .aggregate(window_secs, now_ts)
            .into_iter()
            .map(|a| BoardEntry {
                miner: a.miner,
                class: a.class,
                eff_work: a.eff_work,
                days_active: a.days_active,
                streak_days: a.streak_days,
            })
            .collect();
        rows.sort_by(|a, b| b.eff_work.cmp(&a.eff_work).then(a.miner.cmp(&b.miner)));
        rows
    }

    /// Pay the whole league prize pool pro-rata (linear, not sqrt — league
    /// prizes reward sustained season effort) over prize-eligible miners by
    /// effective work. Floor each share; leftover dust goes largest-remainder
    /// (remainder desc, miner id asc). Drains the balance to 0. Returns empty
    /// (balance kept) when there is nothing to pay or nobody eligible.
    pub fn split_league_pool(&mut self, window_secs: u64, now_ts: u64) -> Vec<(u64, u64)> {
        let balance = self.league_pool;
        if balance == 0 {
            return Vec::new();
        }
        let mut rows: Vec<(u64, u64)> = self
            .aggregate(window_secs, now_ts)
            .into_iter()
            .filter(|a| a.class.prize_eligible() && a.eff_work > 0)
            .map(|a| (a.miner, a.eff_work))
            .collect();
        if rows.is_empty() {
            return Vec::new();
        }
        rows.sort_by_key(|(id, _)| *id);
        let total: u128 = rows.iter().map(|(_, w)| *w as u128).sum();
        let mut pays = vec![0u64; rows.len()];
        let mut fracs: Vec<(u128, u64)> = Vec::new();
        let mut assigned = 0u64;
        for (i, (_, w)) in rows.iter().enumerate() {
            let num = balance as u128 * *w as u128;
            pays[i] = (num / total) as u64;
            assigned += pays[i];
            fracs.push((num % total, rows[i].0));
        }
        fracs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut dust = balance - assigned;
        for (_, id) in fracs {
            if dust == 0 {
                break;
            }
            pays[rows.iter().position(|(m, _)| *m == id).expect("id present")] += 1;
            dust -= 1;
        }
        self.league_pool = 0;
        debug_assert_eq!(pays.iter().sum::<u64>(), balance);
        rows.into_iter().zip(pays).map(|((id, _), p)| (id, p)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: u64 = DAY_SECS;

    fn rec(miner: u64, day: u64, work: u64) -> ShareRec {
        ShareRec { miner, ts: day * D + 100, work, overnight: false }
    }

    #[test]
    fn window_excludes_stale_shares() {
        let mut l = Ledger::new();
        l.submit(rec(1, 1, 100)); // stale
        l.submit(rec(1, 30, 100)); // in window
        let agg = l.aggregate(3 * D, 30 * D + 200);
        assert_eq!(agg.len(), 1);
        assert_eq!(agg[0].raw_work, 100);
    }

    #[test]
    fn streak_boosts_effective_over_raw() {
        let mut l = Ledger::new();
        for d in 1..=10u64 {
            l.submit(rec(1, d, 100)); // 10-day streak
        }
        l.submit(rec(2, 10, 1000)); // newcomer, 10x raw of one share
        let agg = l.aggregate(3 * D, 10 * D + 200);
        let (a1, a2) = (&agg[0], &agg[1]);
        assert_eq!(a1.streak_days, 10);
        assert_eq!(a1.streak_per_mille, 100);
        // Miner 1: 3 window shares × 100 × 1.10 = 330. Miner 2: 1000 × 1.01.
        assert_eq!(a1.eff_work, 330);
        assert_eq!(a2.eff_work, 1010);
    }

    #[test]
    fn overnight_flag_weights_share() {
        let mut l = Ledger::new();
        l.submit(ShareRec { miner: 1, ts: 5 * D, work: 1000, overnight: true });
        l.submit(rec(2, 5, 1000));
        let agg = l.aggregate(3 * D, 5 * D + 200);
        // Overnight: 1000 × 1.05 × streak(1d → 1.01) = 1060. Plain: 1010.
        assert_eq!(agg[0].eff_work, 1060);
        assert_eq!(agg[1].eff_work, 1010);
    }

    #[test]
    fn jackpot_payout_conserves_bonus() {
        let mut l = Ledger::new();
        for d in 1..=5u64 {
            l.submit(rec(1, d, 1_000_000));
            l.submit(rec(2, d, 100));
            l.submit(rec(3, d, 100));
            l.submit(rec(4, d, 100));
            l.submit(rec(5, d, 100));
        }
        let pays = l.jackpot_payout(1_000_000, 30 * D, 5 * D + 200);
        assert_eq!(pays.iter().map(|(_, p)| p).sum::<u64>(), 1_000_000);
        assert!(pays[0].1 <= 250_000); // dominant miner capped at 25%
    }

    #[test]
    fn league_split_drains_and_conserves() {
        let mut l = Ledger::new();
        l.submit(rec(1, 9, 3000));
        l.submit(rec(2, 9, 1000));
        l.credit_league(1000);
        let pays = l.split_league_pool(3 * D, 9 * D + 200);
        assert_eq!(pays, vec![(1, 750), (2, 250)]);
        assert_eq!(l.league_balance(), 0);
        // Empty pool pays nothing and keeps nothing.
        assert!(l.split_league_pool(3 * D, 9 * D + 200).is_empty());
    }

    #[test]
    fn farms_take_no_league_prizes() {
        let mut l = Ledger::new();
        // 600 MH/s sustained over the class window: farm territory.
        l.submit(ShareRec { miner: 9, ts: 9 * D, work: 600_000_000 * D, overnight: false });
        l.submit(rec(1, 9, 1000));
        l.credit_league(1000);
        let pays = l.split_league_pool(3 * D, 9 * D + 200);
        assert_eq!(pays, vec![(1, 1000)]);
        let board = l.leaderboard(3 * D, 9 * D + 200);
        assert_eq!(board[0].miner, 9); // still ranked…
        assert_eq!(board[0].class, DeviceClass::Farm); // …but flagged
    }

    #[test]
    fn retention_bounds_raw_shares_but_keeps_streaks() {
        let mut l = Ledger::new();
        for d in 1..=80u64 {
            l.submit(rec(1, d, 100));
        }
        // Raw log holds only the retention tail (32 days + slack).
        assert!(
            l.raw_share_count() <= 34,
            "raw log grew unboundedly: {}",
            l.raw_share_count()
        );
        // The rollup still sees the whole 80-day run.
        let agg = l.aggregate(3 * D, 80 * D + 200);
        assert_eq!(agg[0].streak_days, 80);
        assert_eq!(agg[0].streak_per_mille, 300); // capped at +30%
        // 3-day window: days 78..=80. 3 × 100 × 1.30 = 390.
        assert_eq!(agg[0].raw_work, 300);
        assert_eq!(agg[0].eff_work, 390);
        assert_eq!(agg[0].days_active, 3);
    }

    #[test]
    fn comeback_weight_is_baked_at_submit() {
        let mut l = Ledger::new();
        l.submit(rec(1, 1, 1000)); // first share ever: no comeback
        l.submit(rec(1, 20, 1000)); // return after 19 days idle: +10%
        l.submit(rec(1, 21, 1000)); // inside the 7-day comeback window: +10%
        let agg = l.aggregate(3 * D, 21 * D + 200);
        // Days 20 and 21 in window: 1000 × 1.10 × 2 = 2200, streak 2 days
        // (+2%) → 2244. The day-1 share is out of window (and prunable)
        // and contributes nothing.
        assert_eq!(agg[0].eff_work, 2244);
        assert_eq!(agg[0].streak_days, 2);
    }

    #[test]
    fn pruning_never_changes_in_retention_queries() {
        // Two ledgers fed the same recent stream; one also saw ancient
        // history that pruning drops. In-retention answers must match.
        let mut bare = Ledger::new();
        let mut with_ancient = Ledger::new();
        for d in 1..=30u64 {
            with_ancient.submit(rec(1, d, 77));
            with_ancient.submit(rec(2, d, 33));
        }
        for d in 34..=40u64 {
            bare.submit(rec(1, d, 77));
            bare.submit(rec(2, d, 33));
            with_ancient.submit(rec(1, d, 77));
            with_ancient.submit(rec(2, d, 33));
        }
        let now = 40 * D + 200;
        assert_eq!(bare.aggregate(3 * D, now), with_ancient.aggregate(3 * D, now));
        assert_eq!(
            bare.jackpot_payout(1_000_003, 3 * D, now),
            with_ancient.jackpot_payout(1_000_003, 3 * D, now)
        );
        // …while the ancient ledger's raw log stayed bounded.
        assert!(with_ancient.raw_share_count() <= 2 * 34);
    }
}
