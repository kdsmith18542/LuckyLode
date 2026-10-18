//! Minimal HTTP JSON-RPC client (std only — no async runtime, no HTTP deps).
//!
//! Speaks just enough HTTP/1.1 to talk to a node's RPC port — plain HTTP or
//! HTTPS (rustls, Mozilla roots) for nodes reached through a TLS reverse
//! proxy. `POST <path>` with Basic auth and `Connection: close`, then reads
//! the whole response (Content-Length, close-delimited, or chunked). The
//! JSON-RPC envelope handling matches the node's conventions
//! (`{"result":…,"error":…,"id":…}`; `error != null` → `Err(message)`).
//!
//! This is deliberately not a general HTTP client: it exists so the
//! reference pool and wallet can talk to the forked node with zero new
//! dependencies.

use crate::tls;
use rustls::ClientConfig;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// Blocking JSON-RPC client for a single node endpoint.
#[derive(Debug, Clone)]
pub struct RpcClient {
    addr: String,
    host: String,
    path: String,
    user: String,
    pass: String,
    tls: Option<Arc<ClientConfig>>,
    server_name: String,
}

impl RpcClient {
    /// Plain-HTTP client for `host:port` (the classic localhost node).
    pub fn new(addr: &str, user: &str, pass: &str) -> Self {
        Self {
            addr: addr.to_string(),
            host: addr.to_string(),
            path: "/".to_string(),
            user: user.to_string(),
            pass: pass.to_string(),
            tls: None,
            server_name: addr.split(':').next().unwrap_or(addr).to_string(),
        }
    }

    /// Client from a URL: `http://host[:port][/path]`,
    /// `https://host[:port][/path]`, or a bare `host[:port]` (plain HTTP).
    /// The path prefix lets a public endpoint live behind a reverse proxy
    /// (e.g. `https://luckylode.cc/node/`).
    pub fn from_url(url: &str, user: &str, pass: &str) -> Result<Self, String> {
        let (tls_on, rest) = match url.strip_prefix("https://") {
            Some(r) => (true, r),
            None => (false, url.strip_prefix("http://").unwrap_or(url)),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(format!("node RPC URL has no host: '{url}'"));
        }
        let default_port = if tls_on { 443 } else { 80 };
        let addr = if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:{default_port}")
        };
        let server_name = authority.split(':').next().unwrap_or(authority).to_string();
        let tls = if tls_on {
            Some(tls::client_config(None, false).map_err(|e| format!("tls config: {e}"))?)
        } else {
            None
        };
        Ok(Self {
            addr,
            host: authority.to_string(),
            path: path.to_string(),
            user: user.to_string(),
            pass: pass.to_string(),
            tls,
            server_name,
        })
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
            "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.path,
            self.host,
            base64(auth.as_bytes()),
            body.len(),
            body
        );
        let sock =
            TcpStream::connect(&self.addr).map_err(|e| format!("connect {}: {e}", self.addr))?;
        sock.set_read_timeout(Some(Duration::from_secs(20)))
            .map_err(|e| format!("set timeout: {e}"))?;
        sock.set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("set timeout: {e}"))?;
        let mut stream = tls::connect(sock, &self.server_name, self.tls.as_ref())
            .map_err(|e| format!("tls {}: {e}", self.addr))?;
        stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        let mut resp = Vec::new();
        stream
            .read_to_end(&mut resp)
            .map_err(|e| format!("read: {e}"))?;
        let split = find_subslice(&resp, b"\r\n\r\n")
            .ok_or_else(|| "malformed HTTP response (no header/body split)".to_string())?;
        let head = String::from_utf8_lossy(&resp[..split]).to_string();
        let mut payload: Vec<u8> = resp[split + 4..].to_vec();
        // nginx (and other proxies) may chunk larger responses; the node
        // itself does not.
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            payload = dechunk(&payload)?;
        }
        let ok_status = head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200");
        let text = String::from_utf8_lossy(&payload);
        // verusd-family nodes carry the JSON-RPC envelope on some
        // non-2xx statuses too (e.g. an over-balance `sendtoaddress`
        // returns HTTP 500 with the -6 error body) — surface it.
        if !ok_status {
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(err) = msg.get("error") {
                    if !err.is_null() {
                        return Err(format!("rpc error: {err}"));
                    }
                }
            }
            let status = head.lines().next().unwrap_or("(no status)");
            return Err(format!("rpc http status: {status}"));
        }
        let msg: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("bad json-rpc body: {e}"))?;
        if let Some(err) = msg.get("error") {
            if !err.is_null() {
                return Err(format!("rpc error: {err}"));
            }
        }
        msg.get("result")
            .cloned()
            .ok_or_else(|| "rpc response missing result".to_string())
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Decode HTTP/1.1 chunked transfer-encoding.
fn dechunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let nl = find_subslice(b, b"\r\n").ok_or("chunked body: missing chunk header")?;
        let line = std::str::from_utf8(&b[..nl]).map_err(|_| "chunked body: bad chunk header")?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| format!("chunked body: bad chunk size '{size_hex}'"))?;
        b = &b[nl + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            return Err("chunked body: truncated chunk".to_string());
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
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
        assert!(c
            .call("x", serde_json::json!([]))
            .unwrap_err()
            .contains("500"));
    }

    #[test]
    fn connection_failure_is_err() {
        // Port 1 is reserved and nothing listens there.
        let c = RpcClient::new("127.0.0.1:1", "u", "p");
        assert!(c.call("x", serde_json::json!([])).is_err());
    }

    #[test]
    fn from_url_parses_scheme_host_port_path() {
        let c = RpcClient::from_url("https://luckylode.cc/node/", "u", "p").expect("https url");
        assert_eq!(c.addr, "luckylode.cc:443");
        assert_eq!(c.host, "luckylode.cc");
        assert_eq!(c.path, "/node/");
        assert_eq!(c.server_name, "luckylode.cc");
        assert!(c.tls.is_some());

        let c = RpcClient::from_url("http://127.0.0.1:9271", "u", "p").expect("http url");
        assert_eq!(c.addr, "127.0.0.1:9271");
        assert_eq!(c.path, "/");
        assert!(c.tls.is_none());

        // Bare host:port (legacy setting form) stays plain HTTP.
        let c = RpcClient::from_url("127.0.0.1:9271", "u", "p").expect("bare addr");
        assert_eq!(c.addr, "127.0.0.1:9271");
        assert!(c.tls.is_none());

        // Bare host gets the scheme default port.
        let c = RpcClient::from_url("example.com", "u", "p").expect("bare host");
        assert_eq!(c.addr, "example.com:80");

        assert!(RpcClient::from_url("", "u", "p").is_err());
        assert!(RpcClient::from_url("https://", "u", "p").is_err());
    }

    #[test]
    fn url_path_prefix_is_used_in_request_line() {
        let resp = "HTTP/1.1 200 OK\r\n\r\n{\"result\":1,\"error\":null,\"id\":\"lm\"}";
        let (addr, h) = one_shot_server(resp);
        let c = RpcClient::from_url(&format!("http://{addr}/node/"), "u", "p").expect("url");
        let v = c.call("getblockcount", serde_json::json!([])).expect("ok");
        assert_eq!(v, serde_json::json!(1));
        let req = h.join().expect("join");
        assert!(req.starts_with("POST /node/ HTTP/1.1"), "{req}");
    }

    #[test]
    fn chunked_response_is_decoded() {
        // {"result":42,"error":null,"id":"lm"} split into 3 chunks of 0xc.
        let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nc\r\n{\"result\":42\r\nc\r\n,\"error\":nul\r\nc\r\nl,\"id\":\"lm\"}\r\n0\r\n\r\n";
        let (addr, h) = one_shot_server(resp);
        let c = RpcClient::new(&addr, "u", "p");
        let v = c.call("getblockcount", serde_json::json!([])).expect("ok");
        assert_eq!(v, serde_json::json!(42));
        h.join().expect("join");
    }

    /// Live check against a real endpoint (explicit runs only):
    /// `LUCKYLODE_RPC_URL=https://luckylode.cc/node/ LUCKYLODE_RPC_USER=u \
    ///  LUCKYLODE_RPC_PASS=p cargo test -p lm-stratum -- --ignored`
    #[test]
    #[ignore]
    fn live_node_roundtrip() {
        let url = match std::env::var("LUCKYLODE_RPC_URL") {
            Ok(u) => u,
            Err(_) => return,
        };
        let user = std::env::var("LUCKYLODE_RPC_USER").unwrap_or_default();
        let pass = std::env::var("LUCKYLODE_RPC_PASS").unwrap_or_default();
        let c = RpcClient::from_url(&url, &user, &pass).expect("client");
        let v = c.call("getblockcount", serde_json::json!([])).expect("live rpc");
        assert!(v.is_u64(), "expected a block count, got {v}");
        // A wallet method (larger, definitely chunked behind the proxy).
        let w = c.call("getwalletinfo", serde_json::json!([])).expect("live rpc");
        assert!(w.get("balance").is_some(), "expected wallet info, got {w}");
    }
}
