//! Device-family divisions (doc 03 §3.3): phones race phones, not servers.
//!
//! The division is the miner's *declared* device family, policed by its
//! observed rate (doc 03 §3.6 — self-declared + statistically policed).
//! The declaration rides in the miner agent string on both wires (the V1
//! subscribe agent; V2 `SetupConnection.vendor` carries the configured
//! agent verbatim and the server prints `v2 <vendor> <device>`); the pool
//! reads the family back out of it. Anything hashing at farm scale lands
//! in the farm division whatever it claims, and a claim the rate
//! contradicts falls back to the rate-implied family — claims only ever
//! get corrected *up*: a slow desktop stays a desktop, a "phone" at
//! 50 kH/s does not.
//!
//! Rate reality (measured LuckyHash, 2026-10-18): emulator/phone-class
//! CPUs ~0.02–2 kH/s, desktop CPUs ~3–10 kH/s, GTX 1080 ~15–21 kH/s.
//! The ceilings below are fraud bounds (generous on purpose: no honest
//! miner may trip one), not performance targets.

/// A phone-class claim hashing above this is not a phone.
pub const PHONE_CEIL_HPS: u64 = 10_000;
/// A CPU claim hashing above this is not a CPU miner.
pub const CPU_CEIL_HPS: u64 = 100_000;
/// Farm scale: at or above this is farm territory whatever the miner
/// claims (doc 03: farms go elsewhere).
pub const FARM_MIN_HPS: u64 = 500_000;

/// A miner's division: its declared device family, unless the observed
/// rate contradicts the claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceFamily {
    Phone,
    Cpu,
    Gpu,
    Farm,
}

impl DeviceFamily {
    /// Stable wire name (dashboard/API JSON).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceFamily::Phone => "phone",
            DeviceFamily::Cpu => "cpu",
            DeviceFamily::Gpu => "gpu",
            DeviceFamily::Farm => "farm",
        }
    }
}

/// Read the declared family out of a miner agent string. Both wires end
/// up in the same shape (`lm-miner/0.2 opencl`, `lm-miner-mobile/0.1
/// android`, `v2 lm-miner cpu`, `lucky-lode-wallet/0.1 cpu`), so one
/// parser serves both. `None` = the miner declared nothing usable.
/// Checked phone-first: a V2 phone agent contains both `android` and the
/// backend device (`cpu`).
#[must_use]
pub fn family_of(agent: &str) -> Option<DeviceFamily> {
    let a = agent.to_ascii_lowercase();
    if a.contains("android") || a.contains("mobile") || a.contains("arm") {
        Some(DeviceFamily::Phone)
    } else if a.contains("opencl") || a.contains("gpu") {
        Some(DeviceFamily::Gpu)
    } else if a.contains("cpu") {
        Some(DeviceFamily::Cpu)
    } else {
        None
    }
}

/// Resolve the division for one miner: the claim stands unless the
/// trailing rate contradicts it.
#[must_use]
pub fn division_for(claimed: Option<DeviceFamily>, hps: u64) -> DeviceFamily {
    if hps >= FARM_MIN_HPS {
        return DeviceFamily::Farm;
    }
    match claimed {
        Some(DeviceFamily::Phone) if hps <= PHONE_CEIL_HPS => DeviceFamily::Phone,
        Some(DeviceFamily::Cpu) if hps <= CPU_CEIL_HPS => DeviceFamily::Cpu,
        Some(DeviceFamily::Gpu) => DeviceFamily::Gpu,
        _ => rate_fallback(hps),
    }
}

/// Rate-implied family for miners that declared nothing (or declared
/// something the rate contradicts). The fraud ceilings double as the
/// boundaries, so the fallback can never disagree with the police rule.
fn rate_fallback(hps: u64) -> DeviceFamily {
    if hps <= PHONE_CEIL_HPS {
        DeviceFamily::Phone
    } else if hps <= CPU_CEIL_HPS {
        DeviceFamily::Cpu
    } else {
        DeviceFamily::Gpu
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_parses_both_wires_phone_first() {
        assert_eq!(family_of("lm-miner-mobile/0.1 android"), Some(DeviceFamily::Phone));
        // V2 phone agents carry the backend device too — android wins.
        assert_eq!(
            family_of("v2 lm-miner-mobile/0.1 android cpu"),
            Some(DeviceFamily::Phone)
        );
        assert_eq!(family_of("lm-miner/0.2 cpu"), Some(DeviceFamily::Cpu));
        assert_eq!(family_of("v2 lm-miner cpu"), Some(DeviceFamily::Cpu));
        assert_eq!(family_of("lm-miner/0.2 opencl"), Some(DeviceFamily::Gpu));
        assert_eq!(
            family_of("lucky-lode-wallet/0.1 opencl"),
            Some(DeviceFamily::Gpu)
        );
        assert_eq!(family_of("lucky-lode-wallet/0.1"), None);
        assert_eq!(family_of(""), None);
    }

    #[test]
    fn claims_stand_but_the_farm_bar_overrides_everything() {
        // Honest miners keep their labels at real rates.
        assert_eq!(division_for(Some(DeviceFamily::Phone), 1_500), DeviceFamily::Phone);
        assert_eq!(division_for(Some(DeviceFamily::Cpu), 8_000), DeviceFamily::Cpu);
        assert_eq!(division_for(Some(DeviceFamily::Gpu), 21_000), DeviceFamily::Gpu);
        // A farm cannot hide in a small division behind any label.
        for claimed in [None, Some(DeviceFamily::Phone), Some(DeviceFamily::Cpu), Some(DeviceFamily::Gpu)] {
            assert_eq!(division_for(claimed, 500_000), DeviceFamily::Farm);
            assert_eq!(division_for(claimed, u64::MAX), DeviceFamily::Farm);
        }
    }

    #[test]
    fn contradicted_claims_fall_back_to_rate() {
        // A "phone" at 50 kH/s is not a phone; a "cpu" at 200 kH/s is
        // not a CPU miner. Slow miners keep their (harmless) labels.
        assert_eq!(division_for(Some(DeviceFamily::Phone), 50_000), DeviceFamily::Cpu);
        assert_eq!(division_for(Some(DeviceFamily::Cpu), 200_000), DeviceFamily::Gpu);
        assert_eq!(division_for(Some(DeviceFamily::Gpu), 500), DeviceFamily::Gpu);
        assert_eq!(division_for(Some(DeviceFamily::Cpu), 500), DeviceFamily::Cpu);
        // Unlabeled miners get the rate-implied family.
        assert_eq!(division_for(None, 0), DeviceFamily::Phone);
        assert_eq!(division_for(None, 9_000), DeviceFamily::Phone);
        assert_eq!(division_for(None, 15_000), DeviceFamily::Cpu);
        assert_eq!(division_for(None, 499_999), DeviceFamily::Gpu);
    }

    #[test]
    fn wire_names_are_stable() {
        assert_eq!(DeviceFamily::Phone.as_str(), "phone");
        assert_eq!(DeviceFamily::Cpu.as_str(), "cpu");
        assert_eq!(DeviceFamily::Gpu.as_str(), "gpu");
        assert_eq!(DeviceFamily::Farm.as_str(), "farm");
    }
}
