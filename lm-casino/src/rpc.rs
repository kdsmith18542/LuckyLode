//! Node access for the casino (std-only JSON-RPC, no async runtime).
//!
//! A [`NodeApi`] is the seam: [`HttpRpc`] talks to the forked node's RPC port
//! exactly like the pool's client (minimal HTTP/1.1 + Basic auth), while
//! [`MockNode`] serves canned responses so custody/engine logic is testable
//! without a live chain.
//!
//! Amounts cross the boundary as decimal LKYL strings/numbers and are handled
//! internally in **micro-LKYL** (1e-6); sub-micro dust on deposits is dropped.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::MICROS_PER_LKYL;

/// The narrow node surface the casino uses.
pub trait NodeApi: Send + Sync {
    /// Call a JSON-RPC method; transport/JSON errors map to `Err(message)`.
    fn call(&self, method: &str, params: Value) -> Result<Value, String>;
}

/// Blocking HTTP JSON-RPC client for a single node endpoint.
#[derive(Debug, Clone)]
pub struct HttpRpc {
    addr: String,
    user: String,
    pass: String,
}

impl HttpRpc {
    /// New client for `addr` (`host:port`) with Basic auth.
    #[must_use]
    pub fn new(addr: &str, user: &str, pass: &str) -> Self {
        Self {
            addr: addr.to_string(),
            user: user.to_string(),
            pass: pass.to_string(),
        }
    }
}

impl NodeApi for HttpRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = serde_json::json!({
            "jsonrpc": "1.0",
            "id": "lm-casino",
            "method": method,
            "params": params,
        })
        .to_string();
        let auth = format!("{}:{}", self.user, self.pass);
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.addr,
            base64(auth.as_bytes()),
            body.len(),
            body
        );
        let mut stream =
            TcpStream::connect(&self.addr).map_err(|e| format!("connect {}: {e}", self.addr))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(|e| format!("set timeout: {e}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("set timeout: {e}"))?;
        stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        let mut resp = Vec::new();
        stream
            .read_to_end(&mut resp)
            .map_err(|e| format!("read: {e}"))?;
        let text = String::from_utf8_lossy(&resp);
        let (head, payload) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| "malformed HTTP response".to_string())?;
        let ok = head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200");
        let msg: Value =
            serde_json::from_str(payload).map_err(|e| format!("bad json-rpc body: {e}"))?;
        if let Some(err) = msg.get("error") {
            if !err.is_null() {
                return Err(format!("rpc error: {err}"));
            }
        }
        if !ok {
            let status = head.lines().next().unwrap_or("(no status)");
            return Err(format!("rpc http status: {status}"));
        }
        msg.get("result")
            .cloned()
            .ok_or_else(|| "rpc response missing result".to_string())
    }
}

/// Canned-response node for tests and `--mock` local runs.
#[derive(Debug, Default)]
pub struct MockNode {
    responses: Mutex<BTreeMap<String, Value>>,
    queue: Mutex<BTreeMap<String, Vec<Value>>>,
    calls: Mutex<Vec<String>>,
}

impl MockNode {
    /// New empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the result returned for `method`.
    pub fn set(&self, method: &str, result: Value) {
        self.responses
            .lock()
            .expect("mock lock")
            .insert(method.to_string(), result);
    }

    /// Queue a result for `method`, consumed in order before the fixed value.
    pub fn push(&self, method: &str, result: Value) {
        self.queue
            .lock()
            .expect("mock lock")
            .entry(method.to_string())
            .or_default()
            .push(result);
    }

    /// Methods called so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("mock lock").clone()
    }
}

impl NodeApi for MockNode {
    fn call(&self, method: &str, _params: Value) -> Result<Value, String> {
        self.calls
            .lock()
            .expect("mock lock")
            .push(method.to_string());
        {
            let mut q = self.queue.lock().expect("mock lock");
            if let Some(list) = q.get_mut(method) {
                if !list.is_empty() {
                    return Ok(list.remove(0));
                }
            }
        }
        self.responses
            .lock()
            .expect("mock lock")
            .get(method)
            .cloned()
            .ok_or_else(|| format!("mock: no response for {method}"))
    }
}

/// Typed node helper over a [`NodeApi`].
pub struct Node {
    api: Box<dyn NodeApi>,
}

impl Node {
    /// HTTP-backed node client.
    #[must_use]
    pub fn http(addr: &str, user: &str, pass: &str) -> Self {
        Self {
            api: Box::new(HttpRpc::new(addr, user, pass)),
        }
    }

    /// Wrap any [`NodeApi`] (used for tests and `--mock`).
    #[must_use]
    pub fn from_api(api: Box<dyn NodeApi>) -> Self {
        Self { api }
    }

    /// Raw call.
    ///
    /// # Errors
    /// Propagates the node/transport error string.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.api.call(method, params)
    }

    /// Create (and label) a fresh deposit address.
    ///
    /// # Errors
    /// Propagates node errors, or if the result is not a string.
    pub fn get_new_address(&self, label: &str) -> Result<String, String> {
        let v = self.call("getnewaddress", serde_json::json!([label]))?;
        v.as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("getnewaddress: expected string, got {v}"))
    }

    /// Total received by `address` (in micro-LKYL), requiring `min_conf`.
    ///
    /// # Errors
    /// Propagates node errors or an unparseable amount.
    pub fn received_by_address(&self, address: &str, min_conf: u64) -> Result<u64, String> {
        let v = self.call(
            "getreceivedbyaddress",
            serde_json::json!([address, min_conf]),
        )?;
        value_to_micros(&v)
    }

    /// Send `micros` micro-LKYL to `address`, returning the txid.
    ///
    /// # Errors
    /// Propagates node errors, or if the result is not a txid string.
    pub fn send_to_address(&self, address: &str, micros: u64) -> Result<String, String> {
        let v = self.call(
            "sendtoaddress",
            serde_json::json!([address, format_micros(micros)]),
        )?;
        v.as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("sendtoaddress: expected txid, got {v}"))
    }
}

/// Parse a decimal LKYL amount (string or JSON number) to micro-LKYL,
/// truncating beyond six decimal places.
///
/// # Errors
/// `Err` on empty, malformed, or negative amounts.
pub fn parse_amount_to_micros(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty amount".to_string());
    }
    if s.starts_with('-') {
        return Err(format!("negative amount: {s}"));
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(format!("malformed amount: {s}"));
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!("malformed amount: {s}"));
    }
    let whole: u64 = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse()
            .map_err(|_| format!("amount overflow: {s}"))?
    };
    let mut frac = frac_part.chars().take(6).collect::<String>();
    while frac.len() < 6 {
        frac.push('0');
    }
    let micros: u64 = if frac.is_empty() {
        0
    } else {
        frac.parse().map_err(|_| format!("malformed amount: {s}"))?
    };
    whole
        .checked_mul(MICROS_PER_LKYL)
        .and_then(|w| w.checked_add(micros))
        .ok_or_else(|| format!("amount overflow: {s}"))
}

/// Format micro-LKYL as a six-decimal LKYL string.
#[must_use]
pub fn format_micros(micros: u64) -> String {
    format!(
        "{}.{:06}",
        micros / MICROS_PER_LKYL,
        micros % MICROS_PER_LKYL
    )
}

/// Format a FUN balance (same fixed point, play money) as a two-decimal
/// chip-style string — `1000.00`. Display only; values stay exact.
#[must_use]
pub fn format_fun(units: u64) -> String {
    format!(
        "{}.{:02}",
        units / MICROS_PER_LKYL,
        (units % MICROS_PER_LKYL) / 10_000
    )
}

/// Convert a JSON value (number or string) to micro-LKYL.
///
/// # Errors
/// `Err` when the value is not a number/string or fails to parse.
pub fn value_to_micros(v: &Value) -> Result<u64, String> {
    match v {
        Value::Number(n) => parse_amount_to_micros(&n.to_string()),
        Value::String(s) => parse_amount_to_micros(s),
        other => Err(format!("expected amount, got {other}")),
    }
}

/// Tiny base64 (RFC 4648) for the Basic auth header.
fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_parse_and_format_round_trip() {
        assert_eq!(parse_amount_to_micros("0"), Ok(0));
        assert_eq!(parse_amount_to_micros("1"), Ok(1_000_000));
        assert_eq!(parse_amount_to_micros("1.5"), Ok(1_500_000));
        assert_eq!(parse_amount_to_micros("0.000001"), Ok(1));
        // Truncates beyond six decimals.
        assert_eq!(parse_amount_to_micros("0.0000019"), Ok(1));
        assert_eq!(parse_amount_to_micros(".5"), Ok(500_000));
        assert_eq!(format_micros(1_500_000), "1.500000");
        assert_eq!(format_micros(1), "0.000001");
        assert!(parse_amount_to_micros("").is_err());
        assert!(parse_amount_to_micros("-1").is_err());
        assert!(parse_amount_to_micros("1.2.3").is_err());
        assert!(parse_amount_to_micros("x").is_err());
    }

    #[test]
    fn node_uses_typed_calls() {
        let mock = MockNode::new();
        mock.set("getnewaddress", serde_json::json!("addr1"));
        mock.set("getreceivedbyaddress", serde_json::json!(2.5));
        mock.set("sendtoaddress", serde_json::json!("txid1"));
        let node = Node::from_api(Box::new(mock));
        assert_eq!(node.get_new_address("lbl").unwrap(), "addr1");
        assert_eq!(node.received_by_address("addr1", 1).unwrap(), 2_500_000);
        assert_eq!(node.send_to_address("addr1", 2_500_000).unwrap(), "txid1");
        let mock = Node::from_api(Box::new(MockNode::new()));
        assert!(mock.get_new_address("lbl").is_err());
    }
}
