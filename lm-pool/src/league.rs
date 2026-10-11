//! Device-class leagues (doc 03 §3.3): phones race phones, not servers.
//!
//! Classification is pool-side from each miner's observed share rate over a
//! trailing window, with self-declared classes policed statistically (doc 03
//! §3.6 — attestation and clustering live in the pool server, not here).
//! Thresholds are pool policy, frozen here so reference pools agree:
//!
//! - 📱 Phone: sustained < 10 MH/s
//! - 🖥️ MiniPC/SBC: 10–60 MH/s
//! - 💻 Laptop/desktop: 60–500 MH/s
//! - 🏭 Farm: above 500 MH/s — still paid normally, but ineligible for
//!   league prizes (doc 03: farms go elsewhere).

/// Phone league ceiling in hashes/second (exclusive).
pub const PHONE_MAX_HPS: u64 = 10_000_000;
/// MiniPC/SBC league ceiling in hashes/second (exclusive).
pub const MINI_MAX_HPS: u64 = 60_000_000;
/// League eligibility ceiling: above this is farm territory (no prizes).
pub const FARM_MIN_HPS: u64 = 500_000_000;

/// A miner's league for one payout/season, from observed rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceClass {
    Phone,
    MiniPc,
    Desktop,
    Farm,
}

impl DeviceClass {
    /// Eligible for league prizes and seasonal status (farms are paid, not feted).
    #[must_use]
    pub fn prize_eligible(self) -> bool {
        !matches!(self, DeviceClass::Farm)
    }
}

/// Classify an observed sustained rate (hashes/second, trailing window).
#[must_use]
pub fn classify(hps: u64) -> DeviceClass {
    if hps < PHONE_MAX_HPS {
        DeviceClass::Phone
    } else if hps < MINI_MAX_HPS {
        DeviceClass::MiniPc
    } else if hps < FARM_MIN_HPS {
        DeviceClass::Desktop
    } else {
        DeviceClass::Farm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_match_doc_classes() {
        assert_eq!(classify(0), DeviceClass::Phone);
        assert_eq!(classify(9_999_999), DeviceClass::Phone);
        assert_eq!(classify(10_000_000), DeviceClass::MiniPc);
        assert_eq!(classify(60_000_000 - 1), DeviceClass::MiniPc);
        assert_eq!(classify(60_000_000), DeviceClass::Desktop);
        assert_eq!(classify(500_000_000 - 1), DeviceClass::Desktop);
        assert_eq!(classify(500_000_000), DeviceClass::Farm);
        assert_eq!(classify(u64::MAX), DeviceClass::Farm);
    }

    #[test]
    fn only_farms_miss_prizes() {
        assert!(DeviceClass::Phone.prize_eligible());
        assert!(DeviceClass::MiniPc.prize_eligible());
        assert!(DeviceClass::Desktop.prize_eligible());
        assert!(!DeviceClass::Farm.prize_eligible());
    }
}
