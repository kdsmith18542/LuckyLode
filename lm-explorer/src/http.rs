//! Read-only HTTP UI + JSON API over the index. std only (mirrors the
//! pool dashboard's server): one thread per connection, `Connection:
//! close`. Routes:
//!   /                      home (tip, recent blocks, search)
//!   /block/<height|hash>   block page        /api/block/<height|hash>
//!   /tx/<txid>             tx page           /api/tx/<txid>
//!   /address/<addr>        address page      /api/address/<addr>
//!   /search?q=             smart redirect    /api/tip  /api/stats

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::index::Indexer;

const CSS: &str = "body{background:#0d1117;color:#e6edf3;font:14px ui-monospace,Menlo,monospace;margin:0 auto;padding:24px;max-width:960px}h1{font-size:20px;color:#ffd166}a{color:#7ee787}table{border-collapse:collapse;width:100%;margin:12px 0}td,th{border:1px solid #30363d;padding:6px 10px;text-align:left;font-size:13px}.dim{color:#8b949e}.card{background:#161b22;border:1px solid #30363d;border-radius:10px;padding:12px;margin:12px 0}input{width:70%;padding:8px;background:#0d1117;color:#e6edf3;border:1px solid #30363d;border-radius:6px}button{padding:8px 14px;background:#161b22;color:#ffd166;border:1px solid #ffd166;border-radius:6px}";

fn page(title: &str, body: String) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{title} — Lucky Lode explorer</title><style>{CSS}</style></head><body><h1><a href=/ style=\"color:#ffd166;text-decoration:none\">Lucky Lode explorer</a></h1>{body}</body></html>"
    )
}

fn coins(v: i64) -> String {
    format!("{}.{:08}", v / 100_000_000, (v % 100_000_000).abs())
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn link_block(h: i64) -> String {
    format!("<a href=/block/{h}>{h}</a>")
}

fn link_tx(t: &str) -> String {
    format!("<a href=/tx/{t}>{}</a>", &t[..t.len().min(16)])
}

fn link_addr(a: &Option<String>) -> String {
    match a {
        Some(x) => format!("<a href=/address/{x}>{x}</a>"),
        None => "<span class=dim>nonstandard</span>".to_string(),
    }
}

fn search_box() -> String {
    "<form action=/search><input name=q placeholder=\"height, block hash, txid or address\"><button>Go</button></form>".to_string()
}

fn home(idx: &Indexer) -> String {
    let stats = idx.stats().unwrap_or(serde_json::json!({"height": -1}));
    let h = stats.get("height").and_then(|v| v.as_i64()).unwrap_or(-1);
    let mut rows = String::new();
    for hh in (0.max(h - 19)..=h).rev() {
        if let Ok(Some(b)) = idx.block_by_height(hh) {
            rows += &format!(
                "<tr><td>{}</td><td><a href=/block/{}>{}</a></td><td>{}</td><td>{}</td></tr>",
                hh,
                b["hash"].as_str().unwrap_or(""),
                &b["hash"].as_str().unwrap_or("")[..16],
                b["ntx"],
                coins(b["pot"].as_i64().unwrap_or(0)),
            );
        }
    }
    page(
        "home",
        format!(
            "{}<div class=card>height <b>{}</b> · hash <span class=dim>{}</span> · supply <b>{}</b> · difficulty {:.2}</div><h2>Recent blocks</h2><table><tr><th>height</th><th>hash</th><th>txs</th><th>pot splash</th></tr>{rows}</table>",
            search_box(),
            h,
            esc(stats.get("hash").and_then(|v| v.as_str()).unwrap_or("")),
            coins(stats.get("supply").and_then(|v| v.as_i64()).unwrap_or(0)),
            stats.get("difficulty").and_then(|v| v.as_f64()).unwrap_or(0.0),
        ),
    )
}

fn block_page(idx: &Indexer, b: &serde_json::Value) -> String {
    let h = b["height"].as_i64().unwrap_or(0);
    let txs = idx.block_txs(h).unwrap_or_default();
    let mut rows = String::new();
    for t in &txs {
        rows += &format!("<tr><td>{}</td></tr>", link_tx(t));
    }
    page(
        &format!("block {h}"),
        format!(
            "<table><tr><th>height</th><td>{}</td></tr><tr><th>hash</th><td>{}</td></tr><tr><th>time</th><td>{}</td></tr><tr><th>confirmations</th><td>{}</td></tr><tr><th>txs</th><td>{}</td></tr><tr><th>difficulty</th><td>{:.2}</td></tr><tr><th>jackpot reserve</th><td>{}</td></tr><tr><th>pot</th><td>{}</td></tr><tr><th>drought</th><td>{}</td></tr></table><h2>Transactions</h2><table>{rows}</table>",
            link_block(h),
            b["hash"].as_str().unwrap_or(""),
            b["time"],
            b["confirmations"],
            b["ntx"],
            b["difficulty"].as_f64().unwrap_or(0.0),
            coins(b["reserve"].as_i64().unwrap_or(0)),
            coins(b["pot"].as_i64().unwrap_or(0)),
            b["drought"],
        ),
    )
}

fn tx_page(_idx: &Indexer, t: &serde_json::Value) -> String {
    let mut ins = String::new();
    if t["coinbase"].as_bool().unwrap_or(false) {
        ins = "<tr><td colspan=3>coinbase (newly minted)</td></tr>".to_string();
    } else {
        for v in t["vin"].as_array().cloned().unwrap_or_default() {
            ins += &format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                link_addr(&v["address"].as_str().map(|s| s.to_string())),
                coins(v["value"].as_i64().unwrap_or(0)),
                link_tx(v["txid"].as_str().unwrap_or("")),
            );
        }
    }
    let mut outs = String::new();
    for v in t["vout"].as_array().cloned().unwrap_or_default() {
        outs += &format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            link_addr(&v["address"].as_str().map(|s| s.to_string())),
            coins(v["value"].as_i64().unwrap_or(0)),
            if v["spent"].as_bool().unwrap_or(false) { "spent" } else { "unspent" },
        );
    }
    page(
        "tx",
        format!(
            "<table><tr><th>txid</th><td>{}</td></tr><tr><th>block</th><td>{} ({} confs)</td></tr><tr><th>fee</th><td>{}</td></tr></table><h2>Inputs</h2><table><tr><th>from</th><th>value</th><th>tx</th></tr>{ins}</table><h2>Outputs</h2><table><tr><th>to</th><th>value</th><th>state</th></tr>{outs}</table>",
            t["txid"].as_str().unwrap_or(""),
            link_block(t["height"].as_i64().unwrap_or(0)),
            t["confirmations"],
            coins(t["fee"].as_i64().unwrap_or(0)),
        ),
    )
}

fn address_page(a: &serde_json::Value) -> String {
    let mut rows = String::new();
    for t in a["txs"].as_array().cloned().unwrap_or_default() {
        rows += &format!(
            "<tr><td>{}</td><td>{}</td></tr>",
            link_tx(t["txid"].as_str().unwrap_or("")),
            link_block(t["height"].as_i64().unwrap_or(0)),
        );
    }
    if a["txs_truncated"].as_bool().unwrap_or(false) {
        rows += "<tr><td colspan=2 class=dim>…truncated at 100</td></tr>";
    }
    page(
        "address",
        format!(
            "<table><tr><th>address</th><td>{}</td></tr><tr><th>balance</th><td>{}</td></tr><tr><th>total received</th><td>{}</td></tr></table><h2>Transactions</h2><table><tr><th>txid</th><th>block</th></tr>{rows}</table>",
            a["address"].as_str().unwrap_or(""),
            coins(a["balance"].as_i64().unwrap_or(0)),
            coins(a["received"].as_i64().unwrap_or(0)),
        ),
    )
}

fn not_found(what: &str) -> String {
    page("not found", format!("<p>Unknown {what}. {}</p>", search_box()))
}

fn json_resp(v: &serde_json::Value) -> String {
    let body = serde_json::to_string(v).unwrap_or_default();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn html_resp(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn err404() -> String {
    let body = "not found";
    format!("HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
}

fn resolve_block(idx: &Indexer, key: &str) -> Option<serde_json::Value> {
    if let Ok(h) = key.parse::<i64>() {
        if h >= 0 {
            return idx.block_by_height(h).unwrap_or(None);
        }
        return None;
    }
    if key.len() == 64 {
        return idx.block_by_hash(key).unwrap_or(None);
    }
    None
}

fn handle(idx: &Indexer, method: &str, path: &str) -> String {
    if method != "GET" {
        return err404();
    }
    let (route, query) = match path.split_once('?') {
        Some((r, q)) => (r, q),
        None => (path, ""),
    };
    let json = query.contains("format=json");
    // API routes
    if let Some(key) = route.strip_prefix("/api/block/") {
        return match resolve_block(idx, key) {
            Some(b) => json_resp(&b),
            None => err404(),
        };
    }
    if let Some(key) = route.strip_prefix("/api/tx/") {
        return match idx.tx(key).unwrap_or(None) {
            Some(t) => json_resp(&t),
            None => err404(),
        };
    }
    if let Some(key) = route.strip_prefix("/api/address/") {
        return json_resp(&idx.address(key).unwrap_or(serde_json::json!({"address": key, "balance": 0, "received": 0, "txs": []})));
    }
    if route == "/api/tip" || route == "/api/stats" {
        return json_resp(&idx.stats().unwrap_or(serde_json::json!({"height": -1})));
    }
    // HTML routes
    if route == "/" {
        return html_resp(&home(idx));
    }
    if let Some(key) = route.strip_prefix("/block/") {
        return match resolve_block(idx, key) {
            Some(b) if json => json_resp(&b),
            Some(b) => html_resp(&block_page(idx, &b)),
            None => html_resp(&not_found("block")),
        };
    }
    if let Some(key) = route.strip_prefix("/tx/") {
        return match idx.tx(key).unwrap_or(None) {
            Some(t) if json => json_resp(&t),
            Some(t) => html_resp(&tx_page(idx, &t)),
            None => html_resp(&not_found("transaction")),
        };
    }
    if let Some(key) = route.strip_prefix("/address/") {
        let a = idx.address(key).unwrap_or(serde_json::json!({"address": key, "balance": 0, "received": 0, "txs": []}));
        if json {
            return json_resp(&a);
        }
        return html_resp(&address_page(&a));
    }
    if route == "/search" {
        let q = query
            .split('&')
            .find_map(|p| p.strip_prefix("q="))
            .unwrap_or("")
            .trim();
        if q.is_empty() {
            return html_resp(&page("search", search_box()));
        }
        if q.parse::<u64>().is_ok() {
            return html_resp(&format!("<head><meta http-equiv=refresh content=\"0;url=/block/{q}\"></head>"));
        }
        if q.len() == 64 && q.chars().all(|c| c.is_ascii_hexdigit()) {
            if resolve_block(idx, q).is_some() {
                return html_resp(&format!("<head><meta http-equiv=refresh content=\"0;url=/block/{q}\"></head>"));
            }
            if idx.tx(q).unwrap_or(None).is_some() {
                return html_resp(&format!("<head><meta http-equiv=refresh content=\"0;url=/tx/{q}\"></head>"));
            }
        }
        return html_resp(&format!("<head><meta http-equiv=refresh content=\"0;url=/address/{q}\"></head>"));
    }
    err404()
}

pub fn serve(listener: TcpListener, idx: Arc<Mutex<Indexer>>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let idx = Arc::clone(&idx);
        std::thread::spawn(move || serve_one(stream, idx));
    }
}

fn serve_one(stream: TcpStream, idx: Arc<Mutex<Indexer>>) {
    let mut reader = BufReader::new(&stream);
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    let mut parts = request.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    // Drain headers.
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
        }
    }
    let resp = match idx.lock() {
        Ok(i) => handle(&i, method, path),
        Err(_) => err404(),
    };
    let _ = (&stream).write_all(resp.as_bytes());
}
