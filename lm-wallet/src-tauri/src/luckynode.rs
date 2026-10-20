//! Embedded luckynode service: heartbeat + draw cosigning from the wallet
//! (replaces the CLI daemon for desktop users; v1).

use lm_luckynode::notarize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Service handle held in app state.
pub struct Service {
    pub stop: Arc<AtomicBool>,
    /// Payout-address key (proves ownership at each registration).
    pub addr_secret: [u8; 32],
    /// Locked bond collateral, re-sent on every registration so the lock
    /// survives even a pool state wipe.
    pub bond_utxo: Option<String>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load or generate the 32-byte ed25519 secret for a node id at `path`.
pub fn load_or_gen_key(path: &std::path::Path) -> Result<[u8; 32], String> {
    if let Ok(data) = std::fs::read(path) {
        if data.len() == 32 {
            let mut k = [0u8; 32];
            k.copy_from_slice(&data);
            return Ok(k);
        }
        return Err(format!("key file {} is not 32 bytes", path.display()));
    }
    let mut key = [0u8; 32];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut key))
        .map_err(|e| format!("/dev/urandom: {e}"))?;
    std::fs::write(path, key).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(key)
}

/// Spawn the heartbeat + cosign loop for one node. Returns the handle.
/// `addr_secret` is the payout address's key: registration proves this app
/// controls the bond/payout address (pool verifies via `verifymessage`).
pub fn start(
    pool_url: String,
    id: String,
    address: String,
    key: [u8; 32],
    addr_secret: [u8; 32],
    bond_utxo: Option<String>,
) -> Service {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let bond_first = bond_utxo.clone();
    let bond_for_svc = bond_utxo.clone();
    std::thread::spawn(move || {
        let reg = match lm_luckynode::register_body(&id, &address, &addr_secret, bond_first.as_deref()) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("luckynode {id}: register sign failed: {e}");
                serde_json::json!({ "id": id, "address": address, "pubkey": id })
            }
        };
        if let Err(e) = crate::pool::post(&pool_url, "api/luckynode/register", &reg.to_string()) {
            eprintln!("luckynode {id}: register failed: {e}");
        }
        while !stop_thread.load(Ordering::Relaxed) {
            let ts = now_secs();
            let msg = notarize::heartbeat_msg(&id, ts);
            match notarize::sign(&key, &msg) {
                Ok(sig) => {
                    let hb = serde_json::json!({
                        "id": id, "timestamp": ts, "sig": notarize::hex(&sig)
                    })
                    .to_string();
                    match crate::pool::post(&pool_url, "api/luckynode/heartbeat", &hb) {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("luckynode {id}: heartbeat: {e}");
                            if e.contains("unknown node") {
                                let reg = match lm_luckynode::register_body(&id, &address, &addr_secret, bond_utxo.as_deref()) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        eprintln!("luckynode {id}: register sign failed: {e}");
                                        serde_json::json!({ "id": id, "address": address, "pubkey": id })
                                    }
                                };
                                let _ = crate::pool::post(
                                    &pool_url,
                                    "api/luckynode/register",
                                    &reg.to_string(),
                                );
                            }
                        }
                    }
                }
                Err(e) => eprintln!("luckynode {id}: sign: {e}"),
            }
            // Cosign any pending draws.
            if let Ok(body) = crate::pool::get(&pool_url, "api/luckynode/pending") {
                let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                for p in v.as_array().cloned().unwrap_or_default() {
                    let draw_id = p.get("draw_id").and_then(|x| x.as_str()).unwrap_or("");
                    let block_hash = p.get("block_hash").and_then(|x| x.as_str()).unwrap_or("");
                    if draw_id.is_empty() || block_hash.is_empty() {
                        continue;
                    }
                    let cmsg = notarize::cosign_msg(draw_id, block_hash);
                    if let Ok(sig) = notarize::sign(&key, &cmsg) {
                        let body = serde_json::json!({
                            "draw_id": draw_id, "id": id, "sig": notarize::hex(&sig)
                        })
                        .to_string();
                        let _ = crate::pool::post(&pool_url, "api/luckynode/notarize", &body);
                    }
                }
            }
            // Sleep in slices so Stop is responsive.
            for _ in 0..12 {
                if stop_thread.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    });
    Service {
        stop,
        addr_secret,
        bond_utxo: bond_for_svc,
    }
}

/// Lock every locally-registered designated bond coin in the connected
/// node (`lockunspent`): neither the node minter nor a manual send may
/// spend tripwire coins. Locks are session-only — the node forgets them
/// on restart — so callers re-arm after (re)starts. Local ids come from
/// the app-dir key files; designations from the pool rows. Returns the
/// locked "txid:vout" list (empty = nothing designated).
pub fn lock_local_bonds(
    pool_url: &str,
    rpc: &lm_stratum::rpc::RpcClient,
    dir: &std::path::Path,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("app dir: {e}"))?;
    for e in entries.flatten() {
        let name = e.file_name().to_str().unwrap_or("").to_string();
        if !name.starts_with("luckynode-") || !name.ends_with(".key") {
            continue;
        }
        let data = std::fs::read(e.path()).unwrap_or_default();
        if data.len() != 32 {
            continue;
        }
        let mut k = [0u8; 32];
        k.copy_from_slice(&data);
        use ed25519_dalek::SigningKey;
        ids.push(notarize::hex(
            &SigningKey::from_bytes(&k).verifying_key().to_bytes(),
        ));
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let body =
        crate::pool::get(pool_url, "api/luckynodes").map_err(|e| format!("pool: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("pool json: {e}"))?;
    let mut outs = Vec::new();
    let mut locked = Vec::new();
    for n in v
        .get("nodes")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let id = n.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let u = n
            .get("bond_utxo")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        if u.is_empty() || !ids.iter().any(|i| i == id) {
            continue;
        }
        let (txid, vout) = lm_luckynode::parse_bond_utxo(u)?;
        outs.push(serde_json::json!({"txid": txid, "vout": vout}));
        locked.push(u.to_string());
    }
    if outs.is_empty() {
        return Ok(Vec::new());
    }
    rpc.call("lockunspent", serde_json::json!([false, outs]))
        .map_err(|e| format!("lockunspent: {e}"))?;
    Ok(locked)
}
