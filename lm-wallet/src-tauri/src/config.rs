//! Persisted wallet settings (app config dir/config.json).

use serde::{Deserialize, Serialize};
use std::path::Path;

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
}

fn default_stratum() -> String {
    "luckylode.cc:3346".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Testnet default until the mainnet ceremony flips it.
            rpc_url: "http://127.0.0.1:9271".to_string(),
            rpc_user: String::new(),
            rpc_pass: String::new(),
            pool_url: "https://luckylode.cc/pool".to_string(),
            pool_stratum: default_stratum(),
            mining_worker: String::new(),
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
