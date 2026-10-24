//! Testnet faucet: drip small amounts to miners' addresses (runbook §5).
//!
//! Serves a branded form at `/` and `POST /api/drip {"address": ADDR}`
//! (GET `?address=` kept as a compatible alias). Each address may drip
//! once per `--per-address-secs` (default 24 h) AND each client IP once
//! per the same window, so a keypair-per-address keygen spray still
//! throttles at the IP layer. There is deliberately no CAPTCHA: it needs
//! a provider account/keys the project has not chosen. Amounts
//! come from the node wallet via `sendtoaddress`, so run it against a
//! funded testnet wallet — never mainnet, never with real value.
//!
//! Rate-limit state persists to `--state-file` (atomic save per drip) so
//! restarts don't reset limits.
//!
//! The page uses RELATIVE paths only (`api/drip`, `../explorer/…`) so it
//! works behind any reverse-proxy prefix (`/faucet/`) as well as on the
//! raw port — absolute `/api/drip` broke the live form with a 404
//! (nginx proxies /faucet/ without a rewrite filter).

use lm_stratum::rpc::RpcClient;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage:
  faucet --node-rpc HOST:PORT --bind HOST:PORT [--rpc-user U] [--rpc-pass P]
         [--amount COINS] [--per-address-secs N] [--state-file PATH] [--allow-mainnet]

defaults: rpc-user=u, rpc-pass=p, amount=1.0, per-address-secs=86400.
Testnet only (anything else needs --allow-mainnet). The wallet is funded from the consensus faucet reserve
(internal/20): the pool's --claim-faucet tops it up from per-block
coinbase claims, so no manual funding is needed while the reserve lasts.";

/// Branded faucet page (same palette as the pool/explorer dashboards).
/// Relative paths only — works under the `/faucet/` reverse proxy and on
/// the raw port. `__AMOUNT__` is replaced with the configured drip size.
const PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Lucky Lode faucet — testnet</title>
<style>
:root{--bg:#0d1117;--card:#161b22;--line:#30363d;--fg:#e6edf3;--dim:#8b949e;--green:#7ee787;--gold:#ffd166;--red:#f85149}
*{box-sizing:border-box}
body{background:var(--bg);color:var(--fg);font:14px ui-monospace,SFMono-Regular,Menlo,monospace;margin:0 auto;padding:24px;max-width:720px}
header{display:flex;align-items:baseline;gap:10px;flex-wrap:wrap;margin-bottom:16px}
h1{font-size:20px;color:var(--gold);margin:0}
.badge{border:1px solid var(--line);border-radius:999px;padding:2px 10px;font-size:12px;color:var(--dim);text-decoration:none}
a.badge:hover{color:var(--gold);border-color:var(--gold)}
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:14px;margin:12px 0}
.warn{border-color:var(--gold)}
label{display:block;color:var(--dim);font-size:12px;text-transform:uppercase;letter-spacing:.05em;margin-bottom:6px}
input{width:100%;padding:10px;background:var(--bg);color:var(--fg);border:1px solid var(--line);border-radius:6px;font:inherit}
button{padding:10px 18px;background:var(--card);color:var(--gold);border:1px solid var(--gold);border-radius:6px;font:inherit;cursor:pointer;margin-top:10px}
button:disabled{opacity:.5;cursor:default}
a{color:var(--green)}
.ok{color:var(--green)}.err{color:var(--red)}.dim{color:var(--dim)}
#out{margin-top:12px;font-size:13px;overflow-wrap:anywhere}
code{background:#0b0f14;border:1px solid var(--line);border-radius:6px;padding:1px 5px;font-size:13px}
footer{color:var(--dim);font-size:12px;margin-top:22px;line-height:1.7}
</style></head><body>
<header>
  <h1>Lucky Lode faucet</h1>
  <span class="badge" style="color:var(--gold);border-color:var(--gold)">TESTNET</span>
  <a class="badge" href="https://luckylode.cc/">luckylode.cc</a>
  <a class="badge" href="../pool/">pool</a>
  <a class="badge" href="../explorer/">explorer</a>
</header>
<div class="card warn">
  <b>Testnet coins only — no value, the chain may restart.</b><br>
  <span class="dim">Drips <b>__AMOUNT__ tLKYL</b> to your address, once per address per day.
  Funded by the consensus faucet reserve (<b>__RESERVE__ tLKYL</b> left) — never by hand.</span>
</div>
<div class="card">
  <label for="a">your testnet address</label>
  <form id="f" action="api/drip" method="post">
    <input id="a" name="address" placeholder="m… — from the wallet app or `luckyd getnewaddress`" spellcheck="false" autocomplete="off">
    <button id="go" type="submit">Drip</button>
  </form>
  <div id="out"></div>
</div>
<footer>
  A pool worker address works too — drip to the same address you mine with.<br>
  API: <code>POST api/drip {"address": "ADDR"}</code> &rarr; JSON <code>{ok,amount,txid}</code>; 429 while rate-limited (per-address <em>and</em> per-IP windows).<br>
  Lucky Lode &middot; <a href="https://luckylode.cc/">luckylode.cc</a> &middot; <a href="https://github.com/kdsmith18542/LuckyLode">github</a>
</footer>
<script>
const f=document.getElementById('f'),inp=document.getElementById('a'),go=document.getElementById('go'),out=document.getElementById('out');
f.addEventListener('submit',async e=>{
  e.preventDefault();
  const a=inp.value.trim();
  if(!a){out.innerHTML='<span class="err">enter an address</span>';return;}
  go.disabled=true;out.innerHTML='<span class="dim">dripping…</span>';
  try{
    const r=await fetch('api/drip',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({address:a})});
    const j=await r.json();
    if(j.ok){
      out.innerHTML='<span class="ok">sent '+j.amount+' tLKYL</span> · tx <a href="../explorer/tx/'+j.txid+'">'+j.txid.slice(0,24)+'…</a>';
    }else if(j.retry_in_secs){
      const h=Math.floor(j.retry_in_secs/3600),m=Math.floor((j.retry_in_secs%3600)/60);
      out.innerHTML='<span class="err">already dripped to this address — next drip in '+h+'h '+m+'m</span>';
    }else{
      out.innerHTML='<span class="err">'+(j.error||'request failed')+'</span>';
    }
  }catch(err){out.innerHTML='<span class="err">request failed: '+err+'</span>';}
  go.disabled=false;
});
</script>
</body></html>"#;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Default)]
struct Limits {
    /// address -> last drip unix time
    last: HashMap<String, u64>,
    /// client IP -> last drip unix time (key-spray defense: one keypair
    /// per address is free, so the per-address window alone bounds a
    /// single identity, not a sprayer).
    ips: HashMap<String, u64>,
}

impl Limits {
    fn load(path: &Path) -> Result<Self, String> {
        // A missing file is a fresh start; a corrupt/unreadable one fails
        // closed — silently resetting rate limits would reopen every
        // address to immediate re-drips.
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("cannot read state file {}: {e}", path.display())),
        };
        // Current shape is an object; pre-IP-limit files were a bare
        // address->time map — still load those (no silent reset).
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if v.get("addresses").is_some() || v.get("ips").is_some() {
                let addresses = v
                    .get("addresses")
                    .and_then(|a| a.as_object())
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| v.as_u64().map(|t| (k.clone(), t)))
                            .collect()
                    })
                    .unwrap_or_default();
                let ips = v
                    .get("ips")
                    .and_then(|a| a.as_object())
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| v.as_u64().map(|t| (k.clone(), t)))
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(Self {
                    last: addresses,
                    ips,
                });
            }
        }
        serde_json::from_str::<HashMap<String, u64>>(&text)
            .map(|m| Self {
                last: m,
                ips: HashMap::new(),
            })
            .map_err(|e| {
                format!(
                    "corrupt state file {}: {e} (rate limits would silently reset — move it aside or delete it)",
                    path.display()
                )
            })
    }

    fn save(&self, path: &Path) {
        let tmp = path.with_extension("tmp");
        let body = serde_json::json!({"addresses": self.last, "ips": self.ips});
        let result = serde_json::to_vec(&body)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                std::fs::write(&tmp, text)
                    .and_then(|_| std::fs::rename(&tmp, path))
                    .map_err(|e| e.to_string())
            });
        if let Err(e) = result {
            // Loud: a lost rate-limit file reopens every address.
            eprintln!(
                "warning: faucet state save failed for {}: {e}",
                path.display()
            );
        }
    }

    /// Seconds until `address` may drip again (0 = now).
    fn wait(&self, address: &str, period: u64, t: u64) -> u64 {
        match self.last.get(address) {
            Some(last) => (last + period).saturating_sub(t),
            None => 0,
        }
    }

    /// Seconds until `ip` may drip again (0 = now). Unknown/empty IPs
    /// are not limited here (the address window still applies) — the
    /// production path always knows the peer or an X-Forwarded-For.
    fn wait_ip(&self, ip: &str, period: u64, t: u64) -> u64 {
        if ip.is_empty() {
            return 0;
        }
        match self.ips.get(ip) {
            Some(last) => (last + period).saturating_sub(t),
            None => 0,
        }
    }
}

struct Ctx {
    rpc: RpcClient,
    amount: f64,
    period: u64,
    state_file: Option<PathBuf>,
    limits: Mutex<Limits>,
}

fn respond(stream: &mut TcpStream, code: &str, ctype: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {code}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
}

fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split_once('?')?.1;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(v.replace('+', " ").chars().collect::<String>());
        }
    }
    None
}

// Minimal percent-decoding for addresses (base58 has no reserved chars,
// but be lenient).
fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.as_bytes().iter().peekable();
    while let Some(&b) = it.next() {
        if b == b'%' {
            let h: String = it.by_ref().take(2).map(|c| *c as char).collect();
            if let Ok(v) = u8::from_str_radix(&h, 16) {
                out.push(v as char);
                continue;
            }
            out.push('%');
            out.push_str(&h);
        } else {
            out.push(b as char);
        }
    }
    out
}

fn handle(mut stream: TcpStream, ctx: &Ctx) {
    // Bounded request read: read until the header terminator (or a cap),
    // with a deadline — the old single 4 KiB read lost split requests and
    // a connected-and-silent peer parked its thread forever. POST bodies
    // are read after the headers, up to a small cap.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
    let mut raw: Vec<u8> = Vec::with_capacity(2048);
    let mut b = [0u8; 4096];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if std::time::Instant::now() > deadline || raw.len() > 16 * 1024 {
            break;
        }
        let n = stream.read(&mut b).unwrap_or(0);
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&b[..n]);
        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let req = String::from_utf8_lossy(&raw);
    let mut lines = req.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_uppercase();
    let path = parts.next().unwrap_or("/");
    // POST bodies (Content-Length only; chunked is refused): read at most
    // 4 KiB past the headers — a JSON address object is tiny.
    let mut body = String::new();
    if method == "POST" {
        let mut len: usize = 0;
        for line in req.lines().skip(1) {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                len = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        if len > 0 && len <= 4096 {
            if let Some(hdr_end) = req.find("\r\n\r\n") {
                let mut taken = raw[hdr_end + 4..].to_vec();
                while taken.len() < len.min(4096) {
                    let n = stream.read(&mut b).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    taken.extend_from_slice(&b[..n]);
                }
                body = String::from_utf8_lossy(&taken[..taken.len().min(len)]).into_owned();
            }
        }
    }
    // Client IP for the per-IP window: X-Forwarded-For when present
    // (production nginx sets it — see the runbook), else the peer.
    // Direct spoofed XFF is accepted only because the loopback/no-proxy
    // path is operator-only; the public path always comes via nginx.
    let mut ip = stream
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    for line in req.lines().skip(1) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("X-Forwarded-For:") {
            if let Some(first) = v.split(',').next() {
                let f = first.trim().to_string();
                if !f.is_empty() {
                    ip = f;
                }
            }
            break;
        } else if let Some(v) = line.strip_prefix("x-forwarded-for:") {
            if let Some(first) = v.split(',').next() {
                let f = first.trim().to_string();
                if !f.is_empty() {
                    ip = f;
                }
            }
            break;
        }
    }

    if path == "/" || path.starts_with("/?") {
        let amount = if ctx.amount.fract() == 0.0 {
            format!("{}", ctx.amount as i64)
        } else {
            format!("{}", ctx.amount)
        };
        // Live faucet reserve (best-effort: the page renders regardless).
        let reserve = ctx
            .rpc
            .call("getjackpotinfo", serde_json::json!([]))
            .ok()
            .and_then(|v| v.get("faucet").and_then(|s| s.as_i64()))
            .map(|sats| {
                let s = format!("{:.8}", sats as f64 / 100_000_000.0);
                s.trim_end_matches('0').trim_end_matches('.').to_string()
            })
            .unwrap_or_else(|| "–".to_string());
        respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            &PAGE
                .replace("__AMOUNT__", &amount)
                .replace("__RESERVE__", &reserve),
        );
        return;
    }

    // Drips are state-changing: POST with a JSON body is the real API
    // (`{"address": "..."}`); GET `api/drip?address=` stays as a
    // compatible alias for the page/docs (never linked anywhere, so
    // prefetch risk stays theoretical; the per-address window bounds any
    // accidental hit to one drip). CAPTCHA is deliberately absent: it
    // needs a provider account/keys the project has not chosen — per-IP
    // windows are the anti-spray layer until then.
    if let Some(rest) = path.strip_prefix("/api/drip") {
        let mut address = query_param(rest, "address").unwrap_or_default();
        if method == "POST" && !body.trim().is_empty() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(a) = v.get("address").and_then(|a| a.as_str()) {
                    address = a.to_string();
                }
            }
        }
        let address = url_decode(&address).trim().to_string();
        if address.is_empty() || address.len() > 64 {
            respond(
                &mut stream,
                "400 Bad Request",
                "application/json",
                r#"{"ok":false,"error":"missing address"}"#,
            );
            return;
        }
        // Validate via the node (right chain, well-formed).
        match ctx
            .rpc
            .call("validateaddress", serde_json::json!([address]))
        {
            Ok(v) if v.get("isvalid").and_then(|b| b.as_bool()).unwrap_or(false) => {}
            _ => {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    r#"{"ok":false,"error":"invalid address for this chain"}"#,
                );
                return;
            }
        }
        let t = now();
        let mut limits = ctx
            .limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let wait_addr = limits.wait(&address, ctx.period, t);
        let wait_ip = limits.wait_ip(&ip, ctx.period, t);
        let wait = wait_addr.max(wait_ip);
        if wait > 0 {
            let body = format!(r#"{{"ok":false,"error":"rate-limited","retry_in_secs":{wait}}}"#);
            respond(
                &mut stream,
                "429 Too Many Requests",
                "application/json",
                &body,
            );
            return;
        }
        match ctx
            .rpc
            .call("sendtoaddress", serde_json::json!([address, ctx.amount]))
        {
            Ok(txid) => {
                limits.last.insert(address.clone(), t);
                if !ip.is_empty() {
                    limits.ips.insert(ip, t);
                }
                if let Some(path) = &ctx.state_file {
                    limits.save(path);
                }
                let txid = txid.as_str().unwrap_or("");
                let body = serde_json::json!({
                    "ok": true,
                    "address": address,
                    "amount": ctx.amount,
                    "txid": txid,
                })
                .to_string();
                respond(&mut stream, "200 OK", "application/json", &body);
            }
            Err(e) => {
                let body = serde_json::json!({"ok": false, "error": format!("send failed: {e}")})
                    .to_string();
                respond(&mut stream, "502 Bad Gateway", "application/json", &body);
            }
        }
        return;
    }

    respond(&mut stream, "404 Not Found", "text/plain", "not found");
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("faucet {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let mut node_rpc: Option<String> = None;
    let mut rpc_user = "u".to_string();
    let mut rpc_pass = "p".to_string();
    let mut bind: Option<String> = None;
    let mut amount = 1.0f64;
    let mut period = 86_400u64;
    let mut state_file: Option<PathBuf> = None;
    let mut allow_mainnet = false;
    let mut i = 0;
    while i < args.len() {
        let next = |flag: &str| -> Result<String, String> {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("--{flag} needs a value"))
        };
        match args[i].as_str() {
            "--node-rpc" => {
                node_rpc = Some(next("node-rpc").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }))
            }
            "--rpc-user" => {
                rpc_user = next("rpc-user").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--rpc-pass" => {
                rpc_pass = next("rpc-pass").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--bind" => {
                bind = Some(next("bind").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }))
            }
            "--amount" => {
                amount = next("amount")
                    .and_then(|v| v.parse().map_err(|_| "--amount expects a number".into()))
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    });
                // NaN passes `<= 0.0` and fails later at sendtoaddress;
                // infinity would drip the universe. Reject both up front.
                if !amount.is_finite() || amount <= 0.0 {
                    eprintln!("error: --amount must be a finite positive number");
                    return ExitCode::from(2);
                }
            }
            "--per-address-secs" => {
                period = next("per-address-secs")
                    .and_then(|v| {
                        v.parse()
                            .map_err(|_| "--per-address-secs expects an integer".into())
                    })
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    });
                if period == 0 {
                    eprintln!("warning: --per-address-secs 0 disables rate limiting entirely");
                }
            }
            "--state-file" => {
                state_file = Some(PathBuf::from(next("state-file").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })))
            }
            "--allow-mainnet" => {
                allow_mainnet = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            other => {
                eprintln!("error: unknown flag '{other}'\n{USAGE}");
                return ExitCode::from(2);
            }
        }
        i += 2;
    }
    let (Some(node_rpc), Some(bind)) = (node_rpc, bind) else {
        eprintln!("error: --node-rpc and --bind are required\n{USAGE}");
        return ExitCode::from(2);
    };
    let limits = match state_file.as_deref().map(Limits::load) {
        Some(Ok(l)) => l,
        Some(Err(e)) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
        None => Limits::default(),
    };
    let rpc = RpcClient::new(&node_rpc, &rpc_user, &rpc_pass);
    // Chain guard: the faucet moves real value — refuse anything but
    // testnet without an explicit opt-in. ("main" covers mainnet AND
    // regtest, which reports "main".)
    let chain = rpc
        .call("getblockchaininfo", serde_json::json!([]))
        .ok()
        .and_then(|v| v.get("chain").and_then(|c| c.as_str()).map(str::to_string));
    match chain.as_deref() {
        Some("test") => {}
        Some(other) if allow_mainnet => {
            println!("faucet: chain {other} (allowed via --allow-mainnet)");
        }
        Some(other) => {
            eprintln!("faucet: refusing to run against chain {other:?} without --allow-mainnet (regtest also reports \"main\" — the flag covers both)");
            return ExitCode::from(2);
        }
        None => {
            eprintln!("faucet: cannot read node chain (getblockchaininfo failed)");
            return ExitCode::from(1);
        }
    }
    let ctx = Arc::new(Ctx {
        rpc,
        amount,
        period,
        state_file,
        limits: Mutex::new(limits),
    });
    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot bind {bind}: {e}");
            return ExitCode::from(1);
        }
    };
    println!("faucet on http://{bind}/ ({amount} per drip, one per {period}s per address)");
    for stream in listener.incoming().flatten() {
        let ctx = Arc::clone(&ctx);
        std::thread::spawn(move || handle(stream, &ctx));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_roundtrip_and_rate_math() {
        let dir = std::env::temp_dir().join("faucet-limits-test.json");
        let _ = std::fs::remove_file(&dir);
        let mut l = Limits::default();
        assert_eq!(l.wait("m1", 86_400, 1_000), 0, "first drip is immediate");
        assert_eq!(l.wait_ip("1.2.3.4", 86_400, 1_000), 0, "first IP is immediate");
        assert_eq!(l.wait_ip("", 86_400, 1_000), 0, "unknown IP never blocks");
        l.last.insert("m1".to_string(), 1_000);
        l.ips.insert("1.2.3.4".to_string(), 2_000);
        assert_eq!(l.wait("m1", 86_400, 1_000), 86_400);
        assert_eq!(l.wait("m1", 86_400, 87_400), 0);
        assert_eq!(l.wait_ip("1.2.3.4", 86_400, 2_000), 86_400);
        assert_eq!(l.wait_ip("5.6.7.8", 86_400, 2_000), 0);
        l.save(&dir);
        let back = Limits::load(&dir).expect("load");
        assert_eq!(back.wait("m1", 86_400, 1_000), 86_400);
        assert_eq!(back.wait_ip("1.2.3.4", 86_400, 2_000), 86_400);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn legacy_bare_map_state_still_loads() {
        // Pre-IP-limit state files were a bare address->time map: they
        // must load with empty IP limits, never as a silent reset.
        let dir = std::env::temp_dir().join("faucet-legacy-state.json");
        let _ = std::fs::remove_file(&dir);
        std::fs::write(&dir, r#"{"m9":12345}"#).expect("write");
        let back = Limits::load(&dir).expect("load");
        assert_eq!(back.wait("m9", 86_400, 12_345), 86_400);
        assert!(back.ips.is_empty());
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn missing_state_is_fresh_but_corrupt_refuses() {
        let dir = std::env::temp_dir().join("faucet-missing-state.json");
        let _ = std::fs::remove_file(&dir);
        assert!(Limits::load(&dir).is_ok(), "absent file = fresh start");
        std::fs::write(&dir, "{oops").expect("write");
        let err = Limits::load(&dir).expect_err("corrupt state must fail closed");
        assert!(err.contains("corrupt state file"), "{err}");
        let _ = std::fs::remove_file(&dir);
    }
}
