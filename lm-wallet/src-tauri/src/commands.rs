//! Tauri commands: everything the UI can ask the backend to do.

use crate::node;
use crate::node_proc;
use crate::{luckynode, miner, pool, wallet, AppState};
use lm_keys::chain::Network;
use lm_luckynode::identity::NodeRpc;
use lm_stratum::rpc::RpcClient;
use serde_json::json;
use std::sync::atomic::Ordering;
use tauri::State;

/// Chain ticker for user-facing strings (mirrors the UI's ticker()).
fn ticker(net: Network) -> &'static str {
    match net {
        Network::Mainnet => "LKYL",
        Network::Testnet => "tLKYL",
        Network::Regtest => "rLKYL",
    }
}

/// Chain + node health for the top status bar, plus managed-node state.
/// Never fails when the node is down: chain fields go null and the UI
/// shows the managed Start button instead of an error.
#[tauri::command(async)]
pub fn node_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    // Reap a managed child that died on its own (non-blocking).
    let managed = {
        let mut slot = state
            .node_proc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let (bin_set, shared_node, creds_missing) = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            !cfg.node_bin.trim().is_empty(),
            is_shared_url(&cfg.rpc_url),
            cfg.rpc_pass.trim().is_empty(),
        )
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
        "creds_missing": creds_missing,
    }))
}

/// Is this URL the shared public node preset?
pub fn is_shared_url(url: &str) -> bool {
    url.contains("luckylode.cc/node")
}

/// What a fresh install can connect to, without typing anything:
/// the found luckyd binaries and the default datadir a managed node
/// would use. Powers the Settings connection picker.
#[tauri::command]
pub fn node_discover() -> Result<serde_json::Value, String> {
    Ok(json!({
        "candidates": node_proc::find_luckyd(),
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
pub fn use_connection_preset(
    state: State<'_, AppState>,
    preset: String,
) -> Result<serde_json::Value, String> {
    let mut cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
/// configured the wallet auto-detects luckyd (PATH + per-OS install
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
        let mut cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let auto_bin = cfg.node_bin.trim().is_empty();
        if auto_bin {
            match node_proc::find_luckyd().first() {
                Some(found) => cfg.node_bin = found.clone(),
                None => {
                    return Err(
                        "no luckyd found — install the Lucky Lode node and put luckyd on your PATH, or set its location under Advanced".to_string(),
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
        let mut slot = state
            .node_proc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(m) = slot.as_mut() {
            match m.child.try_wait() {
                Ok(None) => {
                    return Err(format!(
                        "managed node already running (pid {}) — stop it first",
                        m.pid
                    ));
                }
                _ => {
                    slot.take(); // reaped a dead child; proceed to start
                }
            }
        }
    }
    // A node started outside the wallet (or a leftover from a previous
    // session) still owns the RPC port and the datadir lock: spawning a
    // second one would fail slowly and confusingly. Name it instead —
    // the Stop button can shut it down.
    if let Ok(info) = node::call(&state, "getblockchaininfo", json!([])) {
        let height = info.get("blocks").and_then(|b| b.as_u64()).unwrap_or(0);
        return Err(format!(
            "a node is already running at {url} (height {height}) — stop it first"
        ));
    }
    // Still starting (slow first launch builds the chain index and
    // answers no RPC yet) or stuck with bad credentials: either way the
    // port and datadir lock are taken, so a second node cannot start.
    // Refuse fast instead of failing obscurely after the RPC wait.
    let port = node_proc::rpc_port_from_url(&url);
    if node_proc::rpc_port_in_use(&host, port) {
        return Err(format!(
            "something is already listening on {host}:{port} — the node may still be starting (first launch builds the chain index, minutes: wait, then Refresh status), or the RPC credentials don't match (Settings → Advanced, or unlock the login keyring)"
        ));
    }
    let datadir = node_proc::resolve_datadir(&datadir_cfg)?;
    std::fs::create_dir_all(&datadir).map_err(|e| format!("mkdir {datadir}: {e}"))?;
    let args = node_proc::build_args(&datadir, &user, &pass, port, &extra);
    let child = node_proc::spawn(&bin, &args)?;
    let pid = child.id();
    {
        let mut slot = state
            .node_proc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(node_proc::ManagedNode { child, pid });
    }
    // Wait for RPC (rebuilds the client from current settings each poll).
    let rpc = node::client(&state)?;
    match node_proc::wait_for_rpc(&rpc, &datadir, std::time::Duration::from_secs(180)) {
        Ok(height) => Ok(
            json!({"pid": pid, "height": height, "node_bin": bin, "auto_bin": auto_bin, "datadir": datadir}),
        ),
        Err(e) => {
            // Never leave a deaf child behind: kill, reap, clear.
            let mut slot = state
                .node_proc
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(mut m) = slot.take() {
                let _ = m.child.kill();
                let _ = m.child.wait();
            }
            Err(e)
        }
    }
}

/// Stop the node on this machine (graceful `stop` RPC, then reap).
/// Managed children take the exact-child path; a node the wallet did not
/// start (manual, or a leftover from a previous session) is stopped via
/// the same RPC when it answers on loopback. A remote/shared node is
/// never touched.
#[tauri::command(async)]
pub fn node_stop(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let managed_pid = {
        let slot = state
            .node_proc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.as_ref().map(|m| m.pid)
    };
    if let Some(pid) = managed_pid {
        // Ask nicely first (lock released: node::call takes cfg, never the slot).
        let _ = node::call(&state, "stop", json!([]));
        let t0 = std::time::Instant::now();
        loop {
            let done = {
                let mut slot = state
                    .node_proc
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                let mut slot = state
                    .node_proc
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(mut m) = slot.take() {
                    let _ = m.child.kill();
                    let _ = m.child.wait();
                }
                return Ok(json!({"stopped": true, "pid": pid, "killed": true}));
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    // Not managed: only a loopback node may be stopped from the wallet
    // (never the shared/remote node). Snapshot the URL under a scoped
    // lock — node::call locks cfg itself.
    let url = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cfg.rpc_url.clone()
    };
    let host = node_host(&url);
    if !is_loopback_host(&host) {
        return Err(format!(
            "node stop refuses {host}: only a node on this machine can be stopped from the wallet"
        ));
    }
    let height = match node::call(&state, "getblockcount", json!([])) {
        Ok(v) => v.as_u64().unwrap_or(0),
        Err(_) => {
            return Err("no node answering on the configured RPC — nothing to stop".to_string())
        }
    };
    let _ = node::call(&state, "stop", json!([]));
    let t0 = std::time::Instant::now();
    loop {
        if node::call(&state, "getblockcount", json!([])).is_err() {
            return Ok(json!({
                "stopped": true,
                "pid": null,
                "external": true,
                "height": height,
            }));
        }
        if t0.elapsed() >= std::time::Duration::from_secs(30) {
            return Err("node did not stop within 30 s (its RPC is still answering)".to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
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
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let addr: String = node::call(&state, "getnewaddress", json!([])).and_then(|v| {
        v.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "getnewaddress: unexpected result".to_string())
    })?;
    Ok(json!({ "address": addr }))
}

/// Import this app's FUNDED keys into the LOCAL node wallet so the node
/// minter can stake them — "stake my app coins".
/// Same-box only (loopback gate): the node ends up holding copies of these
/// spending keys, correct on one machine and wrong anywhere else. UTXO age
/// is chain data, so mature coins become eligible immediately after the
/// rescan — no transfer, no maturity wait.
#[tauri::command(async)]
pub fn stake_app_coins(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let host = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        node_host(&cfg.rpc_url)
    };
    if !is_loopback_host(&host) {
        return Err(format!(
            "stake-my-coins only works against YOUR node on this machine — connected to {host}"
        ));
    }
    let c = wctx(&state)?;
    let scan = wallet::scan(
        &c.rpc,
        &c.seed,
        c.net,
        c.recv_next,
        c.change_next,
        &app_dir(&state),
    )?;
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
        node::call(
            &state,
            "importprivkey",
            json!([wif, "lucky-lode-app", false]),
        )?;
    }
    // One rescan (idempotent re-import of the first key with rescan=true)
    // so the node recognizes the UTXOs; eligibility follows from chain age.
    node::call(
        &state,
        "importprivkey",
        json!([&wifs[0], "lucky-lode-app", true]),
    )?;
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
/// This only ever touches YOUR node. The default
/// config points at the shared public node — flipping setgenerate there
/// would reach into shared infrastructure, so anything but loopback is
/// refused (point Settings → Node at 127.0.0.1 to stake your own coins).
#[tauri::command(async)]
pub fn set_staking(state: State<'_, AppState>, on: bool) -> Result<serde_json::Value, String> {
    let host = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    hostport
        .split(':')
        .next()
        .unwrap_or(hostport)
        .to_lowercase()
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
    let scan = wallet::scan(
        &c.rpc,
        &c.seed,
        c.net,
        c.recv_next,
        c.change_next,
        &app_dir(&state),
    )?;
    let (rmax, cmax) = utxo_maxes(&scan.utxos);
    advance_cursors(&state, rmax, cmax);
    // Bond lock: never spend luckynode payout addresses — one stray Send
    // stops the node being draw-eligible (ownership gate).
    let protected: Vec<String> = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            "nothing spendable: {:.8} {} sits in protected addresses (release in Settings → Wallet)",
            locked as f64 / 100_000_000.0,
            ticker(c.net)
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
                "{e} — plus {:.8} {} locked in protected addresses (release in Settings → Wallet)",
                locked as f64 / 100_000_000.0,
                ticker(c.net)
            )
        } else {
            e
        }
    })?;
    // Single-use 2-minute slot, keyed by the committed txid.
    let txid = draft.txid.clone();
    {
        let mut pend = state
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut p = state
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let mut cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let mut s = state
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let mut s = state
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(sess) = s.as_mut() {
        sess.wipe();
    }
    *s = None;
}

/// Advance HD cursors past the highest used index (persisted on change).
fn advance_cursors(state: &State<'_, AppState>, recv_max: Option<u32>, change_max: Option<u32>) {
    let mut cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// Wallet balances (available / immature / unconfirmed / guarded) — OUR
/// addresses only, summed from our UTXO set. Base units (sats), like
/// before. `guarded` = stake-guarded PoS rewards: consensus-valid but
/// relay-unspendable, so they are reported apart, never inside balance.
#[tauri::command(async)]
pub fn wallet_balances(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let c = wctx(&state)?;
    let tip = node::call(&state, "getblockcount", json!([]))?
        .as_u64()
        .ok_or("getblockcount: unexpected")?;
    let scan = wallet::scan(
        &c.rpc,
        &c.seed,
        c.net,
        c.recv_next,
        c.change_next,
        &app_dir(&state),
    )?;
    let (rmax, cmax) = utxo_maxes(&scan.utxos);
    advance_cursors(&state, rmax, cmax);
    let b = wallet::balances_of(&scan, tip);
    Ok(json!({
        "balance": b.available,
        "unconfirmed": b.unconfirmed,
        "immature": b.immature,
        "guarded": b.guarded,
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
    let scan = wallet::scan(
        &c.rpc,
        &c.seed,
        c.net,
        c.recv_next,
        c.change_next,
        &app_dir(&state),
    )?;
    let (rmax, cmax) = utxo_maxes(&scan.utxos);
    let map = wallet::own_script_map(&c.seed, c.net, c.recv_next, c.change_next)?;
    let n = count.unwrap_or(50) as usize;
    let start = scan.txids.len().saturating_sub(n);
    let page = wallet::activity(
        &c.rpc,
        &scan.txids[start..],
        &map,
        n,
        scan.tip,
        &app_dir(&state),
    )?;
    advance_cursors(
        &state,
        rmax.max(page.used_recv_max),
        cmax.max(page.used_change_max),
    );
    Ok(json!(page
        .rows
        .iter()
        .map(|r| json!({
            "txid": r.txid,
            "category": r.category,
            "amount": r.amount,
            "address": r.address,
            "time": r.time,
            "confirmations": r.confirmations,
        }))
        .collect::<Vec<_>>()))
}

/// Saved contacts (address book, M2).
#[tauri::command]
pub fn contacts_list(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let mut cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match cfg.contacts.iter_mut().find(|c| c.address == address) {
        Some(c) => c.name = name,
        None => cfg.contacts.push(crate::config::Contact { name, address }),
    }
    cfg.save(&state.cfg_path)
}

/// Remove a contact by address.
#[tauri::command]
pub fn contact_remove(state: State<'_, AppState>, address: String) -> Result<(), String> {
    let mut cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cfg.contacts.retain(|c| c.address != address);
    cfg.save(&state.cfg_path)
}

/// Bond lock: mark an address unspendable (Send refuses its UTXOs).
/// Luckynode payout addresses land here automatically at register.
#[tauri::command]
pub fn protect_address(
    state: State<'_, AppState>,
    address: String,
) -> Result<serde_json::Value, String> {
    let mut cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !cfg.protected_addrs.iter().any(|a| a == &address) {
        cfg.protected_addrs.push(address);
    }
    cfg.save(&state.cfg_path)?;
    Ok(json!({ "protected": cfg.protected_addrs }))
}

/// Bond lock release: make an address spendable again (draw eligibility
/// stays protected until released here (legacy entries; new identities use on-chain registration).
#[tauri::command]
pub fn unprotect_address(
    state: State<'_, AppState>,
    address: String,
) -> Result<serde_json::Value, String> {
    let mut cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cfg.protected_addrs.retain(|a| a != &address);
    cfg.save(&state.cfg_path)?;
    Ok(json!({ "protected": cfg.protected_addrs }))
}

/// QR code (SVG string) for an address — the Receive view renders it.
/// An optional `amount` (decimal string) turns it into a payment request
/// ("address?amount=x", BIP21-style without a scheme so any scanner
/// extracts both).
#[tauri::command]
pub fn address_qr(address: String, amount: Option<String>) -> Result<String, String> {
    if address.is_empty() {
        return Err("no address".to_string());
    }
    let mut text = address;
    if let Some(a) = amount.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        let v: f64 = a.parse().map_err(|_| format!("'{a}' is not a number"))?;
        if !(v.is_finite() && v > 0.0) {
            return Err("amount must be positive".to_string());
        }
        text = format!("{text}?amount={a}");
    }
    let qr = qrcodegen::QrCode::encode_text(&text, qrcodegen::QrCodeEcc::Low)
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
        let mut cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
pub fn send(
    state: State<'_, AppState>,
    to: String,
    amount: String,
) -> Result<serde_json::Value, String> {
    let amt = wallet::parse_amount_lkyl(&amount)?;
    let c = wctx(&state)?;
    let scan = wallet::scan(
        &c.rpc,
        &c.seed,
        c.net,
        c.recv_next,
        c.change_next,
        &app_dir(&state),
    )?;
    let (rmax, cmax) = utxo_maxes(&scan.utxos);
    advance_cursors(&state, rmax, cmax);
    // Bond lock: never spend luckynode payout addresses — one stray Send
    // stops the node being draw-eligible (ownership gate).
    let protected: Vec<String> = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            "nothing spendable: {:.8} {} sits in protected addresses (release in Settings → Wallet)",
            locked as f64 / 100_000_000.0,
            ticker(c.net)
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
                "{e} — plus {:.8} {} locked in protected addresses (release in Settings → Wallet)",
                locked as f64 / 100_000_000.0,
                ticker(c.net)
            )
        } else {
            e
        }
    })?;
    if let Some(idx) = r.change_used {
        let mut cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let dir = state
        .cfg_path
        .parent()
        .ok_or("no config dir")?
        .to_path_buf();
    if !wallet::has_any_wallet(&dir) {
        return Ok(json!({ "has_wallet": false }));
    }
    if wallet::is_legacy_plaintext(&dir) {
        return Ok(json!({ "has_wallet": true, "legacy": true, "locked": true }));
    }
    let locked = state
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_none();
    if locked {
        return Ok(json!({ "has_wallet": true, "legacy": false, "locked": true }));
    }
    // Snapshot the cursors under a scoped lock: node::client below locks
    // `cfg` itself, so holding our guard across it deadlocked the app
    // (frozen main thread right after unlock — 2026-10-16).
    let (recv_next, change_next, protected_addrs) = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (cfg.recv_next, cfg.change_next, cfg.protected_addrs.clone())
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
pub fn wallet_create(
    state: State<'_, AppState>,
    password: String,
) -> Result<serde_json::Value, String> {
    let dir = state
        .cfg_path
        .parent()
        .ok_or("no config dir")?
        .to_path_buf();
    let phrase = wallet::create_wallet_enc(&dir, &password)?;
    let (seed, phrase2) = wallet::unlock_seed(&dir, &password)?;
    *state
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(wallet::Session::new(seed, phrase2));
    Ok(json!({ "phrase": phrase }))
}

/// Install a backup phrase (replaces any wallet), then rescan cursors.
#[tauri::command(async)]
pub fn wallet_restore(
    state: State<'_, AppState>,
    phrase: String,
    password: String,
) -> Result<serde_json::Value, String> {
    let dir = state
        .cfg_path
        .parent()
        .ok_or("no config dir")?
        .to_path_buf();
    wallet::restore_wallet_enc(&dir, &phrase, &password)?;
    let (seed, phrase2) = wallet::unlock_seed(&dir, &password)?;
    *state
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(wallet::Session::new(seed, phrase2));
    rescan_inner(&state)
}

/// Unlock with the spending password.
#[tauri::command(async)]
pub fn wallet_unlock(
    state: State<'_, AppState>,
    password: String,
) -> Result<serde_json::Value, String> {
    let dir = state
        .cfg_path
        .parent()
        .ok_or("no config dir")?
        .to_path_buf();
    if wallet::is_legacy_plaintext(&dir) {
        return Err("this wallet predates encryption — migrate it first".to_string());
    }
    let (seed, phrase) = wallet::unlock_seed(&dir, &password)?;
    *state
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(wallet::Session::new(seed, phrase));
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
pub fn wallet_migrate(
    state: State<'_, AppState>,
    password: String,
) -> Result<serde_json::Value, String> {
    let dir = state
        .cfg_path
        .parent()
        .ok_or("no config dir")?
        .to_path_buf();
    let phrase = wallet::migrate_plaintext(&dir, &password)?;
    let (seed, phrase2) = wallet::unlock_seed(&dir, &password)?;
    debug_assert_eq!(phrase, phrase2);
    *state
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(wallet::Session::new(seed, phrase2));
    Ok(json!({ "ok": true }))
}

/// Change the vault password (needs the old one).
#[tauri::command(async)]
pub fn wallet_change_password(
    state: State<'_, AppState>,
    old_password: String,
    new_password: String,
) -> Result<serde_json::Value, String> {
    let dir = state
        .cfg_path
        .parent()
        .ok_or("no config dir")?
        .to_path_buf();
    wallet::change_password(&dir, &old_password, &new_password)?;
    Ok(json!({ "ok": true }))
}

/// Show the phrase (backup display; only while unlocked — the UI must
/// gate this behind a warning).
#[tauri::command]
pub fn wallet_reveal(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let s = state
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let sess = s
        .as_ref()
        .ok_or_else(|| "wallet is locked — unlock it first".to_string())?;
    Ok(json!({ "phrase": sess.phrase.clone() }))
}

fn rescan_inner(state: &State<'_, AppState>) -> Result<serde_json::Value, String> {
    let c = wctx(state)?;
    let (recv_next, change_next, used) = wallet::full_scan_used(&c.rpc, &c.seed, c.net)?;
    {
        let mut cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Never move cursors backwards (shown addresses stay valid).
        if recv_next > cfg.recv_next {
            cfg.recv_next = recv_next;
        }
        if change_next > cfg.change_next {
            cfg.change_next = change_next;
        }
        cfg.save(&state.cfg_path)?;
    }
    let cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// Consensus Luckynode Draw status (doc 06 §6.7): the current-epoch
/// draw from the node's GBT advisories, the last landed claim, and the
/// chain state of the caller's known `ln-` identities.
#[tauri::command(async)]
pub fn luckynodes(
    state: State<'_, AppState>,
    names: Vec<String>,
) -> Result<serde_json::Value, String> {
    use lm_luckynode::identity;
    let rpc = node::client(&state)?;
    let rpc = luckynode::NodeClient(rpc);
    let epoch_len = identity::resolve_epoch_len(&rpc, None)?;
    let gbt: serde_json::Value = rpc.call("getblocktemplate", json!([{"capabilities": []}]))?;
    let draw = serde_json::json!({
        "epoch": gbt.get("prizedrawepoch").and_then(|v| v.as_u64()).unwrap_or(0),
        "budget": gbt.get("prizedrawbudget").and_then(|v| v.as_u64()).unwrap_or(0),
        "remaining": gbt.get("prizedrawremaining").and_then(|v| v.as_u64()).unwrap_or(0),
        "valid": gbt.get("prizedrawvalid").and_then(|v| v.as_bool()).unwrap_or(false),
        "winner": gbt.get("prizedrawwinner").and_then(|v| v.as_str()).unwrap_or(""),
    });
    let ji: serde_json::Value = rpc.call("getjackpotinfo", json!([]))?;
    let last_claim = serde_json::json!({
        "prize": ji.get("prize").and_then(|v| v.as_u64()).unwrap_or(0),
        "epoch": ji.get("prizeclaimepoch").and_then(|v| v.as_u64()).unwrap_or(0),
        "amount": ji.get("prizeclaimamount").and_then(|v| v.as_u64()).unwrap_or(0),
    });
    let mut identities = Vec::new();
    for name in &names {
        match identity::identity_status(&rpc, name, epoch_len) {
            Ok(st) => identities.push(json!({
                "name": st.name,
                "address": st.address,
                "exists": st.exists,
                "update_height": st.update_height,
                "tip_height": st.tip_height,
                "epoch": st.epoch,
                "eligible": st.eligible,
            })),
            Err(e) => identities.push(json!({ "name": name, "error": e })),
        }
    }
    Ok(json!({
        "epoch_len": epoch_len,
        "draw": draw,
        "last_claim": last_claim,
        "identities": identities,
    }))
}

/// Pool dashboard stats (blocks, hashrate, workers, payouts, draws).
#[tauri::command(async)]
pub fn pool_stats(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let url = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pool_url
        .clone();
    let body = pool::get(&url, "api/stats")?;
    serde_json::from_str(&body).map_err(|e| format!("pool json: {e}"))
}

/// In-flight Luckynode registration work (`<appdir>/luckynode-pending.json`).
/// Persisting it is what makes retries safe: the name commitment and the
/// broadcast registration txid are resumed, never re-minted. A corrupt
/// file fails closed for the same reason.
const LUCKYNODE_PENDING_FILE: &str = "luckynode-pending.json";

fn load_luckynode_pending(
    state: &State<'_, AppState>,
) -> Result<std::collections::HashMap<String, lm_luckynode::identity::PendingIdentity>, String> {
    let path = app_dir(state).join(LUCKYNODE_PENDING_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "corrupt {}: {e} (refusing to mint another fee-paying commitment — move it aside to start over)",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn save_luckynode_pending(
    state: &State<'_, AppState>,
    map: &std::collections::HashMap<String, lm_luckynode::identity::PendingIdentity>,
) -> Result<(), String> {
    let path = app_dir(state).join(LUCKYNODE_PENDING_FILE);
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec(map).map_err(|e| format!("pending json: {e}"))?;
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        if let Some(dir) = path.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    };
    write().map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Register a `ln-` Luckynode identity on chain (doc 06 §6.7): name
/// commitment → confirmation → identity registration, all funded from
/// the connected node's own wallet. The payout address may be app-held
/// or node-held — the node never needs its key. Winners are paid by
/// directed coinbase claims, so no ownership proof, bond, or pool
/// account is involved.
#[tauri::command(async)]
pub fn luckynode_register(
    state: State<'_, AppState>,
    name: String,
    address: String,
) -> Result<serde_json::Value, String> {
    use lm_luckynode::identity::{self, EnsureOutcome};
    if !identity::is_ln_name(&name) {
        return Err(format!(
            "identity name must live in the ln- namespace (got {name:?})"
        ));
    }
    if !lm_pool::payout::valid_payout_address(&address) {
        return Err(format!("'{address}' is not a valid payout address"));
    }
    let rpc = node::client(&state)?;
    let rpc = luckynode::NodeClient(rpc);
    let epoch_len = identity::resolve_epoch_len(&rpc, None)?;
    let mut pending_map = load_luckynode_pending(&state)?;
    let pending = pending_map.get(&name).cloned();
    match identity::ensure_identity(&rpc, &name, &address, epoch_len, pending)? {
        EnsureOutcome::Ready(st) => {
            if pending_map.remove(&name).is_some() {
                save_luckynode_pending(&state, &pending_map)?;
            }
            Ok(json!({
                "name": st.name,
                "address": st.address,
                "update_height": st.update_height,
                "eligible": st.eligible,
            }))
        }
        EnsureOutcome::Waiting { message, pending } => {
            let fresh = pending_map.get(&name) != Some(&pending);
            pending_map.insert(name.clone(), pending);
            if fresh {
                save_luckynode_pending(&state, &pending_map)?;
            }
            Err(format!(
                "{message} — saved; retrying resumes this transaction and will not pay another commitment. Mine a block on the node (or wait for one), then retry"
            ))
        }
    }
}

/// Start the embedded heartbeat service for an `ln-` identity: it keeps
/// the registration live (update when the chain crosses into a new
/// epoch) so the consensus draw can pick it. The service runs while the
/// app is open; for 24/7 heartbeats run the standalone daemon.
#[tauri::command]
pub fn luckynode_service_start(
    state: State<'_, AppState>,
    name: String,
    address: String,
) -> Result<serde_json::Value, String> {
    use lm_luckynode::identity;
    if !identity::is_ln_name(&name) {
        return Err(format!(
            "identity name must live in the ln- namespace (got {name:?})"
        ));
    }
    if !lm_pool::payout::valid_payout_address(&address) {
        return Err(format!("'{address}' is not a valid payout address"));
    }
    let mut services = state
        .services
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if services.contains_key(&name) {
        return Ok(json!({ "running": true, "already": true }));
    }
    let rpc = node::client(&state)?;
    let rpc = luckynode::NodeClient(rpc);
    let epoch_len = identity::resolve_epoch_len(&rpc, None)?;
    let svc = luckynode::start(rpc, name.clone(), address, epoch_len, 60);
    services.insert(name, svc);
    drop(services);
    Ok(json!({ "running": true }))
}

#[tauri::command]
pub fn luckynode_service_stop(
    state: State<'_, AppState>,
    name: String,
) -> Result<serde_json::Value, String> {
    let mut services = state
        .services
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(svc) = services.remove(&name) {
        svc.stop.store(true, Ordering::Relaxed);
        return Ok(json!({ "running": false }));
    }
    Ok(json!({ "running": false, "was": false }))
}

#[tauri::command]
pub fn luckynode_service_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let services = state
        .services
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let ids: Vec<&String> = services.keys().collect();
    Ok(json!({ "running": ids }))
}

#[tauri::command]
pub fn get_config(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    notifications: Option<bool>,
) -> Result<(), String> {
    let mut cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    if let Some(v) = notifications {
        cfg.notifications = v;
    }
    cfg.save(&state.cfg_path)
}

// ---- Built-in pool miner ----------------------------------

/// Persist the mining-specific settings (stratum endpoint + worker address).
#[tauri::command(rename_all = "snake_case")]
pub fn set_mining_config(
    state: State<'_, AppState>,
    pool_stratum: String,
    mining_worker: String,
) -> Result<(), String> {
    let mut cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
) -> Result<(), String> {
    let mut slot = state
        .miner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(m) = slot.as_ref() {
        if m.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .running
        {
            return Err("miner already running".to_string());
        }
    }
    // Empty worker = mine into the wallet's own current receive address
    // (needs an unlocked wallet).
    let worker = if worker.trim().is_empty() {
        let seed = session_seed(&state)?;
        let rpc = node::client(&state)?;
        let net = wallet::network_of(&rpc)?;
        let recv_next = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_next;
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

/// Solo mining against the wallet's own node: GBT templates with
/// claim-aware coinbases (jackpot + pot + prize), no pool in the loop
/// (item 51). Rewards land wherever the node's GBT coinbase pays, so
/// this refuses unless that script is one of this wallet's addresses —
/// otherwise solo finds would pay a stranger (node wallet, a stale
/// `-mineraddress`, ...). Point the node at this wallet first with
/// `miner_solo_point_payouts` (or `-mineraddress` in Node flags by
/// hand). Loopback nodes only: a shared node can never pay this wallet.
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)] // Tauri command surface: one arg per UI field.
pub fn miner_solo_start(
    state: State<'_, AppState>,
    device: String,
    threads: usize,
    duty: u32,
    max_temp: i64,
    gpu_platform: Option<usize>,
    gpu_device: Option<usize>,
    gpu_batch: Option<usize>,
    max_power: Option<u32>,
) -> Result<(), String> {
    {
        let slot = state
            .miner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(m) = slot.as_ref() {
            if m.status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .running
            {
                return Err("miner already running".to_string());
            }
        }
    }
    let host = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        node_host(&cfg.rpc_url)
    };
    if !is_loopback_host(&host) {
        return Err(
            "solo mining needs Node on this computer (pool mode works with any node)".to_string(),
        );
    }
    let rpc = node::client(&state)?;
    let seed = session_seed(&state)?;
    let net = wallet::network_of(&rpc)?;
    let (recv_next, change_next) = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (cfg.recv_next, cfg.change_next)
    };
    let label = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        node_host(&cfg.rpc_url)
    };
    let m = miner::start_solo(
        rpc,
        label,
        device,
        threads,
        duty,
        max_temp,
        gpu_platform.unwrap_or(0),
        gpu_device.unwrap_or(0),
        gpu_batch.unwrap_or(512),
        max_power.unwrap_or(0),
        seed,
        net,
        recv_next,
        change_next,
    )?;
    *state
        .miner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(m);
    Ok(())
}

/// Point the managed node's coinbase at this wallet: writes
/// `-mineraddress=<current receive address>` into the Node flags
/// (replacing any stale one). Takes effect on the next node (re)start —
/// the solo-mining readiness check tells the operator when a restart is
/// due. Loopback only; refuses on a shared node.
#[tauri::command]
pub fn miner_solo_point_payouts(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let host = {
        let cfg = state
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        node_host(&cfg.rpc_url)
    };
    if !is_loopback_host(&host) {
        return Err("that only makes sense for Node on this computer".to_string());
    }
    let seed = session_seed(&state)?;
    let rpc = node::client(&state)?;
    let net = wallet::network_of(&rpc)?;
    let recv_next = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .recv_next;
    let addr = wallet::current_receive(&seed, net, recv_next)?;
    let mut cfg = state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut kept: Vec<String> = cfg
        .node_args
        .split_whitespace()
        .filter(|t| !t.starts_with("-mineraddress") && !t.starts_with("-minetolocalwallet"))
        .map(str::to_string)
        .collect();
    kept.push(format!("-mineraddress={addr}"));
    kept.push("-minetolocalwallet=0".to_string());
    cfg.node_args = kept.join(" ");
    cfg.save(&state.cfg_path)?;
    Ok(json!({ "mineraddress": addr }))
}

/// Ask the miner to stop (returns immediately; status flips when the
/// search loop reaches a chunk boundary).
#[tauri::command]
pub fn miner_stop(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(m) = state
        .miner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        m.stop.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
pub fn miner_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let slot = state
        .miner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match slot.as_ref() {
        Some(m) => {
            let s = m
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            serde_json::to_value(s).map_err(|e| e.to_string())
        }
        None => Ok(json!({ "running": false })),
    }
}

/// Recent miner log lines (ring buffer, oldest first).
#[tauri::command]
pub fn miner_log(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let slot = state
        .miner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(slot
        .as_ref()
        .map(|m| {
            m.log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned()
                .collect()
        })
        .unwrap_or_default())
}

/// Fire-and-forget desktop notification (M4): the UI detects events
/// (block found, payout received, draw won) on its existing polls and
/// calls here. Respects the Settings → Wallet toggle; the UI dedupes
/// repeat events per key so the backend stays stateless.
#[tauri::command(async)]
pub fn notify(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    title: String,
    body: String,
) -> Result<serde_json::Value, String> {
    if !state
        .cfg
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .notifications
    {
        return Ok(serde_json::json!({"ok": false, "skipped": "disabled"}));
    }
    let title: String = title.chars().take(80).collect();
    let body: String = body.chars().take(240).collect();
    if title.trim().is_empty() {
        return Err("missing title".to_string());
    }
    use tauri_plugin_notification::NotificationExt;
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"ok": true}))
}

#[cfg(test)]
mod tests {}
