//! Luckynodes: bonded service nodes for the game layer,
//! pool-side v1 — no consensus changes).
//!
//! - [`Registry`]: bonded nodes, heartbeat liveness, active set.
//! - Tenure-weighted [`draw_winner`]: deterministic lottery seeded by a
//!   public block hash; tiers 1x (<30d), 1.5x (30-90d), 2x (90d+).
//! - [`notarize`]: ed25519 heartbeat/cosign messages (provable service,
//!   slashable misbehavior — no fake crypto, even in tests).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Tenure tier edges (continuous active-service seconds).
pub const TIER_1_SECS: u64 = 30 * 86400;
pub const TIER_2_SECS: u64 = 90 * 86400;

/// Weight in millis (1x / 1.5x / 2x) for a tenure in seconds.
#[must_use]
pub fn tenure_weight_millis(tenure_secs: u64) -> u64 {
    if tenure_secs >= TIER_2_SECS {
        2000
    } else if tenure_secs >= TIER_1_SECS {
        1500
    } else {
        1000
    }
}

/// One registered luckynode. `id` is the hex ed25519 pubkey (the identity
/// that signs heartbeats and cosigns); `address` is the payout address
/// (bond lives here; the pool verifies the balance via the node).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub address: String,
    pub registered_at: u64,
    /// First heartbeat of the current continuous-service stint
    /// (downtime beyond the reset gap restarts tenure).
    pub tenure_start: u64,
    pub last_heartbeat: u64,
    pub heartbeats: u64,
    /// Payout address ownership proven at registration (signed message
    /// verified by the pool via the node's `verifymessage`). Draw
    /// eligibility folds this into the bond verdict each tick.
    #[serde(default)]
    pub owned: bool,
    /// Designated locked-collateral UTXO (txid, vout). When set, the pool
    /// verifies it is still unspent every tick; spending it evicts the
    /// node immediately. Balance-only checks let
    /// funds drift in and out between ticks — a designated UTXO does not.
    #[serde(default)]
    pub bond_utxo: Option<(String, u32)>,
    pub bond_ok: bool,
    pub bond_checked_at: u64,
}

/// Bonded-node registry with heartbeat liveness.
#[derive(Debug, Default)]
pub struct Registry {
    nodes: HashMap<String, Node>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore a persisted registry verbatim (state-file load). Tenure,
    /// heartbeat counts and ownership survive a pool restart instead of
    /// resetting to zero.
    pub fn restore(&mut self, nodes: Vec<Node>) {
        self.nodes = nodes.into_iter().map(|n| (n.id.clone(), n)).collect();
    }

    /// Designate (or replace) the locked bond UTXO. Returns false for
    /// unknown ids. Spending the designated UTXO is eviction.
    pub fn designate_bond(&mut self, id: &str, txid: &str, vout: u32) -> bool {
        match self.nodes.get_mut(id) {
            Some(n) => {
                n.bond_utxo = Some((txid.to_string(), vout));
                true
            }
            None => false,
        }
    }

    /// Register (or re-register) a node. Re-registration keeps tenure
    /// only if the node never went dark; a fresh id starts tenure now.
    /// `owned` records whether the payout address signed the registration
    /// (proof of control; the pool verifies it).
    pub fn register(&mut self, id: &str, address: &str, now: u64, owned: bool) {
        if let Some(n) = self.nodes.get_mut(id) {
            n.address = address.to_string();
            n.owned = owned;
        } else {
            self.nodes.insert(
                id.to_string(),
                Node {
                    id: id.to_string(),
                    address: address.to_string(),
                    registered_at: now,
                    tenure_start: now,
                    last_heartbeat: 0,
                    heartbeats: 0,
                    owned,
                    bond_utxo: None,
                    bond_ok: false,
                    bond_checked_at: 0,
                },
            );
        }
    }

    /// Record a heartbeat; returns false for unknown ids. A gap longer
    /// than `reset_gap_secs` restarts tenure (downtime resets).
    pub fn heartbeat(&mut self, id: &str, now: u64, reset_gap_secs: u64) -> bool {
        match self.nodes.get_mut(id) {
            None => false,
            Some(n) => {
                // A gap longer than the reset window restarts tenure
                // (downtime resets); first heartbeat starts it.
                if n.last_heartbeat == 0
                    || now.saturating_sub(n.last_heartbeat) > reset_gap_secs
                {
                    n.tenure_start = now;
                }
                n.last_heartbeat = now;
                n.heartbeats += 1;
                true
            }
        }
    }

    /// Mark the bond check result (the pool verifies the address balance
    /// via the node; this crate only records the verdict).
    pub fn set_bond(&mut self, id: &str, ok: bool, now: u64) -> bool {
        match self.nodes.get_mut(id) {
            None => false,
            Some(n) => {
                n.bond_ok = ok;
                n.bond_checked_at = now;
                true
            }
        }
    }

    /// Nodes with a heartbeat inside the trailing window (active service).
    /// Holding a bond without heartbeats does not qualify.
    #[must_use]
    pub fn active(&self, now: u64, window_secs: u64) -> Vec<&Node> {
        self.nodes
            .values()
            .filter(|n| n.last_heartbeat > 0 && now.saturating_sub(n.last_heartbeat) <= window_secs)
            .collect()
    }

    /// Draw eligibles: active service AND a verified bond. Either missing
    /// disqualifies (passive stake and unfunded nodes never win).
    #[must_use]
    pub fn draw_eligibles(&self, now: u64, window_secs: u64) -> Vec<&Node> {
        self.active(now, window_secs)
            .into_iter()
            .filter(|n| n.bond_ok)
            .collect()
    }

    /// Continuous active-service seconds for draws.
    #[must_use]
    pub fn tenure_secs(&self, n: &Node, now: u64) -> u64 {
        now.saturating_sub(n.tenure_start)
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// All registered nodes (for bond sweeps; liveness still gates draws).
    #[must_use]
    pub fn all_nodes(&self) -> Vec<&Node> {
        self.nodes.values().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Deterministic tenure-weighted lottery: `seed` is the public epoch-start
/// block hash (anyone can recompute). Weights are tenure millis; the
/// winner is `u64(seed[..8]) % total` walked over id-sorted entrants.
#[must_use]
pub fn draw_winner<'a>(entrants: &[(&'a str, u64)], seed: &[u8; 32]) -> Option<&'a str> {
    let total: u128 = entrants.iter().map(|(_, w)| *w as u128).sum();
    if total == 0 || entrants.is_empty() {
        return None;
    }
    let mut sorted: Vec<(&'a str, u64)> = entrants.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut r = u128::from(u64::from_le_bytes(seed[..8].try_into().expect("seed len")));
    r %= total;
    for (id, w) in &sorted {
        let w = *w as u128;
        if r < w {
            return Some(*id);
        }
        r -= w;
    }
    sorted.last().map(|(id, _)| *id)
}

/// Signed messages (canonical strings; ed25519 over UTF-8 bytes).
pub mod notarize {
    /// Canonical heartbeat message a luckynode signs.
    #[must_use]
    pub fn heartbeat_msg(node_id: &str, timestamp: u64) -> String {
        format!("luckynode-heartbeat:v1:{node_id}:{timestamp}")
    }

    /// Canonical draw-cosign message (draw id + the block hash it covers).
    #[must_use]
    pub fn cosign_msg(draw_id: &str, block_hash: &str) -> String {
        format!("luckynode-cosign:v1:{draw_id}:{block_hash}")
    }

    /// Sign with a 32-byte ed25519 secret key; returns 64-byte signature.
    pub fn sign(secret_32: &[u8], msg: &str) -> Result<Vec<u8>, String> {
        use ed25519_dalek::{Signer, SigningKey};
        let sk = SigningKey::from_bytes(
            secret_32
                .try_into()
                .map_err(|_| "secret key must be 32 bytes".to_string())?,
        );
        Ok(sk.sign(msg.as_bytes()).to_bytes().to_vec())
    }

    /// Verify a 64-byte signature against a 32-byte pubkey.
    pub fn verify(pubkey_32: &[u8], msg: &str, sig_64: &[u8]) -> bool {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let pk: [u8; 32] = match pubkey_32.try_into() {
            Ok(b) => b,
            Err(_) => return false,
        };
        let Ok(pk) = VerifyingKey::from_bytes(&pk) else {
            return false;
        };
        let Ok(sig) = Signature::from_slice(sig_64) else {
            return false;
        };
        pk.verify(msg.as_bytes(), &sig).is_ok()
    }

    /// hex helpers (no extra deps).
    #[must_use]
    pub fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn unhex(s: &str) -> Result<Vec<u8>, String> {
        if !s.len().is_multiple_of(2) {
            return Err("odd hex length".to_string());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse a locked-collateral reference ("txid:vout"), shared by
/// registration input and any client that re-arms a lock from it.
pub fn parse_bond_utxo(s: &str) -> Result<(String, u32), String> {
    let (txid, vout) = s.split_once(':').ok_or("bond_utxo must be txid:vout")?;
    if txid.len() != 64
        || !txid.chars().all(|c| c.is_ascii_hexdigit())
        || vout.parse::<u32>().is_err()
    {
        return Err("bond_utxo must be txid:vout (64 hex + index)".to_string());
    }
    Ok((txid.to_string(), vout.parse::<u32>().map_err(|e| e.to_string())?))
}
/// Registration body proving the registrar controls the payout address:
/// the address's key signs a canonical, timestamped message (the pool
/// verifies via the node's `verifymessage`). Shared by the wallet's embedded
/// service and the CLI daemon so the format never drifts.
/// `bond_utxo` is the optional locked collateral ("txid:vout"): when
/// present the pool evicts the node the moment that output is spent.
/// Shape-checked here so typos fail fast instead of pool-side.
pub fn register_body(
    id: &str,
    address: &str,
    payout_secret: &[u8; 32],
    bond_utxo: Option<&str>,
) -> Result<serde_json::Value, String> {
    let ts = now_secs();
    let msg = format!("luckynode-register:v1:{id}:{address}:{ts}");
    let sig = lm_keys::sign_message(payout_secret, &msg)?;
    let mut body = serde_json::json!({
        "id": id,
        "address": address,
        "pubkey": id,
        "msg": msg,
        "sig": sig,
        "ts": ts,
    });
    if let Some(utxo) = bond_utxo {
        parse_bond_utxo(utxo)?;
        body["bond_utxo"] = serde_json::Value::String(utxo.to_string());
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenure_tiers() {
        assert_eq!(tenure_weight_millis(0), 1000);
        assert_eq!(tenure_weight_millis(TIER_1_SECS - 1), 1000);
        assert_eq!(tenure_weight_millis(TIER_1_SECS), 1500);
        assert_eq!(tenure_weight_millis(TIER_2_SECS - 1), 1500);
        assert_eq!(tenure_weight_millis(TIER_2_SECS), 2000);
        assert_eq!(tenure_weight_millis(u64::MAX), 2000);
    }

    #[test]
    fn bond_utxo_parses_and_rejects_shapes() {
        let (t, v) = parse_bond_utxo(&("ab".repeat(32) + ":3")).expect("valid");
        assert_eq!((t.len(), v), (64, 3));
        assert!(parse_bond_utxo("nope").is_err());
        assert!(parse_bond_utxo("ab:3").is_err());
        assert!(parse_bond_utxo(&("ab".repeat(32) + ":x")).is_err());
    }

    #[test]
    fn register_body_carries_optional_bond_utxo() {
        let secret = [7u8; 32];
        let bare = register_body("id1", "mAddr", &secret, None).expect("bare");
        assert!(bare.get("bond_utxo").is_none());
        assert_eq!(bare.get("id").and_then(|v| v.as_str()), Some("id1"));
        let utxo = "ab".repeat(32) + ":3";
        let locked =
            register_body("id1", "mAddr", &secret, Some(&utxo)).expect("locked");
        assert_eq!(
            locked.get("bond_utxo").and_then(|v| v.as_str()),
            Some(utxo.as_str())
        );
        // Junk shapes fail fast, before any pool round-trip.
        assert!(register_body("id1", "mAddr", &secret, Some("nope")).is_err());
        assert!(register_body("id1", "mAddr", &secret, Some("ab:3")).is_err());
        let bad_vout = "ab".repeat(32) + ":x";
        assert!(register_body("id1", "mAddr", &secret, Some(&bad_vout)).is_err());
    }

    #[test]
    fn registry_liveness() {
        let mut r = Registry::new();
        r.register("a", "addr-a", 1000, true);
        r.register("b", "addr-b", 1000, false);
        // No heartbeats: nobody active.
        assert!(r.active(2000, 1800).is_empty());
        assert!(r.heartbeat("a", 1500, 86400));
        assert!(!r.heartbeat("ghost", 1500, 86400));
        // a heartbeat at 1500 is inside the window at 2000; b is not.
        let act = r.active(2000, 1800);
        assert_eq!(act.len(), 1);
        assert_eq!(act[0].id, "a");
        // Tenure starts at first heartbeat.
        assert_eq!(r.tenure_secs(&act[0].clone(), 2000), 500);
        // Long gap resets tenure.
        assert!(r.heartbeat("a", 2000 + 90000, 86400));
        let n = r.get("a").expect("a");
        assert_eq!(n.tenure_start, 2000 + 90000);
        assert_eq!(n.heartbeats, 2);
    }

    #[test]
    fn designate_bond_survives_restore() {
        // Locked-collateral UTXO: designated at register, still there
        // after a state-file round trip; unknown ids refuse.
        let mut r = Registry::new();
        r.register("a", "addr-a", 1000, true);
        assert!(r.get("a").expect("a").bond_utxo.is_none());
        assert!(r.designate_bond("a", &"ab".repeat(32), 1));
        assert!(!r.designate_bond("ghost", &"ab".repeat(32), 0));
        let snap: Vec<Node> = r.all_nodes().iter().map(|n| (*n).clone()).collect();
        let json = serde_json::to_string(&snap).expect("ser");
        let back: Vec<Node> = serde_json::from_str(&json).expect("de");
        let mut r2 = Registry::new();
        r2.restore(back);
        let n = r2.get("a").expect("a");
        assert_eq!(n.bond_utxo, Some(("ab".repeat(32), 1)));
        assert_eq!(n.registered_at, 1000); // tenure survived, not reset
        assert!(n.owned);
    }

    #[test]
    fn draw_eligibles_needs_service_and_bond() {
        let mut r = Registry::new();
        r.register("funded-active", "addr1", 1000, true);
        r.register("unfunded-active", "addr2", 1000, false);
        r.register("funded-idle", "addr3", 1000, true);
        assert!(r.heartbeat("funded-active", 1500, 86400));
        assert!(r.heartbeat("unfunded-active", 1500, 86400));
        // funded-idle never heartbeats.
        assert!(r.set_bond("funded-active", true, 1600));
        assert!(r.set_bond("funded-idle", true, 1600));
        // unfunded-active has no bond verdict (false).
        let elig = r.draw_eligibles(2000, 1800);
        assert_eq!(elig.len(), 1);
        assert_eq!(elig[0].id, "funded-active");
        // Losing the bond mid-epoch disqualifies immediately.
        assert!(r.set_bond("funded-active", false, 2100));
        assert!(r.draw_eligibles(2200, 1800).is_empty());
    }

    #[test]
    fn draw_deterministic_and_weighted() {        let e = [("alice", 1000u64), ("bob", 2000u64)];
        let s1 = [7u8; 32];
        let s2 = [9u8; 32];
        // Deterministic: same seed, same winner, every time.
        assert_eq!(draw_winner(&e, &s1), draw_winner(&e, &s1));
        // Empty / zero-weight: none.
        let empty: [(&str, u64); 0] = [];
        assert_eq!(draw_winner(&empty, &s1), None);
        assert_eq!(draw_winner(&[("x", 0)], &s1), None);
        // Single entrant always wins.
        assert_eq!(draw_winner(&[("solo", 1000)], &s2), Some("solo"));
        // Weight proportionality over many seeds: bob (2x) wins ~2/3.
        let mut alice = 0u32;
        let mut bob = 0u32;
        for i in 0u32..3000 {
            let mut seed = [0u8; 32];
            seed[..4].copy_from_slice(&i.to_le_bytes());
            seed[4..8].copy_from_slice(&0x9e3779b9u32.to_le_bytes());
            match draw_winner(&e, &seed) {
                Some("alice") => alice += 1,
                Some("bob") => bob += 1,
                _ => panic!("unexpected"),
            }
        }
        let frac = bob as f64 / (alice + bob) as f64;
        assert!((frac - 2.0 / 3.0).abs() < 0.03, "bob frac {frac}");
    }

    #[test]
    fn notarize_roundtrip_and_tamper() {
        use ed25519_dalek::SigningKey;
        let secret = [42u8; 32];
        let sk = SigningKey::from_bytes(&secret);
        let pk = sk.verifying_key().to_bytes();
        let msg = notarize::heartbeat_msg("node1", 12345);
        let sig = notarize::sign(&secret, &msg).expect("sign");
        assert_eq!(sig.len(), 64);
        assert!(notarize::verify(&pk, &msg, &sig));
        // Tampered message fails.
        assert!(!notarize::verify(&pk, "luckynode-heartbeat:v1:node1:12346", &sig));
        // Wrong key fails.
        let other_pk = SigningKey::from_bytes(&[7u8; 32]).verifying_key().to_bytes();
        assert!(!notarize::verify(&other_pk, &msg, &sig));
        // Cosign message format is stable.
        assert_eq!(
            notarize::cosign_msg("draw-3", "abc"),
            "luckynode-cosign:v1:draw-3:abc"
        );
    }
}
