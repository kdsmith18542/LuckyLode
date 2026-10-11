//! Stratum V1 message layer with the Lucky Lode job profile (`lowmine/1`).
//!
//! Framing is classic Stratum V1 (newline-delimited JSON-RPC over TCP:
//! `mining.subscribe`, `mining.authorize`, `mining.notify`, `mining.submit`)
//! so generic V1 tooling connects at the transport layer. The *job payload*
//! is purpose-shaped for EmberHash (doc 05): a fixed header template plus a
//! u64 nonce the miner grinds — Bitcoin's coinbase/merkle dance buys nothing
//! before the forked node exists, so it is omitted by design:
//!
//! ```text
//! notify params: [job_id, header_hex, height, pool_target_hex, net_target_hex, clean]
//! submit params: [worker, job_id, nonce_hex]      (nonce = 16 hex chars, u64 big-endian)
//! ```
//!
//! Documented deviations from public-pool V1: empty `extranonce1`/size 0 in
//! the subscribe response (no extranonce space is needed — the full u64
//! nonce is the search space), no `mining.set_difficulty` (both targets ride
//! in every notify), no overnight flag on shares (a future extension; the
//! overnight bonus only credits flagged shares).
//!
//! Reject codes (in the JSON-RPC `error` tuple `[code, message, null]`):
//! 20 stale share, 21 duplicate share, 22 below pool target, 23 unknown job,
//! 24 malformed nonce. Transport errors use standard JSON-RPC codes
//! (-32700 parse error, -32600 invalid request, -32601 method not found).

use lm_pow::{meets_target, pow_hash};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// A mining job: everything a worker needs to grind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub header: Vec<u8>,
    pub height: u64,
    pub pool_target: [u8; 32],
    pub net_target: [u8; 32],
}

/// Share verdict from pure evaluation (no ledger side effects).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Acceptable share; `block` is true when it also meets the network target.
    Accept { block: bool },
    Stale,
    Duplicate,
    LowDifficulty,
    UnknownJob,
    BadNonce,
}

impl Verdict {
    /// (accept, JSON-RPC error tuple) for the submit response.
    #[must_use]
    pub fn response(self) -> (bool, Option<(i64, String)>) {
        match self {
            Verdict::Accept { .. } => (true, None),
            Verdict::Stale => (false, Some((20, "stale share".to_string()))),
            Verdict::Duplicate => (false, Some((21, "duplicate share".to_string()))),
            Verdict::LowDifficulty => (false, Some((22, "share below pool target".to_string()))),
            Verdict::UnknownJob => (false, Some((23, "unknown job".to_string()))),
            Verdict::BadNonce => (false, Some((24, "malformed nonce".to_string()))),
        }
    }
}

/// Recent-job registry with duplicate tracking. Only jobs in the window
/// validate; anything older is stale, anything never issued is unknown.
#[derive(Debug, Default)]
pub struct JobRegistry {
    jobs: HashMap<String, Job>,
    order: Vec<String>,
    seen: HashSet<(String, u64)>,
    window: usize,
}

impl JobRegistry {
    pub fn new(window: usize) -> Self {
        Self { jobs: HashMap::new(), order: Vec::new(), seen: HashSet::new(), window: window.max(1) }
    }

    /// Issue a job (evicts the oldest beyond the window, with its nonces).
    pub fn issue(&mut self, job: Job) {
        if !self.jobs.contains_key(&job.id) {
            self.order.push(job.id.clone());
        }
        self.jobs.insert(job.id.clone(), job);
        while self.order.len() > self.window {
            let old = self.order.remove(0);
            self.jobs.remove(&old);
            self.seen.retain(|(j, _)| j != &old);
        }
    }

    /// Pure evaluation of a submit: job lookup → dupe check → PoW check.
    /// Marks `(job, nonce)` seen on every *evaluable* submit so replays are
    /// duplicates even when the first attempt was low-difficulty.
    pub fn evaluate(&mut self, job_id: &str, nonce_hex: &str) -> (Verdict, Option<JobSubmit>) {
        let job = match self.jobs.get(job_id) {
            Some(j) => j.clone(),
            None => return (Verdict::UnknownJob, None),
        };
        // Stale = validly issued but superseded (not the newest).
        let is_latest = self.order.last().is_some_and(|id| id == job_id);
        let nonce = match parse_nonce(nonce_hex) {
            Some(n) => n,
            None => return (Verdict::BadNonce, None),
        };
        if !self.seen.insert((job_id.to_string(), nonce)) {
            return (Verdict::Duplicate, None);
        }
        if !is_latest {
            return (Verdict::Stale, None);
        }
        let out = pow_hash(&job.header, nonce, job.height);
        if !meets_target(&out, &job.pool_target) {
            return (Verdict::LowDifficulty, None);
        }
        let block = meets_target(&out, &job.net_target);
        (Verdict::Accept { block }, Some(JobSubmit { job, nonce, out }))
    }
}

/// An accepted share, ready for ledger recording / block handling.
#[derive(Debug, Clone)]
pub struct JobSubmit {
    pub job: Job,
    pub nonce: u64,
    pub out: [u8; 32],
}

/// Parse the submit nonce: exactly 16 hex chars, u64 big-endian.
pub fn parse_nonce(s: &str) -> Option<u64> {
    if s.len() != 16 {
        return None;
    }
    u64::from_str_radix(s, 16).ok()
}

/// Render a nonce for submit responses and logs.
#[must_use]
pub fn render_nonce(nonce: u64) -> String {
    format!("{nonce:016x}")
}

/// Pool-side miner id for a worker name: FNV-1a 64. Deterministic across
/// processes; collisions are vanishingly unlikely at pool scale and only
/// affect display grouping (never consensus — ledger ids are opaque).
#[must_use]
pub fn worker_id(worker: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in worker.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Expected max-target hashes per share at a pool target (ledger `work`
/// credit): `u64::MAX / top64(target)`. Max target → 1 work unit.
#[must_use]
pub fn work_for_share(pool_target: &[u8; 32]) -> u64 {
    let mut top = [0u8; 8];
    top.copy_from_slice(&pool_target[..8]);
    u64::MAX / u64::from_be_bytes(top).max(1)
}

// --- JSON-RPC wire shapes -------------------------------------------------

/// Any incoming line: request (`method` set) or response (never sent by our
/// server, parsed by the client).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Option<serde_json::Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
}

#[must_use]
pub fn response(id: &serde_json::Value, result: serde_json::Value) -> String {
    serde_json::json!({"id": id, "result": result, "error": null}).to_string()
}

#[must_use]
pub fn error_response(id: &serde_json::Value, code: i64, message: &str) -> String {
    serde_json::json!({"id": id, "result": null, "error": [code, message, null]}).to_string()
}

#[must_use]
pub fn notify(job: &Job, clean: bool) -> String {
    serde_json::json!({
        "id": null,
        "method": "mining.notify",
        "params": [
            job.id,
            hex_of(&job.header),
            job.height,
            hex_of(&job.pool_target),
            hex_of(&job.net_target),
            clean,
        ],
    })
    .to_string()
}

pub fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Decode a notify `params` array into a [`Job`] (client side).
pub fn job_from_params(params: &serde_json::Value) -> Option<Job> {
    let a = params.as_array()?;
    if a.len() != 6 {
        return None;
    }
    let id = a[0].as_str()?.to_string();
    let header = parse_hex(a[1].as_str()?)?;
    let height = a[2].as_u64()?;
    let pool = parse_hex(a[3].as_str()?)?;
    let net = parse_hex(a[4].as_str()?)?;
    if pool.len() != 32 || net.len() != 32 {
        return None;
    }
    let mut pool_target = [0u8; 32];
    let mut net_target = [0u8; 32];
    pool_target.copy_from_slice(&pool);
    net_target.copy_from_slice(&net);
    Some(Job { id, header, height, pool_target, net_target })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_v() -> Vec<u8> {
        (0u8..80).collect()
    }

    fn job(id: &str) -> Job {
        Job {
            id: id.to_string(),
            header: header_v(),
            height: 1,
            pool_target: [0xffu8; 32],
            net_target: [0xffu8; 32],
        }
    }

    #[test]
    fn nonce_hex_roundtrip() {
        assert_eq!(parse_nonce(&render_nonce(0)), Some(0));
        assert_eq!(parse_nonce(&render_nonce(u64::MAX)), Some(u64::MAX));
        assert_eq!(parse_nonce("ff0d88d9"), None); // wrong length
        assert_eq!(parse_nonce("zzzzzzzzzzzzzzzz"), None);
    }

    #[test]
    fn worker_ids_stable_and_distinct() {
        assert_eq!(worker_id("alice"), worker_id("alice"));
        assert_ne!(worker_id("alice"), worker_id("bob"));
    }

    #[test]
    fn work_unit_is_one_at_max_target() {
        assert_eq!(work_for_share(&[0xffu8; 32]), 1);
        let mut half = [0u8; 32];
        half[0] = 0x7f;
        half[1..].fill(0xff);
        assert_eq!(work_for_share(&half), 2);
    }

    #[test]
    fn accept_then_duplicate_then_stale() {
        let mut reg = JobRegistry::new(4);
        reg.issue(job("a"));
        reg.issue(job("b"));
        let (v, s) = reg.evaluate("b", &render_nonce(0));
        assert_eq!(v, Verdict::Accept { block: true }); // max targets: share + block
        assert!(s.is_some());
        let (v, _) = reg.evaluate("b", &render_nonce(0));
        assert_eq!(v, Verdict::Duplicate);
        let (v, _) = reg.evaluate("a", &render_nonce(1)); // superseded, fresh nonce
        assert_eq!(v, Verdict::Stale);
        let (v, _) = reg.evaluate("zzz", &render_nonce(0));
        assert_eq!(v, Verdict::UnknownJob);
        let (v, _) = reg.evaluate("b", "short");
        assert_eq!(v, Verdict::BadNonce);
    }

    #[test]
    fn low_difficulty_rejects_without_block() {
        let mut reg = JobRegistry::new(4);
        let mut j = job("a");
        j.pool_target = [0x00u8; 32]; // nothing validates
        j.net_target = [0x00u8; 32];
        reg.issue(j);
        let (v, s) = reg.evaluate("a", &render_nonce(0));
        assert_eq!(v, Verdict::LowDifficulty);
        assert!(s.is_none());
    }

    #[test]
    fn registry_window_evicts_old_jobs() {
        let mut reg = JobRegistry::new(2);
        reg.issue(job("a"));
        reg.issue(job("b"));
        reg.issue(job("c"));
        let (v, _) = reg.evaluate("a", &render_nonce(9));
        assert_eq!(v, Verdict::UnknownJob); // evicted, not merely stale
    }

    #[test]
    fn notify_roundtrips_through_json() {
        let j = job("beef");
        let line = notify(&j, true);
        let msg: Message = serde_json::from_str(&line).expect("valid json");
        assert_eq!(msg.method.as_deref(), Some("mining.notify"));
        let back = job_from_params(msg.params.as_ref().expect("params")).expect("job");
        assert_eq!(back, j);
        assert_eq!(msg.params.as_ref().unwrap().as_array().unwrap()[5], true);
    }
}
