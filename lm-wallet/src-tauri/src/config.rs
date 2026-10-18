//! Persisted wallet settings (app config dir/config.json).

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Node JSON-RPC endpoint (localhost by default).
    pub rpc_url: String,
    pub rpc_user: String,
    pub rpc_pass: String,
    /// Pool HTTP API (dashboard + luckynode endpoints).
    pub pool_url: String,
    /// Stratum endpoint for the built-in miner (host:port).
    #[serde(default = "default_stratum")]
    pub pool_stratum: String,
    /// Default worker/payout address for the built-in miner.
    #[serde(default)]
    pub mining_worker: String,
    /// Saved contacts (address book; empty for older config files).
    #[serde(default)]
    pub contacts: Vec<Contact>,
    /// HD cursors: next receive/change index to hand out (self-custody).
    #[serde(default)]
    pub recv_next: u32,
    #[serde(default)]
    pub change_next: u32,
    /// Do-not-spend addresses: luckynode payout/bond addresses registered
    /// from this app are locked here automatically (Sybil control: spending
    /// them kills draw eligibility). Send refuses these UTXOs; the wallet
    /// section in Settings lists them with release buttons.
    #[serde(default)]
    pub protected_addrs: Vec<String>,
    /// verusd binary this app may start/stop (Settings → Node). Empty =
    /// unmanaged (the wallet only connects to a node someone else runs).
    #[serde(default)]
    pub node_bin: String,
    /// Datadir for the managed node. Empty resolves to ~/.luckylodetest.
    #[serde(default)]
    pub node_datadir: String,
    /// Extra flags for the managed node (default "-testnet"; add -mint to
    /// stake from the node wallet, -regtest for throwaway chains).
    #[serde(default = "default_node_args")]
    pub node_args: String,
}

fn default_stratum() -> String {
    "luckylode.cc:3346".to_string()
}

fn default_node_args() -> String {
    "-testnet".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Public testnet node by default (TLS via the site's reverse
            // proxy); point it at your own node (e.g. 127.0.0.1:9271) in
            // Settings to run against localhost. Testnet RPC creds are
            // public (u/p) — mainnet with the ceremony.
            // Local-first: like every standard
            // wallet (Core, Verus native), we default to YOUR node on
            // localhost. Port 19271 is the daemon's own LUCKYLODETEST
            // rpcport (chainparams.cpp; a flagless `-testnet` node binds
            // it on a clean machine — a chain conf `rpcport=` overrides it
            // per install). The shared public node is an explicit opt-in
            // preset in Settings (staking + private ops refuse it).
            rpc_url: "http://127.0.0.1:19271".to_string(),
            rpc_user: "u".to_string(),
            rpc_pass: "p".to_string(),
            pool_url: "https://luckylode.cc/pool".to_string(),
            pool_stratum: default_stratum(),
            mining_worker: String::new(),
            contacts: Vec::new(),
            recv_next: 0,
            change_next: 0,
            protected_addrs: Vec::new(),
            node_bin: String::new(),
            node_datadir: String::new(),
            node_args: default_node_args(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, s).map_err(|e| format!("write {}: {e}", path.display()))
    }
}
