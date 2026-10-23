//! Embedded luckynode service: register + heartbeat one `ln-` LuckyID
//! identity via the wallet's node, so the consensus Luckynode Draw
//! (doc 06 §6.7) can pick it.
//!
//! Same shared flows as the standalone daemon
//! ([`lm_luckynode::identity`]): name commitment → registration →
//! per-epoch identity-update heartbeats. All spending comes from the
//! connected node's own wallet; the payout address may be app-held or
//! node-held. There are no ed25519 side-identities, no pool endpoints,
//! no bonds, and no cosigning — the chain is the registry.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lm_luckynode::identity::{self, EnsureOutcome, NodeRpc, PendingIdentity};

/// NodeRpc over the wallet's node client. Newtype because the orphan
/// rule forbids implementing the shared trait for `RpcClient` directly.
pub struct NodeClient(pub lm_stratum::rpc::RpcClient);

impl NodeRpc for NodeClient {
    fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        self.0.call(method, params)
    }
}

/// Service handle held in app state.
pub struct Service {
    pub stop: Arc<AtomicBool>,
    pub name: String,
}

/// Spawn the register + heartbeat loop for one `ln-` identity. Idempotent:
/// an existing live registration is a no-op, and heartbeats fire at most
/// once per epoch. Errors log and retry — an unfunded node wallet (or a
/// node that is still syncing) resolves without restarting the service.
pub fn start(
    rpc: NodeClient,
    name: String,
    address: String,
    epoch_len: u64,
    interval_secs: u64,
) -> Service {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let svc_name = name.clone();
    std::thread::spawn(move || {
        let mut pending: Option<PendingIdentity> = None;
        let mut last_hb_tip: Option<u64> = None;
        loop {
            if stop_thread.load(Ordering::Relaxed) {
                return;
            }
            // Registration pass (resumed across ticks via `pending`).
            let ready = loop {
                if stop_thread.load(Ordering::Relaxed) {
                    return;
                }
                match identity::ensure_identity(&rpc, &name, &address, epoch_len, pending.clone()) {
                    Ok(EnsureOutcome::Ready(st)) => {
                        eprintln!("luckynode {name}: ready ({})", describe(&st));
                        pending = None;
                        break true;
                    }
                    Ok(EnsureOutcome::Waiting {
                        message,
                        pending: next,
                    }) => {
                        eprintln!("luckynode {name}: waiting: {message}");
                        pending = Some(next);
                    }
                    Err(e) => {
                        eprintln!("luckynode {name}: registration failed: {e}");
                        break false;
                    }
                }
                for _ in 0..6 {
                    if stop_thread.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(interval_secs.max(5)));
                }
            };
            if !ready {
                // Back off a full interval before retrying a hard failure
                // (funds, node down) instead of hot-looping RPC errors.
                for _ in 0..12 {
                    if stop_thread.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(interval_secs.max(5)));
                }
                continue;
            }
            // Heartbeat + status each interval. Never resubmit at the
            // same tip height — an idle chain cannot confirm the
            // pending update and retries would only spam the mempool.
            match identity::heartbeat_if_due(&rpc, &name, epoch_len, last_hb_tip) {
                Ok(Some(tip)) => {
                    last_hb_tip = Some(tip);
                    eprintln!("luckynode {name}: heartbeat submitted at tip {tip}");
                }
                Ok(None) => {}
                Err(e) => eprintln!("luckynode {name}: heartbeat failed: {e}"),
            }
            match identity::identity_status(&rpc, &name, epoch_len) {
                Ok(st) => eprintln!("luckynode {name}: live ({})", describe(&st)),
                Err(e) => eprintln!("luckynode {name}: status failed: {e}"),
            }
            for _ in 0..12 {
                if stop_thread.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(interval_secs.max(5)));
            }
        }
    });
    Service {
        stop,
        name: svc_name,
    }
}

fn describe(st: &lm_luckynode::identity::IdentityState) -> String {
    format!(
        "update@{} tip {} epoch {}/{} eligible={}",
        st.update_height, st.tip_height, st.epoch, st.epoch_len, st.eligible
    )
}
