//! Tauri commands: everything the UI can ask the backend to do.

use crate::node;
use crate::node_proc;
use crate::{luckynode, miner, pool, wallet, AppState};
use lm_keys::chain::Network;
use lm_stratum::rpc::RpcClient;
use serde_json::json;
use std::sync::atomic::Ordering;
use tauri::State;

/// Chain + node health for the top status bar, plus managed-node state.
/// Never fails when the node is down: chain fields go null and the UI
/// shows the managed Start button instead of an error.
#[tauri::command(async)]
pub fn node_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    // Reap a managed child that died on its own (non-blocking).
    let managed = {
        let mut slot = state.node_proc.lock().expect("node proc");
        match slot.as_mut() {
            Some(m) => match m.child.try_wait() {
                Ok(None) => json!({"managed": true, "running": true, "pid": m.pid}),
                _ => {
                    slot.take();
                    json!({"managed": false, "running": false, "pid": null})
                }
            },
            None => json!({"managed": false, "running": false, "pid": null}),
        }
    };
    // Binary configured (without it Start refuses immediately)?
    let (bin_set, shared_node) = {
        let cfg = state.cfg.lock().expect("cfg");
        (!cfg.node_bin.trim().is_empty(), is_shared_url(&cfg.rpc_url))
    };
    let info = node::call(&state, "getblockchaininfo", json!([])).ok();
    Ok(json!({
        "chain": info.as_ref().and_then(|v| v.get("chain")).unwrap_or(&json!(null)),
        "blocks": info.as_ref().and_then(|v| v.get("blocks")).unwrap_or(&json!(null)),
        "headers": info.as_ref().and_then(|v| v.get("headers")).unwrap_or(&json!(null)),
        "verificationprogress": info.as_ref().and_then(|v| v.get("verificationprogress")).unwrap_or(&json!(null)),
        "connections": info.as_ref().and_then(|v| v.get("connections")).unwrap_or(&json!(null)),
        "managed": managed.get("managed"),
        "managed_running": managed.get("running"),
        "managed_pid": managed.get("pid"),
        "node_bin_set": bin_set,
        "shared_node": shared_node,
    }))
}

/// Is this URL the shared public node preset?
pub fn is_shared_url(url: &str) -> bool {
    url.contains("luckylode.cc/node")
}

/// What a fresh install can connect to, without typing anything:
/// the found verusd binaries and the default datadir a managed node
/// would use. Powers the Settings connection picker.
#[tauri::command]
pub fn node_discover() -> Result<serde_json::Value, String> {
    Ok(json!({
        "candidates": node_proc::find_verusd(),
        "default_datadir": node_proc::default_datadir().ok(),
    }))
}

/// One-click connection presets (Settings → Node). Nobody should need to
/// know what an RPC credential is to use this wallet:
/// - "shared": Lucky Lode's public testnet node over TLS (creds are the
///   published testnet pair — public by design, embedded here so the
///   user never types them).
/// - "local": a node this app manages on this machine; placeholder/empty
///   creds are replaced with wallet-generated ones (the wallet passes
///   them to the node itself — the user never sees them).
#[tauri::command]
pub fn use_connection_preset(state: State<'_, AppState>, preset: String) -> Result<serde_json::Value, String> {
    let mut cfg = state.cfg.lock().expect("cfg");
    let (url, generated) = match preset.as_str() {
        "shared" => {
            cfg.rpc_url = "https://luckylode.cc/node/".to_string();
            cfg.rpc_user = node_proc::PLACEHOLDER_USER.to_string();
            cfg.rpc_pass = node_proc::PLACEHOLDER_PASS.to_string();
            (cfg.rpc_url.clone(), false)
        }
        "local" => {
            cfg.rpc_url = format!("http://127.0.0.1:{}", node_proc::DEFAULT_RPC_PORT);
            let generated = node_proc::needs_generated_creds(&cfg.rpc_user, &cfg.rpc_pass);
            if generated {
                let (u, p) = node_proc::generate_rpc_creds();
                cfg.rpc_user = u;
                cfg.rpc_pass = p;
            }
            (cfg.rpc_url.clone(), generated)
        }
        other => return Err(format!("unknown preset {other:?} (shared | local)")),
    };
    cfg.save(&state.cfg_path)?;
    Ok(json!({ "rpc_url": url, "generated_creds": generated }))
}

/// Start YOUR node on this machine. Zero-config path: if no binary is
/// configured the wallet auto-detects verusd (PATH + per-OS install
/// locations) and remembers the hit, and if the RPC credentials are the
/// shipped placeholders it generates a pair and passes them to the node
/// itself — the user never types either. Loopback only — same guard as
/// the staking toggle: with a remote rpc_url the wallet would spawn a
/// node it then ignores, so that is refused. Foreground child, no
/// supervision: if it crashes it stays down until you Start again.
#[tauri::command(async)]
pub fn node_start(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    // Resolve bin + creds (auto-detect / auto-generate), persisting any
    // change so Start/Stop and the connection stay consistent.
    let (bin, datadir_cfg, user, pass, url, extra, auto_bin) = {
        let mut cfg = state.cfg.lock().expect("cfg");
        let auto_bin = cfg.node_bin.trim().is_empty();
        if auto_bin {
            match node_proc::find_verusd().first() {
                Some(found) => cfg.node_bin = found.clone(),
                None => {
                    return Err(
                        "no verusd found — install the Lucky Lode node and put verusd on your PATH, or set its location under Advanced".to_string(),
                    );
                }
            }
        }
        if node_proc::needs_generated_creds(&cfg.rpc_user, &cfg.rpc_pass) {
            let (u, p) = node_proc::generate_rpc_creds();
            cfg.rpc_user = u;
            cfg.rpc_pass = p;
        }
        cfg.save(&state.cfg_path)?;
        (
            cfg.node_bin.clone(),
            cfg.node_datadir.clone(),
            cfg.rpc_user.clone(),
            cfg.rpc_pass.clone(),
            cfg.rpc_url.clone(),
            cfg.node_args.clone(),
            auto_bin,
        )
    };
    let host = node_host(&url);
    if !is_loopback_host(&host) {
        return Err(format!(
            "node start refuses {host}: a node this app manages runs on this machine — switch Settings → Node to \"this computer\" first"
        ));
    }
    {
        let mut slot = state.node_proc.lock().expect("node proc");
        if let Some(m) = slot.as_mut() {
            match m.child.try_wait() {
                Ok(None) => {
                    return Err(format!("managed node already running (pid {}) — stop it first", m.pid));
                }
                _ => {
                    slot.take(); // reaped a dead child; proceed to start
                }
            }
        }
    }
    let datadir = node_proc::resolve_datadir(&datadir_cfg)?;
    std::fs::create_dir_all(&datadir).map_err(|e| format!("mkdir {datadir}: {e}"))?;
    let port = node_proc::rpc_port_from_url(&url);
    let args = node_proc::build_args(&datadir, &user, &pass, port, &extra);
    let child = node_proc::spawn(&bin, &args)?;
    let pid = child.id();
    {
        let mut slot = state.node_proc.lock().expect("node proc");
        *slot = Some(node_proc::ManagedNode { child, pid });
    }
    // Wait for RPC (rebuilds the client from current settings each poll).
    let rpc = node::client(&state)?;
    match node_proc::wait_for_rpc(&rpc, &datadir, std::time::Duration::from_secs(180)) {
        Ok(height) => Ok(json!({"pid": pid, "height": height, "node_bin": bin, "auto_bin": auto_bin, "datadir": datadir})),
        Err(e) => {
            // Never leave a deaf child behind: kill, reap, clear.
            let mut slot = state.node_proc.lock().expect("node proc");
            if let Some(mut m) = slot.take() {
                let _ = m.child.kill();
                let _ = m.child.wait();
            }
            Err(e)
        }
    }
}

/// Stop the node THIS APP started (graceful `stop` RPC, then reap).
/// Refuses when nothing is managed — your manual node is yours to stop.
#[tauri::command(async)]
pub fn node_stop(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let pid = {
        let slot = state.node_proc.lock().expect("node proc");
        match slot.as_ref() {
            Some(m) => m.pid,
            None => return Err("no managed node running (start one, or stop your manual node yourself)".to_string()),
        }
    };
    // Ask nicely first (lock released: node::call takes cfg, never the slot).
    let _ = node::call(&state, "stop", json!([]));
    let t0 = std::time::Instant::now();
    loop {
        let done = {
            let mut slot = state.node_proc.lock().expect("node proc");
            match slot.as_mut() {
                Some(m) => match m.child.try_wait() {
                    Ok(None) => false,
                    _ => {
                        slot.take();
                        true
                    }
                },
                None => true,
            }
        };
        if done {
            return Ok(json!({"stopped": true, "pid": pid}));
        }
        if t0.elapsed() >= std::time::Duration::from_secs(30) {
            let mut slot = state.node_proc.lock().expect("node proc");
            if let Some(mut m) = slot.take() {
                let _ = m.child.kill();
                let _ = m.child.wait();
            }
            return Ok(json!({"stopped": true, "pid": pid, "killed": true}));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Node-side staking state (read-only). Staking happens in the CONNECTED
/// NODE's wallet (`-mint` / `setgenerate`), never in this app's keys — the
/// fork's PoS minter signs from the node's wallet, so app-held coins can't
/// stake. Toggle with `set_staking`; to stake, fund the node wallet.
#[tauri::command(async)]
pub fn staking_info(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let mining = node::call(&state, "getmininginfo", json!([])).unwrap_or(json!(null));
    let wallet = node::call(&state, "getwalletinfo", json!([])).unwrap_or(json!(null));
    // Node RPC returns float coins; the UI wants base units.
    let coins = |v: Option<&serde_json::Value>| -> u64 {
        (v.and_then(|x| x.as_f64()).unwrap_or(0.0) * 100_000_000.0).round() as u64
    };
    let node_host = {
        let cfg = state.cfg.lock().expect("cfg");
        node_host(&cfg.rpc_url)
    };
    Ok(json!({
        "staking_on": mining.get("staking"),
        "staking_supply": coins(mining.get("stakingsupply")),
        "eligible_outputs": wallet.get("eligible_staking_outputs").and_then(|v| v.as_u64()).unwrap_or(0),
        "eligible_balance": coins(wallet.get("eligible_staking_balance")),
        "node_balance": coins(wallet.get("balance")),
        // No passphrase set → no `unlocked_until` field → unlocked by
        // definition. Otherwise unlocked only while the timeout is future.
        "node_unlocked": match wallet.get("unlocked_until").and_then(|v| v.as_i64()) {
            None => true,
            Some(t) => t > std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        },
        "unlocked_until": wallet.get("unlocked_until"),
        "node_host": node_host,
    }))
}

/// A fresh address from the CONNECTED node's wallet — the staking-fund
/// target. The app's coins can never stake in their own wallet (fork
/// rule), so funding happens app → node address, then Start stakes them.
#[tauri::command(async)]
pub fn node_funding_address(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let addr: String = node::call(&state, "getnewaddress", json!([]))
        .and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| "getnewaddress: unexpected result".to_string())
        })?;
    Ok(json!({ "address": addr }))
}

/// Import this app's FUNDED keys into the LOCAL node wallet so the node
/// minter can stake them — "stake my app coins" (review fix 2026-10-18).
/// Same-box only (loopback gate): the node ends up holding copies of these
/// spending keys, correct on one machine and wrong anywhere else. UTXO age
/// is chain data, so mature coins become eligible immediately after the
/// rescan — no transfer, no maturity wait.
#[tauri::command(async)]
pub fn stake_app_coins(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let host = {
        let cfg = state.cfg.lock().expect("cfg");
        node_host(&cfg.rpc_url)
    };
    if !is_loopback_host(&host) {
        return Err(format!(
            "stake-my-coins only works against YOUR node on this machine — connected to {host}"
        ));
    }
    let c = wctx(&state)?;
    let scan = wallet::scan(&c.rpc, &c.seed, c.net, c.recv_next, c.change_next, &app_dir(&state))?;
    let mut slots: Vec<(u32, u32)> = scan.utxos.iter().map(|u| (u.change, u.index)).collect();
    slots.sort_unstable();
    slots.dedup();
    if slots.is_empty() {
        return Err("this app has no funded addresses to stake yet".to_string());
    }
    let mut wifs = Vec::new();
    for (change, index) in &slots {
        let kp = lm_keys::hd::external_key(&c.seed, c.net, 0, *change, *index)?;
        wifs.push(kp.wif(c.net));
    }
    for wif in &wifs {
        node::call(&state, "importprivkey", json!([wif, "lucky-lode-app", false]))?;
    }
    // One rescan (idempotent re-import of the first key with rescan=true)
    // so the node recognizes the UTXOs; eligibility follows from chain age.
    node::call(&state, "importprivkey", json!([&wifs[0], "lucky-lode-app", true]))?;
    let wallet_info = node::call(&state, "getwalletinfo", json!([]))?;
    let coins = |v: Option<&serde_json::Value>| -> u64 {
        (v.and_then(|x| x.as_f64()).unwrap_or(0.0) * 100_000_000.0).round() as u64
    };
    Ok(json!({
        "imported": wifs.len(),
        "eligible_outputs": wallet_info.get("eligible_staking_outputs").and_then(|v| v.as_u64()).unwrap_or(0),
        "eligible_balance": coins(wallet_info.get("eligible_staking_balance")),
    }))
}

/// Staking toggle for the connected node: `true` = stake only (no CPU
/// mining), `false` = stop generation entirely. Honest label: this stakes
/// the NODE wallet's coins (it must hold mature, unlocked coins); the
/// app's own coins never stake under this fork.
///
/// Review fix 2026-10-18: this only ever touches YOUR node. The default
/// config points at the shared public node — flipping setgenerate there
/// would reach into shared infrastructure, so anything but loopback is
/// refused (point Settings → Node at 127.0.0.1 to stake your own coins).
#[tauri::command(async)]
pub fn set_staking(state: State<'_, AppState>, on: bool) -> Result<serde_json::Value, String> {
    let host = {
        let cfg = state.cfg.lock().expect("cfg");
        node_host(&cfg.rpc_url)
    };
    if !is_loopback_host(&host) {
        return Err(format!(
            "staking toggle refuses {host}: it runs setgenerate on the CONNECTED node, and that is not yours (Settings → Node → point it at 127.0.0.1). This app's coins never stake."
        ));
    }
    if on {
        node::call(&state, "setgenerate", json!([true, 0]))?;
    } else {
        node::call(&state, "setgenerate", json!([false]))?;
    }
    staking_info(state)
}

/// Host part of an `rpc_url` (scheme + optional path stripped, port
/// dropped) for the loopback gate.
fn node_host(url: &str) -> String {
    let no_scheme = url.rsplit("://").next().unwrap_or(url);
    let hostport = no_scheme.split('/').next().unwrap_or(no_scheme);
    hostport.split(':').next().unwrap_or(hostport).to_lowercase()
}

fn is_loopback_host(h: &str) -> bool {
    matches!(h, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Preview a payment: build + sign locally, stash the signed bytes for a
/// one-shot confirm. Nothing broadcasts. The UI shows the exact fee and
/// change for review; confirm can only broadcast these bytes.
#[tauri::command(async)]
pub fn send_preview(
    state: State<'_, AppState>,
    to: String,
    amount: String,
) -> Result<serde_json::Value, String> {
    let amt = wallet::parse_amount_lkyl(&amount)?;
    let c = wctx(&state)?;
    let scan = wallet::scan(&c.rpc, &c.seed, c.net, c.recv_next, c.change_next, &app_dir(&state))?;
    let (rmax, cmax) = utxo_maxes(&scan.utxos);
    advance_cursors(&state, rmax, cmax);
    // Bond lock: never spend luckynode payout addresses — one stray Send
    // stops the node being draw-eligible (ownership gate).
    let protected: Vec<String> = {
        let cfg = state.cfg.lock().expect("cfg");
        cfg.protected_addrs.clone()
    };
    let mut spendable = Vec::new();
    let mut locked = 0u64;
    for u in &scan.utxos {
        if protected.iter().any(|a| a == &u.address) {
            locked = locked.saturating_add(u.value);
        } else {
            spendable.push(u.clone());
        }
    }
    if spendable.is_empty() && locked > 0 {
        return Err(format!(
            "nothing spendable: {:.8} LKYL sits in luckynode bond addresses (release in Settings → Wallet)",
            locked as f64 / 100_000_000.0
        ));
    }
    let draft = wallet::prepare_payment(
        &c.seed,
        c.net,
        &spendable,
        to.trim(),
        amt,
        c.change_next,
        wallet::spend_epoch(&c.rpc),
    )
    .map_err(|e| {
        if locked > 0 {
            format!(
                "{e} — plus {:.8} LKYL locked in luckynode bond addresses (release in Settings → Wallet)",
                locked as f64 / 100_000_000.0
            )
        } else {
            e
        }
    })?;
    // Single-use 2-minute slot, keyed by the committed txid.
    let txid = draft.txid.clone();
    {
        let mut pend = state.pending.lock().expect("pending");
        let now = std::time::Instant::now();
        pend.retain(|_, p| p.expires_at > now);
        pend.insert(
            txid.clone(),
            crate::PendingSend {
                hex: draft.hex,
                fee: draft.fee,
                change: draft.change,
                change_used: draft.change_used,
                expires_at: now + std::time::Duration::from_secs(120),
            },
        );
    }
    Ok(json!({
        "txid": txid,
        "to": to.trim(),
        "amount_sats": amt,
        "fee_sats": draft.fee,
        "change_sats": draft.change,
    }))
}

/// Confirm a previewed payment: broadcast the exact reviewed bytes.
/// Single-use — the slot is consumed (double-click safe).
#[tauri::command(async)]
pub fn send_confirm(state: State<'_, AppState>, txid: String) -> Result<serde_json::Value, String> {
    let pend = {
        let mut p = state.pending.lock().expect("pending");
        p.remove(&txid)
    };
    let Some(draft) = pend else {
        return Err("preview expired or already sent — review again".to_string());
    };
    if draft.expires_at <= std::time::Instant::now() {
        return Err("preview expired (2 min) — review again".to_string());
    }
    let c = wctx(&state)?;
    let sent = wallet::broadcast_payment(&c.rpc, &draft.hex, &txid)?;
    if let Some(idx) = draft.change_used {
        let mut cfg = state.cfg.lock().expect("cfg");
        if idx.saturating_add(1) > cfg.change_next {
            cfg.change_next = idx + 1;
            let _ = cfg.save(&state.cfg_path);
        }
    }
    Ok(json!({ "txid": sent, "fee_sats": draft.fee, "change_sats": draft.change }))
}

/// Self-custody context: node RPC + its network + our seed + HD cursors.
struct WCtx {
    rpc: RpcClient,
    net: Network,
    seed: [u8; 64],
    recv_next: u32,
    change_next: u32,
}

/// App dir (config + wallet + the txmeta RPC cache live here).
fn app_dir(state: &State<'_, AppState>) -> std::path::PathBuf {
    state
        .cfg_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn wctx(state: &State<'_, AppState>) -> Result<WCtx, String> {
    let rpc = node::client(state)?;
    let net = wallet::network_of(&rpc)?;
    let cfg = state.cfg.lock().expect("cfg");
    Ok(WCtx {
        rpc,
        net,
        seed: session_seed(state)?,
        recv_next: cfg.recv_next,
        change_next: cfg.change_next,
    })
}

/// Live session seed (touches idle timer). Errors when locked — every
/// money command funnels through here, so a locked wallet cannot spend,
/// sign, or reveal.
fn session_seed(state: &State<'_, AppState>) -> Result<[u8; 64], String> {
    use std::time::Instant;
    let mut s = state.session.lock().expect("session");
    let sess = s
        .as_mut()
        .ok_or_else(|| "wallet is locked — unlock it first".to_string())?;
    if sess.expired() {
        sess.wipe();
        *s = None;
        return Err("wallet locked itself after 15 min idle — unlock again".to_string());
    }
    sess.last_active = Instant::now();
    Ok(sess.seed)
}

/// Drop the session, wiping key material.
fn lock_session(state: &State<'_, AppState>) {
    let mut s = state.session.lock().expect("session");
    if let Some(sess) = s.as_mut() {
        sess.wipe();
    }
    *s = None;
}

/// Advance HD cursors past the highest used index (persisted on change).
fn advance_cursors(state: &State<'_, AppState>, recv_max: Option<u32>, change_max: Option<u32>) {
    let mut cfg = state.cfg.lock().expect("cfg");
    let mut dirty = false;
    if let Some(m) = recv_max {
        let next = m.saturating_add(1);
        if next > cfg.recv_next {
            cfg.recv_next = next;
            dirty = true;
        }
    }
    if let Some(m) = change_max {
        let next = m.saturating_add(1);
        if next > cfg.change_next {
            cfg.change_next = next;
            dirty = true;
        }
    }
    if dirty {
        let _ = cfg.save(&state.cfg_path);
    }
}

fn utxo_maxes(utxos: &[wallet::ScannedUtxo]) -> (Option<u32>, Option<u32>) {
    let mut rmax: Option<u32> = None;
    let mut cmax: Option<u32> = None;
    for u in utxos {
        let slot = if u.change == 0 { &mut rmax } else { &mut cmax };
        *slot = Some(slot.unwrap_or(0).max(u.index));
    }
    (rmax, cmax)
}

/// Wallet balances (available / immature / unconfirmed) — OUR addresses
/// only, summed from our UTXO set. Base units (sats), like before.
#[tauri::command(async)]
pub fn wallet_balances(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let c = wctx(&state)?;
    let tip = node::call(&state, "getblockcount", json!([]))?
        .as_u64()
        .ok_or("getblockcount: unexpected")?;
    let scan = wallet::scan(&c.rpc, &c.seed, c.net, c.recv_next, c.change_next, &app_dir(&state))?;
    let (rmax, cmax) = utxo_maxes(&scan.utxos);
    advance_cursors(&state, rmax, cmax);
    let b = wallet::balances_of(&scan, tip);
    Ok(json!({
        "balance": b.available,
        "unconfirmed": b.unconfirmed,
        "immature": b.immature,
        "staking_outputs": 0,
        "staking_balance": 0,
        "txcount": b.txcount,
    }))
}

#[tauri::command(async)]
pub fn transactions(
    state: State<'_, AppState>,
    count: Option<u64>,
) -> Result<serde_json::Value, String> {
    let c = wctx(&state)?;
    let scan = wallet::scan(&c.rpc, &c.seed, c.net, c.recv_next, c.change_next, &app_dir(&state))?;
    let (rmax, cmax) = utxo_maxes(&scan.utxos);
    let map = wallet::own_script_map(&c.seed, c.net, c.recv_next, c.change_next)?;
    let n = count.unwrap_or(50) as usize;
    let start = scan.txids.len().saturating_sub(n);
    let page = wallet::activity(&c.rpc, &scan.txids[start..], &map, n, &app_dir(&state))?;
    advance_cursors(&state, rmax.max(page.used_recv_max), cmax.max(page.used_change_max));
    Ok(json!(
        page.rows
            .iter()
            .map(|r| json!({
                "txid": r.txid,
                "category": r.category,
                "amount": r.amount,
                "address": r.address,
                "time": r.time,
                "confirmations": r.confirmations,
            }))
            .collect::<Vec<_>>()
    ))
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

/// Bond lock: mark an address unspendable (Send refuses its UTXOs).
/// Luckynode payout addresses land here automatically at register.
#[tauri::command]
pub fn protect_address(state: State<'_, AppState>, address: String) -> Result<serde_json::Value, String> {
    let mut cfg = state.cfg.lock().expect("cfg");
    if !cfg.protected_addrs.iter().any(|a| a == &address) {
        cfg.protected_addrs.push(address);
    }
    cfg.save(&state.cfg_path)?;
    Ok(json!({ "protected": cfg.protected_addrs }))
}

/// Bond lock release: make an address spendable again (draw eligibility
/// is lost if this held a node bond — the pool re-checks every tick).
#[tauri::command]
pub fn unprotect_address(state: State<'_, AppState>, address: String) -> Result<serde_json::Value, String> {
    let mut cfg = state.cfg.lock().expect("cfg");
    cfg.protected_addrs.retain(|a| a != &address);
    cfg.save(&state.cfg_path)?;
    Ok(json!({ "protected": cfg.protected_addrs }))
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

/// Wallet transaction detail for the activity view: decoded from the
/// chain, amounts relative to OUR addresses (same shape as before).
#[tauri::command(async)]
pub fn tx_detail(state: State<'_, AppState>, txid: String) -> Result<serde_json::Value, String> {
    let c = wctx(&state)?;
    if txid.len() != 64 {
        return Err("not a txid".to_string());
    }
    let map = wallet::own_script_map(&c.seed, c.net, c.recv_next, c.change_next)?;
    wallet::tx_detail(&c.rpc, &txid, &map, &app_dir(&state))
}

/// Next receive address (HD cursor advances; previously shown addresses
/// stay valid forever).
#[tauri::command]
pub fn new_address(state: State<'_, AppState>) -> Result<String, String> {
    let c = wctx(&state)?;
    let addr = wallet::current_receive(&c.seed, c.net, c.recv_next)?;
    {
        let mut cfg = state.cfg.lock().expect("cfg");
        cfg.recv_next = cfg.recv_next.saturating_add(1);
        cfg.save(&state.cfg_path)?;
    }
    Ok(addr)
}

/// Receive addresses: the current one plus every previously shown one
/// (persisted cursor, so the list survives restarts).
#[tauri::command]
pub fn receive_list(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let c = wctx(&state)?;
    let current = wallet::current_receive(&c.seed, c.net, c.recv_next)?;
    let mut previous = Vec::new();
    for i in 0..c.recv_next {
        previous.push(wallet::receive_address(&c.seed, c.net, i)?);
    }
    Ok(json!({ "current": current, "previous": previous }))
}

/// Send funds (transparent). Built AND signed locally — the node only
/// broadcasts. Returns txid + exact fee/change.
#[tauri::command(async)]
pub fn send(state: State<'_, AppState>, to: String, amount: String) -> Result<serde_json::Value, String> {
    let amt = wallet::parse_amount_lkyl(&amount)?;
    let c = wctx(&state)?;
    let scan = wallet::scan(&c.rpc, &c.seed, c.net, c.recv_next, c.change_next, &app_dir(&state))?;
    let (rmax, cmax) = utxo_maxes(&scan.utxos);
    advance_cursors(&state, rmax, cmax);
    // Bond lock: never spend luckynode payout addresses — one stray Send
    // stops the node being draw-eligible (ownership gate).
    let protected: Vec<String> = {
        let cfg = state.cfg.lock().expect("cfg");
        cfg.protected_addrs.clone()
    };
    let mut spendable = Vec::new();
    let mut locked: u64 = 0;
    for u in &scan.utxos {
        if protected.iter().any(|a| a == &u.address) {
            locked = locked.saturating_add(u.value);
        } else {
            spendable.push(u.clone());
        }
    }
    if spendable.is_empty() && locked > 0 {
        return Err(format!(
            "nothing spendable: {:.8} LKYL sits in luckynode bond addresses (release in Settings → Wallet)",
            locked as f64 / 100_000_000.0
        ));
    }
    let r = wallet::send_payment(
        &c.rpc,
        &c.seed,
        c.net,
        &spendable,
        to.trim(),
        amt,
        c.change_next,
    )
    .map_err(|e| {
        if locked > 0 {
            format!(
                "{e} — plus {:.8} LKYL locked in luckynode bond addresses (release in Settings → Wallet)",
                locked as f64 / 100_000_000.0
            )
        } else {
            e
        }
    })?;
    if let Some(idx) = r.change_used {
        let mut cfg = state.cfg.lock().expect("cfg");
        if idx.saturating_add(1) > cfg.change_next {
            cfg.change_next = idx + 1;
            let _ = cfg.save(&state.cfg_path);
        }
    }
    Ok(json!({ "txid": r.txid, "fee_sats": r.fee, "change_sats": r.change }))
}

// ---- Self-custody wallet management --------------------------------------

/// First-run gate + Settings display (no seed material, ever, in this).
#[tauri::command(async)]
pub fn wallet_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let dir = state.cfg_path.parent().ok_or("no config dir")?.to_path_buf();
    if !wallet::has_any_wallet(&dir) {
        return Ok(json!({ "has_wallet": false }));
    }
    if wallet::is_legacy_plaintext(&dir) {
        return Ok(json!({ "has_wallet": true, "legacy": true, "locked": true }));
    }
    let locked = state.session.lock().expect("session").is_none();
    if locked {
        return Ok(json!({ "has_wallet": true, "legacy": false, "locked": true }));
    }
    // Snapshot the cursors under a scoped lock: node::client below locks
    // `cfg` itself, so holding our guard across it deadlocked the app
    // (frozen main thread right after unlock — 2026-10-16).
    let (recv_next, change_next, protected_addrs) = {
        let cfg = state.cfg.lock().expect("cfg");
        (
            cfg.recv_next,
            cfg.change_next,
            cfg.protected_addrs.clone(),
        )
    };
    // Network follows the connected node; failure just omits it.
    let pair: Option<(String, String)> = (|| {
        let rpc = node::client(&state).ok()?;
        let net = wallet::network_of(&rpc).ok()?;
        let seed = session_seed(&state).ok()?;
        let current = wallet::current_receive(&seed, net, recv_next).ok()?;
        Some((format!("{net:?}").to_lowercase(), current))
    })();
    let (net, current) = match pair {
        Some((n, c)) => (Some(n), Some(c)),
        None => (None, None),
    };
    Ok(json!({
        "has_wallet": true,
        "legacy": false,
        "locked": false,
        "network": net,
        "recv_next": recv_next,
        "change_next": change_next,
        "receive_address": current,
        "protected_addrs": protected_addrs,
    }))
}

/// Create a fresh 12-word wallet, sealed immediately. Returns the phrase
/// ONCE for backup, and unlocks the session.
#[tauri::command(async)]
pub fn wallet_create(state: State<'_, AppState>, password: String) -> Result<serde_json::Value, String> {
    let dir = state.cfg_path.parent().ok_or("no config dir")?.to_path_buf();
    let phrase = wallet::create_wallet_enc(&dir, &password)?;
    let (seed, phrase2) = wallet::unlock_seed(&dir, &password)?;
    *state.session.lock().expect("session") = Some(wallet::Session::new(seed, phrase2));
    Ok(json!({ "phrase": phrase }))
}

/// Install a backup phrase (replaces any wallet), then rescan cursors.
#[tauri::command(async)]
pub fn wallet_restore(state: State<'_, AppState>, phrase: String, password: String) -> Result<serde_json::Value, String> {
    let dir = state.cfg_path.parent().ok_or("no config dir")?.to_path_buf();
    wallet::restore_wallet_enc(&dir, &phrase, &password)?;
    let (seed, phrase2) = wallet::unlock_seed(&dir, &password)?;
    *state.session.lock().expect("session") = Some(wallet::Session::new(seed, phrase2));
    rescan_inner(&state)
}

/// Unlock with the spending password.
#[tauri::command(async)]
pub fn wallet_unlock(state: State<'_, AppState>, password: String) -> Result<serde_json::Value, String> {
    let dir = state.cfg_path.parent().ok_or("no config dir")?.to_path_buf();
    if wallet::is_legacy_plaintext(&dir) {
        return Err("this wallet predates encryption — migrate it first".to_string());
    }
    let (seed, phrase) = wallet::unlock_seed(&dir, &password)?;
    *state.session.lock().expect("session") = Some(wallet::Session::new(seed, phrase));
    Ok(json!({ "ok": true }))
}

/// Lock now (wipes key material from RAM).
#[tauri::command]
pub fn wallet_lock(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    lock_session(&state);
    Ok(json!({ "ok": true }))
}

/// One-time upgrade for pre-encryption wallets: set a password, seal the
/// seed, shred the plaintext, unlock.
#[tauri::command(async)]
pub fn wallet_migrate(state: State<'_, AppState>, password: String) -> Result<serde_json::Value, String> {
    let dir = state.cfg_path.parent().ok_or("no config dir")?.to_path_buf();
    let phrase = wallet::migrate_plaintext(&dir, &password)?;
    let (seed, phrase2) = wallet::unlock_seed(&dir, &password)?;
    debug_assert_eq!(phrase, phrase2);
    *state.session.lock().expect("session") = Some(wallet::Session::new(seed, phrase2));
    Ok(json!({ "ok": true }))
}

/// Change the vault password (needs the old one).
#[tauri::command(async)]
pub fn wallet_change_password(
    state: State<'_, AppState>,
    old_password: String,
    new_password: String,
) -> Result<serde_json::Value, String> {
    let dir = state.cfg_path.parent().ok_or("no config dir")?.to_path_buf();
    wallet::change_password(&dir, &old_password, &new_password)?;
    Ok(json!({ "ok": true }))
}

/// Show the phrase (backup display; only while unlocked — the UI must
/// gate this behind a warning).
#[tauri::command]
pub fn wallet_reveal(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let s = state.session.lock().expect("session");
    let sess = s
        .as_ref()
        .ok_or_else(|| "wallet is locked — unlock it first".to_string())?;
    Ok(json!({ "phrase": sess.phrase.clone() }))
}

fn rescan_inner(state: &State<'_, AppState>) -> Result<serde_json::Value, String> {
    let c = wctx(state)?;
    let (recv_next, change_next, used) = wallet::full_scan_used(&c.rpc, &c.seed, c.net)?;
    {
        let mut cfg = state.cfg.lock().expect("cfg");
        // Never move cursors backwards (shown addresses stay valid).
        if recv_next > cfg.recv_next {
            cfg.recv_next = recv_next;
        }
        if change_next > cfg.change_next {
            cfg.change_next = change_next;
        }
        cfg.save(&state.cfg_path)?;
    }
    let cfg = state.cfg.lock().expect("cfg");
    Ok(json!({
        "recv_next": cfg.recv_next,
        "change_next": cfg.change_next,
        "used_addresses": used,
    }))
}

/// Re-run BIP44 discovery (e.g. after restore on another machine).
#[tauri::command(async)]
pub fn wallet_rescan(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    rescan_inner(&state)
}
/// Game/network stats for the Game view.
#[tauri::command(async)]
pub fn game_info(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let jackpot = node::call(&state, "getjackpotinfo", json!([])).unwrap_or(json!(null));
    let mining = node::call(&state, "getmininginfo", json!([])).unwrap_or(json!(null));
    Ok(json!({ "jackpot": jackpot, "mining": mining }))
}

/// Live emission phase (docs/04 §4.1) at the current tip.
#[tauri::command(async)]
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
#[tauri::command(async)]
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
#[tauri::command(async)]
pub fn luckynodes(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let url = state.cfg.lock().expect("cfg").pool_url.clone();
    let body = pool::get(&url, "api/luckynodes")?;
    serde_json::from_str(&body).map_err(|e| format!("pool json: {e}"))
}

/// Pool dashboard stats (blocks, hashrate, workers, payouts, draws).
#[tauri::command(async)]
pub fn pool_stats(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let url = state.cfg.lock().expect("cfg").pool_url.clone();
    let body = pool::get(&url, "api/stats")?;
    serde_json::from_str(&body).map_err(|e| format!("pool json: {e}"))
}

/// Generate (or load) a luckynode identity and register it with the pool.
/// The payout address MUST be one this wallet controls: registration signs a
/// canonical message with the address's key, and the pool proves ownership
/// via the node's `verifymessage` (Sybil control: no key, no node).
#[tauri::command(async)]
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
    // Payout-address key: scan this wallet's HD derivations (both chains,
    // out past the used cursors by the gap limit).
    let seed = session_seed(&state)?;
    let (recv_next, change_next) = {
        let cfg = state.cfg.lock().expect("cfg");
        (cfg.recv_next, cfg.change_next)
    };
    let net = {
        let rpc = node::client(&state)?;
        wallet::network_of(&rpc)?
    };
    let addr_secret = address_key(&seed, net, &address, recv_next, change_next)?;
    let reg = lm_luckynode::register_body(&id, &address, &addr_secret)?;
    let body = pool::post(
        &url,
        "api/luckynode/register",
        &serde_json::to_string(&reg).map_err(|e| format!("json: {e}"))?,
    )?;
    // Bond lock: this payout address now holds a draw-eligible bond — Send
    // must not touch it (release in Settings → Wallet).
    {
        let mut cfg = state.cfg.lock().expect("cfg");
        if !cfg.protected_addrs.iter().any(|a| a == &address) {
            cfg.protected_addrs.push(address.clone());
            let _ = cfg.save(&state.cfg_path);
        }
    }
    Ok(json!({ "id": id, "key_path": key_path.display().to_string(), "pool": body }))
}

/// Find this wallet's private key for `address` (scans receive+change
/// chains out to the gap limit past the cursors). Proves app-side control
/// of the address.
fn address_key(
    seed: &[u8; 64],
    net: lm_keys::chain::Network,
    address: &str,
    recv_next: u32,
    change_next: u32,
) -> Result<[u8; 32], String> {
    let mut chains = [
        (0u32, recv_next),
        (1u32, change_next),
    ];
    for (chain, used) in &mut chains {
        let count = used.saturating_add(wallet::GAP);
        for d in wallet::derive_range(seed, net, *chain, 0, count)? {
            if d.address == address {
                let kp = lm_keys::hd::external_key(seed, net, 0, *chain, d.index)?;
                return Ok(*kp.secret());
            }
        }
    }
    Err("payout address is not one of this wallet's addresses".to_string())
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
    let seed = session_seed(&state)?;
    let (recv_next, change_next) = {
        let cfg = state.cfg.lock().expect("cfg");
        (cfg.recv_next, cfg.change_next)
    };
    let net = {
        let rpc = node::client(&state)?;
        wallet::network_of(&rpc)?
    };
    let addr_secret = address_key(&seed, net, &address, recv_next, change_next)?;
    let svc = luckynode::start(url, id.clone(), address, key, addr_secret);
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
    serde_json::to_value(&*cfg).map_err(|e| e.to_string())
}

/// Quick connection check for the Settings tab (uses the CURRENT saved
/// config, so Save first — the button reports exactly what the app sees).
#[tauri::command(async)]
pub fn test_rpc(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let height = node::call(&state, "getblockcount", json!([]))?;
    let chain = node::call(&state, "getblockchaininfo", json!([]))
        .ok()
        .and_then(|v| v.get("chain").cloned())
        .unwrap_or(json!(null));
    Ok(json!({ "ok": true, "height": height, "chain": chain }))
}

#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub fn set_config(
    state: State<'_, AppState>,
    rpc_url: String,
    rpc_user: String,
    rpc_pass: String,
    pool_url: String,
    node_bin: Option<String>,
    node_datadir: Option<String>,
    node_args: Option<String>,
) -> Result<(), String> {
    let mut cfg = state.cfg.lock().expect("cfg");
    cfg.rpc_url = rpc_url;
    cfg.rpc_user = rpc_user;
    cfg.rpc_pass = rpc_pass;
    cfg.pool_url = pool_url;
    // Node lifecycle fields ride along when the UI sends them; older
    // callers omit them (None = leave untouched).
    if let Some(v) = node_bin {
        cfg.node_bin = v;
    }
    if let Some(v) = node_datadir {
        cfg.node_datadir = v;
    }
    if let Some(v) = node_args {
        cfg.node_args = v;
    }
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
#[allow(clippy::too_many_arguments)] // Tauri command surface: one arg per UI field.
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
    tls: Option<bool>,
) -> Result<(), String> {
    let mut slot = state.miner.lock().expect("miner");
    if let Some(m) = slot.as_ref() {
        if m.status.lock().expect("miner status").running {
            return Err("miner already running".to_string());
        }
    }
    // Empty worker = mine into the wallet's own current receive address
    // (needs an unlocked wallet).
    let worker = if worker.trim().is_empty() {
        let seed = session_seed(&state)?;
        let rpc = node::client(&state)?;
        let net = wallet::network_of(&rpc)?;
        let recv_next = state.cfg.lock().expect("cfg").recv_next;
        wallet::current_receive(&seed, net, recv_next)?
    } else {
        worker
    };
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
        tls.unwrap_or(false),
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
