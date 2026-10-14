//! Embedded luckynode service: heartbeat + draw cosigning from the wallet
//! (replaces the CLI daemon for desktop users; internal/18 v1).

use lm_luckynode::notarize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Service handle held in app state.
pub struct Service {
    pub stop: Arc<AtomicBool>,
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
pub fn start(pool_url: String, id: String, address: String, key: [u8; 32]) -> Service {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    std::thread::spawn(move || {
        let reg = serde_json::json!({ "id": id, "address": address, "pubkey": id }).to_string();
        if let Err(e) = crate::pool::post(&pool_url, "api/luckynode/register", &reg) {
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
                                let _ = crate::pool::post(
                                    &pool_url,
                                    "api/luckynode/register",
                                    &reg,
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
    Service { stop }
}
