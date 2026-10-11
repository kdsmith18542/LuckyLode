//! Minimal HTTP JSON-RPC client (std only — no async runtime, no HTTP deps).
//!
//! Speaks just enough HTTP/1.1 to talk to a local node's RPC port:
//! `POST /` with Basic auth and `Connection: close`, then reads the whole
//! response. JSON-RPC envelope handling matches the node's conventions
//! (`{"result":…,"error":…,"id":…}`; `error != null` → `Err(message)`).
//!
//! This is deliberately not a general HTTP client: it exists so the
//! reference pool can talk to the forked node with zero new dependencies.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Blocking JSON-RPC client for a single node endpoint.
#[derive(Debug, Clone)]
pub struct RpcClient {
    addr: String,
    user: String,
    pass: String,
}

impl RpcClient {
    pub fn new(addr: &str, user: &str, pass: &str) -> Self {
        Self { addr: addr.to_string(), user: user.to_string(), pass: pass.to_string() }
    }

    /// Call `method` with `params` (array), returning the JSON `result`.
    /// Transport errors, malformed responses, and JSON-RPC errors all map
    /// to `Err(message)` — callers treat failure as "keep last template".
    pub fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({
            "jsonrpc": "1.0",
            "id": "lm",
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
        let mut stream = TcpStream::connect(&self.addr)
            .map_err(|e| format!("connect {}: {e}", self.addr))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(|e| format!("set timeout: {e}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("set timeout: {e}"))?;
        stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).map_err(|e| format!("read: {e}"))?;
        let text = String::from_utf8_lossy(&resp);
        let (head, payload) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| "malformed HTTP response (no header/body split)".to_string())?;
        if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
            let status = head.lines().next().unwrap_or("(no status)");
            return Err(format!("rpc http status: {status}"));
        }
        let msg: serde_json::Value =
            serde_json::from_str(payload).map_err(|e| format!("bad json-rpc body: {e}"))?;
        if let Some(err) = msg.get("error") {
            if !err.is_null() {
                return Err(format!("rpc error: {err}"));
            }
        }
        msg.get("result").cloned().ok_or_else(|| "rpc response missing result".to_string())
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
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Serve exactly one canned HTTP response and return the request text.
    fn one_shot_server(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let h = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).expect("read request");
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            sock.write_all(response.as_bytes()).expect("write response");
            req
        });
        (addr, h)
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"u:p"), "dTpw");
    }

    #[test]
    fn call_sends_auth_and_parses_result() {
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"result\":42,\"error\":null,\"id\":\"lm\"}";
        let (addr, h) = one_shot_server(resp);
        let c = RpcClient::new(&addr, "u", "p");
        let v = c.call("getblockcount", serde_json::json!([])).expect("ok");
        assert_eq!(v, serde_json::json!(42));
        let req = h.join().expect("join");
        assert!(req.starts_with("POST / HTTP/1.1"), "{req}");
        assert!(req.contains("Authorization: Basic dTpw"), "{req}");
        assert!(req.contains("\"method\":\"getblockcount\""), "{req}");
    }

    #[test]
    fn rpc_error_maps_to_err() {
        let resp = "HTTP/1.1 200 OK\r\n\r\n{\"result\":null,\"error\":{\"code\":-32601,\"message\":\"nope\"},\"id\":\"lm\"}";
        let (addr, _h) = one_shot_server(resp);
        let c = RpcClient::new(&addr, "u", "p");
        let e = c.call("bad", serde_json::json!([])).unwrap_err();
        assert!(e.contains("nope"), "{e}");
    }

    #[test]
    fn http_error_status_is_rejected() {
        let resp = "HTTP/1.1 500 Internal Server Error\r\n\r\n{}";
        let (addr, _h) = one_shot_server(resp);
        let c = RpcClient::new(&addr, "u", "p");
        assert!(c.call("x", serde_json::json!([])).unwrap_err().contains("500"));
    }

    #[test]
    fn connection_failure_is_err() {
        // Port 1 is reserved and nothing listens there.
        let c = RpcClient::new("127.0.0.1:1", "u", "p");
        assert!(c.call("x", serde_json::json!([])).is_err());
    }
}
