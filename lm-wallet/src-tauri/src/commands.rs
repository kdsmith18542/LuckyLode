//! Tauri commands: everything the UI can ask the backend to do.

use crate::node;
use crate::{luckynode, miner, pool, AppState};
use serde_json::json;
use std::sync::atomic::Ordering;
use tauri::State;

/// Chain + node health for the top status bar.
#[tauri::command]
pub fn node_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let info = node::call(&state, "getblockchaininfo", json!([]))?;
    Ok(json!({
        "chain": info.get("chain"),
        "blocks": info.get("blocks"),
        "headers": info.get("headers"),
        "verificationprogress": info.get("verificationprogress"),
        "connections": info.get("connections"),
    }))
}

/// Wallet balances (available / immature / staking-eligible).
#[tauri::command]
pub fn wallet_balances(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let info = node::call(&state, "getwalletinfo", json!([]))?;
    Ok(json!({
        "balance": info.get("balance"),
        "unconfirmed": info.get("unconfirmed_balance"),
        "immature": info.get("immature_balance"),
        "staking_outputs": info.get("eligible_staking_outputs"),
        "staking_balance": info.get("eligible_staking_balance"),
        "txcount": info.get("txcount"),
    }))
}

#[tauri::command]
pub fn transactions(
    state: State<'_, AppState>,
    count: Option<u64>,
) -> Result<serde_json::Value, String> {
    node::call(&state, "listtransactions", json!(["*", count.unwrap_or(50)]))
}

/// Saved contacts (address book, internal/19 M2).
#[tauri::command]
pub fn contacts_list(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let cfg = state.cfg.lock().expect("cfg");
    serde_json::to_value(&cfg.contacts).map_err(|e| e.to_string())
}

/// Add or update a contact (address is the key; re-adding replaces the name).
#[tauri::command]
pub fn contact_add(
    state: State<'_, AppState>,
    name: String,
    address: String,
) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("contact needs a name".to_string());
    }
    let address = address.trim().to_string();
    if !lm_pool::payout::valid_payout_address(&address) {
        return Err(format!("'{address}' is not a valid Lucky Lode address"));
    }
    let mut cfg = state.cfg.lock().expect("cfg");
    match cfg.contacts.iter_mut().find(|c| c.address == address) {
        Some(c) => c.name = name,
        None => cfg.contacts.push(crate::config::Contact { name, address }),
    }
    cfg.save(&state.cfg_path)
}

/// Remove a contact by address.
#[tauri::command]
pub fn contact_remove(state: State<'_, AppState>, address: String) -> Result<(), String> {
    let mut cfg = state.cfg.lock().expect("cfg");
    cfg.contacts.retain(|c| c.address != address);
    cfg.save(&state.cfg_path)
}

/// QR code (SVG string) for an address — the Receive view renders it.
#[tauri::command]
pub fn address_qr(address: String) -> Result<String, String> {
    if address.is_empty() {
        return Err("no address".to_string());
    }
    let qr = qrcodegen::QrCode::encode_text(&address, qrcodegen::QrCodeEcc::Low)
        .map_err(|e| format!("qr: {e}"))?;
    let n = qr.size();
    let scale = 4i32;
    let border = 4i32;
    let dim = (n + border * 2) * scale;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {dim} {dim}\" width=\"180\" height=\"180\" shape-rendering=\"crispEdges\"><rect width=\"{dim}\" height=\"{dim}\" fill=\"#ffffff\"/>"
    );
    for y in 0..n {
        for x in 0..n {
            if qr.get_module(x, y) {
                let px = (x + border) * scale;
                let py = (y + border) * scale;
                svg += &format!(
                    "<rect x=\"{px}\" y=\"{py}\" width=\"{scale}\" height=\"{scale}\" fill=\"#0d1117\"/>"
                );
            }
        }
    }
    svg += "</svg>";
    Ok(svg)
}

/// Wallet transaction detail for the activity view (internal/19 M2).
#[tauri::command]
pub fn tx_detail(state: State<'_, AppState>, txid: String) -> Result<serde_json::Value, String> {
    let t = node::call(&state, "gettransaction", json!([txid]))?;
    Ok(json!({
        "txid": t.get("txid"),
        "amount": t.get("amount"),
        "fee": t.get("fee"),
        "confirmations": t.get("confirmations"),
        "time": t.get("time"),
        "blockhash": t.get("blockhash"),
        "blockheight": t.get("blockheight"),
        "details": t.get("details"),
    }))
}

#[tauri::command]
pub fn new_address(state: State<'_, AppState>) -> Result<String, String> {
    node::call(&state, "getnewaddress", json!([]))?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "getnewaddress: unexpected result".to_string())
}

/// Send funds (transparent). Address shape is validated here; the node is
/// the final arbiter at send time.
#[tauri::command]
pub fn send(state: State<'_, AppState>, to: String, amount: String) -> Result<String, String> {
    if !lm_pool::payout::valid_payout_address(&to) {
        return Err(format!("'{to}' is not a valid Lucky Lode address"));
    }
    let amt: f64 = amount
        .parse()
        .map_err(|_| format!("'{amount}' is not a number"))?;
    if !(amt > 0.0) {
        return Err("amount must be positive".to_string());
    }
    node::call(&state, "sendtoaddress", json!([to, amount]))?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "sendtoaddress: unexpected result".to_string())
}

/// Game/network stats for the Game view.
#[tauri::command]
pub fn game_info(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let jackpot = node::call(&state, "getjackpotinfo", json!([])).unwrap_or(json!(null));
    let mining = node::call(&state, "getmininginfo", json!([])).unwrap_or(json!(null));
    Ok(json!({ "jackpot": jackpot, "mining": mining }))
}

/// Live emission phase (docs/04 §4.1) at the current tip.
#[tauri::command]
pub fn emission_info(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let height = node::call(&state, "getblockcount", json!([]))?
        .as_u64()
        .ok_or("getblockcount: unexpected")?;
    let e = crate::emission::emission(height);
    Ok(json!({
        "height": e.height,
        "era": e.era,
        "phase": e.phase,
        "reward": e.reward,
        "net": e.net,
        "next_milestone": e.next_milestone.map(|(h, l)| json!({"height": h, "label": l})),
        "progress": e.progress,
    }))
}

/// The NEXT block's draw class (pre-committed: parent = current tip).
#[tauri::command]
pub fn next_draw(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let height = node::call(&state, "getblockcount", json!([]))?
        .as_u64()
        .ok_or("getblockcount: unexpected")?;
    let tip = node::call(&state, "getblockhash", json!([height]))?
        .as_str()
        .ok_or("getblockhash: unexpected")?
        .to_string();
    let jdiv = node::call(&state, "getjackpotinfo", json!([]))
        .ok()
        .and_then(|v| v.get("jackpotdivisor").and_then(|x| x.as_u64()))
        .unwrap_or(100);
    let pdiv = node::call(&state, "getjackpotinfo", json!([]))
        .ok()
        .and_then(|v| v.get("potdivisor").and_then(|x| x.as_u64()))
        .unwrap_or(1000);
    let next = crate::draws::next_draw(&tip, height + 1, jdiv, pdiv);
    Ok(json!({ "tip_height": height, "next": next }))
}

/// Luckynode registry + recent draws (pool API).
#[tauri::command]
pub fn luckynodes(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let url = state.cfg.lock().expect("cfg").pool_url.clone();
    let body = pool::get(&url, "api/luckynodes")?;
    serde_json::from_str(&body).map_err(|e| format!("pool json: {e}"))
}

/// Pool dashboard stats (blocks, hashrate, workers, payouts, draws).
#[tauri::command]
pub fn pool_stats(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let url = state.cfg.lock().expect("cfg").pool_url.clone();
    let body = pool::get(&url, "api/stats")?;
    serde_json::from_str(&body).map_err(|e| format!("pool json: {e}"))
}

/// Generate (or load) a luckynode identity and register it with the pool.
/// Returns the node id + the key path (0600); the key never leaves disk.
#[tauri::command]
pub fn luckynode_register(
    state: State<'_, AppState>,
    address: String,
) -> Result<serde_json::Value, String> {
    if !lm_pool::payout::valid_payout_address(&address) {
        return Err(format!("'{address}' is not a valid payout address"));
    }
    // Identity: one key file per address (stable across restarts).
    let dir = state
        .cfg_path
        .parent()
        .ok_or("no config dir")?
        .to_path_buf();
    let key_path = dir.join(format!("luckynode-{}.key", &address[..8.min(address.len())]));
    let key = luckynode::load_or_gen_key(&key_path)?;
    let id = {
        use ed25519_dalek::SigningKey;
        lm_luckynode::notarize::hex(&SigningKey::from_bytes(&key).verifying_key().to_bytes())
    };
    let url = state.cfg.lock().expect("cfg").pool_url.clone();
    let body = pool::post(
        &url,
        "api/luckynode/register",
        &json!({ "id": id, "address": address, "pubkey": id }).to_string(),
    )?;
    Ok(json!({ "id": id, "key_path": key_path.display().to_string(), "pool": body }))
}

/// Start the embedded heartbeat/cosign service for a node id.
#[tauri::command]
pub fn luckynode_service_start(
    state: State<'_, AppState>,
    id: String,
    address: String,
) -> Result<serde_json::Value, String> {
    let url = state.cfg.lock().expect("cfg").pool_url.clone();
    let dir = state
        .cfg_path
        .parent()
        .ok_or("no config dir")?
        .to_path_buf();
    let key_path = dir.join(format!("luckynode-{}.key", &address[..8.min(address.len())]));
    let key = luckynode::load_or_gen_key(&key_path)?;
    // Confirm the id matches the key (no mismatched service).
    let derived = {
        use ed25519_dalek::SigningKey;
        lm_luckynode::notarize::hex(&SigningKey::from_bytes(&key).verifying_key().to_bytes())
    };
    if derived != id {
        return Err("node id does not match the stored key for this address".to_string());
    }
    let mut services = state.services.lock().expect("services");
    if services.contains_key(&id) {
        return Ok(json!({ "running": true, "already": true }));
    }
    let svc = luckynode::start(url, id.clone(), address, key);
    services.insert(id, svc);
    Ok(json!({ "running": true }))
}

#[tauri::command]
pub fn luckynode_service_stop(
    state: State<'_, AppState>,
    id: String,
) -> Result<serde_json::Value, String> {
    let mut services = state.services.lock().expect("services");
    if let Some(svc) = services.remove(&id) {
        svc.stop.store(true, Ordering::Relaxed);
        return Ok(json!({ "running": false }));
    }
    Ok(json!({ "running": false, "was": false }))
}

#[tauri::command]
pub fn luckynode_service_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let services = state.services.lock().expect("services");
    let ids: Vec<&String> = services.keys().collect();
    Ok(json!({ "running": ids }))
}

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let cfg = state.cfg.lock().expect("cfg");
    Ok(serde_json::to_value(&*cfg).map_err(|e| e.to_string())?)
}

/// Quick connection check for the Settings tab (uses the CURRENT saved
/// config, so Save first — the button reports exactly what the app sees).
#[tauri::command]
pub fn test_rpc(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let height = node::call(&state, "getblockcount", json!([]))?;
    let chain = node::call(&state, "getblockchaininfo", json!([]))
        .ok()
        .and_then(|v| v.get("chain").cloned())
        .unwrap_or(json!(null));
    Ok(json!({ "ok": true, "height": height, "chain": chain }))
}

#[tauri::command(rename_all = "snake_case")]
pub fn set_config(
    state: State<'_, AppState>,
    rpc_url: String,
    rpc_user: String,
    rpc_pass: String,
    pool_url: String,
) -> Result<(), String> {
    let mut cfg = state.cfg.lock().expect("cfg");
    cfg.rpc_url = rpc_url;
    cfg.rpc_user = rpc_user;
    cfg.rpc_pass = rpc_pass;
    cfg.pool_url = pool_url;
    cfg.save(&state.cfg_path)
}

// ---- Built-in pool miner (internal/19) ----------------------------------

/// Persist the mining-specific settings (stratum endpoint + worker address).
#[tauri::command(rename_all = "snake_case")]
pub fn set_mining_config(
    state: State<'_, AppState>,
    pool_stratum: String,
    mining_worker: String,
) -> Result<(), String> {
    let mut cfg = state.cfg.lock().expect("cfg");
    cfg.pool_stratum = pool_stratum;
    cfg.mining_worker = mining_worker;
    cfg.save(&state.cfg_path)
}

/// Start the embedded miner. `device` = cpu | opencl | auto.
#[tauri::command(rename_all = "snake_case")]
pub fn miner_start(
    state: State<'_, AppState>,
    pool: String,
    worker: String,
    device: String,
    threads: usize,
    duty: u32,
    max_temp: i64,
    gpu_platform: Option<usize>,
    gpu_device: Option<usize>,
    gpu_batch: Option<usize>,
    max_power: Option<u32>,
) -> Result<(), String> {
    let mut slot = state.miner.lock().expect("miner");
    if let Some(m) = slot.as_ref() {
        if m.status.lock().expect("miner status").running {
            return Err("miner already running".to_string());
        }
    }
    let m = miner::start(
        pool,
        worker,
        device,
        threads,
        duty,
        max_temp,
        gpu_platform.unwrap_or(0),
        gpu_device.unwrap_or(0),
        gpu_batch.unwrap_or(512),
        max_power.unwrap_or(0),
    )?;
    *slot = Some(m);
    Ok(())
}

/// OpenCL devices for the GPU picker: [{platform, device, name}].
#[tauri::command]
pub fn miner_gpus() -> Vec<serde_json::Value> {
    lm_miner::opencl::GpuDevice::list_devices()
        .into_iter()
        .map(|(p, d, name)| json!({ "platform": p, "device": d, "name": name }))
        .collect()
}

/// Ask the miner to stop (returns immediately; status flips when the
/// search loop reaches a chunk boundary).
#[tauri::command]
pub fn miner_stop(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(m) = state.miner.lock().expect("miner").as_ref() {
        m.stop.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
pub fn miner_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let slot = state.miner.lock().expect("miner");
    match slot.as_ref() {
        Some(m) => {
            let s = m.status.lock().expect("miner status").clone();
            serde_json::to_value(s).map_err(|e| e.to_string())
        }
        None => Ok(json!({ "running": false })),
    }
}

/// Recent miner log lines (ring buffer, oldest first).
#[tauri::command]
pub fn miner_log(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let slot = state.miner.lock().expect("miner");
    Ok(slot
        .as_ref()
        .map(|m| m.log.lock().expect("miner log").iter().cloned().collect())
        .unwrap_or_default())
}
