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
//! (`--epoch` pins it instead). Heartbeats land at most once per epoch,
//! plus one catch-up pass when starting behind. The in-flight name
//! commitment is tracked in `--state-file` (plus in-process) so a retry
//! resumes the same commitment instead of minting another fee-paying
//! one.

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
        let mut s = TcpStream::connect(&self.node)
            .map_err(|e| format!("connect {}: {e}", self.node))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| format!("timeout: {e}"))?;
        s.write_all(req.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).map_err(|e| format!("read: {e}"))?;
        let text = String::from_utf8_lossy(&raw);
        let (_, rbody) = text.split_once("\r\n\r\n").ok_or("malformed http response")?;
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
                epoch = Some(args.get(i).unwrap_or_else(|| usage()).parse().unwrap_or_else(|_| usage()));
            }
            "--state-file" => {
                i += 1;
                state_file = Some(args.get(i).unwrap_or_else(|| usage()).clone());
            }
            "--heartbeat-interval" => {
                i += 1;
                interval = args.get(i).unwrap_or_else(|| usage()).parse().unwrap_or_else(|_| usage());
            }
            "--register-only" => register_only = true,
            "--once" => once = true,
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
    let rpc = Rpc {
        node: node.clone(),
        auth: base64(format!("{user}:{pass}").as_bytes()),
    };
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
    // commitment also persists in --state-file (when set) so a restart
    // resumes it instead of starting over.
    fn load_pending(path: &Option<String>) -> Option<identity::PendingCommitment> {
        let p = path.as_ref()?;
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
        Some(identity::PendingCommitment {
            txid: v.get("commitment_txid")?.as_str()?.to_string(),
            reservation: v.get("namereservation")?.clone(),
        })
    }
    fn save_pending(path: &Option<String>, p: Option<&identity::PendingCommitment>) {
        let Some(path) = path.as_ref() else {
            return;
        };
        let v = match p {
            Some(c) => serde_json::json!({"commitment_txid": c.txid, "namereservation": c.reservation}),
            None => serde_json::json!({}),
        };
        if let Err(e) = std::fs::write(path, v.to_string()) {
            eprintln!("luckynode: state file write failed: {e}");
        }
    }
    let mut pending = load_pending(&state_file);
    let st = loop {
        match identity::ensure_identity(&rpc, &name, &payout, epoch_len, pending.clone()) {
            Ok(EnsureOutcome::Ready(st)) => {
                println!("ready: {}", show(&st));
                save_pending(&state_file, None);
                break st;
            }
            Ok(EnsureOutcome::Waiting { message, commitment }) => {
                eprintln!("waiting: {message} — mine a block (or wait for one)");
                pending = commitment.clone();
                save_pending(&state_file, commitment.as_ref());
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
        match identity::heartbeat_if_due(&rpc, &name, epoch_len) {
            Ok(true) => println!("heartbeat submitted (epoch crossed)"),
            Ok(false) => {}
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
