//! Per-worker stats for the dashboard/API (internal/17 §17.1).
//!
//! Bounded by construction: one FIFO-capped share log serves the rolling
//! hashrate windows; per-worker counters are O(workers). Pure logic —
//! the server owns the lock, tests own the clock.

use std::collections::{HashMap, VecDeque};

/// Rolling hashrate windows, seconds (60s / 10m / 1h / 24h).
pub const RATE_WINDOWS: [u64; 4] = [60, 600, 3_600, 86_400];
/// Share-log cap (≈ a day of shares at reference rates; 24 B × 100k).
pub const SHARE_LOG_CAP: usize = 100_000;
/// A worker with no share in this long renders offline.
pub const ONLINE_WINDOW_SECS: u64 = 300;

#[derive(Debug, Clone, Default)]
pub struct WorkerStat {
    pub name: String,
    pub agent: String,
    pub shares: u64,
    pub rejects: u64,
    pub first_seen: u64,
    pub last_seen: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerSnapshot {
    pub miner: u64,
    pub name: String,
    pub agent: String,
    pub shares: u64,
    pub rejects: u64,
    pub first_seen: u64,
    pub last_seen: u64,
    pub online: bool,
    /// Hashrate over [`RATE_WINDOWS`] (H/s), same order.
    pub rates: [f64; 4],
}

#[derive(Debug, Default)]
pub struct WorkerStats {
    workers: HashMap<u64, WorkerStat>,
    /// (ts_secs, miner, work) accepted-share log; FIFO-capped and
    /// lazily trimmed past the longest window.
    log: VecDeque<(u64, u64, u64)>,
}

impl WorkerStats {
    /// Register/refresh a worker at authorize time. `agent` comes from the
    /// connection's subscribe (empty when the client sent none).
    pub fn authorized(&mut self, miner: u64, name: &str, agent: &str, now: u64) {
        let w = self.workers.entry(miner).or_default();
        if !name.is_empty() {
            w.name = name.to_string();
        }
        if !agent.is_empty() {
            w.agent = agent.to_string();
        }
        if w.first_seen == 0 {
            w.first_seen = now;
        }
        w.last_seen = now;
    }

    /// Record an accepted share with its work (base-unit hashes).
    pub fn accepted(&mut self, miner: u64, work: u64, now: u64) {
        let w = self.workers.entry(miner).or_default();
        w.shares += 1;
        if w.first_seen == 0 {
            w.first_seen = now;
        }
        w.last_seen = now;
        self.log.push_back((now, miner, work));
        while self.log.len() > SHARE_LOG_CAP {
            self.log.pop_front();
        }
        let cutoff = now.saturating_sub(RATE_WINDOWS[3]);
        while let Some(&(ts, _, _)) = self.log.front() {
            if ts < cutoff {
                self.log.pop_front();
            } else {
                break;
            }
        }
    }

    /// Record a rejected share (duplicate/stale/bad): keeps liveness.
    pub fn rejected(&mut self, miner: u64, now: u64) {
        let w = self.workers.entry(miner).or_default();
        w.rejects += 1;
        w.last_seen = now;
    }

    /// Snapshot sorted by 10-minute hashrate (desc), miner id tiebreak.
    pub fn snapshot(&self, now: u64) -> Vec<WorkerSnapshot> {
        let mut sums: HashMap<u64, [u64; 4]> = HashMap::new();
        for &(ts, miner, work) in self.log.iter() {
            let age = now.saturating_sub(ts);
            let e = sums.entry(miner).or_insert([0; 4]);
            for (i, window) in RATE_WINDOWS.iter().enumerate() {
                if age <= *window {
                    e[i] += work;
                }
            }
        }
        let mut out: Vec<WorkerSnapshot> = self
            .workers
            .iter()
            .map(|(&miner, w)| {
                let sums = sums.get(&miner).copied().unwrap_or([0; 4]);
                let mut rates = [0.0f64; 4];
                for (i, window) in RATE_WINDOWS.iter().enumerate() {
                    rates[i] = sums[i] as f64 / *window as f64;
                }
                WorkerSnapshot {
                    miner,
                    name: w.name.clone(),
                    agent: w.agent.clone(),
                    shares: w.shares,
                    rejects: w.rejects,
                    first_seen: w.first_seen,
                    last_seen: w.last_seen,
                    online: now.saturating_sub(w.last_seen) <= ONLINE_WINDOW_SECS,
                    rates,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            b.rates[1]
                .partial_cmp(&a.rates[1])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.miner.cmp(&b.miner))
        });
        out
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_sum_and_decay() {
        let mut s = WorkerStats::default();
        s.authorized(7, "rig", "lm-miner/0.2 opencl", 1_000);
        s.accepted(7, 60_000, 1_000);
        s.accepted(7, 120_000, 1_030);
        // At t=1060: both shares are inside 60s? No — ages 60 and 30.
        // 60s window: age<=60 includes both (inclusive boundary).
        let snap = s.snapshot(1_060);
        assert_eq!(snap.len(), 1);
        let w = &snap[0];
        assert_eq!(w.shares, 2);
        assert!(w.online);
        assert_eq!(w.rates[0], 180_000.0 / 60.0);
        // At t=1200: the first share (age 200) left the 60s window, the
        // second (age 170) too; both stay inside 10m.
        let snap = s.snapshot(1_200);
        assert_eq!(snap[0].rates[0], 0.0);
        assert_eq!(snap[0].rates[1], 180_000.0 / 600.0);
    }

    #[test]
    fn offline_after_quiet_window() {
        let mut s = WorkerStats::default();
        s.authorized(1, "a", "", 0);
        s.accepted(1, 1, 100);
        assert!(!s.snapshot(100 + ONLINE_WINDOW_SECS).is_empty());
        assert!(!s.snapshot(100 + ONLINE_WINDOW_SECS + 1)[0].online);
    }

    #[test]
    fn old_entries_trimmed_past_longest_window() {
        let mut s = WorkerStats::default();
        s.accepted(1, 10, 1_000);
        // A much later share drops the 24h-stale entry on insert.
        s.accepted(1, 20, 1_000 + RATE_WINDOWS[3] + 5);
        let snap = s.snapshot(1_000 + RATE_WINDOWS[3] + 5);
        assert_eq!(snap[0].rates[3], 20.0 / 86_400.0);
    }

    #[test]
    fn rejects_track_liveness_without_work() {
        let mut s = WorkerStats::default();
        s.rejected(9, 500);
        let snap = s.snapshot(500);
        assert_eq!((snap[0].shares, snap[0].rejects), (0, 1));
        assert!(snap[0].online);
        assert_eq!(snap[0].rates, [0.0; 4]);
    }

    #[test]
    fn snapshot_sorted_by_ten_minute_rate() {
        let mut s = WorkerStats::default();
        s.accepted(1, 1_000, 0);
        s.accepted(2, 10_000, 0);
        let snap = s.snapshot(0);
        assert_eq!(snap[0].miner, 2);
        assert_eq!(snap[1].miner, 1);
    }
}
