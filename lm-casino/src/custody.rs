//! LKYL custody (doc 14 §14.5/§14.7): per-player deposit addresses, deposit
//! scanning/crediting, and withdrawals sent from the house wallet.
//!
//! Deposits are credited only up to the amount the node reports received at
//! the required confirmations, and each satoshi is credited at most once
//! (`credited[address]` high-water mark). Withdrawals debit the ledger first
//! (a hold) and refund it if the node send fails, so a failed send can never
//! destroy a balance.

use std::collections::BTreeMap;

use crate::ledger::Ledger;
use crate::rpc::Node;

/// One credited deposit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deposit {
    pub player: u64,
    pub micros: u64,
}

fn err(e: crate::ledger::CasError) -> String {
    format!("{e:?}")
}

/// Deposit-address and withdrawal manager.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Custody {
    addresses: BTreeMap<u64, String>,
    credited: BTreeMap<String, u64>,
    total_deposited: u64,
    total_withdrawn: u64,
    min_conf: u64,
    withdraw_min: u64,
    withdraw_max: u64,
}

impl Custody {
    /// New custody policy.
    #[must_use]
    pub fn new(min_conf: u64, withdraw_min: u64, withdraw_max: u64) -> Self {
        Self {
            addresses: BTreeMap::new(),
            credited: BTreeMap::new(),
            total_deposited: 0,
            total_withdrawn: 0,
            min_conf,
            withdraw_min,
            withdraw_max,
        }
    }

    /// The player's deposit address, created on first use.
    ///
    /// # Errors
    /// Propagates node errors.
    pub fn address_for(&mut self, node: &Node, player: u64) -> Result<String, String> {
        if let Some(a) = self.addresses.get(&player) {
            return Ok(a.clone());
        }
        let label = format!("casino-{player}");
        let addr = node.get_new_address(&label)?;
        self.addresses.insert(player, addr.clone());
        Ok(addr)
    }

    /// Known deposit address for `player`, if any.
    #[must_use]
    pub fn address_of(&self, player: u64) -> Option<&str> {
        self.addresses.get(&player).map(String::as_str)
    }

    /// Poll every known deposit address and credit any new, confirmed funds.
    ///
    /// # Errors
    /// Propagates node errors; ledger credit errors cannot occur for a
    /// positive deposit.
    pub fn scan(&mut self, node: &Node, ledger: &mut Ledger) -> Result<Vec<Deposit>, String> {
        let mut out = Vec::new();
        for (player, addr) in &self.addresses {
            let received = node.received_by_address(addr, self.min_conf)?;
            let already = self.credited.get(addr).copied().unwrap_or(0);
            if received > already {
                let delta = received - already;
                ledger.deposit(*player, delta).map_err(err)?;
                self.credited.insert(addr.clone(), received);
                self.total_deposited += delta;
                out.push(Deposit {
                    player: *player,
                    micros: delta,
                });
            }
        }
        Ok(out)
    }

    /// Withdraw `micros` from `player` to an external `address`.
    ///
    /// # Errors
    /// - amount below [`Custody::withdraw_min`] or above `withdraw_max`;
    /// - insufficient player balance;
    /// - node send failure (the held amount is refunded first).
    pub fn withdraw(
        &mut self,
        node: &Node,
        ledger: &mut Ledger,
        player: u64,
        address: &str,
        micros: u64,
    ) -> Result<String, String> {
        if micros < self.withdraw_min {
            return Err(format!("below minimum withdrawal ({})", self.withdraw_min));
        }
        if micros > self.withdraw_max {
            return Err(format!("above maximum withdrawal ({})", self.withdraw_max));
        }
        ledger.withdraw(player, micros).map_err(err)?;
        match node.send_to_address(address, micros) {
            Ok(txid) => {
                self.total_withdrawn += micros;
                Ok(txid)
            }
            Err(e) => {
                // Refund the hold; the money never left the house.
                ledger.deposit(player, micros).map_err(err)?;
                Err(e)
            }
        }
    }

    /// Total credited deposits (micro-LKYL).
    #[must_use]
    pub fn total_deposited(&self) -> u64 {
        self.total_deposited
    }

    /// Total withdrawn (micro-LKYL).
    #[must_use]
    pub fn total_withdrawn(&self) -> u64 {
        self.total_withdrawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::EdgeSplit;
    use crate::rpc::MockNode;

    fn node_with(received: f64) -> Node {
        let m = MockNode::new();
        m.set("getnewaddress", serde_json::json!("addr-1"));
        m.set("getreceivedbyaddress", serde_json::json!(received));
        Node::from_api(Box::new(m))
    }

    #[test]
    fn address_is_created_once_and_cached() {
        let node = node_with(0.0);
        let mut c = Custody::new(1, 1_000, 1_000_000_000);
        assert_eq!(c.address_for(&node, 7).unwrap(), "addr-1");
        assert_eq!(c.address_for(&node, 7).unwrap(), "addr-1");
        assert_eq!(c.address_of(7), Some("addr-1"));
    }

    #[test]
    fn scan_credits_only_the_delta_and_only_once() {
        let mut c = Custody::new(1, 1_000, 1_000_000_000);
        let mut l = Ledger::new(1_000_000_000, EdgeSplit::default(), 8_000);
        let n1 = node_with(1.0);
        c.address_for(&n1, 1).unwrap();
        let d1 = c.scan(&n1, &mut l).unwrap();
        assert_eq!(
            d1,
            vec![Deposit {
                player: 1,
                micros: 1_000_000
            }]
        );
        assert_eq!(l.balance(1), 1_000_000);
        // Re-scanning the same chain state credits nothing.
        assert!(c.scan(&n1, &mut l).unwrap().is_empty());
        // More received → only the delta is credited.
        let n2 = node_with(1.25);
        let d2 = c.scan(&n2, &mut l).unwrap();
        assert_eq!(
            d2,
            vec![Deposit {
                player: 1,
                micros: 250_000
            }]
        );
        assert_eq!(l.balance(1), 1_250_000);
    }

    #[test]
    fn withdraw_debits_sends_and_bounds() {
        let mut c = Custody::new(1, 1_000, 500_000);
        let mut l = Ledger::new(1_000_000_000, EdgeSplit::default(), 8_000);
        let n = node_with(0.0);
        c.address_for(&n, 1).unwrap();
        l.deposit(1, 100_000).unwrap();
        // A node that echoes a txid for sendtoaddress.
        let m = MockNode::new();
        m.set("sendtoaddress", serde_json::json!("txid-1"));
        let spender = Node::from_api(Box::new(m));
        // Below min.
        assert!(c.withdraw(&spender, &mut l, 1, "dest", 500).is_err());
        // Above max.
        assert!(c.withdraw(&spender, &mut l, 1, "dest", 600_000).is_err());
        // OK.
        let txid = c.withdraw(&spender, &mut l, 1, "dest", 50_000).unwrap();
        assert_eq!(txid, "txid-1");
        assert_eq!(l.balance(1), 50_000);
        assert_eq!(c.total_withdrawn(), 50_000);
        assert!(l.is_conserving());
    }

    #[test]
    fn failed_send_refunds_the_hold() {
        let mut c = Custody::new(1, 1_000, 500_000);
        let mut l = Ledger::new(1_000_000_000, EdgeSplit::default(), 8_000);
        l.deposit(1, 100_000).unwrap();
        // Mock with no sendtoaddress response → Err.
        let failing = Node::from_api(Box::new(MockNode::new()));
        assert!(c.withdraw(&failing, &mut l, 1, "dest", 50_000).is_err());
        assert_eq!(l.balance(1), 100_000);
        assert_eq!(c.total_withdrawn(), 0);
        assert!(l.is_conserving());
    }
}
