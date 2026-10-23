//! Minimal std-only HTTP(S) client for the pool API (GET/POST JSON), same
//! framing discipline as the pool's own clients. HTTPS reuses the
//! miner's rustls stack (`lm_stratum::tls`, Mozilla roots by default).

use lm_stratum::tls;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Parse a URL into (tls?, server-name, host:port, path-prefix).
fn split(url: &str) -> Result<(bool, String, String, String), String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!(
            "pool URL must start with http:// or https://: {url}"
        ));
    };
    let (hostport, path) = match rest.split_once('/') {
        Some((h, p)) => (h.to_string(), format!("/{p}")),
        None => (rest.to_string(), String::new()),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| format!("bad port in pool URL: {url}"))?,
        ),
        None => (hostport.clone(), if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(format!("empty host in pool URL: {url}"));
    }
    Ok((
        tls,
        host,
        format!(
            "{}:{}",
            hostport.split(':').next().unwrap_or(&hostport),
            port
        ),
        path,
    ))
}

fn request(method: &str, url: &str, body: Option<&str>) -> Result<String, String> {
    let (tls, server_name, hostport, prefix) = split(url)?;
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {prefix} HTTP/1.1\r\nHost: {server_name}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let sock = TcpStream::connect(&hostport).map_err(|e| format!("connect {hostport}: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| e.to_string())?;
    let mut stream: Box<dyn tls::ReadWrite> = if tls {
        let cfg = tls::client_config(None, false).map_err(|e| format!("tls config: {e}"))?;
        tls::connect(sock, &server_name, Some(&cfg)).map_err(|e| format!("tls: {e}"))?
    } else {
        tls::connect(sock, &server_name, None).map_err(|e| format!("tls: {e}"))?
    };
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("malformed response")?;
    let status = head.lines().next().unwrap_or("");
    if !status.contains(" 200") {
        return Err(format!(
            "{status}: {}",
            body.chars().take(160).collect::<String>()
        ));
    }
    Ok(body.to_string())
}

/// GET `<pool_url>/<path>` (path like "api/luckynodes").
pub fn get(pool_url: &str, path: &str) -> Result<String, String> {
    request(
        "GET",
        &format!("{}/{}", pool_url.trim_end_matches('/'), path),
        None,
    )
}

#[cfg(test)]
mod live_tests {
    // Live-network checks (ignored in CI; run explicitly to verify the
    // HTTPS stack against the real pool).
    #[test]
    #[ignore]
    fn https_pool_stats_roundtrip() {
        let body = super::get("https://luckylode.cc/pool", "api/stats").expect("https pool api");
        assert!(body.contains("last_blocks") || body.contains("pool"));
    }
}
