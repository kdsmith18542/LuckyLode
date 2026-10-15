//! Participation oracle (doc 14 §14.4): turns verified PoW shares, PoS stake
//! and Luckynode service into the [`crate::tier`] inputs.
//!
//! Sources (all read-only, best-effort — a missing source degrades that input
//! to zero rather than failing the tier):
//! - **PoW:** the pool's `GET /api/stats` → `workers[].{miner,name,shares}`.
//! - **PoS:** the node's `getstakinginfo` → `staked` (operator-attested per
//!   player via the mapping for testnet; an on-chain address lookup replaces
//!   this at mainnet).
//! - **Luckynode:** the pool's `GET /api/luckynodes` → `nodes[].{id,address,
//!   tenure_secs,bond_ok,last_heartbeat}`.
//!
//! Players are mapped to identities by the operator (`/api/admin/map`).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::rpc::value_to_micros;
use crate::tier::{participation_score, tier_for, Participation, Tier};

/// Operator mapping from a casino player to their mining/staking identities.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Mapping {
    /// Pool worker name (or numeric miner id as a string).
    pub worker: String,
    /// Attested PoS stake in micro-LKYL.
    pub stake_micros: u64,
    /// Luckynode id, if the player runs one.
    pub luckynode_id: Option<String>,
}

/// Minimal HTTP GET returning JSON.
pub trait HttpGet: Send + Sync {
    /// GET `url`, parse the body as JSON.
    fn get(&self, url: &str) -> Result<Value, String>;
}

/// std-only HTTP/1.1 GET client (no TLS — pool/node APIs are local/plain).
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpJson;

impl HttpGet for HttpJson {
    fn get(&self, url: &str) -> Result<Value, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("only http:// supported: {url}"))?;
        let (hostport, path) = match rest.split_once('/') {
            Some((h, p)) => (h, format!("/{p}")),
            None => (rest, "/".to_string()),
        };
        let mut stream =
            TcpStream::connect(hostport).map_err(|e| format!("connect {hostport}: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(req.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&resp);
        let (_, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| "malformed HTTP response".to_string())?;
        serde_json::from_str(body).map_err(|e| format!("bad json from {url}: {e}"))
    }
}

/// Canned-response GET for tests.
#[derive(Debug, Default)]
pub struct MockHttp {
    responses: Mutex<BTreeMap<String, Value>>,
}

impl MockHttp {
    /// New empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the JSON returned for `url`.
    pub fn set(&self, url: &str, value: Value) {
        self.responses
            .lock()
            .expect("mock lock")
            .insert(url.to_string(), value);
    }
}

impl HttpGet for MockHttp {
    fn get(&self, url: &str) -> Result<Value, String> {
        self.responses
            .lock()
            .expect("mock lock")
            .get(url)
            .cloned()
            .ok_or_else(|| format!("mock: no response for {url}"))
    }
}

/// One pool worker row (`/api/stats` → `workers[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    pub miner: u64,
    pub name: String,
    pub shares: u64,
}

/// One luckynode row (`/api/luckynodes` → `nodes[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Luckynode {
    pub id: String,
    pub address: String,
    pub tenure_secs: u64,
    pub bond_ok: bool,
    pub last_heartbeat: u64,
}

/// Parse `workers[]` from a pool stats document.
#[must_use]
pub fn parse_workers(v: &Value) -> Vec<Worker> {
    v.get("workers")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|w| {
                    Some(Worker {
                        miner: w.get("miner")?.as_u64()?,
                        name: w.get("name")?.as_str()?.to_string(),
                        shares: w.get("shares").and_then(Value::as_u64).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `nodes[]` from a luckynode list document.
#[must_use]
pub fn parse_luckynodes(v: &Value) -> Vec<Luckynode> {
    v.get("nodes")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|n| {
                    Some(Luckynode {
                        id: n.get("id")?.as_str()?.to_string(),
                        address: n
                            .get("address")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        tenure_secs: n.get("tenure_secs").and_then(Value::as_u64).unwrap_or(0),
                        bond_ok: n.get("bond_ok").and_then(Value::as_bool).unwrap_or(false),
                        last_heartbeat: n
                            .get("last_heartbeat")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `staked` from a `getstakinginfo` result, in micro-LKYL.
#[must_use]
pub fn parse_staked_micros(v: &Value) -> u64 {
    v.get("staked")
        .and_then(|s| value_to_micros(s).ok())
        .unwrap_or(0)
}

/// The oracle: fetches pool/luckynode snapshots and applies mappings.
pub struct Oracle {
    /// Pool base URL, e.g. `http://127.0.0.1:8080`.
    pub pool_url: Option<String>,
    /// Luckynode list URL, e.g. `http://127.0.0.1:8080`.
    pub luckynode_url: Option<String>,
    /// HTTP transport.
    pub http: Box<dyn HttpGet>,
    /// Player → identities.
    pub mappings: BTreeMap<u64, Mapping>,
}

impl Oracle {
    /// Oracle with no sources configured (everything degrades to zero).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            pool_url: None,
            luckynode_url: None,
            http: Box::new(HttpJson),
            mappings: BTreeMap::new(),
        }
    }

    /// Set a player's mapping.
    pub fn set_mapping(&mut self, player: u64, mapping: Mapping) {
        self.mappings.insert(player, mapping);
    }

    /// Remove a player's mapping (unmap). Returns whether one existed.
    pub fn remove_mapping(&mut self, player: u64) -> bool {
        self.mappings.remove(&player).is_some()
    }

    /// Current mapping for a player.
    #[must_use]
    pub fn mapping(&self, player: u64) -> Option<&Mapping> {
        self.mappings.get(&player)
    }

    /// Best-effort participation for `player`.
    #[must_use]
    pub fn participation(&self, player: u64) -> Participation {
        let Some(m) = self.mappings.get(&player) else {
            return Participation::default();
        };
        let pow_shares = self
            .pool_url
            .as_ref()
            .and_then(|u| {
                let url = format!("{}/api/stats", u.trim_end_matches('/'));
                self.http.get(&url).ok()
            })
            .map(|v| {
                parse_workers(&v)
                    .into_iter()
                    .find(|w| w.name == m.worker || w.miner.to_string() == m.worker)
                    .map(|w| w.shares)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let luckynode_service_days = m
            .luckynode_id
            .as_ref()
            .and_then(|id| {
                let base = self.luckynode_url.as_ref()?;
                let url = format!("{}/api/luckynodes", base.trim_end_matches('/'));
                let v = self.http.get(&url).ok()?;
                parse_luckynodes(&v).into_iter().find(|n| &n.id == id)
            })
            .filter(|n| n.bond_ok)
            .map(|n| n.tenure_secs / 86_400)
            .unwrap_or(0);
        Participation {
            pow_shares,
            pos_stake_micros: m.stake_micros,
            luckynode_service_days,
        }
    }

    /// Best-effort tier for `player`.
    #[must_use]
    pub fn tier(&self, player: u64) -> Tier {
        tier_for(&self.participation(player))
    }

    /// Score backing the tier (for display).
    #[must_use]
    pub fn score(&self, player: u64) -> u64 {
        participation_score(&self.participation(player))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stats() -> Value {
        json!({
            "pool": {"workers": 2},
            "workers": [
                {"miner": 7, "name": "j1", "shares": 25_000, "online": true},
                {"miner": 9, "name": "phone", "shares": 400, "online": true}
            ]
        })
    }

    fn nodes() -> Value {
        json!({
            "nodes": [
                {"id": "node-a", "address": "L1", "tenure_secs": 30 * 86_400, "bond_ok": true, "last_heartbeat": 100},
                {"id": "node-b", "address": "L2", "tenure_secs": 90 * 86_400, "bond_ok": false, "last_heartbeat": 100}
            ]
        })
    }

    #[test]
    fn parsers_handle_shapes_and_garbage() {
        let w = parse_workers(&stats());
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].name, "j1");
        assert_eq!(w[1].shares, 400);
        assert!(parse_workers(&json!({})).is_empty());
        let n = parse_luckynodes(&nodes());
        assert_eq!(n.len(), 2);
        assert!(n[0].bond_ok);
        assert!(!n[1].bond_ok);
        assert_eq!(parse_staked_micros(&json!({"staked": 12.5})), 12_500_000);
        assert_eq!(parse_staked_micros(&json!({})), 0);
    }

    #[test]
    fn participation_combines_all_three_sources() {
        let mock = MockHttp::new();
        mock.set("http://pool/api/stats", stats());
        mock.set("http://pool/api/luckynodes", nodes());
        let mut o = Oracle {
            pool_url: Some("http://pool".to_string()),
            luckynode_url: Some("http://pool".to_string()),
            http: Box::new(mock),
            mappings: BTreeMap::new(),
        };
        o.set_mapping(
            1,
            Mapping {
                worker: "j1".to_string(),
                stake_micros: 500_000_000,
                luckynode_id: Some("node-a".to_string()),
            },
        );
        // Matches by worker name, counts 30 days of active service.
        let p = o.participation(1);
        assert_eq!(p.pow_shares, 25_000);
        assert_eq!(p.pos_stake_micros, 500_000_000);
        assert_eq!(p.luckynode_service_days, 30);
        assert!(o.tier(1).level >= 1);
    }

    #[test]
    fn unmapped_or_bondless_players_degrade_to_zero() {
        let mock = MockHttp::new();
        mock.set("http://pool/api/stats", stats());
        mock.set("http://pool/api/luckynodes", nodes());
        let mut o = Oracle {
            pool_url: Some("http://pool".to_string()),
            luckynode_url: Some("http://pool".to_string()),
            http: Box::new(mock),
            mappings: BTreeMap::new(),
        };
        assert_eq!(o.participation(99), Participation::default());
        // Unbonded node does not count toward service days.
        o.set_mapping(
            2,
            Mapping {
                worker: "phone".to_string(),
                stake_micros: 0,
                luckynode_id: Some("node-b".to_string()),
            },
        );
        assert_eq!(o.participation(2).luckynode_service_days, 0);
        assert_eq!(o.participation(2).pow_shares, 400);
    }

    #[test]
    fn unreachable_sources_do_not_panic() {
        let o = Oracle {
            pool_url: Some("http://127.0.0.1:1".to_string()),
            luckynode_url: Some("http://127.0.0.1:1".to_string()),
            http: Box::new(HttpJson),
            mappings: BTreeMap::new(),
        };
        assert_eq!(o.tier(1).level, 0);
    }
}
