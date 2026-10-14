//! House ledger (doc 14 §14.3/§14.7): player balances, bets/wins, bankroll
//! limits and automatic throttling, with **exact** money conservation.
//!
//! Invariant (holds after every operation):
//! `bankroll + Σ balances == initial_bankroll + Σ deposits − Σ withdrawals`.
//! Earmarking the edge ([`Ledger::distribute_edge`]) never moves custody; it
//! only labels already-collected coins.

use std::collections::BTreeMap;

use crate::edge::{EdgeAllocation, EdgeSplit};

/// Ledger error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasError {
    /// Amounts must be positive.
    ZeroAmount,
    /// The player cannot cover the operation.
    InsufficientFunds,
    /// The house bankroll cannot cover the required win (throttle bets).
    HouseCannotCover,
}

/// House ledger.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Ledger {
    initial_bankroll: u64,
    bankroll: u64,
    balances: BTreeMap<u64, u64>,
    total_deposits: u64,
    total_withdrawals: u64,
    total_bets: u64,
    total_wins: u64,
    distributed: u64,
    edge_split: EdgeSplit,
    edge: EdgeAllocation,
    max_win_multiple: u64,
}

impl Ledger {
    /// New ledger with a seeded house `bankroll`, an edge split and the
    /// game's max-win multiple (bounds per-bet exposure).
    #[must_use]
    pub fn new(bankroll: u64, edge_split: EdgeSplit, max_win_multiple: u64) -> Self {
        assert!(max_win_multiple > 0, "max win multiple must be positive");
        Self {
            initial_bankroll: bankroll,
            bankroll,
            balances: BTreeMap::new(),
            total_deposits: 0,
            total_withdrawals: 0,
            total_bets: 0,
            total_wins: 0,
            distributed: 0,
            edge_split,
            edge: EdgeAllocation::default(),
            max_win_multiple,
        }
    }

    /// Player balance (0 if the account has never been seen).
    #[must_use]
    pub fn balance(&self, player: u64) -> u64 {
        self.balances.get(&player).copied().unwrap_or(0)
    }

    /// Credit a deposit.
    ///
    /// # Errors
    /// [`CasError::ZeroAmount`] on a zero amount.
    pub fn deposit(&mut self, player: u64, amount: u64) -> Result<(), CasError> {
        if amount == 0 {
            return Err(CasError::ZeroAmount);
        }
        *self.balances.entry(player).or_insert(0) += amount;
        self.total_deposits += amount;
        Ok(())
    }

    /// Debit a withdrawal.
    ///
    /// # Errors
    /// [`CasError::ZeroAmount`] / [`CasError::InsufficientFunds`].
    pub fn withdraw(&mut self, player: u64, amount: u64) -> Result<(), CasError> {
        if amount == 0 {
            return Err(CasError::ZeroAmount);
        }
        let bal = self.balance(player);
        if bal < amount {
            return Err(CasError::InsufficientFunds);
        }
        *self.balances.entry(player).or_insert(0) -= amount;
        self.total_withdrawals += amount;
        Ok(())
    }

    /// Largest total bet the bankroll can cover at the max-win cap.
    #[must_use]
    pub fn max_bet(&self) -> u64 {
        self.bankroll / self.max_win_multiple
    }

    /// Whether the bankroll has fallen below `floor` (throttle new bets).
    #[must_use]
    pub fn is_throttled(&self, floor: u64) -> bool {
        self.bankroll < floor
    }

    /// Debit a bet from the player into the house bankroll.
    ///
    /// # Errors
    /// [`CasError::ZeroAmount`] / [`CasError::InsufficientFunds`] /
    /// [`CasError::HouseCannotCover`] (bet exceeds the exposure cap).
    pub fn place_bet(&mut self, player: u64, amount: u64) -> Result<(), CasError> {
        if amount == 0 {
            return Err(CasError::ZeroAmount);
        }
        if self.balance(player) < amount {
            return Err(CasError::InsufficientFunds);
        }
        if amount > self.max_bet() {
            return Err(CasError::HouseCannotCover);
        }
        *self.balances.entry(player).or_insert(0) -= amount;
        self.bankroll += amount;
        self.total_bets += amount;
        Ok(())
    }

    /// Credit a win from the house bankroll to the player.
    ///
    /// # Errors
    /// [`CasError::HouseCannotCover`] if the bankroll cannot pay it.
    pub fn settle(&mut self, player: u64, win: u64) -> Result<(), CasError> {
        if win > self.bankroll {
            return Err(CasError::HouseCannotCover);
        }
        self.bankroll -= win;
        *self.balances.entry(player).or_insert(0) += win;
        self.total_wins += win;
        Ok(())
    }

    /// Credit a house grant (bonus/airdrop) from the bankroll to a player.
    ///
    /// # Errors
    /// [`CasError::ZeroAmount`] / [`CasError::HouseCannotCover`].
    pub fn grant(&mut self, player: u64, amount: u64) -> Result<(), CasError> {
        if amount == 0 {
            return Err(CasError::ZeroAmount);
        }
        if amount > self.bankroll {
            return Err(CasError::HouseCannotCover);
        }
        self.bankroll -= amount;
        *self.balances.entry(player).or_insert(0) += amount;
        Ok(())
    }

    /// Net gaming revenue so far (bets − wins), signed.
    #[must_use]
    pub fn net_ggr(&self) -> i128 {
        self.total_bets as i128 - self.total_wins as i128
    }

    /// Earmark the not-yet-distributed positive edge across the four
    /// destinations. Returns the allocation (zero when there is nothing to
    /// distribute, e.g. while down on variance).
    pub fn distribute_edge(&mut self) -> EdgeAllocation {
        let gross = self.total_bets.saturating_sub(self.total_wins);
        let amount = gross.saturating_sub(self.distributed);
        if amount == 0 {
            return EdgeAllocation::default();
        }
        let alloc = self.edge_split.allocate(amount);
        self.edge.dev += alloc.dev;
        self.edge.pot += alloc.pot;
        self.edge.airdrop += alloc.airdrop;
        self.edge.services += alloc.services;
        self.distributed += amount;
        alloc
    }

    /// Cumulative edge earmarked to date.
    #[must_use]
    pub fn edge_buckets(&self) -> EdgeAllocation {
        self.edge
    }

    /// Total custody = bankroll + all player balances.
    #[must_use]
    pub fn custody(&self) -> u64 {
        self.bankroll + self.balances.values().sum::<u64>()
    }

    /// The conservation invariant expected of [`Ledger::custody`].
    #[must_use]
    pub fn expected_custody(&self) -> u64 {
        self.initial_bankroll + self.total_deposits - self.total_withdrawals
    }

    /// Whether the ledger satisfies the conservation invariant.
    #[must_use]
    pub fn is_conserving(&self) -> bool {
        self.custody() == self.expected_custody()
    }

    /// Current bankroll.
    #[must_use]
    pub fn bankroll(&self) -> u64 {
        self.bankroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> Ledger {
        Ledger::new(1_000_000_000, EdgeSplit::default(), 8_000)
    }

    #[test]
    fn conservation_across_a_sequence() {
        let mut l = ledger();
        l.deposit(1, 5_000_000).unwrap();
        l.deposit(2, 3_000_000).unwrap();
        l.place_bet(1, 10_000).unwrap();
        l.settle(1, 4_000).unwrap(); // loss for player
        l.place_bet(2, 10_000).unwrap();
        l.settle(2, 90_000).unwrap(); // win
        l.withdraw(1, 1_000_000).unwrap();
        l.distribute_edge();
        assert!(l.is_conserving(), "custody {}", l.custody());
        assert_eq!(l.balance(1), 5_000_000 - 10_000 + 4_000 - 1_000_000);
        assert_eq!(l.balance(2), 3_000_000 - 10_000 + 90_000);
    }

    #[test]
    fn rejects_zero_and_overspend() {
        let mut l = ledger();
        assert_eq!(l.deposit(1, 0), Err(CasError::ZeroAmount));
        assert_eq!(l.withdraw(9, 0), Err(CasError::ZeroAmount));
        l.deposit(1, 1_000).unwrap();
        assert_eq!(l.place_bet(1, 2_000), Err(CasError::InsufficientFunds));
        assert_eq!(l.withdraw(1, 2_000), Err(CasError::InsufficientFunds));
    }

    #[test]
    fn exposure_cap_bounds_bets() {
        let mut l = Ledger::new(1_000_000, EdgeSplit::default(), 8_000);
        l.deposit(1, 1_000_000).unwrap();
        assert_eq!(l.max_bet(), 125);
        assert_eq!(l.place_bet(1, 126), Err(CasError::HouseCannotCover));
        assert!(l.place_bet(1, 125).is_ok());
    }

    #[test]
    fn settle_beyond_bankroll_is_refused() {
        let mut l = Ledger::new(100, EdgeSplit::default(), 8_000);
        l.deposit(1, 1_000).unwrap();
        assert_eq!(l.settle(1, 1_000), Err(CasError::HouseCannotCover));
    }

    #[test]
    fn edge_distribution_is_exact_and_bounded() {
        let mut l = ledger();
        l.deposit(1, 10_000_000).unwrap();
        // Player bets 100k total and wins 80k: gross edge 20k.
        l.place_bet(1, 100_000).unwrap();
        l.settle(1, 80_000).unwrap();
        let a = l.distribute_edge();
        assert_eq!(a.total(), 20_000);
        assert_eq!(l.edge_buckets().total(), 20_000);
        // Nothing more to distribute.
        assert_eq!(l.distribute_edge().total(), 0);
        // A winning player must not create negative distribution.
        l.place_bet(1, 100_000).unwrap();
        l.settle(1, 250_000).unwrap();
        assert_eq!(l.distribute_edge().total(), 0);
    }

    #[test]
    fn throttle_when_bankroll_low() {
        let l = Ledger::new(500, EdgeSplit::default(), 8_000);
        assert!(l.is_throttled(1_000));
        assert!(!l.is_throttled(100));
    }

    #[test]
    fn grants_move_bankroll_to_player_and_conserve() {
        let mut l = ledger();
        l.grant(1, 500_000).unwrap();
        assert_eq!(l.balance(1), 500_000);
        assert!(l.is_conserving());
        assert_eq!(l.grant(1, 0), Err(CasError::ZeroAmount));
        assert_eq!(l.grant(1, u64::MAX), Err(CasError::HouseCannotCover));
    }
}
