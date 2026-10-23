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
    /// Node daemon binary (luckyd) this app may start/stop (Settings → Node). Empty =
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
    /// Desktop notifications for mining/payout/draw events (Settings →
    /// Wallet). On by default; the UI dedupes repeat events per key.
    #[serde(default = "default_true")]
    pub notifications: bool,
}

fn default_true() -> bool {
    true
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
            // localhost. Port 42253 is what a flagless `-testnet`
            // node binds for RPC on a clean machine (empirical, stable
            // across reinstalls; verified again 2026-10-18 with an empty
            // datadir — the chainparams ASSETCHAINS_RPCPORT value is not
            // used by the -testnet boot path, and a chain conf `rpcport=`
            // still overrides per install). The shared public node is an
            // explicit opt-in preset in Settings.
            rpc_url: "http://127.0.0.1:42253".to_string(),
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
            notifications: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Self {
        let mut cfg: Config = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // The RPC password may live in the OS keyring instead of this
        // file (see `save`). Sentinel in the file + a live keyring
        // entry = password never touches disk; keyring unavailable
        // (locked session, fresh reboot) resolves to empty and the
        // connection prompts for re-entry in Settings.
        if cfg.rpc_pass == KEYRING_SENTINEL {
            cfg.rpc_pass = keyring_load().unwrap_or_default();
        }
        cfg
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        // Prefer the OS keyring for the RPC password; the file then
        // carries only a sentinel. The public testnet placeholder pair
        // stays in the file (public by design, and a fresh keyring
        // after reboot must not lock novices out of the default node).
        // If the keyring is unavailable the password stays in the file
        // (0600) — never block a connection on it.
        let mut for_disk = self.clone();
        let placeholder = for_disk.rpc_user == crate::node_proc::PLACEHOLDER_USER
            && for_disk.rpc_pass == crate::node_proc::PLACEHOLDER_PASS;
        if !placeholder && !for_disk.rpc_pass.is_empty() && for_disk.rpc_pass != KEYRING_SENTINEL {
            // Only move the password into the keyring when it can be read
            // back immediately. A locked/unavailable collection can accept
            // a store yet fail every load. And never let an *empty*
            // in-memory password collapse the file to a sentinel: that
            // permanently destroys the stored credential the next time it
            // loads (this is how the Advanced password field "cleared
            // itself" — any save with an empty password wiped it).
            let stored = keyring_store(&for_disk.rpc_pass);
            if stored && keyring_load().as_deref() == Some(for_disk.rpc_pass.as_str()) {
                for_disk.rpc_pass = KEYRING_SENTINEL.to_string();
            }
        }
        let s = serde_json::to_string_pretty(&for_disk).map_err(|e| e.to_string())?;
        // Atomic write: a crash (or SIGKILL) between truncate and write
        // must not leave a half-written file that the next load reads as
        // corrupt — a corrupt file resets the WHOLE config to defaults.
        // Rename keeps the tmp file's mode, but enforce 0600 on the final
        // path too (pre-existing files may predate the chmod).
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, s).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod {}: {e}", tmp.display()))?;
        }
        #[cfg(windows)]
        {
            // Windows rename fails when the destination exists.
            let _ = std::fs::remove_file(path);
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("chmod {}: {e}", path.display()))?;
        }
        Ok(())
    }
}

/// Marker stored in config.json when the real RPC password is in the
/// OS keyring.
const KEYRING_SENTINEL: &str = "@keyring";
const KEYRING_SERVICE: &str = "cc.luckylode.wallet";
const KEYRING_ACCOUNT: &str = "rpc_pass";

fn keyring_store(pass: &str) -> bool {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .and_then(|e| e.set_password(pass))
        .is_ok()
}

fn keyring_load() -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .ok()
        .and_then(|e| e.get_password().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The public testnet placeholder pair never goes to the keyring —
    /// a fresh keyring after reboot must not lock a default install out
    /// of the shared node.
    #[test]
    fn placeholder_creds_stay_in_the_file() {
        let dir = std::env::temp_dir().join(format!("lk-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let path = dir.join("config.json");
        let cfg = Config::default();
        assert_eq!(cfg.rpc_pass, "p");
        cfg.save(&path).expect("save");
        let on_disk: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.rpc_pass, "p");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty in-memory password must never collapse the file to the
    /// keyring sentinel: with nothing to store, sentinel-writing would
    /// permanently destroy a previously stored credential on the next
    /// load (the "Advanced password clears itself" bug — any save while
    /// the password was unresolved wiped it).
    #[test]
    fn empty_password_never_becomes_sentinel() {
        let dir = std::env::temp_dir().join(format!("lk-cfgempty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let path = dir.join("config.json");
        let cfg = Config {
            rpc_user: "luckywallet".to_string(),
            rpc_pass: String::new(),
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        let on_disk: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_ne!(on_disk.rpc_pass, KEYRING_SENTINEL, "empty must stay empty");
        assert!(
            !path.with_extension("tmp").exists(),
            "no tmp file left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When the OS keyring accepts the password, the file only ever
    /// carries the sentinel — and `load` resolves it back. Skipped on
    /// boxes without a usable keyring (graceful fallback keeps the
    /// plaintext path).
    #[test]
    fn real_password_roundtrips_via_keyring() {
        let secret = "test-secret-not-a-placeholder";
        if !keyring_store(secret) {
            eprintln!("keyring unavailable; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("lk-cfgkr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let path = dir.join("config.json");
        let cfg = Config {
            rpc_user: "alice".to_string(),
            rpc_pass: secret.to_string(),
            ..Config::default()
        };
        cfg.save(&path).expect("save");
        let on_disk: Config =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            on_disk.rpc_pass, KEYRING_SENTINEL,
            "file must not carry the password"
        );
        let loaded = Config::load(&path);
        assert_eq!(loaded.rpc_pass, secret, "load resolves the keyring entry");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
