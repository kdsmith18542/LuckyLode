//! Per-worker stats for the dashboard/API.
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
/// Workers idle longer than this disappear from the workers display
/// (dashboard/API). Purely cosmetic: they reappear the moment they
/// authorize or submit again, and accounting (`names`, `miner_row`,
/// payouts) is never filtered by it.
pub const WORKER_HIDE_SECS: u64 = 6 * 3_600;

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
    pub miner: String,
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
    workers: HashMap<String, WorkerStat>,
    /// (ts_secs, miner, work) accepted-share log; FIFO-capped and
    /// lazily trimmed past the longest window.
    log: VecDeque<(u64, String, u64)>,
}

impl WorkerStats {
    /// Register/refresh a worker at authorize time. `agent` comes from the
    /// connection's subscribe (empty when the client sent none).
    pub fn authorized(&mut self, miner: &str, agent: &str, now: u64) {
        // The worker NAME is the money identity:
        // it keys the payout ledger and doubles as the payout address, so
        // a later authorize can never redirect someone else's balance.
        let w = self.workers.entry(miner.to_string()).or_default();
        w.name = miner.to_string();
        if !agent.is_empty() {
            w.agent = agent.to_string();
        }
        if w.first_seen == 0 {
            w.first_seen = now;
        }
        w.last_seen = now;
    }

    /// Record an accepted share with its work (base-unit hashes).
    pub fn accepted(&mut self, miner: &str, work: u64, now: u64) {
        let w = self.workers.entry(miner.to_string()).or_default();
        w.shares += 1;
        if w.first_seen == 0 {
            w.first_seen = now;
        }
        w.last_seen = now;
        self.log.push_back((now, miner.to_string(), work));
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

    /// Persistable miner-id → worker-name map (payout destinations must
    /// survive a pool restart before miners reconnect).
    #[must_use]
    pub fn names(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .workers
            .iter()
            .filter(|(_, w)| !w.name.is_empty())
            .map(|(id, w)| (id.clone(), w.name.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Restore names from a persisted snapshot (offline until the miner
    /// reconnects; rates and shares start fresh by design).
    pub fn restore_names(&mut self, names: &[(String, String)]) {
        for (miner, name) in names {
            let w = self.workers.entry(miner.clone()).or_default();
            w.name = name.clone();
        }
    }

    /// Last-known connect-time agent per miner (device labels for division
    /// resolution). Live authorizes keep this fresh; restored entries let
    /// idle miners keep their declared family instead of falling back to
    /// the rate-implied one. Empty agents are skipped (unlabeled miners
    /// stay on the rate fallback by design).
    pub fn agents(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .workers
            .iter()
            .filter(|(_, w)| !w.agent.is_empty())
            .map(|(id, w)| (id.clone(), w.agent.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Restore agents from a persisted snapshot (same dormancy rule as
    /// names: entries stay hidden from the visible snapshot until the
    /// miner reconnects; never overwrites a live agent).
    pub fn restore_agents(&mut self, agents: &[(String, String)]) {
        for (miner, agent) in agents {
            let w = self.workers.entry(miner.clone()).or_default();
            if w.agent.is_empty() {
                w.agent = agent.clone();
            }
        }
    }

    /// Miner id currently registered under `name` (worker names double as
    /// payout addresses and are unique per live worker).
    #[must_use]
    pub fn miner_for_name(&self, name: &str) -> Option<String> {
        self.workers
            .iter()
            .find(|(_, w)| w.name == name)
            .map(|(id, _)| id.clone())
    }

    /// Record a rejected share (duplicate/stale/bad): keeps liveness.
    pub fn rejected(&mut self, miner: &str, now: u64) {
        let w = self.workers.entry(miner.to_string()).or_default();
        w.rejects += 1;
        w.last_seen = now;
    }

    /// Snapshot sorted by 10-minute hashrate (desc), miner id tiebreak.
    pub fn snapshot(&self, now: u64) -> Vec<WorkerSnapshot> {
        self.snapshot_filtered(now, None)
    }

    /// Dashboard/API list: like [`Self::snapshot`], but workers idle past
    /// [`WORKER_HIDE_SECS`] are omitted (they come back on their next
    /// share or authorize). Accounting callers must use the unfiltered
    /// snapshot so idle-but-owed miners stay payable.
    pub fn snapshot_visible(&self, now: u64) -> Vec<WorkerSnapshot> {
        self.snapshot_filtered(now, Some(WORKER_HIDE_SECS))
    }

    fn snapshot_filtered(&self, now: u64, hide_after: Option<u64>) -> Vec<WorkerSnapshot> {
        let mut sums: HashMap<String, [u64; 4]> = HashMap::new();
        for (ts, miner, work) in self.log.iter() {
            let age = now.saturating_sub(*ts);
            let e = sums.entry(miner.clone()).or_insert([0; 4]);
            for (i, window) in RATE_WINDOWS.iter().enumerate() {
                if age <= *window {
                    e[i] += *work;
                }
            }
        }
        let mut out: Vec<WorkerSnapshot> = self
            .workers
            .iter()
            .filter(|(_, w)| hide_after.is_none_or(|h| now.saturating_sub(w.last_seen) <= h))
            .map(|(miner, w)| {
                let sums = sums.get(miner).copied().unwrap_or([0; 4]);
                let mut rates = [0.0f64; 4];
                for (i, window) in RATE_WINDOWS.iter().enumerate() {
                    rates[i] = sums[i] as f64 / *window as f64;
                }
                WorkerSnapshot {
                    miner: miner.clone(),
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
        s.authorized("rig", "lm-miner/0.2 opencl", 1_000);
        s.accepted("rig", 60_000, 1_000);
        s.accepted("rig", 120_000, 1_030);
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
        s.authorized("a", "", 0);
        s.accepted("rig", 1, 100);
        assert!(!s.snapshot(100 + ONLINE_WINDOW_SECS).is_empty());
        assert!(!s.snapshot(100 + ONLINE_WINDOW_SECS + 1)[0].online);
    }

    #[test]
    fn old_entries_trimmed_past_longest_window() {
        let mut s = WorkerStats::default();
        s.accepted("rig", 10, 1_000);
        // A much later share drops the 24h-stale entry on insert.
        s.accepted("rig", 20, 1_000 + RATE_WINDOWS[3] + 5);
        let snap = s.snapshot(1_000 + RATE_WINDOWS[3] + 5);
        assert_eq!(snap[0].rates[3], 20.0 / 86_400.0);
    }

    #[test]
    fn rejects_track_liveness_without_work() {
        let mut s = WorkerStats::default();
        s.rejected("rig", 500);
        let snap = s.snapshot(500);
        assert_eq!((snap[0].shares, snap[0].rejects), (0, 1));
        assert!(snap[0].online);
        assert_eq!(snap[0].rates, [0.0; 4]);
    }

    #[test]
    fn snapshot_sorted_by_ten_minute_rate() {
        let mut s = WorkerStats::default();
        s.accepted("m1", 1_000, 0);
        s.accepted("m2", 10_000, 0);
        let snap = s.snapshot(0);
        assert_eq!(snap[0].miner, "m2");
        assert_eq!(snap[1].miner, "m1");
    }

    #[test]
    fn stale_workers_hidden_from_visible_snapshot() {
        let mut s = WorkerStats::default();
        s.authorized("rig", "", 0);
        s.accepted("rig", 1, 100);
        let edge = 100 + WORKER_HIDE_SECS;
        assert_eq!(s.snapshot_visible(edge).len(), 1, "visible at the 6h edge");
        assert!(s.snapshot_visible(edge + 1).is_empty(), "hidden past 6h");
        assert_eq!(s.snapshot(edge + 1).len(), 1, "accounting keeps it");
        // Returning to mining brings it straight back.
        s.accepted("rig", 2, edge + 5);
        let snap = s.snapshot_visible(edge + 5);
        assert_eq!(snap.len(), 1);
        assert!(snap[0].online);
    }

    #[test]
    fn authorizing_revives_hidden_worker() {
        let mut s = WorkerStats::default();
        s.authorized("rig", "", 0);
        s.accepted("rig", 1, 100);
        let t = 100 + WORKER_HIDE_SECS + 1;
        assert!(s.snapshot_visible(t).is_empty());
        s.authorized("rig", "", t);
        assert_eq!(s.snapshot_visible(t).len(), 1);
    }

    #[test]
    fn hidden_worker_keeps_its_name_for_payouts() {
        let mut s = WorkerStats::default();
        s.authorized("addr", "", 0);
        let t = WORKER_HIDE_SECS + 1;
        assert!(s.snapshot_visible(t).is_empty());
        assert_eq!(s.names(), vec![("addr".to_string(), "addr".to_string())]);
    }

    #[test]
    fn agents_survive_a_restart_without_reviving_rows() {
        let mut s = WorkerStats::default();
        s.authorized("gpu-rig", "lm-miner/0.2 opencl", 100);
        s.authorized("silent", "", 100); // unlabeled: no agent recorded
        assert_eq!(
            s.agents(),
            vec![("gpu-rig".to_string(), "lm-miner/0.2 opencl".to_string())]
        );
        // Restore into a fresh table: dormant (hidden) but labeled.
        let mut t = WorkerStats::default();
        t.restore_agents(&s.agents());
        t.restore_names(&s.names());
        let far = 100 + WORKER_HIDE_SECS + 1;
        assert!(
            t.snapshot_visible(far).is_empty(),
            "restore must not revive rows"
        );
        assert_eq!(
            t.agents(),
            vec![("gpu-rig".to_string(), "lm-miner/0.2 opencl".to_string())]
        );
        // A live re-authorize with a new agent wins over the restored one.
        t.authorized("gpu-rig", "lm-miner/0.3 opencl", far);
        assert_eq!(
            t.agents(),
            vec![("gpu-rig".to_string(), "lm-miner/0.3 opencl".to_string())]
        );
        // Restoring over a live agent never downgrades it.
        t.restore_agents(&[("gpu-rig".to_string(), "old-agent".to_string())]);
        assert_eq!(
            t.agents(),
            vec![("gpu-rig".to_string(), "lm-miner/0.3 opencl".to_string())]
        );
    }
}
