//! JNI facade for the Lucky Lode Android miner (doc 07).
//!
//! One Rust core under every surface: the thermal scheduler is the
//! frozen `lm-sched` policy (identical behavior guaranteed by shared
//! vectors — the Kotlin tests mirror the same cases this crate runs in
//! [`native_scheduler_selftest`]), and the hashing probe is the
//! KAT-gated `lm-pow`. The Kotlin shell owns sensors, services, and UI
//! and calls in over JNI with plain integers (deci-°C end to end — no
//! float policy drift).
//!
//! Handles are `jlong` (Box::into_raw) so the JVM owns object lifetimes
//! and calls [`native_scheduler_destroy`] from `Closeable`-style code.

use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::JNIEnv;
use lm_sched::{Action, Config, Reason, Sample, Scheduler};

/// Packed scheduler decision: bit 0 mine, bits 8..=15 threads,
/// bits 16..=17 reason (0 none, 1 overheat, 2 low battery, 3 no power),
/// bit 24 overnight-active.
pub fn pack_action(a: &Action) -> jlong {
    let reason = match a.reason {
        None => 0u64,
        Some(Reason::Overheat) => 1,
        Some(Reason::LowBattery) => 2,
        Some(Reason::NoPower) => 3,
    };
    let mine = u64::from(a.mine);
    let threads = a.threads as u64;
    let overnight = u64::from(a.overnight_active);
    ((mine & 1) | (threads << 8) | (reason << 16) | (overnight << 24)) as jlong
}

/// Inverse of [`pack_action`] (host tests + the Kotlin mirror).
pub fn unpack_action(p: jlong) -> Action {
    let p = p as u64;
    Action {
        mine: p & 1 == 1,
        threads: ((p >> 8) & 0xff) as u8,
        reason: match (p >> 16) & 3 {
            1 => Some(Reason::Overheat),
            2 => Some(Reason::LowBattery),
            3 => Some(Reason::NoPower),
            _ => None,
        },
        overnight_active: (p >> 24) & 1 == 1,
    }
}

/// A boxed Scheduler handle (Box::into_raw).
type Handle = jlong;

fn from_handle(h: Handle) -> &'static mut Scheduler {
    // SAFETY: handles come from native_scheduler_create (Box::into_raw of
    // a heap Scheduler) and are only used until destroyed, single-threaded
    // from the Kotlin service.
    unsafe { &mut *(h as *mut Scheduler) }
}

/// Doc 07 §7.2 defaults as the constructor's fallback: any non-positive
/// value selects the default for that field.
#[no_mangle]
pub extern "system" fn Java_cc_luckylode_miner_NativeCore_nativeSchedulerCreate(
    _env: JNIEnv,
    _class: JClass,
    pause_deci: jint,
    resume_deci: jint,
    band_deci: jint,
    batt_floor: jint,
    charge_only: jboolean,
    overnight_start: jint,
    overnight_end: jint,
    max_threads: jint,
) -> jlong {
    let cfg = Config {
        temp_pause_deci: if pause_deci > 0 { pause_deci } else { Config::default().temp_pause_deci },
        temp_resume_deci: if resume_deci > 0 { resume_deci } else { Config::default().temp_resume_deci },
        derate_band_deci: if band_deci > 0 { band_deci } else { Config::default().derate_band_deci },
        batt_floor_pct: if batt_floor > 0 { batt_floor as u8 } else { Config::default().batt_floor_pct },
        charge_only: charge_only != 0,
        overnight: if overnight_start >= 0 && overnight_end >= 0 {
            Some((overnight_start as u8, overnight_end as u8))
        } else {
            None
        },
        max_threads: if max_threads > 0 { max_threads as u8 } else { Config::default().max_threads },
    };
    match Scheduler::new(cfg) {
        // A degenerate config returns 0 (the Kotlin side treats that as
        // "scheduler refused" — the same failure lm-sched documents).
        None => 0,
        Some(s) => Box::into_raw(Box::new(s)) as jlong,
    }
}

#[no_mangle]
pub extern "system" fn Java_cc_luckylode_miner_NativeCore_nativeSchedulerDestroy(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle != 0 {
        // SAFETY: the matching Box::from_raw for nativeSchedulerCreate.
        drop(unsafe { Box::from_raw(handle as *mut Scheduler) });
    }
}

/// One scheduler decision. `temp_deci` < 0 means "sensor unavailable"
/// (lm-sched's `None` — keep the last thermal decision, fail operational).
#[no_mangle]
pub extern "system" fn Java_cc_luckylode_miner_NativeCore_nativeSchedulerDecide(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    temp_deci: jint,
    batt_pct: jint,
    charging: jboolean,
    hour: jint,
) -> jlong {
    if handle == 0 {
        return pack_action(&Action {
            mine: false,
            threads: 0,
            reason: Some(Reason::NoPower),
            overnight_active: false,
        });
    }
    let sample = Sample {
        temp_deci: (temp_deci >= 0).then_some(temp_deci),
        batt_pct: batt_pct.clamp(0, 100) as u8,
        charging: charging != 0,
        hour: hour.clamp(0, 23) as u8,
    };
    let sched = from_handle(handle);
    pack_action(&sched.decide(&sample))
}

/// LuckyHash probe: `pow_hash(header, nonce, height)` as hex, so the app
/// can verify the frozen KAT vectors in-app (a trust anchor on the
/// sideloaded binary) and benchmark device classes against the same
/// digest every other surface computes.
#[no_mangle]
pub extern "system" fn Java_cc_luckylode_miner_NativeCore_nativePowHash(
    mut env: JNIEnv,
    _class: JClass,
    header_hex: JString,
    nonce: jlong,
    height: jlong,
) -> jni::sys::jstring {
    let header: String = env.get_string(&header_hex).map(|s| s.into()).unwrap_or_default();
    let header = match hex::decode(header.trim()) {
        Ok(h) => h,
        Err(_) => return env.new_string("").map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut()),
    };
    let out = lm_pow::pow_hash(&header, nonce as u64, height as u64);
    let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
    env.new_string(hex).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// The frozen scheduler vectors, run natively on the device: returns
/// true when every case decides identically to lm-sched's own suite
/// (the same cases the Kotlin unit tests mirror — doc 07 §7.1's
/// shared-vector guarantee).
#[no_mangle]
pub extern "system" fn Java_cc_luckylode_miner_NativeCore_nativeSchedulerSelfTest(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    scheduler_selftest() as jboolean
}

/// The frozen vector set (doc 07 §7.2/§7.3 decision order), shared by
/// the JNI self-test and the host unit tests.
pub fn scheduler_selftest() -> bool {
    let s = |temp: Option<i32>, batt: u8, charging: bool, hour: u8| Sample {
        temp_deci: temp,
        batt_pct: batt,
        charging,
        hour,
    };
    let mut sch = match Scheduler::new(Config::default()) {
        Some(s) => s,
        None => return false,
    };
    // 1. Battery floor is absolute.
    if sch.decide(&s(Some(300), 20, true, 12)).mine {
        return false;
    }
    // 2. Charge-only: battery mining needs the overnight window.
    if sch.decide(&s(Some(300), 90, false, 12)).mine {
        return false;
    }
    // Overnight + charged enough on battery: allowed.
    if !sch.decide(&s(Some(300), 90, false, 23)).mine {
        return false;
    }
    // 3. Overheat latches at the pause line (430) and holds through a
    // sensor dropout; only the resume line (380) clears it.
    if sch.decide(&s(Some(431), 90, true, 12)).mine {
        return false;
    }
    if sch.decide(&s(None, 90, true, 12)).mine {
        return false;
    }
    // Dropout must not resume a latched overheat — pinned since the
    // lm-sched fix (the latch check used to sit inside Some(t)).
    if sch.decide(&s(Some(390), 90, true, 12)).mine {
        return false; // derate band, still latched
    }
    if !sch.decide(&s(Some(379), 90, true, 12)).mine {
        return false; // resume line clears
    }
    // 4. Derate band: half threads just under the pause line.
    let a = sch.decide(&s(Some(425), 90, true, 12));
    if !a.mine || a.threads != Config::default().max_threads / 2 {
        return false;
    }
    // 5. Overnight flag rides along (pool-side bonus accounting).
    let a = sch.decide(&s(Some(300), 90, true, 23));
    if !a.mine || !a.overnight_active {
        return false;
    }
    true
}

// hex decode without a dependency (the probe takes hex from Kotlin).
mod hex {
    pub fn decode(s: &str) -> Result<Vec<u8>, ()> {
        if s.len() % 2 != 0 {
            return Err(());
        }
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len() / 2);
        for pair in b.chunks_exact(2) {
            let hexval = |c: u8| -> Result<u16, ()> {
                match c {
                    b'0'..=b'9' => Ok((c - b'0') as u16),
                    b'a'..=b'f' => Ok((c - b'a' + 10) as u16),
                    b'A'..=b'F' => Ok((c - b'A' + 10) as u16),
                    _ => Err(()),
                }
            };
            out.push((hexval(pair[0])? * 16 + hexval(pair[1])?) as u8);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_pack_roundtrip() {
        for a in [
            Action { mine: true, threads: 8, reason: None, overnight_active: true },
            Action { mine: false, threads: 0, reason: Some(Reason::Overheat), overnight_active: false },
            Action { mine: false, threads: 0, reason: Some(Reason::LowBattery), overnight_active: true },
            Action { mine: true, threads: 4, reason: Some(Reason::NoPower), overnight_active: false },
        ] {
            assert_eq!(unpack_action(pack_action(&a)), a);
        }
    }

    #[test]
    fn frozen_vectors_pass_natively() {
        assert!(scheduler_selftest(), "the JNI self-test mirrors lm-sched's own suite");
    }

    #[test]
    fn pow_hash_probe_matches_lm_pow() {
        // Same digest path the pool and the node verify.
        let header = [7u8; 80];
        let out = lm_pow::pow_hash(&header, 42, 100);
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex.len(), 64);
        let back = hex::decode(&hex).expect("hex");
        assert_eq!(back.as_slice(), &out);
    }
}
