//! Owner/manager bonus engine (doc 14 §14.4): rule-based and manual exclusive
//! bonuses for **top miners** and **top casino spenders**, with a budget cap
//! and a full audit log.

/// What a bonus was awarded for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BonusKind {
    /// Exclusive bonus for a top miner (by verified share/uptime).
    TopMiner,
    /// Exclusive bonus for a top casino spender (by wager volume).
    TopSpender,
    /// A manually granted bonus.
    Manual,
}

/// One audited award.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BonusAward {
    pub player: u64,
    pub kind: BonusKind,
    pub micros: u64,
    pub reason: String,
}

/// Bonus engine error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BonusError {
    /// Amounts must be positive.
    ZeroAmount,
    /// The award would exceed the remaining budget.
    ExceedsBudget,
}

/// Budgeted bonus engine.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BonusEngine {
    budget: u64,
    awarded: u64,
    log: Vec<BonusAward>,
}

impl Default for BonusEngine {
    fn default() -> Self {
        Self::new(0)
    }
}

impl BonusEngine {
    /// New engine with a total bonus `budget` in micro-LKYL.
    #[must_use]
    pub fn new(budget: u64) -> Self {
        Self {
            budget,
            awarded: 0,
            log: Vec::new(),
        }
    }

    /// Budget still available.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.budget.saturating_sub(self.awarded)
    }

    /// Total configured budget.
    #[must_use]
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Award `micros` to `player`, enforcing the budget.
    ///
    /// # Errors
    /// [`BonusError::ZeroAmount`] / [`BonusError::ExceedsBudget`].
    pub fn award(
        &mut self,
        player: u64,
        kind: BonusKind,
        micros: u64,
        reason: impl Into<String>,
    ) -> Result<BonusAward, BonusError> {
        if micros == 0 {
            return Err(BonusError::ZeroAmount);
        }
        if micros > self.remaining() {
            return Err(BonusError::ExceedsBudget);
        }
        let award = BonusAward {
            player,
            kind,
            micros,
            reason: reason.into(),
        };
        self.awarded += micros;
        self.log.push(award.clone());
        Ok(award)
    }

    /// The `n` highest-metric entries (descending metric, tie-break player id
    /// ascending) — used to select top miners / top spenders deterministically.
    #[must_use]
    pub fn top_n(entries: &[(u64, u64)], n: usize) -> Vec<u64> {
        let mut v: Vec<(u64, u64)> = entries.to_vec();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().take(n).map(|(p, _)| p).collect()
    }

    /// Award `micros_each` to the top `n` entries. Atomic: the whole batch is
    /// refused if the budget cannot cover it.
    ///
    /// # Errors
    /// [`BonusError::ZeroAmount`] / [`BonusError::ExceedsBudget`].
    pub fn award_top(
        &mut self,
        kind: BonusKind,
        entries: &[(u64, u64)],
        n: usize,
        micros_each: u64,
        reason: impl Into<String>,
    ) -> Result<Vec<BonusAward>, BonusError> {
        if micros_each == 0 {
            return Err(BonusError::ZeroAmount);
        }
        let recipients = Self::top_n(entries, n);
        let need = micros_each.saturating_mul(recipients.len() as u64);
        if need > self.remaining() {
            return Err(BonusError::ExceedsBudget);
        }
        let reason = reason.into();
        recipients
            .into_iter()
            .map(|p| self.award(p, kind, micros_each, reason.clone()))
            .collect()
    }

    /// The audit log.
    #[must_use]
    pub fn log(&self) -> &[BonusAward] {
        &self.log
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_enforced() {
        let mut e = BonusEngine::new(1_000);
        assert_eq!(
            e.award(1, BonusKind::Manual, 0, "z"),
            Err(BonusError::ZeroAmount)
        );
        e.award(1, BonusKind::Manual, 600, "ok").unwrap();
        assert_eq!(e.remaining(), 400);
        assert_eq!(
            e.award(2, BonusKind::Manual, 500, "over"),
            Err(BonusError::ExceedsBudget)
        );
        assert_eq!(e.log().len(), 1);
    }

    #[test]
    fn top_n_orders_and_tie_breaks() {
        let entries = [(3u64, 10u64), (1, 50), (2, 50), (4, 5)];
        // 50,50 then... player id ascending on ties: 1 then 2.
        assert_eq!(BonusEngine::top_n(&entries, 2), vec![1, 2]);
        assert_eq!(BonusEngine::top_n(&entries, 3), vec![1, 2, 3]);
    }

    #[test]
    fn award_top_is_atomic_on_budget() {
        let mut e = BonusEngine::new(1_000);
        let entries = [(1u64, 10u64), (2, 20), (3, 30)];
        // 3 recipients × 400 = 1200 > 1000: refused, nothing logged.
        assert_eq!(
            e.award_top(BonusKind::TopMiner, &entries, 3, 400, "top miners"),
            Err(BonusError::ExceedsBudget)
        );
        assert!(e.log().is_empty());
        // 2 recipients × 400 = 800: allowed.
        let awarded = e
            .award_top(BonusKind::TopMiner, &entries, 2, 400, "top miners")
            .unwrap();
        assert_eq!(awarded.len(), 2);
        assert_eq!(e.remaining(), 200);
        assert_eq!(e.log().len(), 2);
    }

    #[test]
    fn top_spender_awards_are_kind_tagged() {
        let mut e = BonusEngine::new(10_000);
        let spenders = [(7u64, 9_000u64), (8, 3_000)];
        e.award_top(BonusKind::TopSpender, &spenders, 1, 100, "top spender")
            .unwrap();
        assert_eq!(e.log()[0].kind, BonusKind::TopSpender);
        assert_eq!(e.log()[0].player, 7);
    }
}
