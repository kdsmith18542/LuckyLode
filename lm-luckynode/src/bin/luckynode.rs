//! luckynode daemon: keep one `ln-` LuckyID identity registered and
//! heartbeating so the consensus Luckynode Draw (doc 06 §6.7) can pick
//! it. All spending comes from the node's own wallet (name-commitment +
//! registration fee); the payout address may be app-held or node-held —
//! the node never needs its key.
//!
//! usage:
//!   luckynode --node-rpc HOST:PORT --rpc-user U --rpc-pass P
//!             --name ln-foo --payout-address ADDR
//!             [--epoch N] [--state-file PATH] [--heartbeat-interval SECS]
//!             [--register-only] [--once]
//!
//! Epoch length defaults to the node's own `prizeepochlen` GBT advisory
//! (`--epoch` pins it instead). Heartbeats land at most once per epoch
//! (plus one catch-up pass when starting behind) and never twice at the
//! same tip height — an idle chain cannot confirm a pending update, so
//! re-submitting per tick would only fill the mempool with equivalent
//! transactions. The in-flight name commitment and the last heartbeat
//! tip height are tracked in `--state-file` (plus in-process) so a
//! retry resumes the same commitment instead of minting another
//! fee-paying one, and a restart on an idle chain does not resubmit.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use lm_luckynode::identity::{self, EnsureOutcome, NodeRpc};
use serde_json::Value;

/// Flush stdout (it is block-buffered under redirection; `process::exit`
/// never flushes). Call before every early exit.
fn flush_out() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  luckynode --node-rpc HOST:PORT --rpc-user U --rpc-pass P --name ln-foo --payout-address ADDR [--epoch N] [--state-file PATH] [--heartbeat-interval SECS] [--register-only] [--once]"
    );
    flush_out();
    std::process::exit(2);
}

/// Minimal JSON-RPC client (std only): POST a method call, return the
/// JSON `result`. Read-to-EOF framing on a fresh connection per call —
/// a daemon's handful of calls per minute never needs pooling.
struct Rpc {
    node: String,
    auth: String,
}

impl Rpc {
    fn call_raw(&self, method: &str, params: &str) -> Result<Value, String> {
        let body = format!(
            "{{\"jsonrpc\":\"1.0\",\"id\":\"ln\",\"method\":\"{method}\",\"params\":{params}}}"
        );
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.node,
            self.auth,
            body.len(),
            body
        );
        let mut s =
            TcpStream::connect(&self.node).map_err(|e| format!("connect {}: {e}", self.node))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| format!("timeout: {e}"))?;
        s.write_all(req.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).map_err(|e| format!("read: {e}"))?;
        let text = String::from_utf8_lossy(&raw);
        let (_, rbody) = text
            .split_once("\r\n\r\n")
            .ok_or("malformed http response")?;
        let v: Value =
            serde_json::from_str(rbody).map_err(|e| format!("bad json: {e}: {rbody:.120}"))?;
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            return Err(format!("node {method}: {err}"));
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| format!("node {method}: no result: {rbody:.120}"))
    }
}

impl NodeRpc for Rpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_raw(method, &params.to_string())
    }
}

/// base64 (std only; RPC basic auth).
fn base64(input: &[u8]) -> String {
    const ALPH: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in input.chunks(3) {
        let mut n = 0u32;
        for (i, &b) in c.iter().enumerate() {
            n |= (b as u32) << (8 * (2 - i));
        }
        let pad = 3 - c.len();
        for i in 0..4 - pad {
            out.push(ALPH[((n >> (6 * (3 - i))) & 63) as usize] as char);
        }
        for _ in 0..pad {
            out.push('=');
        }
    }
    out
}

fn show(st: &lm_luckynode::identity::IdentityState) -> String {
    format!(
        "{}: update@{} tip {} epoch {}/{} eligible={} payout={}",
        st.name,
        if st.exists {
            st.update_height.to_string()
        } else {
            "-".to_string()
        },
        st.tip_height,
        st.epoch,
        st.epoch_len,
        st.eligible,
        st.address
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("usage:\n  luckynode --node-rpc HOST:PORT --rpc-user U --rpc-pass P --name ln-foo --payout-address ADDR [--epoch N] [--state-file PATH] [--heartbeat-interval SECS] [--register-only] [--once] [--allow-mainnet]");
        flush_out();
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("luckynode {}", env!("CARGO_PKG_VERSION"));
        flush_out();
        return;
    }
    let mut node = String::new();
    let mut user = String::new();
    let mut pass = String::new();
    let mut name = String::new();
    let mut payout = String::new();
    let mut epoch: Option<u64> = None;
    let mut state_file: Option<String> = None;
    let mut interval: u64 = 60;
    let mut register_only = false;
    let mut once = false;
    let mut allow_mainnet = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--node-rpc" => {
                i += 1;
                node = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--rpc-user" => {
                i += 1;
                user = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--rpc-pass" => {
                i += 1;
                pass = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--name" => {
                i += 1;
                name = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--payout-address" => {
                i += 1;
                payout = args.get(i).unwrap_or_else(|| usage()).clone();
            }
            "--epoch" => {
                i += 1;
                epoch = Some(
                    args.get(i)
                        .unwrap_or_else(|| usage())
                        .parse()
                        .unwrap_or_else(|_| usage()),
                );
            }
            "--state-file" => {
                i += 1;
                state_file = Some(args.get(i).unwrap_or_else(|| usage()).clone());
            }
            "--heartbeat-interval" => {
                i += 1;
                interval = args
                    .get(i)
                    .unwrap_or_else(|| usage())
                    .parse()
                    .unwrap_or_else(|_| usage());
            }
            "--register-only" => register_only = true,
            "--once" => once = true,
            "--allow-mainnet" => allow_mainnet = true,
            _ => usage(),
        }
        i += 1;
    }
    if node.is_empty() || user.is_empty() || name.is_empty() || payout.is_empty() {
        usage();
    }
    if !identity::is_ln_name(&name) {
        eprintln!("luckynode: --name must live in the ln- namespace (got {name:?})");
        flush_out();
        std::process::exit(2);
    }
    // Local state first (fail fast, no network): a corrupt state file
    // refuses before anything fee-paying is even attempted.
    let mut state = load_state(&state_file).unwrap_or_else(|e| {
        eprintln!("luckynode: {e}");
        flush_out();
        std::process::exit(1);
    });
    let rpc = Rpc {
        node: node.clone(),
        auth: base64(format!("{user}:{pass}").as_bytes()),
    };
    // Chain guard: registration spends node-wallet funds, so say which
    // chain we see and refuse anything but testnet without an explicit
    // opt-in ("main" covers mainnet AND regtest, which reports "main").
    let chain = rpc
        .call("getblockchaininfo", serde_json::json!([]))
        .ok()
        .and_then(|v| v.get("chain").and_then(|c| c.as_str()).map(str::to_string));
    match chain.as_deref() {
        Some("test") => {}
        Some(other) if allow_mainnet => {
            println!("luckynode: chain {other} (allowed via --allow-mainnet)");
            flush_out();
        }
        Some(other) => {
            eprintln!(
                "luckynode: refusing to spend on chain {other:?} without --allow-mainnet (regtest also reports \"main\" — the flag covers both)"
            );
            flush_out();
            std::process::exit(2);
        }
        None => {
            eprintln!("luckynode: cannot read node chain (getblockchaininfo failed)");
            flush_out();
            std::process::exit(1);
        }
    }
    // Payout preflight: a typo'd address must never reach the fee-paying
    // registration sequence.
    let payout_ok = rpc
        .call("validateaddress", serde_json::json!([payout]))
        .ok()
        .and_then(|v| v.get("isvalid").and_then(|b| b.as_bool()))
        .unwrap_or(false);
    if !payout_ok {
        eprintln!("luckynode: payout address {payout:?} is not valid on this node");
        flush_out();
        std::process::exit(2);
    }
    let epoch_len = match identity::resolve_epoch_len(&rpc, epoch) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("luckynode: {e}");
            flush_out();
            std::process::exit(1);
        }
    };
    println!("luckynode {name} -> node {node} (payout {payout}, epoch {epoch_len})");

    // Registration pass (idempotent): commit → confirm → register →
    // confirm, or a no-op when already live. Waiting states loop
    // in-process so one invocation creates at most one commitment —
    // re-running the binary while a commitment is unconfirmed must not
    // mint another fee-paying commitment each time. The in-flight
    // commitment and the last heartbeat tip height persist in
    // --state-file (when set) so a restart resumes them instead of
    // starting over.
    #[derive(Default)]
    struct State {
        pending: identity::PendingIdentity,
        last_heartbeat_tip: Option<u64>,
    }
    fn load_state(path: &Option<String>) -> Result<State, String> {
        let mut st = State::default();
        let Some(p) = path.as_ref() else {
            return Ok(st);
        };
        // A missing file is a fresh start; anything else that stops us
        // reading (or parses wrong) fails CLOSED — silently dropping a
        // pending fee-paying commitment is how duplicates get minted.
        let text = match std::fs::read_to_string(p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(st),
            Err(e) => {
                return Err(format!(
                    "cannot read state file {p}: {e} (refusing to run blind — move it aside or fix permissions)"
                ))
            }
        };
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            format!(
                "corrupt state file {p}: {e} (refusing to run blind — move it aside or delete it)"
            )
        })?;
        if let (Some(txid), Some(reservation)) = (
            v.get("commitment_txid").and_then(|t| t.as_str()),
            v.get("namereservation"),
        ) {
            st.pending.commitment = Some(identity::PendingCommitment {
                txid: txid.to_string(),
                reservation: reservation.clone(),
            });
        }
        st.pending.registration = v
            .get("registration_txid")
            .and_then(|t| t.as_str())
            .map(str::to_string);
        st.last_heartbeat_tip = v.get("last_heartbeat_tip").and_then(|t| t.as_u64());
        Ok(st)
    }
    fn save_state(path: &Option<String>, st: &State) {
        let Some(path) = path.as_ref() else {
            return;
        };
        let mut v = serde_json::Map::new();
        if let Some(c) = st.pending.commitment.as_ref() {
            v.insert("commitment_txid".into(), serde_json::json!(c.txid));
            v.insert("namereservation".into(), c.reservation.clone());
        }
        if let Some(txid) = st.pending.registration.as_ref() {
            v.insert("registration_txid".into(), serde_json::json!(txid));
        }
        if let Some(tip) = st.last_heartbeat_tip {
            v.insert("last_heartbeat_tip".into(), serde_json::json!(tip));
        }
        let bytes = serde_json::Value::Object(v).to_string().into_bytes();
        // Atomic: tmp + fsync + rename + dir fsync. A crash mid-write
        // must never leave a truncated file that reads back as "no
        // pending commitment".
        let tmp = format!("{path}.tmp");
        let result = std::fs::File::create(&tmp).and_then(|mut f| {
            use std::io::Write;
            f.write_all(&bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
            if let Some(dir) = std::path::Path::new(path).parent() {
                if let Ok(d) = std::fs::File::open(dir) {
                    let _ = d.sync_all();
                }
            }
            Ok(())
        });
        if let Err(e) = result {
            eprintln!("luckynode: state file write failed: {e}");
        }
    }
    let st = loop {
        match identity::ensure_identity(
            &rpc,
            &name,
            &payout,
            epoch_len,
            Some(state.pending.clone()),
        ) {
            Ok(EnsureOutcome::Ready(st)) => {
                println!("ready: {}", show(&st));
                state.pending = Default::default();
                save_state(&state_file, &state);
                break st;
            }
            Ok(EnsureOutcome::Waiting { message, pending }) => {
                eprintln!("waiting: {message} — mine a block (or wait for one)");
                state.pending = pending;
                save_state(&state_file, &state);
                std::thread::sleep(Duration::from_secs(interval.max(5)));
            }
            Err(e) => {
                eprintln!("luckynode: registration failed: {e}");
                flush_out();
                std::process::exit(1);
            }
        }
    };
    if register_only {
        return;
    }
    let _ = st;

    loop {
        match identity::heartbeat_if_due(&rpc, &name, epoch_len, state.last_heartbeat_tip) {
            Ok(Some(tip)) => {
                state.last_heartbeat_tip = Some(tip);
                save_state(&state_file, &state);
                println!("heartbeat submitted at tip {tip} (epoch crossed or lapsed)");
            }
            Ok(None) => {}
            Err(e) => eprintln!("luckynode: heartbeat failed: {e}"),
        }
        match identity::identity_status(&rpc, &name, epoch_len) {
            Ok(st) => println!("live: {}", show(&st)),
            Err(e) => eprintln!("luckynode: status failed: {e}"),
        }
        if once {
            return;
        }
        std::thread::sleep(Duration::from_secs(interval.max(5)));
    }
}
