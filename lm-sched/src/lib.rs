//! Thermal/power-aware mining scheduler policy (lowmine doc 07).
//!
//! Pure decision logic with no platform calls: the host app samples sensors
//! (temperature, battery, charging, clock) and feeds [`Sample`]s here; the
//! scheduler answers [`Action`]. Deterministic integer math throughout
//! (temperatures in deci-Celsius) so any port — Kotlin/JNI today, Swift
//! tomorrow — behaves identically on identical inputs.
//!
//! Policy shape:
//!
//! - **Hysteresis on heat:** pause at/above `temp_pause_deci`, resume only
//!   at/below `temp_resume_deci`. No flapping.
//! - **Derate band:** within `derate_band_deci` below the pause line, run at
//!   half threads instead of stopping.
//! - **Power gate:** mine only when charging, unless `charge_only` is off or
//!   an enabled overnight window covers the current hour (with a healthier
//!   battery floor).
//! - **Battery floor:** below `batt_floor_pct`, always pause.
//! - **Unknown temperature** (`None`) skips thermal checks but never the
//!   power/battery gates — fail operational on missing sensors, fail safe
//!   on power.

/// Scheduler configuration. All thresholds are normative defaults in
/// doc 07 §7.2; pools/apps may expose a subset to users.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Pause mining at/above this temperature (deci-°C, e.g. 430 = 43.0°C).
    pub temp_pause_deci: i32,
    /// Resume only at/below this (must be < pause — hysteresis).
    pub temp_resume_deci: i32,
    /// Half-thread derate when closer than this to the pause line (deci-°C).
    pub derate_band_deci: i32,
    /// Battery floor in percent; below always pauses.
    pub batt_floor_pct: u8,
    /// If true, battery mining is allowed only inside the overnight window.
    pub charge_only: bool,
    /// Optional overnight window `(start_hour, end_hour)` in 0..24, end
    /// exclusive, may wrap midnight (e.g. `(22, 6)`).
    pub overnight: Option<(u8, u8)>,
    /// Full-speed thread count (derate halves, minimum 1).
    pub max_threads: u8,
}

impl Default for Config {
    /// Doc 07 §7.2 reference defaults (phone-first).
    fn default() -> Self {
        Self {
            temp_pause_deci: 430,
            temp_resume_deci: 380,
            derate_band_deci: 50,
            batt_floor_pct: 25,
            charge_only: true,
            overnight: Some((22, 6)),
            max_threads: 8,
        }
    }
}

/// One sensor snapshot from the host app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// SoC temperature in deci-°C; `None` if the sensor is unavailable.
    pub temp_deci: Option<i32>,
    /// Battery percent 0..=100.
    pub batt_pct: u8,
    /// External power attached.
    pub charging: bool,
    /// Local hour 0..24 (overnight window only).
    pub hour: u8,
}

/// Why mining is paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Overheat,
    LowBattery,
    NoPower,
}

/// Scheduler output for one sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    /// Whether worker threads should run.
    pub mine: bool,
    /// Thread count when mining (derated or full).
    pub threads: u8,
    /// Set when paused.
    pub reason: Option<Reason>,
    /// True while inside the overnight window (pool-side bonus accounting).
    pub overnight_active: bool,
}

/// Hysteretic thermal/power scheduler.
#[derive(Debug, Clone)]
pub struct Scheduler {
    cfg: Config,
    overheat_latch: bool,
}

impl Scheduler {
    /// `None` when `temp_resume >= temp_pause`, zero threads, or empty band.
    pub fn new(cfg: Config) -> Option<Self> {
        if cfg.temp_resume_deci >= cfg.temp_pause_deci
            || cfg.max_threads == 0
            || cfg.derate_band_deci < 0
        {
            return None;
        }
        Some(Self {
            cfg,
            overheat_latch: false,
        })
    }

    fn in_window(&self, hour: u8) -> bool {
        match self.cfg.overnight {
            None => false,
            Some((s, e)) if s == e => true, // degenerate: whole day
            Some((s, e)) if s < e => hour >= s && hour < e,
            Some((s, e)) => hour >= s || hour < e, // wraps midnight
        }
    }

    /// Fold one sample into scheduler state and answer.
    pub fn decide(&mut self, sample: &Sample) -> Action {
        let overnight_active = self.in_window(sample.hour);

        // 1. Battery floor is absolute.
        if sample.batt_pct < self.cfg.batt_floor_pct {
            self.overheat_latch = false; // power state resets thermal narrative
            return Action {
                mine: false,
                threads: 0,
                reason: Some(Reason::LowBattery),
                overnight_active,
            };
        }
        // 2. Power gate.
        let power_ok = sample.charging
            || !self.cfg.charge_only
            || (overnight_active && sample.batt_pct >= self.cfg.batt_floor_pct.saturating_add(20));
        if !power_ok {
            return Action {
                mine: false,
                threads: 0,
                reason: Some(Reason::NoPower),
                overnight_active,
            };
        }
        // 3. Thermal gate with hysteresis latch.
        if let Some(t) = sample.temp_deci {
            if t >= self.cfg.temp_pause_deci {
                self.overheat_latch = true;
            } else if t <= self.cfg.temp_resume_deci {
                self.overheat_latch = false;
            }
            if self.overheat_latch {
                return Action {
                    mine: false,
                    threads: 0,
                    reason: Some(Reason::Overheat),
                    overnight_active,
                };
            }
            // 4. Derate band just under the pause line.
            if t > self.cfg.temp_pause_deci - self.cfg.derate_band_deci {
                let threads = (self.cfg.max_threads / 2).max(1);
                return Action {
                    mine: true,
                    threads,
                    reason: None,
                    overnight_active,
                };
            }
        }
        Action {
            mine: true,
            threads: self.cfg.max_threads,
            reason: None,
            overnight_active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(temp: Option<i32>, batt: u8, charging: bool, hour: u8) -> Sample {
        Sample {
            temp_deci: temp,
            batt_pct: batt,
            charging,
            hour,
        }
    }

    #[test]
    fn rejects_degenerate_config() {
        let bad = Config {
            temp_resume_deci: 430,
            ..Config::default()
        }; // no hysteresis
        assert!(Scheduler::new(bad).is_none());
        let bad = Config {
            max_threads: 0,
            ..Config::default()
        };
        assert!(Scheduler::new(bad).is_none());
    }

    #[test]
    fn overheat_latch_holds_until_resume_line() {
        let mut sc = Scheduler::new(Config::default()).unwrap();
        // 43.0°C trips the latch.
        let a = sc.decide(&s(Some(430), 80, true, 12));
        assert!(!a.mine && a.reason == Some(Reason::Overheat));
        // Cooling to 40.0°C (between resume 38 and pause 43) stays paused.
        let b = sc.decide(&s(Some(400), 80, true, 12));
        assert!(!b.mine && b.reason == Some(Reason::Overheat));
        // 38.0°C releases.
        let c = sc.decide(&s(Some(380), 80, true, 12));
        assert!(c.mine && c.reason.is_none() && c.threads == 8);
    }

    #[test]
    fn derate_band_halves_threads() {
        let mut sc = Scheduler::new(Config::default()).unwrap();
        // 41.0°C is within 5°C under the 43.0°C line: half threads.
        let a = sc.decide(&s(Some(410), 80, true, 12));
        assert!(a.mine && a.threads == 4);
        // 37.9°C: full speed.
        let b = sc.decide(&s(Some(379), 80, true, 12));
        assert!(b.mine && b.threads == 8);
    }

    #[test]
    fn power_gates() {
        let mut sc = Scheduler::new(Config::default()).unwrap();
        // Unplugged midday (overnight 22-6 inactive): paused.
        let a = sc.decide(&s(Some(300), 80, false, 12));
        assert!(!a.mine && a.reason == Some(Reason::NoPower) && !a.overnight_active);
        // Unplugged at 23:00 with healthy battery: overnight allows it.
        let b = sc.decide(&s(Some(300), 80, false, 23));
        assert!(b.mine && b.overnight_active);
        // ...but not with a weak battery (needs floor + 20 = 45%).
        let c = sc.decide(&s(Some(300), 40, false, 23));
        assert!(!c.mine && c.reason == Some(Reason::NoPower));
        // Below the absolute floor: LowBattery, even on charger.
        let d = sc.decide(&s(Some(300), 10, true, 12));
        assert!(!d.mine && d.reason == Some(Reason::LowBattery));
    }

    #[test]
    fn overnight_window_wraps_midnight() {
        let mut sc = Scheduler::new(Config::default()).unwrap();
        for (hour, want) in [
            (21, false),
            (22, true),
            (0, true),
            (5, true),
            (6, false),
            (12, false),
        ] {
            let a = sc.decide(&s(Some(300), 80, true, hour));
            assert_eq!(a.overnight_active, want, "hour={hour}");
        }
    }

    #[test]
    fn missing_sensor_skips_thermal_not_power() {
        let mut sc = Scheduler::new(Config::default()).unwrap();
        let a = sc.decide(&s(None, 80, true, 12));
        assert!(a.mine && a.threads == 8);
        let b = sc.decide(&s(None, 10, true, 12));
        assert!(!b.mine && b.reason == Some(Reason::LowBattery));
    }

    #[test]
    fn full_day_script() {
        // Overnight mine -> morning unplug pause -> evening overheat trip.
        let mut sc = Scheduler::new(Config::default()).unwrap();
        let script = [
            (s(Some(350), 90, false, 2), true, 8),  // overnight on battery
            (s(Some(350), 90, false, 7), false, 0), // morning, unplugged
            (s(Some(350), 90, true, 9), true, 8),   // plugged at work
            (s(Some(450), 90, true, 18), false, 0), // evening heat trip
            (s(Some(400), 90, true, 19), false, 0), // cooled but latched
            (s(Some(400), 90, true, 23), false, 0), // latched despite window
        ];
        for (sample, want_mine, want_threads) in script {
            let a = sc.decide(&sample);
            assert_eq!((a.mine, a.threads), (want_mine, want_threads));
        }
    }
}
