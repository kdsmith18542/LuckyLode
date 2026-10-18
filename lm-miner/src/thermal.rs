//! Thermal and duty policy for `lm-miner` — the standalone miner must be
//! incapable of cooking hardware by default (the node miner has the same
//! guards; this closes the gap that let a 6-thread test run hit 90C).
//!
//! Design: [`MiningPolicy::decide`] is a pure function of (temperature,
//! chunk elapsed, internal state) returning how long to sleep. No clocks,
//! no sleeps, no sensors inside — fully unit-testable. The search loop
//! reads the sensor, asks, sleeps. Defaults: 75C cap (hysteresis 2C,
//! 30s minimum dwell, escalating 5->60s backoff) and 100% duty.

use std::time::Duration;

/// Default thermal cap in Celsius (0 = disabled via CLI).
pub const DEFAULT_MAX_TEMP_C: i64 = 75;
const HYSTERESIS_C: i32 = 2;
/// Width of the proportional duty band below the cap: full duty at
/// `cap - BAND`, ~zero duty approaching `cap`. Smooths the equilibrium
/// instead of bang-bang pause/resume (observed GPU sawtooth at cap).
const THERMAL_SOFT_BAND_C: i32 = 8;
const MIN_DWELL: Duration = Duration::from_secs(30);
const BACKOFF_MIN_SECS: u64 = 5;
const BACKOFF_MAX_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiningPolicy {
    /// `None` disables the thermal guard.
    pub max_temp_c: Option<i32>,
    /// 1..=100: percent of each chunk spent hashing.
    pub duty_percent: u8,
}

impl MiningPolicy {
    pub fn new(max_temp: i64, duty: i64) -> Result<Self, String> {
        if !(1..=100).contains(&duty) {
            return Err(format!("duty must be 1..=100, got {duty}"));
        }
        if max_temp < 0 {
            return Err(format!(
                "max-temp must be >= 0 (0 disables), got {max_temp}"
            ));
        }
        Ok(Self {
            max_temp_c: if max_temp == 0 {
                None
            } else {
                Some(max_temp as i32)
            },
            duty_percent: duty as u8,
        })
    }

    /// No guards (library default for deterministic callers/tests).
    pub fn unguarded() -> Self {
        Self {
            max_temp_c: None,
            duty_percent: 100,
        }
    }

    /// Per-thread runtime state (cloned policy + private bookkeeping).
    pub fn runtime(&self) -> PolicyRuntime {
        PolicyRuntime {
            policy: self.clone(),
            paused: false,
            pause_started: Duration::ZERO,
            backoff_secs: BACKOFF_MIN_SECS,
            clock: Duration::ZERO,
        }
    }
}

/// Private per-thread state. `clock` is a monotonic stand-in advanced by
/// the caller (real loops pass their elapsed wall time); keeping it here
/// makes dwell/backoff transitions testable without sleeping.
#[derive(Debug, Clone)]
pub struct PolicyRuntime {
    policy: MiningPolicy,
    paused: bool,
    pause_started: Duration,
    backoff_secs: u64,
    clock: Duration,
}

impl PolicyRuntime {
    /// How long the loop should sleep after a chunk. `temp_c` < 0 means the
    /// sensor is unavailable (guard becomes a no-op); `chunk_elapsed` is
    /// pure hashing time since the last decision.
    pub fn decide(&mut self, temp_c: i32, chunk_elapsed: Duration) -> Duration {
        // Wall-clock semantics: the caller sleeps for the returned duration,
        // so the clock must advance by work AND sleep — otherwise the dwell
        // check (30s) advances at work-speed only and a paused miner takes
        // ~an hour of escalating sleeps to resume (found in testing).
        self.clock += chunk_elapsed;
        let mut sleep = Duration::ZERO;

        // Duty pacing: sleep (100-duty)/duty of the work time.
        if self.policy.duty_percent < 100 {
            let d = self.policy.duty_percent as u64;
            let extra = chunk_elapsed.as_millis() as u64 * (100 - d) / d;
            sleep += Duration::from_millis(extra.min(60_000));
        }

        // Thermal guard: pause at/above cap, resume below cap-2 after the
        // minimum dwell; escalating backoff while still hot.
        if let Some(cap) = self.policy.max_temp_c {
            if temp_c >= 0 {
                if !self.paused && temp_c >= cap {
                    self.paused = true;
                    self.pause_started = self.clock;
                    self.backoff_secs = BACKOFF_MIN_SECS;
                    eprintln!(
                        "thermal: pausing at {temp_c}C (cap {cap}C, backoff {}s)",
                        self.backoff_secs
                    );
                }
                if self.paused {
                    let dwell_ok = self.clock - self.pause_started >= MIN_DWELL;
                    if dwell_ok && temp_c < cap - HYSTERESIS_C {
                        self.paused = false;
                        self.backoff_secs = BACKOFF_MIN_SECS;
                        eprintln!("thermal: resuming at {temp_c}C");
                    } else {
                        sleep += Duration::from_secs(self.backoff_secs);
                        if self.backoff_secs < BACKOFF_MAX_SECS {
                            self.backoff_secs = (self.backoff_secs * 2).min(BACKOFF_MAX_SECS);
                        }
                    }
                }
            }
        }
        // Proportional band (post pause/resume, so the resuming decision
        // also paces): scale extra sleep from 0 at `cap - BAND` to ~full
        // at `cap`, so the device settles just below the cap instead of
        // oscillating pause/resume.
        if let Some(cap) = self.policy.max_temp_c {
            if temp_c >= 0 && !self.paused && temp_c >= cap - THERMAL_SOFT_BAND_C && temp_c < cap {
                let over = (temp_c - (cap - THERMAL_SOFT_BAND_C)) as u64;
                let extra = chunk_elapsed.as_millis() as u64 * over / THERMAL_SOFT_BAND_C as u64;
                sleep += Duration::from_millis(extra.min(60_000));
            }
        }
        self.clock += sleep;
        sleep
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }
}

/// Hottest package sensor in whole Celsius, or -1 when unavailable
/// (Linux hwmon k10temp/coretemp; containers, non-x86).
pub fn read_temp_c() -> i32 {
    #[cfg(target_os = "linux")]
    {
        for hwmon in 0..16 {
            let dir = format!("/sys/class/hwmon/hwmon{hwmon}");
            let name = match std::fs::read_to_string(format!("{dir}/name")) {
                Ok(n) => n,
                Err(_) => continue,
            };
            if !(name.starts_with("k10temp") || name.starts_with("coretemp")) {
                continue;
            }
            let mut hottest = -1;
            for t in 1..=8 {
                if let Ok(s) = std::fs::read_to_string(format!("{dir}/temp{t}_input")) {
                    if let Ok(milli) = s.trim().parse::<i64>() {
                        if milli > 0 {
                            let c = (milli / 1000) as i32;
                            if c > hottest {
                                hottest = c;
                            }
                        }
                    }
                }
            }
            return hottest;
        }
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(d: Duration) -> u64 {
        d.as_millis() as u64
    }

    #[test]
    fn unguarded_policy_never_sleeps() {
        let mut rt = MiningPolicy::unguarded().runtime();
        assert_eq!(rt.decide(95, Duration::from_secs(1)), Duration::ZERO);
        assert!(!rt.is_paused());
    }

    #[test]
    fn duty_paces_proportionally() {
        let mut rt = MiningPolicy::new(0, 50).unwrap().runtime();
        // 1s of work at 50% duty -> ~1s of sleep
        assert_eq!(ms(rt.decide(-1, Duration::from_secs(1))), 1000);
        let mut rt = MiningPolicy::new(0, 25).unwrap().runtime();
        // 1s of work at 25% duty -> ~3s of sleep
        assert_eq!(ms(rt.decide(-1, Duration::from_secs(1))), 3000);
        // capped at 60s per chunk
        let mut rt = MiningPolicy::new(0, 1).unwrap().runtime();
        assert_eq!(ms(rt.decide(-1, Duration::from_secs(600))), 60_000);
    }

    #[test]
    fn unknown_sensor_never_pauses() {
        let mut rt = MiningPolicy::new(75, 100).unwrap().runtime();
        for _ in 0..10 {
            assert_eq!(rt.decide(-1, Duration::from_secs(1)), Duration::ZERO);
        }
        assert!(!rt.is_paused());
    }

    #[test]
    fn thermal_trip_dwell_resume_and_backoff() {
        let mut rt = MiningPolicy::new(75, 100).unwrap().runtime();
        // well below the cap (and below the proportional band): no pause
        assert_eq!(rt.decide(60, Duration::from_secs(1)), Duration::ZERO);
        // at cap: trip, 5s backoff
        assert_eq!(ms(rt.decide(75, Duration::from_secs(1))), 5000);
        assert!(rt.is_paused());
        // still hot: backoff escalates 10, 20 (sleeps count toward the dwell)
        assert_eq!(ms(rt.decide(80, Duration::from_secs(1))), 10_000);
        assert_eq!(ms(rt.decide(80, Duration::from_secs(1))), 20_000);
        // by now 5+10+20 = 35s of sleep have passed since the trip (>30s
        // dwell); a cool chunk resumes immediately
        assert_eq!(rt.decide(60, Duration::from_secs(1)), Duration::ZERO);
        assert!(!rt.is_paused());
        // hysteresis: 73 (the cap-2 band) must NOT re-trip — it now gets
        // proportional pacing instead (band 67..75)
        let d = rt.decide(73, Duration::from_secs(1));
        assert!(!rt.is_paused());
        assert_eq!(ms(d), 750); // over=6 of 8 -> 750ms
                                // 75 trips again with the reset backoff
        assert_eq!(ms(rt.decide(75, Duration::from_secs(1))), 5000);
    }

    #[test]
    fn hysteresis_boundary_is_exact() {
        let mut rt = MiningPolicy::new(75, 100).unwrap().runtime();
        rt.decide(75, Duration::from_secs(1)); // trip
        assert!(rt.is_paused());
        // cap-2 = 73: 73 is NOT below 73 -> stays paused
        assert!(rt.decide(73, Duration::from_secs(60)) > Duration::ZERO);
        // 72 resumes (dwell now satisfied); inside the proportional band
        // it paces rather than running full tilt, but the pause clears.
        let d = rt.decide(72, Duration::from_secs(1));
        assert!(!rt.is_paused());
        assert_eq!(ms(d), 625); // over=5 of 8
                                // well below the band: no pacing
        assert_eq!(rt.decide(60, Duration::from_secs(1)), Duration::ZERO);
    }

    #[test]
    fn dwell_advances_with_sleep_time_not_work_time() {
        // found in testing: with ~0.45s work chunks, a paused miner must resume
        // within a bounded number of decisions (sleeps advance the clock),
        // not after ~67 escalating decisions.
        let mut rt = MiningPolicy::new(75, 100).unwrap().runtime();
        rt.decide(80, Duration::from_millis(450)); // trip
        assert!(rt.is_paused());
        let mut decisions = 0;
        while rt.is_paused() {
            let d = rt.decide(70, Duration::from_millis(450));
            decisions += 1;
            assert!(
                decisions < 10,
                "dwell should be satisfied in <10 decisions, took {decisions}"
            );
            assert!(d >= Duration::ZERO);
        }
        // 5s + 10 + 20 + 40(partial) -> resumes by the 5th decision at most
        assert!(decisions <= 6, "took {decisions} decisions");
    }

    #[test]
    fn proportional_band_smooths_toward_the_cap() {
        let mut rt = MiningPolicy::new(80, 100).unwrap().runtime();
        // soft edge: no extra sleep yet
        assert_eq!(rt.decide(72, Duration::from_secs(1)), Duration::ZERO);
        // halfway: ~50% extra sleep
        assert_eq!(ms(rt.decide(76, Duration::from_secs(1))), 500);
        // near the cap: almost fully paced
        assert_eq!(ms(rt.decide(79, Duration::from_secs(1))), 875);
        // at the cap the pause logic still takes over
        assert_eq!(ms(rt.decide(80, Duration::from_secs(1))), 5000);
        assert!(rt.is_paused());
        // proportional adds to duty pacing (duty is a floor, thermal
        // adds rest on top): 1000ms duty + 500ms thermal
        let mut rt = MiningPolicy::new(80, 50).unwrap().runtime();
        assert_eq!(ms(rt.decide(76, Duration::from_secs(1))), 1500);
    }

    #[test]
    fn sensor_read_does_not_panic() {
        let t = read_temp_c();
        assert!((-1..150).contains(&t), "implausible temp {t}");
    }

    #[test]
    fn policy_validation() {
        assert!(MiningPolicy::new(75, 0).is_err());
        assert!(MiningPolicy::new(75, 101).is_err());
        assert!(MiningPolicy::new(-1, 100).is_err());
        assert!(MiningPolicy::new(0, 100).is_ok()); // 0 = off
        assert_eq!(MiningPolicy::new(0, 100).unwrap().max_temp_c, None);
    }
}
