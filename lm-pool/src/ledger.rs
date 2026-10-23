//! Share ledger: PPLNS windows, effective-share aggregation, jackpot and
//! prize-fund payouts, seasonal leaderboard.
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

use crate::division::{division_for, DeviceFamily};
use crate::streak::{
    best_streak, current_streak, day_index, streak_bonus_per_mille, Comeback, DAY_SECS,
    OVERNIGHT_PERMILLE,
};
use lm_rewards::{split_jackpot, Share};
use std::collections::{HashMap, VecDeque};

/// Recommended PPLNS window (doc 06 §6.3): trailing ~3 days of shares.
/// Season/divisions query window — block credits use
/// [`PPLNS_WINDOW_SHARES`] instead (traditional last-N-shares).
pub const PPLNS_WINDOW_SECS: u64 = 3 * DAY_SECS;
/// Traditional PPLNS window: the last N accepted shares network-wide
/// (classic "pay per last N shares"). Adapts to hashrate instead of wall
/// time, so a stopped miner ages out once N newer shares arrive — not
/// after a fixed multi-day tail.
pub const PPLNS_WINDOW_SHARES: usize = 2_000;
/// Trailing window for hashrate (hence division) estimation.
pub const CLASS_WINDOW_SECS: u64 = DAY_SECS;
/// Season view window for division boards (doc 03 §3.3: seasonal
/// leaderboards per device class). Covered by [`SHARE_RETENTION_SECS`].
pub const SEASON_WINDOW_SECS: u64 = 30 * DAY_SECS;
/// Raw-share retention (review finding G): covers the longest supported
/// query window (30-day season views) with slack. Older history is rolled
/// up into per-miner day lists and comeback state (kept forever, O(1) per
/// miner), so streaks/comeback survive pruning while the raw share log
/// stays bounded at any share rate. Queries must not use windows longer
/// than this; the reference pool's real windows are 1–3 days.
pub const SHARE_RETENTION_SECS: u64 = 32 * DAY_SECS;

/// One verified share as the pool recorded it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShareRec {
    /// Opaque pool-side miner id (also the split tiebreak domain).
    pub miner: String,
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
    pub miner: String,
    pub raw_work: u64,
    pub eff_work: u64,
    pub days_active: u32,
    pub streak_days: u32,
    pub streak_per_mille: u64,
    /// Sustained trailing-24h rate (hashes/second) — the fraud-police
    /// input for division resolution.
    pub hps: u64,
    /// Rate-implied family (the pool server re-resolves the display
    /// division with the miner's declared agent via
    /// [`crate::division::division_for`]).
    pub class: DeviceFamily,
}

/// One seasonal leaderboard row, ranked by effective work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardEntry {
    pub miner: String,
    pub class: DeviceFamily,
    pub eff_work: u64,
    pub days_active: u32,
    pub streak_days: u32,
    /// Sustained trailing-24h rate (hashes/second) — the fraud-police
    /// input for division resolution.
    pub hps: u64,
}

/// Rank a leaderboard within each device family (doc 03 §3.3: phones
/// race phones). Ranks are 1-based positions in the board's own order
/// (effective work desc, miner id asc); families come back in division
/// ladder order — Phone, Cpu, Gpu, Farm. Every family is present even
/// when empty, so callers can render a stable section.
#[must_use]
pub fn rank_by_division(board: Vec<BoardEntry>) -> Vec<(DeviceFamily, Vec<(u32, BoardEntry)>)> {
    const LADDER: [DeviceFamily; 4] = [
        DeviceFamily::Phone,
        DeviceFamily::Cpu,
        DeviceFamily::Gpu,
        DeviceFamily::Farm,
    ];
    LADDER
        .iter()
        .map(|&class| {
            let rows: Vec<(u32, BoardEntry)> = board
                .iter()
                .filter(|e| e.class == class)
                .enumerate()
                .map(|(i, e)| (i as u32 + 1, e.clone()))
                .collect();
            (class, rows)
        })
        .collect()
}

/// The pool's share log: raw accepted shares plus the compacted
/// per-miner day rollup (streaks, comeback state). Holds no money —
///
/// jackpot/pot splits pay out of per-block PPLNS weights and per-height
/// prize-inflow credits live in the pool server's `prize_credit_log`,
/// not here.
///
/// Bounded by construction (review finding G): raw shares live for
/// [`SHARE_RETENTION_SECS`]; per-miner UTC active days and comeback state
/// are kept for the full history (O(1) each per miner), so streaks and
/// comeback weights survive pruning. The comeback weight of each raw
/// share is resolved once at submit, which is what lets old records go
/// without changing what queries compute.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Ledger {
    shares: VecDeque<StoredShare>,
    /// Per-miner UTC active days (sorted, unique) — the compacted rollup.
    days: HashMap<String, Vec<u64>>,
    /// Per-miner latest share timestamp (streak reset check).
    last_seen: HashMap<String, u64>,
    /// Per-miner comeback state machine, advanced at submit time.
    comeback: HashMap<String, Comeback>,
}

/// One retained share plus its submit-time comeback weight (per-mille).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
        let miner = rec.miner.clone();
        let cb = self
            .comeback
            .entry(miner.clone())
            .or_default()
            .weight_for(rec.ts);
        let day = day_index(rec.ts);
        let days = self.days.entry(miner.clone()).or_default();
        match days.last().copied() {
            Some(last) if last == day => {}
            Some(last) if last < day => days.push(day),
            _ => {
                if let Err(pos) = days.binary_search(&day) {
                    days.insert(pos, day);
                }
            }
        }
        let seen = self.last_seen.entry(miner).or_default();
        if rec.ts > *seen {
            *seen = rec.ts;
        }
        let cutoff = rec.ts.saturating_sub(SHARE_RETENTION_SECS);
        self.shares.push_back(StoredShare {
            rec,
            comeback_pm: cb,
        });
        // Append order ≈ time order, so the oldest records sit at the
        // front; prune against the newest submission.
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

    fn in_window(ts: u64, now_ts: u64, window_secs: u64) -> bool {
        ts <= now_ts && ts > now_ts.saturating_sub(window_secs)
    }

    /// Aggregate every miner with ≥1 in-window share. Result sorted by miner
    /// id ascending. Miners with zero effective work are included here (the
    /// jackpot split itself excludes them) so leaderboards stay complete.
    /// `window_secs` must not exceed [`SHARE_RETENTION_SECS`] (longer
    /// windows see the retained tail only).
    pub fn aggregate(&self, window_secs: u64, now_ts: u64) -> Vec<MinerAgg> {
        self.aggregate_iter(
            self.shares
                .iter()
                .filter(|s| Self::in_window(s.rec.ts, now_ts, window_secs)),
            now_ts,
        )
    }

    /// Traditional PPLNS: aggregate over the last `n` accepted shares
    /// network-wide (classic "pay per last N shares", the block-credit
    /// window). A stopped miner drops out as soon as `n` newer shares
    /// arrive, regardless of wall-clock time.
    pub fn aggregate_shares(&self, n: usize, now_ts: u64) -> Vec<MinerAgg> {
        let start = self.shares.len().saturating_sub(n);
        self.aggregate_iter(self.shares.iter().skip(start), now_ts)
    }

    fn aggregate_iter<'a, I>(&self, iter: I, now_ts: u64) -> Vec<MinerAgg>
    where
        I: Iterator<Item = &'a StoredShare>,
    {
        struct Acc {
            raw_work: u64,
            eff_sum: u128,
            window_days: Vec<u64>,
            class_work: u64,
        }
        let mut acc: HashMap<String, Acc> = HashMap::new();
        for stored in iter {
            let r = &stored.rec;
            let e = acc.entry(r.miner.clone()).or_insert_with(|| Acc {
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
            e.eff_sum += r.work as u128 * (1000 + ov) as u128 * (1000 + stored.comeback_pm) as u128
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
            let eff_work =
                (e.eff_sum * (1000 + streak_pm) as u128 / 1000).min(u64::MAX as u128) as u64;
            // Sustained trailing-24h rate. The ledger resolves the
            // rate-implied family only; the pool server re-resolves the
            // display division with the miner's declared agent
            // (division_for): labels decide, this rate polices.
            let hps = e.class_work / CLASS_WINDOW_SECS.max(1);
            out.push(MinerAgg {
                miner,
                raw_work: e.raw_work,
                eff_work,
                days_active: window_days.len() as u32,
                streak_days,
                streak_per_mille: streak_pm,
                hps,
                class: division_for(None, hps),
            });
        }
        out.sort_by(|a, b| a.miner.cmp(&b.miner));
        out
    }

    /// Jackpot distribution over a PPLNS window (doc 06 §6.3): effective
    /// shares feed the reference sqrt/cap/dust split. Returns
    /// `(miner, payout)` sorted by miner id. Empty when nobody has effective
    /// work (bonus stays in reserve — caller decides).
    pub fn jackpot_payout(&self, bonus: u64, window_secs: u64, now_ts: u64) -> Vec<(String, u64)> {
        let shares: Vec<Share> = self
            .aggregate(window_secs, now_ts)
            .into_iter()
            .map(|a| Share {
                miner: a.miner,
                shares: a.eff_work,
            })
            .collect();
        split_jackpot(&shares, bonus)
    }

    /// [`Self::jackpot_payout`] over the traditional last-N-shares window
    /// (the block-credit default).
    pub fn jackpot_payout_shares(&self, bonus: u64, n: usize, now_ts: u64) -> Vec<(String, u64)> {
        let shares: Vec<Share> = self
            .aggregate_shares(n, now_ts)
            .into_iter()
            .map(|a| Share {
                miner: a.miner,
                shares: a.eff_work,
            })
            .collect();
        split_jackpot(&shares, bonus)
    }

    /// Seasonal board over a window: effective work desc, miner id asc.
    /// Farm-class rows are included but flagged via [`DeviceFamily`] —
    /// the pool server re-resolves each row's division with the miner's
    /// declared agent before display.
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
                hps: a.hps,
            })
            .collect();
        rows.sort_by(|a, b| b.eff_work.cmp(&a.eff_work).then(a.miner.cmp(&b.miner)));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: u64 = DAY_SECS;

    fn rec(miner: &str, day: u64, work: u64) -> ShareRec {
        ShareRec {
            miner: miner.to_string(),
            ts: day * D + 100,
            work,
            overnight: false,
        }
    }

    #[test]
    fn window_excludes_stale_shares() {
        let mut l = Ledger::new();
        l.submit(rec("m1", 1, 100)); // stale
        l.submit(rec("m1", 30, 100)); // in window
        let agg = l.aggregate(3 * D, 30 * D + 200);
        assert_eq!(agg.len(), 1);
        assert_eq!(agg[0].raw_work, 100);
    }

    #[test]
    fn shares_window_selects_trailing_n_network_wide() {
        let mut l = Ledger::new();
        l.submit(rec("m1", 1, 100)); // oldest
        l.submit(rec("m2", 1, 100));
        l.submit(rec("m1", 1, 100)); // newest
        let agg = l.aggregate_shares(1, 2 * D);
        assert_eq!(agg.len(), 1, "only the newest share is in a 1-share window");
        assert_eq!(agg[0].miner, "m1");
        let agg = l.aggregate_shares(2, 2 * D);
        let miners: Vec<String> = agg.iter().map(|a| a.miner.clone()).collect();
        assert_eq!(
            miners,
            vec!["m1".to_string(), "m2".to_string()],
            "last two shares, both miners"
        );
    }

    #[test]
    fn shares_window_ages_out_stopped_miner() {
        let mut l = Ledger::new();
        l.submit(rec("m1", 1, 100)); // miner 1 stops here
        for _ in 0..5 {
            l.submit(rec("m2", 1, 100));
        }
        let agg = l.aggregate_shares(5, 2 * D);
        assert_eq!(agg.len(), 1);
        assert_eq!(
            agg[0].miner, "m2",
            "5 newer shares pushed the stopped miner out"
        );
    }

    #[test]
    fn jackpot_split_over_shares_window_conserves() {
        let mut l = Ledger::new();
        l.submit(rec("m1", 1, 400));
        l.submit(rec("m2", 1, 900));
        l.submit(rec("m3", 1, 2_500));
        let bonus = 999_983u64;
        let pays = l.jackpot_payout_shares(bonus, 3, 2 * D);
        assert_eq!(pays.iter().map(|(_, p)| *p).sum::<u64>(), bonus);
        assert_eq!(pays.len(), 3);
    }

    #[test]
    fn streak_boosts_effective_over_raw() {
        let mut l = Ledger::new();
        for d in 1..=10u64 {
            l.submit(rec("m1", d, 100)); // 10-day streak
        }
        l.submit(rec("m2", 10, 1000)); // newcomer, 10x raw of one share
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
        l.submit(ShareRec {
            miner: "m1".to_string(),
            ts: 5 * D,
            work: 1000,
            overnight: true,
        });
        l.submit(rec("m2", 5, 1000));
        let agg = l.aggregate(3 * D, 5 * D + 200);
        // Overnight: 1000 × 1.05 × streak(1d → 1.01) = 1060. Plain: 1010.
        assert_eq!(agg[0].eff_work, 1060);
        assert_eq!(agg[1].eff_work, 1010);
    }

    #[test]
    fn jackpot_payout_conserves_bonus() {
        let mut l = Ledger::new();
        for d in 1..=5u64 {
            l.submit(rec("m1", d, 1_000_000));
            l.submit(rec("m2", d, 100));
            l.submit(rec("m3", d, 100));
            l.submit(rec("m4", d, 100));
            l.submit(rec("m5", d, 100));
        }
        let pays = l.jackpot_payout(1_000_000, 30 * D, 5 * D + 200);
        assert_eq!(pays.iter().map(|(_, p)| p).sum::<u64>(), 1_000_000);
        assert!(pays[0].1 <= 250_000); // dominant miner capped at 25%
    }

    #[test]
    fn farms_are_ranked_but_flagged() {
        let mut l = Ledger::new();
        // 600 MH/s sustained over the class window: farm territory.
        l.submit(ShareRec {
            miner: "m9".to_string(),
            ts: 9 * D,
            work: 600_000_000 * D,
            overnight: false,
        });
        l.submit(rec("m1", 9, 1000));
        let board = l.leaderboard(3 * D, 9 * D + 200);
        assert_eq!(board[0].miner, "m9"); // still ranked…
        assert_eq!(board[0].class, DeviceFamily::Farm); // …but flagged
    }

    #[test]
    fn streaks_serve_full_history_without_a_window() {
        let mut l = Ledger::new();
        // m1: 5 consecutive days, then a 3-day gap, then active today —
        // current streak 1, best 5, 6 active days total.
        for d in 10..=14u64 {
            l.submit(rec("m1", d, 100));
        }
        l.submit(rec("m1", 18, 100));
        // m2: same days as m1's first run but silent since — the 48 h
        // gap rule zeroes the current streak; the record survives.
        for d in 10..=13u64 {
            l.submit(rec("m2", d, 100));
        }
        let out = l.streaks(18 * D + 100);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].miner, "m1");
        assert_eq!(
            (
                out[0].days_active,
                out[0].streak_days,
                out[0].best_streak_days
            ),
            (6, 1, 5)
        );
        assert_eq!(out[1].miner, "m2");
        assert_eq!(
            (
                out[1].days_active,
                out[1].streak_days,
                out[1].best_streak_days
            ),
            (4, 0, 4)
        );
    }

    #[test]
    fn streaks_survive_retention_pruning() {
        let mut l = Ledger::new();
        for d in 1..=80u64 {
            l.submit(rec("m1", d, 100));
        }
        assert!(l.raw_share_count() <= 34, "precondition: raw log pruned");
        let out = l.streaks(80 * D + 100);
        assert_eq!(out[0].days_active, 80);
        assert_eq!(out[0].streak_days, 80);
        assert_eq!(out[0].best_streak_days, 80);
    }

    #[test]
    fn retention_bounds_raw_shares_but_keeps_streaks() {
        let mut l = Ledger::new();
        for d in 1..=80u64 {
            l.submit(rec("m1", d, 100));
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
        l.submit(rec("m1", 1, 1000)); // first share ever: no comeback
        l.submit(rec("m1", 20, 1000)); // return after 19 days idle: +10%
        l.submit(rec("m1", 21, 1000)); // inside the 7-day comeback window: +10%
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
            with_ancient.submit(rec("m1", d, 77));
            with_ancient.submit(rec("m2", d, 33));
        }
        for d in 34..=40u64 {
            bare.submit(rec("m1", d, 77));
            bare.submit(rec("m2", d, 33));
            with_ancient.submit(rec("m1", d, 77));
            with_ancient.submit(rec("m2", d, 33));
        }
        let now = 40 * D + 200;
        assert_eq!(
            bare.aggregate(3 * D, now),
            with_ancient.aggregate(3 * D, now)
        );
        assert_eq!(
            bare.jackpot_payout(1_000_003, 3 * D, now),
            with_ancient.jackpot_payout(1_000_003, 3 * D, now)
        );
        // …while the ancient ledger's raw log stayed bounded.
        assert!(with_ancient.raw_share_count() <= 2 * 34);
    }

    #[test]
    fn rank_by_division_numbers_within_their_division() {
        let entry = |miner: &str, class: DeviceFamily, work: u64| BoardEntry {
            miner: miner.to_string(),
            class,
            eff_work: work,
            days_active: 1,
            streak_days: 0,
            hps: 1,
        };
        let board = vec![
            entry("a1", DeviceFamily::Phone, 100),
            entry("g1", DeviceFamily::Gpu, 400),
            entry("a2", DeviceFamily::Phone, 50),
            entry("f1", DeviceFamily::Farm, 999),
            entry("c1", DeviceFamily::Cpu, 10),
        ];
        let ranked = rank_by_division(board);
        // Ladder order, every family present.
        assert_eq!(ranked.len(), 4);
        assert_eq!(ranked[0].0, DeviceFamily::Phone);
        assert_eq!(ranked[1].0, DeviceFamily::Cpu);
        assert_eq!(ranked[2].0, DeviceFamily::Gpu);
        assert_eq!(ranked[3].0, DeviceFamily::Farm);
        // Ranks are per-family, 1-based, in the board's own order.
        let arms = &ranked[0].1;
        assert_eq!(arms.len(), 2);
        assert_eq!((arms[0].0, arms[0].1.miner.as_str()), (1, "a1"));
        assert_eq!((arms[1].0, arms[1].1.miner.as_str()), (2, "a2"));
        assert_eq!(
            (ranked[1].1[0].0, ranked[1].1[0].1.miner.as_str()),
            (1, "c1")
        );
        assert_eq!(
            (ranked[3].1[0].0, ranked[3].1[0].1.miner.as_str()),
            (1, "f1")
        );
        // Empty families stay present so callers render stable sections.
        assert!(rank_by_division(Vec::new())[2].1.is_empty());
    }
}

/// Full-history streak state per miner — the compacted day rollup only,
/// no raw-share scan and no window: O(miners), so the public API can
/// serve it on every stats poll. Streaks survive raw-log pruning by
/// construction (the rollup is kept forever).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreakState {
    pub miner: String,
    /// Active UTC days over the miner's full history.
    pub days_active: u32,
    /// Current streak (0 once the 48 h gap rule fires).
    pub streak_days: u32,
    /// Longest consecutive-day run ever recorded.
    pub best_streak_days: u32,
}

impl Ledger {
    /// Streak state for every miner the rollup knows (sorted by miner).
    #[must_use]
    pub fn streaks(&self, now_ts: u64) -> Vec<StreakState> {
        let mut out: Vec<StreakState> = self
            .days
            .iter()
            .map(|(miner, days)| {
                let last_ts = self.last_seen.get(miner).copied().unwrap_or(0);
                StreakState {
                    miner: miner.clone(),
                    days_active: days.len() as u32,
                    streak_days: current_streak(days, last_ts, now_ts),
                    best_streak_days: best_streak(days),
                }
            })
            .collect();
        out.sort_by(|a, b| a.miner.cmp(&b.miner));
        out
    }
}
