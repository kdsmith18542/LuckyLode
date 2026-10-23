//! Consensus Luckynode Draw participation over node RPC (doc 06 §6.7).
//!
//! Registration is a LuckyID `ln-` name (payout = the primary address),
//! liveness is an identity update transaction landing inside the epoch
//! window, tenure is first registration, and the sybil cost is the
//! identity fee schedule. No pool, no ed25519 side-identities, no bonds:
//! the chain is the registry, and winners are paid by directed coinbase
//! claims that any block builder attaches.
//!
//! Both consumers — the `luckynode` daemon and the wallet's embedded
//! service — drive these flows through this module so the RPC sequences
//! never drift. All spending comes from the node's own wallet
//! (commitment + registration fee); the primary payout address may be
//! app-held or node-held — the node never needs its key.

use serde_json::{json, Value};

/// Minimal node-RPC surface the flows need. Implemented by the daemon's
/// inline client and by the wallet's `RpcClient`.
pub trait NodeRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

/// Identity state resolved from chain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityState {
    pub name: String,
    pub address: String,
    /// The identity exists on chain (and is not revoked).
    pub exists: bool,
    /// Latest update height — registration counts as the first update.
    pub update_height: u64,
    pub tip_height: u64,
    pub epoch: u64,
    pub epoch_len: u64,
    /// An update landed inside the current epoch window, so the next
    /// epoch-boundary draw can pick this identity.
    pub eligible: bool,
}

/// Epoch length for draw windows: the node's own `prizeepochlen` GBT
/// advisory, unless the operator pins an override. Never guess from the
/// chain label — regtest reports chain "main" but draws every 3 blocks.
pub fn resolve_epoch_len(rpc: &dyn NodeRpc, over: Option<u64>) -> Result<u64, String> {
    if let Some(v) = over {
        if v >= 1 {
            return Ok(v);
        }
        return Err("--epoch must be >= 1".to_string());
    }
    let gbt = rpc.call("getblocktemplate", json!([{"capabilities": []}]))?;
    match gbt.get("prizeepochlen").and_then(|v| v.as_u64()) {
        Some(v) if v >= 1 => Ok(v),
        _ => Err("node did not advertise prizeepochlen (unit-2 build required)".to_string()),
    }
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// Eligibility for the draw the next boundary fixes: an update inside
/// the trailing epoch window (`update >= E*len - len`, integer math).
#[must_use]
pub fn eligible_at(update_height: u64, tip_height: u64, epoch_len: u64) -> bool {
    if epoch_len == 0 {
        return false;
    }
    update_height / epoch_len + 1 >= tip_height / epoch_len
}

/// Read the identity + tip state. Absent (or revoked) identities read as
/// `exists: false` — never an error, so callers can branch to
/// registration.
pub fn identity_status(
    rpc: &dyn NodeRpc,
    name: &str,
    epoch_len: u64,
) -> Result<IdentityState, String> {
    status_inner(rpc, name, epoch_len)
}

fn status_inner(rpc: &dyn NodeRpc, name: &str, epoch_len: u64) -> Result<IdentityState, String> {
    let tip: Value = rpc.call("getblockcount", json!([]))?;
    let tip_height = tip.as_u64().unwrap_or(0);
    let epoch = tip_height.checked_div(epoch_len.max(1)).unwrap_or(0);
    // Absent identities surface as a node-side "not found" RPC error —
    // that means "not registered", not a transport failure.
    let got: Value = match rpc.call("getidentity", json!([format!("{name}@")])) {
        Ok(v) => v,
        Err(e) if e.to_lowercase().contains("not found") => Value::Null,
        Err(e) => return Err(e),
    };
    let ident = got.get("identity");
    let active = got
        .get("status")
        .and_then(|s| s.as_str())
        .map(|s| s == "active")
        .unwrap_or(false);
    let address = ident
        .and_then(|i| i.get("primaryaddresses"))
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|a| a.as_str())
        .unwrap_or("")
        .to_string();
    let update_height = u64_field(&got, "blockheight");
    let exists = ident.is_some() && active && !address.is_empty() && update_height > 0;
    Ok(IdentityState {
        name: name.to_string(),
        address,
        exists,
        update_height: if exists { update_height } else { 0 },
        tip_height,
        epoch,
        epoch_len,
        eligible: exists && eligible_at(update_height, tip_height, epoch_len),
    })
}

/// In-flight name commitment: the txid plus the reservation JSON the
/// registration must reference. Callers persist this across restarts
/// (daemon state file, wallet localStorage) so a retry resumes the same
/// commitment instead of minting another fee-paying one.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCommitment {
    pub txid: String,
    pub reservation: Value,
}

/// Outcome of one `ensure_identity` pass.
#[derive(Debug, Clone, PartialEq)]
pub enum EnsureOutcome {
    /// On chain (newly registered or already there).
    Ready(IdentityState),
    /// A registration step landed in the mempool but is not confirmed
    /// yet — mine a block (or wait for one) and run the pass again,
    /// passing back `commitment` so no second commitment is created.
    Waiting {
        message: String,
        commitment: Option<PendingCommitment>,
    },
}

/// Register `name` with `primary_address` as its payout address unless
/// it already exists. Idempotent: an existing live identity is a
/// no-op returning its status, and a pending commitment is resumed, not
/// replaced. Steps (all node-wallet funded): name commitment →
/// confirmation → identity registration → confirmation.
pub fn ensure_identity(
    rpc: &dyn NodeRpc,
    name: &str,
    primary_address: &str,
    epoch_len: u64,
    pending: Option<PendingCommitment>,
) -> Result<EnsureOutcome, String> {
    if !is_ln_name(name) {
        return Err(format!(
            "identity name must live in the ln- namespace (got {name:?})"
        ));
    }
    let st = status_inner(rpc, name, epoch_len)?;
    if st.exists {
        return Ok(EnsureOutcome::Ready(st));
    }
    // Resume a previously created commitment (same txid + reservation)
    // instead of minting a second fee-paying one.
    let (txid, reservation) = match pending {
        Some(p) => (p.txid, p.reservation),
        None => {
            // Control address for the commitment: a fresh node-wallet
            // address. The identity's payout address stays whatever the
            // operator chose (node-held or app-held — the node never
            // needs its key).
            let control = rpc
                .call("getnewaddress", json!([]))?
                .as_str()
                .unwrap_or("")
                .to_string();
            if control.is_empty() {
                return Err("getnewaddress returned no address".to_string());
            }
            let commitment: Value = rpc.call("registernamecommitment", json!([name, control]))?;
            let txid = commitment
                .get("txid")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let reservation = commitment
                .get("namereservation")
                .cloned()
                .unwrap_or(Value::Null);
            if txid.is_empty() || reservation.is_null() {
                return Err(format!("registernamecommitment failed: {commitment}"));
            }
            (txid, reservation)
        }
    };
    if confirmations(rpc, &txid)? == 0 {
        return Ok(EnsureOutcome::Waiting {
            message: format!("name commitment {txid} unconfirmed"),
            commitment: Some(PendingCommitment { txid, reservation }),
        });
    }
    let chain_id = rpc
        .call("getblockchaininfo", json!([]))?
        .get("chainid")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    if chain_id.is_empty() {
        return Err("getblockchaininfo returned no chainid".to_string());
    }
    let reg: Value = rpc.call(
        "registeridentity",
        json!([{
            "txid": txid,
            "namereservation": reservation,
            "identity": {
                "name": name,
                "parent": chain_id,
                "primaryaddresses": [primary_address],
                "minimumsignatures": 1,
            }
        }]),
    )?;
    let reg_txid = reg.as_str().unwrap_or("").to_string();
    if reg_txid.is_empty() {
        return Err(format!("registeridentity failed: {reg}"));
    }
    if confirmations(rpc, &reg_txid)? == 0 {
        return Ok(EnsureOutcome::Waiting {
            message: format!("identity registration {reg_txid} unconfirmed"),
            commitment: None,
        });
    }
    let st = status_inner(rpc, name, epoch_len)?;
    if st.exists {
        Ok(EnsureOutcome::Ready(st))
    } else {
        Err(format!(
            "registration {reg_txid} confirmed but {name}@ not found"
        ))
    }
}

/// Heartbeat when the chain moved into a new epoch since the last
/// update (or liveness already lapsed): an `updateidentity` with the
/// unchanged definition.
///
/// `last_submitted_tip` is the tip height observed when the previous
/// update was submitted (persist it across restarts when possible).
/// When it equals the current tip, no update is submitted: on an idle
/// chain the pending update cannot confirm, and re-submitting every
/// tick would only fill the mempool with equivalent transactions.
/// Returns `Some(tip)` when an update was submitted at that height,
/// `None` otherwise.
pub fn heartbeat_if_due(
    rpc: &dyn NodeRpc,
    name: &str,
    epoch_len: u64,
    last_submitted_tip: Option<u64>,
) -> Result<Option<u64>, String> {
    if epoch_len == 0 {
        return Err("epoch length must be >= 1".to_string());
    }
    let st = status_inner(rpc, name, epoch_len)?;
    if !st.exists {
        return Err(format!("{name}@ is not registered; register first"));
    }
    let crossed = st.tip_height / epoch_len != st.update_height / epoch_len;
    let lapsed = st.tip_height.saturating_sub(st.update_height) >= epoch_len;
    if !crossed && !lapsed {
        return Ok(None);
    }
    if last_submitted_tip == Some(st.tip_height) {
        return Ok(None);
    }
    let txid: Value = rpc.call("updateidentity", json!([{ "name": name }]))?;
    if txid.as_str().unwrap_or("").is_empty() {
        return Err(format!("updateidentity failed: {txid}"));
    }
    Ok(Some(st.tip_height))
}

/// Confirmations of a wallet transaction (0 when unconfirmed/unknown).
/// `gettransaction` sees node-wallet transactions; anything else reads
/// as unconfirmed rather than erroring.
fn confirmations(rpc: &dyn NodeRpc, txid: &str) -> Result<u64, String> {
    match rpc.call("gettransaction", json!([txid])) {
        Ok(v) => Ok(v.get("confirmations").and_then(|c| c.as_u64()).unwrap_or(0)),
        Err(_) => Ok(0),
    }
}

/// The consensus draw namespace: top-level names starting `ln-`
/// (case-insensitive; the node enforces the rest).
#[must_use]
pub fn is_ln_name(name: &str) -> bool {
    let mut l = name.to_lowercase();
    if l.len() < 4 {
        return false;
    }
    l.truncate(3);
    l == "ln-"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Scripted node: method -> queue of responses.
    struct Mock {
        calls: RefCell<Vec<(String, Value)>>,
        out: HashMap<String, Vec<Value>>,
    }

    impl Mock {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                out: HashMap::new(),
            }
        }
        fn push(mut self, method: &str, v: Value) -> Self {
            self.out.entry(method.to_string()).or_default().push(v);
            self
        }
    }

    impl NodeRpc for Mock {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.borrow_mut().push((method.to_string(), params));
            self.out
                .get(method)
                .and_then(|q| q.first())
                .cloned()
                .ok_or_else(|| format!("no mock for {method}"))
        }
    }

    /// Mock whose queued responses pop in order (for multi-call flows).
    struct Seq {
        calls: RefCell<Vec<(String, Value)>>,
        out: RefCell<HashMap<String, Vec<Value>>>,
    }

    impl Seq {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                out: RefCell::new(HashMap::new()),
            }
        }
        fn push(&self, method: &str, v: Value) {
            self.out
                .borrow_mut()
                .entry(method.to_string())
                .or_default()
                .push(v);
        }
    }

    impl NodeRpc for Seq {
        fn call(&self, method: &str, params: Value) -> Result<Value, String> {
            self.calls.borrow_mut().push((method.to_string(), params));
            self.out
                .borrow_mut()
                .get_mut(method)
                .and_then(|q| {
                    if q.is_empty() {
                        None
                    } else {
                        Some(q.remove(0))
                    }
                })
                .ok_or_else(|| format!("no mock for {method}"))
        }
    }

    #[test]
    fn ln_namespace_gate() {
        assert!(is_ln_name("ln-test"));
        assert!(is_ln_name("LN-Test"));
        assert!(!is_ln_name("test"));
        assert!(!is_ln_name("ln-"));
        assert!(!is_ln_name("ln"));
        assert!(!is_ln_name("xln-test"));
    }

    #[test]
    fn eligibility_windows() {
        // epoch 3: update 44 live at tip 45, dead at tip 48.
        assert!(eligible_at(44, 45, 3));
        assert!(!eligible_at(44, 48, 3));
        // registration at a boundary covers its own epoch...
        assert!(eligible_at(39, 39, 3));
        // ...and exactly one epoch back.
        assert!(eligible_at(39, 42, 3));
        assert!(!eligible_at(39, 45, 3));
        assert!(!eligible_at(0, 0, 0));
    }

    #[test]
    fn status_reads_absent_as_not_exists() {
        let m = Mock::new()
            .push("getblockcount", json!(55u64))
            .push("getidentity", json!({}));
        let st = identity_status(&m, "ln-ghost", 3).expect("status");
        assert!(!st.exists);
        assert_eq!((st.tip_height, st.epoch, st.eligible), (55, 18, false));
    }

    #[test]
    fn status_treats_node_not_found_error_as_absent() {
        // The node reports unknown identities as RPC error -5
        // ("Identity not found") — that means unregistered, not broken.
        struct ErrMock;
        impl NodeRpc for ErrMock {
            fn call(&self, method: &str, _params: Value) -> Result<Value, String> {
                match method {
                    "getblockcount" => Ok(json!(55u64)),
                    _ => Err(
                        "node getidentity: {\"code\":-5,\"message\":\"Identity not found\"}"
                            .to_string(),
                    ),
                }
            }
        }
        let st = identity_status(&ErrMock, "ln-ghost", 3).expect("status");
        assert!(!st.exists);
        assert_eq!(st.update_height, 0);
    }

    #[test]
    fn status_reads_live_identity() {
        let m = Mock::new().push("getblockcount", json!(46u64)).push(
            "getidentity",
            json!({
                "identity": {"primaryaddresses": ["Laddr"]},
                "status": "active",
                "blockheight": 44u64,
            }),
        );
        let st = identity_status(&m, "ln-test", 3).expect("status");
        assert!(st.exists);
        assert_eq!(st.address, "Laddr");
        assert!(st.eligible); // 44 in window [42,44] at tip 46
    }

    #[test]
    fn status_revoked_is_not_exists() {
        let m = Mock::new().push("getblockcount", json!(46u64)).push(
            "getidentity",
            json!({
                "identity": {"primaryaddresses": ["Laddr"]},
                "status": "revoked",
                "blockheight": 44u64,
            }),
        );
        let st = identity_status(&m, "ln-test", 3).expect("status");
        assert!(!st.exists);
    }

    #[test]
    fn heartbeat_skips_when_current() {
        let m = Mock::new().push("getblockcount", json!(40u64)).push(
            "getidentity",
            json!({
                "identity": {"primaryaddresses": ["Laddr"]},
                "status": "active",
                "blockheight": 39u64,
            }),
        );
        // epoch 13 at both heights: no update.
        assert_eq!(heartbeat_if_due(&m, "ln-test", 3, None).expect("hb"), None);
        assert_eq!(m.calls.borrow().len(), 2);
    }

    #[test]
    fn heartbeat_fires_on_epoch_cross() {
        let m = Mock::new()
            .push("getblockcount", json!(42u64))
            .push(
                "getidentity",
                json!({
                    "identity": {"primaryaddresses": ["Laddr"]},
                    "status": "active",
                    "blockheight": 41u64,
                }),
            )
            .push("updateidentity", json!("txid-upd"));
        assert_eq!(
            heartbeat_if_due(&m, "ln-test", 3, None).expect("hb"),
            Some(42)
        );
    }

    #[test]
    fn heartbeat_skips_without_tip_advance() {
        // Epoch crossed, but the tip has not moved since the last
        // submission: the pending update cannot confirm on an idle
        // chain, so resubmitting would only spam the mempool.
        let m = Mock::new().push("getblockcount", json!(42u64)).push(
            "getidentity",
            json!({
                "identity": {"primaryaddresses": ["Laddr"]},
                "status": "active",
                "blockheight": 41u64,
            }),
        );
        assert_eq!(
            heartbeat_if_due(&m, "ln-test", 3, Some(42)).expect("hb"),
            None
        );
        assert_eq!(m.calls.borrow().len(), 2);
    }

    #[test]
    fn heartbeat_resubmits_after_tip_advance() {
        let m = Mock::new()
            .push("getblockcount", json!(43u64))
            .push(
                "getidentity",
                json!({
                    "identity": {"primaryaddresses": ["Laddr"]},
                    "status": "active",
                    "blockheight": 41u64,
                }),
            )
            .push("updateidentity", json!("txid-upd"));
        assert_eq!(
            heartbeat_if_due(&m, "ln-test", 3, Some(42)).expect("hb"),
            Some(43)
        );
    }

    #[test]
    fn heartbeat_fails_closed_when_missing() {
        let m = Mock::new()
            .push("getblockcount", json!(42u64))
            .push("getidentity", json!({}));
        assert!(heartbeat_if_due(&m, "ln-test", 3, None).is_err());
    }

    #[test]
    fn ensure_is_noop_when_live() {
        let m = Mock::new().push("getblockcount", json!(52u64)).push(
            "getidentity",
            json!({
                "identity": {"primaryaddresses": ["LM"]},
                "status": "active",
                "blockheight": 49u64,
            }),
        );
        match ensure_identity(&m, "ln-test", "LM", 3, None).expect("ensure") {
            EnsureOutcome::Ready(st) => assert!(st.exists),
            EnsureOutcome::Waiting { .. } => panic!("should be ready"),
        }
        // No registration RPCs fired.
        assert_eq!(m.calls.borrow().len(), 2);
    }

    #[test]
    fn ensure_full_flow_waits_then_registers() {
        let s = Seq::new();
        // status: absent (3 passes: pre-check, then two post-register reads share one scripted pair — see below).
        s.push("getblockcount", json!(60u64));
        s.push("getidentity", json!({}));
        s.push("getnewaddress", json!("Lctrl"));
        s.push(
            "registernamecommitment",
            json!({"txid": "cmtx", "namereservation": {"name": "ln-new"}}),
        );
        s.push("gettransaction", json!({"confirmations": 0u64}));
        // First pass: commitment unconfirmed → waiting.
        match ensure_identity(&s, "ln-new", "LM", 3, None).expect("ensure") {
            EnsureOutcome::Waiting {
                message: m,
                commitment,
            } => {
                assert!(m.contains("cmtx"));
                assert_eq!(commitment.map(|c| c.txid), Some("cmtx".to_string()));
            }
            EnsureOutcome::Ready(_) => panic!("should wait"),
        }
        // Second pass: everything confirms.
        s.push("getblockcount", json!(61u64));
        s.push("getidentity", json!({}));
        s.push("getnewaddress", json!("Lctrl"));
        s.push(
            "registernamecommitment",
            json!({"txid": "cmtx", "namereservation": {"name": "ln-new"}}),
        );
        s.push("gettransaction", json!({"confirmations": 1u64}));
        s.push("getblockchaininfo", json!({"chainid": "iCHAIN"}));
        s.push("registeridentity", json!("regtx"));
        s.push("gettransaction", json!({"confirmations": 1u64}));
        s.push("getblockcount", json!(61u64));
        s.push(
            "getidentity",
            json!({
                "identity": {"primaryaddresses": ["LM"]},
                "status": "active",
                "blockheight": 61u64,
            }),
        );
        match ensure_identity(&s, "ln-new", "LM", 3, None).expect("ensure") {
            EnsureOutcome::Ready(st) => {
                assert!(st.exists);
                assert_eq!(st.address, "LM");
            }
            EnsureOutcome::Waiting { .. } => panic!("should be ready"),
        }
    }

    #[test]
    fn ensure_rejects_non_ln_names_before_any_rpc() {
        let m = Mock::new();
        assert!(ensure_identity(&m, "test", "LM", 3, None).is_err());
        assert!(m.calls.borrow().is_empty());
    }
}
