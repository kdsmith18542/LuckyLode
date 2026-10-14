//! House-edge split (doc 14 §14.3).
//!
//! The ~9% edge (RTP 91%) is divided, largest-remainder exact, across four
//! destinations: the **dev fund** (docs 04 §4.2), the **Lucky Pot** top-up
//! (doc 06 §6.6), the **miner airdrop** (auto-airdrop to active
//! PoW/PoS/Luckynode participants), and future **game services**.

/// Per-mille weights for the four edge destinations. Must sum to 1000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EdgeSplit {
    pub dev_permille: u64,
    pub pot_permille: u64,
    pub airdrop_permille: u64,
    pub services_permille: u64,
}

/// An exact allocation of an edge amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct EdgeAllocation {
    pub dev: u64,
    pub pot: u64,
    pub airdrop: u64,
    pub services: u64,
}

impl EdgeAllocation {
    /// Total allocated (must equal the input amount).
    #[must_use]
    pub fn total(&self) -> u64 {
        self.dev + self.pot + self.airdrop + self.services
    }
}

impl Default for EdgeSplit {
    fn default() -> Self {
        Self {
            dev_permille: 300,
            pot_permille: 250,
            airdrop_permille: 250,
            services_permille: 200,
        }
    }
}

impl EdgeSplit {
    /// Whether the weights are a valid per-mille partition.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.dev_permille + self.pot_permille + self.airdrop_permille + self.services_permille
            == 1000
    }

    /// Split `amount` micro-LKYL across the four destinations, exactly.
    ///
    /// # Panics
    /// Panics if the weights do not sum to 1000 (call [`is_valid`] first).
    #[must_use]
    pub fn allocate(&self, amount: u64) -> EdgeAllocation {
        assert!(self.is_valid(), "edge weights must sum to 1000");
        let parts = [
            self.dev_permille,
            self.pot_permille,
            self.airdrop_permille,
            self.services_permille,
        ];
        let mut out = [0u64; 4];
        let mut assigned = 0u64;
        let mut remainders: Vec<(u64, usize)> = Vec::with_capacity(4);
        for (i, w) in parts.iter().enumerate() {
            let num = amount as u128 * *w as u128;
            out[i] = (num / 1000) as u64;
            assigned += out[i];
            remainders.push(((num % 1000) as u64, i));
        }
        // Largest-remainder dust, fixed-order tie-break (dev, pot, airdrop, services).
        let mut dust = amount - assigned;
        remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for (_, i) in remainders {
            if dust == 0 {
                break;
            }
            out[i] += 1;
            dust -= 1;
        }
        EdgeAllocation {
            dev: out[0],
            pot: out[1],
            airdrop: out[2],
            services: out[3],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valid_and_sums_to_1000() {
        assert!(EdgeSplit::default().is_valid());
    }

    #[test]
    fn allocation_conserves_amount() {
        let s = EdgeSplit::default();
        for amount in [0u64, 1, 7, 999, 1_000_000, 123_457, u64::MAX / 4] {
            let a = s.allocate(amount);
            assert_eq!(a.total(), amount, "amount {amount}");
        }
    }

    #[test]
    fn allocation_is_proportional() {
        let s = EdgeSplit::default();
        let a = s.allocate(1_000_000);
        assert_eq!(a.dev, 300_000);
        assert_eq!(a.pot, 250_000);
        assert_eq!(a.airdrop, 250_000);
        assert_eq!(a.services, 200_000);
    }

    #[test]
    fn dust_goes_to_largest_remainders() {
        // 1 micro over 30/25/25/20 -> floors 0, dust 1 -> dev (largest remainder).
        let a = EdgeSplit::default().allocate(1);
        assert_eq!(a.dev, 1);
        assert_eq!(a.pot + a.airdrop + a.services, 0);
    }

    #[test]
    #[should_panic(expected = "sum to 1000")]
    fn invalid_weights_panic() {
        let bad = EdgeSplit {
            dev_permille: 1,
            ..EdgeSplit::default()
        };
        let _ = bad.allocate(1000);
    }
}
