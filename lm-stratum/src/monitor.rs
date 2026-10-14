//! Node health tracker: tip progress, reserve accounting, disk pressure.
//!
//! Pure decision logic (no I/O) so the alerting rules are unit-tested; the
//! `monitor` binary feeds it RPC snapshots and routes the events to
//! stderr / an optional HTTP status endpoint.
//!
//! Rules (internal/testnet-runbook.md §4):
//! - tip stall: no height advance for `stall_secs` (after a startup grace
//!   of the same length, so initial block download doesn't page)
//! - reserve: a *drop* is only legitimate when the block's coinbase is
//!   large enough to have paid the bonus — the binary checks that against
//!   `getblock`; the tracker only reports the direction + delta
//! - disk: free percentage below a floor

/// One observed reserve/tip sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub height: u64,
    pub reserve: u64,
}

/// What the tracker decided about a sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    TipAdvanced {
        from: u64,
        to: u64,
    },
    TipStalled {
        height: u64,
        stalled_secs: u64,
    },
    ReserveRose {
        from: u64,
        to: u64,
    },
    ReserveFell {
        from: u64,
        to: u64,
        height: u64,
    },
}

impl Event {
    /// Human-readable line for logs / status pages.
    pub fn message(&self) -> String {
        match self {
            Event::TipAdvanced { from, to } => format!("tip advanced {from} -> {to}"),
            Event::TipStalled {
                height,
                stalled_secs,
            } => format!("TIP STALL: height {height} unchanged for {stalled_secs}s"),
            Event::ReserveRose { from, to } => format!("reserve rose {from} -> {to}"),
            Event::ReserveFell { from, to, height } => {
                format!("reserve fell {from} -> {to} at height {height}")
            }
        }
    }

    /// Critical events are the ones an operator must act on.
    pub fn is_critical(&self) -> bool {
        matches!(self, Event::TipStalled { .. })
    }
}

/// Stateful tracker; feed it one [`Snapshot`] per poll.
#[derive(Debug, Clone)]
pub struct Tracker {
    started: u64,
    stall_secs: u64,
    last: Option<Snapshot>,
    last_advance: u64,
    stall_reported: bool,
}

impl Tracker {
    pub fn new(now: u64, stall_secs: u64) -> Self {
        Self {
            started: now,
            stall_secs,
            last: None,
            last_advance: now,
            stall_reported: false,
        }
    }

    /// Resume from persisted state (restart must not false-alarm).
    pub fn resume(now: u64, stall_secs: u64, last: Option<Snapshot>, last_advance: u64) -> Self {
        Self {
            started: now,
            stall_secs,
            last,
            last_advance: last_advance.min(now),
            stall_reported: false,
        }
    }

    pub fn last(&self) -> Option<Snapshot> {
        self.last
    }

    pub fn last_advance(&self) -> u64 {
        self.last_advance
    }

    pub fn observe(&mut self, now: u64, height: u64, reserve: u64) -> Vec<Event> {
        let mut events = Vec::new();
        match self.last {
            None => {
                // First sample: baseline only.
                self.last_advance = now;
            }
            Some(prev) => {
                if height > prev.height {
                    events.push(Event::TipAdvanced {
                        from: prev.height,
                        to: height,
                    });
                    self.last_advance = now;
                    self.stall_reported = false;
                }
                if reserve > prev.reserve {
                    events.push(Event::ReserveRose {
                        from: prev.reserve,
                        to: reserve,
                    });
                } else if reserve < prev.reserve {
                    events.push(Event::ReserveFell {
                        from: prev.reserve,
                        to: reserve,
                        height,
                    });
                }
            }
        }
        // Stall: same height for longer than the window, past the grace
        // period, reported once per stall episode.
        if !self.stall_reported
            && now.saturating_sub(self.started) > self.stall_secs
            && now.saturating_sub(self.last_advance) > self.stall_secs
        {
            events.push(Event::TipStalled {
                height,
                stalled_secs: now.saturating_sub(self.last_advance),
            });
            self.stall_reported = true;
        }
        self.last = Some(Snapshot { height, reserve });
        events
    }
}

/// Free space check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskStatus {
    pub path: String,
    pub free_pct: u64,
}

/// Parse `df -P <path>` output into a free-percentage. Split out so the
/// fragile part (external tool output) is unit-tested.
pub fn parse_df(output: &str) -> Option<u64> {
    // Header + one data row; `Use%` is the 5th field, e.g. "47%".
    let row = output.lines().nth(1)?;
    let field = row.split_whitespace().nth(4)?;
    let used: u64 = field.strip_suffix('%')?.parse().ok()?;
    Some(100u64.saturating_sub(used))
}

/// Available memory as a percentage of total, from `/proc/meminfo`.
/// Uses `MemAvailable` (kernel's own reclaimable estimate).
pub fn parse_meminfo_available_pct(content: &str) -> Option<u64> {
    let field = |name: &str| -> Option<u64> {
        content
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|v| v.parse().ok())
    };
    let total = field("MemTotal:")?;
    let avail = field("MemAvailable:")?;
    if total == 0 {
        return None;
    }
    Some((avail * 100) / total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_is_baseline_only() {
        let mut t = Tracker::new(1000, 600);
        assert!(t.observe(1000, 5, 100).is_empty());
        assert_eq!(t.last().map(|s| s.height), Some(5));
    }

    #[test]
    fn advance_and_reserve_moves() {
        let mut t = Tracker::new(0, 600);
        t.observe(0, 5, 100);
        let e = t.observe(60, 6, 110);
        assert!(e.contains(&Event::TipAdvanced { from: 5, to: 6 }));
        assert!(e.contains(&Event::ReserveRose { from: 100, to: 110 }));
        let e = t.observe(120, 7, 90);
        assert!(e.contains(&Event::ReserveFell {
            from: 110,
            to: 90,
            height: 7
        }));
    }

    #[test]
    fn stall_reports_once_after_grace() {
        let mut t = Tracker::new(0, 600);
        t.observe(0, 5, 100);
        // Within the window: no stall.
        assert!(!t.observe(300, 5, 100).iter().any(|e| e.is_critical()));
        // Past the window: stall, once.
        let e = t.observe(700, 5, 100);
        assert_eq!(
            e.iter().filter(|e| e.is_critical()).count(),
            1,
            "{e:?}"
        );
        let e = t.observe(800, 5, 100);
        assert!(!e.iter().any(|e| e.is_critical()), "must not repeat: {e:?}");
        // Recovery resets; a later stall reports again.
        t.observe(900, 6, 100);
        assert!(t.observe(1700, 6, 100).iter().any(|e| e.is_critical()));
    }

    #[test]
    fn startup_grace_suppresses_immediate_stall() {
        // A node that is up but not yet advancing at process start must
        // not page: grace = stall_secs from `started`.
        let mut t = Tracker::new(0, 600);
        t.observe(0, 0, 100);
        let e = t.observe(400, 0, 100);
        assert!(!e.iter().any(|e| e.is_critical()));
    }

    #[test]
    fn resume_does_not_false_alarm() {
        // Restart after a long pause: last_advance persisted as old, but
        // the process just started, so the grace window applies.
        let t0 = Tracker::resume(10_000, 600, Some(Snapshot { height: 42, reserve: 7 }), 1_000);
        let mut t = t0;
        let e = t.observe(10_000, 42, 7);
        assert!(!e.iter().any(|e| e.is_critical()), "{e:?}");
    }

    #[test]
    fn meminfo_parsing() {
        let mi = "MemTotal:       16384000 kB\nMemFree:         1000000 kB\nMemAvailable:    8192000 kB\n";
        assert_eq!(parse_meminfo_available_pct(mi), Some(50));
        assert_eq!(parse_meminfo_available_pct("garbage"), None);
        assert_eq!(
            parse_meminfo_available_pct("MemTotal: 0 kB\nMemAvailable: 0 kB\n"),
            None
        );
    }

    #[test]
    fn df_parsing() {
        let out = "Filesystem     1024-blocks     Used Available Capacity Mounted on\n/dev/sda1        100000000 47000000  53000000      47% /\n";
        assert_eq!(parse_df(out), Some(53));
        assert_eq!(parse_df("garbage"), None);
    }
}
