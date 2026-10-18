//! Streak, comeback, and overnight share-weighting (doc 03 §3.4).
//!
//! Pool-side retention engine, pure integer math (per-mille multipliers over
//! `u64` work units, `u128` intermediates — no floats, deterministic on every
//! architecture). Days are UTC calendar days (`ts / 86_400`).
//!
//! Rules (pool policy — tunable without consensus changes, defaults frozen
//! here so every reference-pool implementation agrees):
//!
//! - **Uptime streak:** +10‰ per consecutive active day, capped at +300‰
//!   (doc 03: +1%/day up to +30%). A streak is alive while the miner's latest
//!   share is less than 48 h old; otherwise it resets to 0.
//! - **Comeback:** a return after ≥14 days idle earns +100‰ (+10%) for the
//!   7 days following the return share.
//! - **Overnight:** shares flagged `overnight` (miner ran charge-only inside
//!   the `lm-sched` overnight window) earn +50‰ (+5%).

/// Seconds per UTC day.
pub const DAY_SECS: u64 = 86_400;
/// Streak alive while the latest share is newer than this (48 h gap resets).
pub const STREAK_RESET_GAP_SECS: u64 = 48 * 3_600;
/// Streak weight per consecutive day, per-mille (+1%/day).
pub const STREAK_PER_DAY_PERMILLE: u64 = 10;
/// Streak weight cap, per-mille (+30% at 30 days).
pub const STREAK_CAP_PERMILLE: u64 = 300;
/// Idle time qualifying a return as a comeback (14 days).
pub const COMEBACK_IDLE_SECS: u64 = 14 * DAY_SECS;
/// Comeback boost duration from the return share (7 days).
pub const COMEBACK_WINDOW_SECS: u64 = 7 * DAY_SECS;
/// Comeback weight, per-mille (+10%).
pub const COMEBACK_PERMILLE: u64 = 100;
/// Overnight-session weight per flagged share, per-mille (+5%).
pub const OVERNIGHT_PERMILLE: u64 = 50;

/// UTC calendar day index for a unix timestamp.
#[must_use]
pub fn day_index(ts: u64) -> u64 {
    ts / DAY_SECS
}

/// Sorted unique active days from share timestamps.
#[must_use]
pub fn active_days(timestamps: &[u64]) -> Vec<u64> {
    let mut days: Vec<u64> = timestamps.iter().map(|&t| day_index(t)).collect();
    days.sort_unstable();
    days.dedup();
    days
}

/// Current streak length in days, given sorted unique `days`, the miner's
/// latest share `last_ts`, and `now_ts`. Returns 0 on an empty history or
/// when the 48 h gap rule fires — including the exact-boundary case
/// (`now - last == 48 h` resets; 1 s less stays alive).
#[must_use]
pub fn current_streak(days: &[u64], last_ts: u64, now_ts: u64) -> u32 {
    if days.is_empty() || now_ts.saturating_sub(last_ts) >= STREAK_RESET_GAP_SECS {
        return 0;
    }
    // Length of the consecutive-day run ending at the latest active day.
    let mut streak = 1u32;
    for i in (1..days.len()).rev() {
        if days[i] == days[i - 1] + 1 {
            streak += 1;
        } else {
            break;
        }
    }
    streak
}

/// Streak weight per-mille for a streak length: `min(days, 30) * 10`.
#[must_use]
pub fn streak_bonus_per_mille(streak_days: u32) -> u64 {
    (streak_days.min(30) as u64).saturating_mul(STREAK_PER_DAY_PERMILLE).min(STREAK_CAP_PERMILLE)
}

/// Comeback state machine over a miner's time-ordered share stream: tracks
/// the return-share timestamp so shares *following* a qualified return
/// inside the 7-day window also earn the boost.
#[derive(Debug, Clone, Default)]
pub struct Comeback {
    prev_ts: Option<u64>,
    window_end: Option<u64>,
}

impl Comeback {
    pub fn new() -> Self {
        Self::default()
    }

    /// Weight per-mille for a share at `ts`.
    ///
    /// Order-insensitive against arrival jitter: the tracked timestamp
    /// is a monotonic max, so an out-of-order share (network threads
    /// deliver with jitter) never rewinds the miner's timeline — it can
    /// neither mint a fresh comeback gap nor disturb the active window.
    /// The stored log is the replay order, so an audit replay of the
    /// log recomputes these weights exactly.
    pub fn weight_for(&mut self, ts: u64) -> u64 {
        if let Some(p) = self.prev_ts {
            if ts <= p {
                // Earlier-than-seen share (out of order): routine weight
                // from the active window only, no state change.
                return if self.window_end.is_some_and(|end| ts < end) {
                    COMEBACK_PERMILLE
                } else {
                    0
                };
            }
        }
        let idle = self.prev_ts.map(|p| ts.saturating_sub(p)).unwrap_or(0);
        self.prev_ts = Some(ts);
        if self.prev_ts.is_some() && idle >= COMEBACK_IDLE_SECS {
            self.window_end = Some(ts.saturating_add(COMEBACK_WINDOW_SECS));
            return COMEBACK_PERMILLE;
        }
        if self.window_end.is_some_and(|end| ts < end) {
            return COMEBACK_PERMILLE;
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: u64 = DAY_SECS;

    #[test]
    fn streak_counts_consecutive_run_only() {
        // Days 10..15 consecutive plus stray day 3: streak is 6, not 7.
        let days = vec![3, 10, 11, 12, 13, 14, 15];
        assert_eq!(current_streak(&days, 15 * D + 100, 15 * D + 200), 6);
        assert_eq!(streak_bonus_per_mille(6), 60);
    }

    #[test]
    fn streak_singles_and_caps() {
        assert_eq!(current_streak(&[7], 7 * D, 7 * D + 1), 1);
        assert_eq!(streak_bonus_per_mille(1), 10);
        assert_eq!(streak_bonus_per_mille(30), 300);
        assert_eq!(streak_bonus_per_mille(365), 300);
        assert!(current_streak(&[], 0, 0) == 0);
    }

    #[test]
    fn gap_boundary_resets_exactly_at_48h() {
        let days = vec![20, 21, 22];
        let last = 22 * D + 500;
        assert_eq!(current_streak(&days, last, last + STREAK_RESET_GAP_SECS - 1), 3);
        assert_eq!(current_streak(&days, last, last + STREAK_RESET_GAP_SECS), 0);
    }

    #[test]
    fn comeback_window_opens_on_qualified_return() {
        let mut c = Comeback::new();
        assert_eq!(c.weight_for(1_000), 0); // first share ever: no history
        assert_eq!(c.weight_for(2_000), 0); // 1 s idle: routine
        // 15 days idle: return share earns the boost and opens 7-day window.
        let ret = 2_000 + 15 * D;
        assert_eq!(c.weight_for(ret), 100);
        assert_eq!(c.weight_for(ret + 3 * D), 100); // inside window
        assert_eq!(c.weight_for(ret + 7 * D), 0); // window end is exclusive
        // Short idle afterwards: no new window.
        assert_eq!(c.weight_for(ret + 7 * D + 100), 0);
    }

    #[test]
    fn comeback_needs_full_fortnight() {
        let mut c = Comeback::new();
        c.weight_for(0);
        // 13 days 23:59:59 idle: not a comeback.
        assert_eq!(c.weight_for(COMEBACK_IDLE_SECS - 1), 0);
    }

    #[test]
    fn comeback_requalifies_after_second_lapse() {
        let mut c = Comeback::new();
        c.weight_for(0);
        let r1 = 20 * D;
        assert_eq!(c.weight_for(r1), 100);
        let r2 = r1 + COMEBACK_WINDOW_SECS + 20 * D; // lapse again, longer
        assert_eq!(c.weight_for(r2), 100);
    }

    #[test]
    fn out_of_order_share_never_rewinds_the_timeline() {
        // Arrival jitter: a late-delivered OLD share arriving after the
        // return must not mint a second gap or close the open window.
        let mut c = Comeback::new();
        c.weight_for(0);
        let ret = 20 * D; // comeback after 20 days
        assert_eq!(c.weight_for(ret), 100);
        // The old share (ts 5) arrives now: routine weight only, no
        // state change — a later real share still sees the open window.
        assert_eq!(c.weight_for(5), 100); // inside the active window
        assert_eq!(c.weight_for(ret + 100), 100); // window still open
        // The OLD share must NOT have reset prev_ts: the next share
        // after the window closes is routine, not a fresh comeback.
        assert_eq!(c.weight_for(ret + COMEBACK_WINDOW_SECS), 0);
        // And no phantom gap: consecutive routine shares stay 0.
        assert_eq!(c.weight_for(ret + COMEBACK_WINDOW_SECS + 10), 0);
    }

    #[test]
    fn duplicate_timestamp_is_not_a_comeback() {
        // Same ts twice (a resubmitted share): the second is out of
        // order (ts <= seen), and must not double-open a window.
        let mut c = Comeback::new();
        c.weight_for(1_000);
        assert_eq!(c.weight_for(1_000), 0);
        // Real gap measured from the ORIGINAL ts only.
        assert_eq!(c.weight_for(1_000 + COMEBACK_IDLE_SECS), 100);
    }

    #[test]
    fn long_gap_sequences_replay_identically_in_stored_order() {
        // The fixture the audit claim was missing: multi-week gaps in the
        // exact order the log stores them. Every replay of this sequence
        // recomputes the same weights (weights are baked at submit).
        let mut c = Comeback::new();
        let d = |x: u64| x * D; // day units
        assert_eq!(c.weight_for(d(0)), 0);
        assert_eq!(c.weight_for(d(21)), 100); // 3-week gap
        assert_eq!(c.weight_for(d(24)), 100); // inside the 7-day window
        assert_eq!(c.weight_for(d(70)), 100); // 46-day gap: requalifies
        assert_eq!(c.weight_for(d(73)), 100);
        assert_eq!(c.weight_for(d(80)), 0); // window closed, routine
        assert_eq!(c.weight_for(d(210)), 100); // another long lapse
    }
}
