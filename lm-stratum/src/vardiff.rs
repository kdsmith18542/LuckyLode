//! Per-connection variable difficulty (vardiff).
//!
//! One share per [`TARGET_SECS_PER_SHARE`] (20 s) per connection: fast miners
//! get harder pool targets, slow miners easier ones, so low-end hardware
//! submits steadily instead of spamming (or never). Retargets every
//! [`RETARGET_WINDOW_SHARES`] accepted shares with proportional control
//! clamped to 4× per step — no oscillation, no runaway. Pool targets only:
//! the network target never moves. Pure integer logic, fully deterministic
//! on synthetic timelines (see tests). Bounds and pacing are pool policy,
//! not consensus.

/// Target seconds per share, per connection.
pub const TARGET_SECS_PER_SHARE: u64 = 20;
/// Accepted shares per retarget evaluation.
pub const RETARGET_WINDOW_SHARES: usize = 8;
/// Easiest pool work: 1 expected max-target hash per share.
pub const MIN_WORK: u64 = 1;
/// Hardest pool work (~1T max-target hashes/share — farm-scale ceiling).
pub const MAX_WORK: u64 = 1 << 40;
/// Max difficulty change per retarget, each direction (4×).
pub const MAX_STEP: u64 = 4;

/// Vardiff controller: feed it accepted-share timestamps, it returns a new
/// work level when a retarget fires *and changes* the level.
#[derive(Debug, Clone)]
pub struct Vardiff {
    times: Vec<u64>,
    window: usize,
    target_spb: u64,
    min_work: u64,
    max_work: u64,
}

impl Vardiff {
    pub fn new() -> Self {
        Self {
            times: Vec::new(),
            window: RETARGET_WINDOW_SHARES,
            target_spb: TARGET_SECS_PER_SHARE,
            min_work: MIN_WORK,
            max_work: MAX_WORK,
        }
    }

    /// Record an accepted share at `ts_secs` (unix time) mined at `work`.
    /// Returns `Some(new_work)` exactly when a full window elapsed and the
    /// clamped proportional level differs — the caller then issues a job.
    pub fn record(&mut self, ts: u64, work: u64) -> Option<u64> {
        self.times.push(ts);
        if self.times.len() < self.window {
            return None;
        }
        let span = self.times.last().copied().unwrap_or(ts).saturating_sub(self.times[0]);
        let expected = self.target_spb.saturating_mul(self.window.saturating_sub(1) as u64);
        // Zero-span burst (all shares in one second): maximally fast → top step.
        let raw = if span == 0 {
            work.saturating_mul(MAX_STEP)
        } else {
            (work as u128 * expected as u128 / span as u128).min(u64::MAX as u128) as u64
        };
        let lo = work.saturating_div(MAX_STEP).max(self.min_work);
        let hi = work.saturating_mul(MAX_STEP).min(self.max_work);
        let new = raw.max(lo).min(hi);
        self.times.clear();
        if new == work { None } else { Some(new) }
    }
}

impl Default for Vardiff {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical pool-target bytes for a work level: `top64 = MAX/work` in
/// bytes `[0..8]` (big-endian), rest zero. Any 256-bit value verifies via
/// the same `meets_target` — only the difficulty ordering matters.
/// `work_for_share(target_for_work(w))` returns `w` within ±1 (floor effects).
#[must_use]
pub fn target_for_work(work: u64) -> [u8; 32] {
    let top = u64::MAX / work.max(1);
    let mut t = [0u8; 32];
    t[..8].copy_from_slice(&top.to_be_bytes());
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::work_for_share;

    #[test]
    fn fast_miner_steps_up_capped() {
        let mut v = Vardiff::new();
        // 8 shares 1 s apart: 20× too fast → capped at 4×.
        for t in 0..7u64 {
            assert_eq!(v.record(t, 10), None);
        }
        assert_eq!(v.record(7, 10), Some(40));
    }

    #[test]
    fn slow_miner_steps_down() {
        let mut v = Vardiff::new();
        // 8 shares 40 s apart (span 280, expected 140): halve.
        for i in 0..7u64 {
            assert_eq!(v.record(i * 40, 100), None);
        }
        assert_eq!(v.record(7 * 40, 100), Some(50));
    }

    #[test]
    fn steady_miner_holds() {
        let mut v = Vardiff::new();
        for i in 0..7u64 {
            assert_eq!(v.record(i * 20, 64), None);
        }
        assert_eq!(v.record(7 * 20, 64), None); // exact pace: no new job
    }

    #[test]
    fn same_second_burst_max_steps() {
        let mut v = Vardiff::new();
        for _ in 0..7 {
            assert_eq!(v.record(1_000, 5), None);
        }
        assert_eq!(v.record(1_000, 5), Some(20));
    }

    #[test]
    fn bounds_pin_both_ends() {
        let mut v = Vardiff::new();
        for t in 0..7u64 {
            v.record(t, MAX_WORK);
        }
        assert_eq!(v.record(7, MAX_WORK), None); // already at ceiling
        let mut v = Vardiff::new();
        for i in 0..7u64 {
            v.record(i * 1_000, MIN_WORK);
        }
        assert_eq!(v.record(7 * 1_000, MIN_WORK), None); // already at floor
    }

    #[test]
    fn work_target_roundtrip_bounded() {
        // Canonical targets quantize to 64 bits: the roundtrip is one-sided
        // (back >= w) with error <= w^2/MAX + 1 (exact for w <= 2^32).
        for w in [1u64, 2, 3, 4, 7, 100, 999_999, 1 << 40] {
            let back = work_for_share(&target_for_work(w));
            let bound = w as u128 * w as u128 / u64::MAX as u128 + 1;
            assert!(back >= w && back as u128 - w as u128 <= bound, "w={w} back={back}");
        }
        assert_eq!(work_for_share(&target_for_work(1)), 1);
    }
}
