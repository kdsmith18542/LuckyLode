//! Reference miner core for EmberHash v1 (Lucky Lode docs 02 §2.3, 05 §5.6).
//!
//! Portable-first Rust: one Argon2id instance per thread (`p = 1` scales
//! across threads, as Chukwa did). Two entry points:
//!
//! - [`search`]: parallel nonce search over `[start, start + count)` with
//!   stride partitioning (thread `i` tries `start + i`, `+ threads`, …).
//!   Deterministic for a fixed `(threads, start, count)` — the same inputs
//!   always check the same nonces.
//! - [`bench`]: timed hashing for the device-matrix measurement plan
//!   (doc 05 §5.6): per-thread and total rates, machine-readable via
//!   [`BenchReport::to_json`].
//!
//! Stratum V2 pool mining and the thermal-scheduler hookup (`lm-sched`
//! thread-count control, Android JNI) are tracked follow-ups — this crate is
//! the solo/benchmark foundation the testnet bake-off builds on.

use lm_pow::{meets_target, pow_hash};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// A found share/solution: the winning nonce and its PoW digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Solution {
    pub nonce: u64,
    pub out: [u8; 32],
}

/// Parallel nonce search.
///
/// Scans nonces in `[start, start + count)` (saturating end) across `threads`
/// threads and returns the first solution found, or `None` when the budget
/// is exhausted, `threads == 0`, or `count == 0`. `target` is a 256-bit
/// big-endian difficulty target (Bitcoin convention, see
/// `lm_pow::difficulty::compact_to_target`).
///
/// Cooperative early exit: once any thread finds a solution the rest stop at
/// their next 1024-nonce checkpoint. With several solutions in range the
/// winner is whichever thread reports first — validity is deterministic,
/// winner identity is scheduling-dependent. Single-threaded searches are
/// fully deterministic (lowest winning nonce wins).
pub fn search(
    header: &[u8],
    height: u64,
    target: &[u8; 32],
    start: u64,
    count: u64,
    threads: usize,
) -> Option<Solution> {
    if threads == 0 || count == 0 {
        return None;
    }
    let step = threads as u64;
    let end = start.saturating_add(count);
    let stop = AtomicBool::new(false);
    let found: Mutex<Option<Solution>> = Mutex::new(None);

    std::thread::scope(|s| {
        for idx in 0..threads as u64 {
            let stop = &stop;
            let found = &found;
            s.spawn(move || {
                let mut n = start.saturating_add(idx);
                // Threads past a tiny range stay idle (still correct).
                if n >= end || idx >= count {
                    return;
                }
                let mut since_check = 0u32;
                loop {
                    let out = pow_hash(header, n, height);
                    if meets_target(&out, target) {
                        if !stop.swap(true, Ordering::SeqCst) {
                            *found.lock().expect("found lock") =
                                Some(Solution { nonce: n, out });
                        }
                        return;
                    }
                    since_check += 1;
                    if since_check >= 1024 {
                        since_check = 0;
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                    n = match n.checked_add(step) {
                        Some(v) if v < end => v,
                        _ => return,
                    };
                }
            });
        }
    });

    let result = found.lock().expect("found lock").take();
    result
}

/// Benchmark report: per-thread and total hash rates for one timed run.
/// All rates in hashes/second; [`BenchReport::ms_per_hash`] is derived from
/// the wall-clock total (the number a device-matrix row compares).
#[derive(Debug, Clone, PartialEq)]
pub struct BenchReport {
    pub threads: usize,
    pub hashes_per_thread: u64,
    pub total_hashes: u64,
    pub elapsed_ms: u128,
    pub total_hps: f64,
    pub per_thread_hps: Vec<f64>,
    pub ms_per_hash: f64,
    /// Best-effort CPU label for matrix rows (`unknown` when unreadable).
    pub cpu: String,
}

impl BenchReport {
    /// Single-line JSON for device-matrix collection (no dependencies —
    /// hand-rolled escaping over a controlled alphabet).
    pub fn to_json(&self) -> String {
        let threads_json: String = self.per_thread_hps
            .iter()
            .map(|h| format!("{h:.3}"))
            .collect::<Vec<_>>()
            .join(",");
        let cpu_escaped: String = self
            .cpu
            .chars()
            .flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                c if c.is_control() => vec!['?'],
                c => vec![c],
            })
            .collect();
        format!(
            "{{\"arch\":\"{}\",\"threads\":{},\"hashes_per_thread\":{},\"total_hashes\":{},\"elapsed_ms\":{},\"total_hps\":{:.3},\"per_thread_hps\":[{threads_json}],\"ms_per_hash\":{:.3},\"cpu\":\"{cpu_escaped}\"}}",
            std::env::consts::ARCH,
            self.threads,
            self.hashes_per_thread,
            self.total_hashes,
            self.elapsed_ms,
            self.total_hps,
            self.ms_per_hash,
        )
    }
}

/// Best-effort CPU model string (Linux `/proc/cpuinfo`, else `unknown`).
/// Kept separate for testability and so matrix tooling can override it.
pub fn cpu_label() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in text.lines() {
                if let Some(model) = line.strip_prefix("model name") {
                    if let Some(v) = model.split(':').nth(1) {
                        let v = v.trim();
                        if !v.is_empty() {
                            return v.to_string();
                        }
                    }
                }
            }
        }
    }
    "unknown".to_string()
}

/// Timed hashing: each of `threads` threads evaluates `hashes_per_thread`
/// EmberHash v1 hashes (nonce = `start + thread_id * hashes_per_thread + k`).
/// Returns `None` for `threads == 0` (nothing to measure).
pub fn bench(header: &[u8], height: u64, hashes_per_thread: u64, threads: usize) -> Option<BenchReport> {
    if threads == 0 {
        return None;
    }
    let wall = Instant::now();
    let mut per_thread_hps = vec![0.0f64; threads];
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(threads);
        for t in 0..threads {
            handles.push(s.spawn(move || {
                let base = (t as u64).saturating_mul(hashes_per_thread);
                let t0 = Instant::now();
                for k in 0..hashes_per_thread {
                    let nonce = base.saturating_add(k);
                    std::hint::black_box(pow_hash(header, nonce, height));
                }
                let secs = t0.elapsed().as_secs_f64().max(1e-9);
                hashes_per_thread as f64 / secs
            }));
        }
        for (t, h) in handles.into_iter().enumerate() {
            per_thread_hps[t] = h.join().expect("bench thread");
        }
    });
    let elapsed_ms = wall.elapsed().as_millis();
    let total_hashes = hashes_per_thread.saturating_mul(threads as u64);
    let total_hps = total_hashes as f64 / wall.elapsed().as_secs_f64().max(1e-9);
    Some(BenchReport {
        threads,
        hashes_per_thread,
        total_hashes,
        elapsed_ms,
        total_hps,
        per_thread_hps,
        ms_per_hash: 1000.0 / total_hps,
        cpu: cpu_label(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_v() -> Vec<u8> {
        (0u8..80).collect()
    }

    #[test]
    fn search_rejects_empty_work() {
        let max = [0xffu8; 32];
        assert!(search(&header_v(), 1, &max, 0, 0, 1).is_none());
        assert!(search(&header_v(), 1, &max, 0, 100, 0).is_none());
    }

    #[test]
    fn impossible_target_searches_budget_then_gives_up() {
        // Zero target: nothing validates (strict-less). Small budget: fast.
        let zero = [0x00u8; 32];
        assert!(search(&header_v(), 1, &zero, 0, 3, 2).is_none());
    }

    #[test]
    fn bench_counts_hashes_and_reports_rates() {
        let r = bench(&header_v(), 1, 2, 2).expect("threads > 0");
        assert_eq!((r.threads, r.hashes_per_thread, r.total_hashes), (2, 2, 4));
        assert_eq!(r.per_thread_hps.len(), 2);
        assert!(r.total_hps > 0.0 && r.ms_per_hash > 0.0);
        assert!(r.per_thread_hps.iter().all(|&h| h > 0.0));
        assert!(bench(&header_v(), 1, 2, 0).is_none());
    }

    #[test]
    fn bench_json_row_is_well_formed() {
        let r = bench(&header_v(), 1, 1, 1).expect("threads > 0");
        let j = r.to_json();
        for key in [
            "\"arch\"",
            "\"threads\":1",
            "\"hashes_per_thread\":1",
            "\"total_hashes\":1",
            "\"total_hps\"",
            "\"per_thread_hps\"",
            "\"ms_per_hash\"",
            "\"cpu\"",
        ] {
            assert!(j.contains(key), "missing {key} in {j}");
        }
        assert!(j.starts_with('{') && j.ends_with('}'));
    }
}
