//! Testnet faucet: drip small amounts to miners' addresses (runbook §5).
//!
//! Serves a branded form at `/` and `POST /api/drip {"address": ADDR}`
//! (state-changing GET was removed: crawlers mint drips). Each address
//! may drip once per `--per-address-secs` (default 24 h) AND each client
//! IP once per the same window, so a keypair-per-address keygen spray
//! still throttles at the IP layer. There is deliberately no CAPTCHA: it
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

/// One coin = 10^8 sats, integer money everywhere past the CLI.
const SATS_PER_COIN: u64 = 100_000_000;
/// Hard ceiling on a single drip: a typo must never drain the reserve
/// (~100 drips would empty the 100k testnet seed at the ceiling).
const MAX_DRIP_SATS: u64 = 100 * SATS_PER_COIN;

/// Parse a `--amount` decimal string to exact sats. Rejects NaN/infinity
/// (f64 back-ends accept both), non-positive, sub-sat dust, and anything
/// over the ceiling — all fail closed at startup, never at drip time.
fn parse_amount_sats(s: &str) -> Result<u64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("--amount expects a number, got {s:?}"))?;
    if !v.is_finite() || v <= 0.0 {
        return Err("--amount must be a finite positive number".to_string());
    }
    let sats = (v * SATS_PER_COIN as f64).round() as u64;
    if sats == 0 {
        return Err("--amount is below one sat".to_string());
    }
    if sats > MAX_DRIP_SATS {
        return Err(format!(
            "--amount exceeds the {MAX} coin per-drip ceiling",
            MAX = MAX_DRIP_SATS / SATS_PER_COIN
        ));
    }
    // Decimal-place guard: the string must mean exactly this many sats
    // (no silent f64 rounding between the flag and the send). Trailing
    // zeros are harmless and trimmed before counting.
    let decimals = s
        .split('.')
        .nth(1)
        .map(|f| f.trim_end_matches('0').len())
        .unwrap_or(0);
    if decimals > 8 {
        return Err(format!("--amount {s:?} has more than 8 decimal places"));
    }
    Ok(sats)
}

/// Format sats as a plain decimal coin string (exact — no f64).
fn coins(sats: u64) -> String {
    if sats.is_multiple_of(SATS_PER_COIN) {
        format!("{}", sats / SATS_PER_COIN)
    } else {
        let s = format!("{}.{:08}", sats / SATS_PER_COIN, sats % SATS_PER_COIN);
        s.trim_end_matches('0').to_string()
    }
}

/// Client IP for the per-IP window. When the peer is loopback the
/// connection came via the local reverse proxy, which appends the real
/// client IP (`$remote_addr`) to X-Forwarded-For — so the LAST entry is
/// the one the proxy saw and the earlier ones are client-controlled.
/// On any other peer XFF is attacker-controlled and ignored entirely:
/// the peer IP is the limit key.
fn client_ip(peer_ip: &str, peer_is_loopback: bool, xff: Option<&str>) -> String {
    if peer_is_loopback {
        if let Some(x) = xff {
            if let Some(last) = x.split(',').map(str::trim).rfind(|s| !s.is_empty()) {
                return last.to_string();
            }
        }
    }
    peer_ip.to_string()
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
        // Durability (same pattern as the pool state save): write, fsync
        // the file, rename, fsync the directory. Without the syncs a
        // crash can lose the rate limits and reopen every address.
        let result = serde_json::to_vec(&body)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                (|| -> std::io::Result<()> {
                    use std::io::Write as _;
                    let mut f = std::fs::File::create(&tmp)?;
                    f.write_all(&text)?;
                    f.sync_all()?;
                    drop(f);
                    std::fs::rename(&tmp, path)?;
                    if let Some(dir) = path.parent() {
                        if let Ok(d) = std::fs::File::open(dir) {
                            let _ = d.sync_all();
                        }
                    }
                    Ok(())
                })()
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
    amount_sats: u64,
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
    // Client IP for the per-IP window: the LAST X-Forwarded-For entry,
    // but only when the peer is loopback (i.e. the connection came via
    // the local reverse proxy, which appends the real client IP). On any
    // other peer XFF is attacker-controlled and ignored entirely.
    let peer = stream.peer_addr().ok();
    let peer_ip = peer
        .as_ref()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    let peer_is_loopback = peer.as_ref().map(|a| a.ip().is_loopback()).unwrap_or(false);
    let mut xff: Option<&str> = None;
    for line in req.lines().skip(1) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if let Some(v) = line
            .strip_prefix("X-Forwarded-For:")
            .or_else(|| line.strip_prefix("x-forwarded-for:"))
        {
            xff = Some(v);
            break;
        }
    }
    let ip = client_ip(&peer_ip, peer_is_loopback, xff);

    if path == "/" || path.starts_with("/?") {
        let amount = coins(ctx.amount_sats);
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

    // Drips are state-changing: POST with a JSON body is the ONLY API
    // (`{"address": "..."}`). GET used to be a compatible alias but a
    // state-changing GET is a crawler/prefetch drip machine, so it now
    // answers 405. CAPTCHA is deliberately absent: it needs a provider
    // account/keys the project has not chosen — per-IP windows are the
    // anti-spray layer until then.
    if path.strip_prefix("/api/drip").is_some() {
        if method != "POST" {
            respond(
                &mut stream,
                "405 Method Not Allowed",
                "application/json",
                r#"{"ok":false,"error":"use POST api/drip"}"#,
            );
            return;
        }
        let mut address = String::new();
        if !body.trim().is_empty() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(a) = v.get("address").and_then(|a| a.as_str()) {
                    address = a.to_string();
                }
            }
        }
        let address = address.trim().to_string();
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
        // Decimal string, not f64: the node parses it exactly.
        let amount_str = coins(ctx.amount_sats);
        match ctx
            .rpc
            .call("sendtoaddress", serde_json::json!([address, amount_str]))
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
                    "amount": ctx.amount_sats as f64 / SATS_PER_COIN as f64,
                    "amount_sats": ctx.amount_sats,
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
    let mut amount_sats = SATS_PER_COIN; // default 1.0
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
                let raw = next("amount").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
                match parse_amount_sats(&raw) {
                    Ok(sats) => amount_sats = sats,
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::from(2);
                    }
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
        amount_sats,
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
    println!(
        "faucet on http://{bind}/ ({} per drip, one per {period}s per address and per IP)",
        coins(amount_sats)
    );
    // Bounded threads like the pool HTTP listener: an 8k-socket flood
    // must 503, not OOM the process (loopback-behind-nginx today, but
    // the guard is cheap).
    let active: Arc<std::sync::atomic::AtomicUsize> =
        Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming().flatten() {
        if active.load(std::sync::atomic::Ordering::SeqCst) >= 64 {
            eprintln!(
                "faucet: connection cap hit, 503 for peer {}",
                stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "?".to_string())
            );
            let mut stream = stream;
            use std::io::Write as _;
            let _ = writeln!(
                stream,
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            continue;
        }
        active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let ctx = Arc::clone(&ctx);
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            handle(stream, &ctx);
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
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
        assert_eq!(
            l.wait_ip("1.2.3.4", 86_400, 1_000),
            0,
            "first IP is immediate"
        );
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
    fn amount_parsing_is_exact_and_capped() {
        assert_eq!(parse_amount_sats("10").expect("int"), 1_000_000_000);
        assert_eq!(parse_amount_sats("0.00000001").expect("dust"), 1);
        assert_eq!(parse_amount_sats("100").expect("ceiling"), 10_000_000_000);
        assert!(parse_amount_sats("100.00000001").is_err(), "over ceiling");
        assert!(parse_amount_sats("0").is_err(), "non-positive");
        assert!(parse_amount_sats("-5").is_err(), "negative");
        assert!(parse_amount_sats("NaN").is_err(), "NaN");
        assert!(parse_amount_sats("inf").is_err(), "infinity");
        assert!(parse_amount_sats("abc").is_err(), "not a number");
        assert!(parse_amount_sats("0.000000001").is_err(), "sub-sat");
        assert!(parse_amount_sats("1.123456789").is_err(), "9 decimals");
        assert_eq!(coins(1_000_000_000), "10");
        assert_eq!(coins(10), "0.0000001");
        assert_eq!(coins(100_000_000), "1");
    }

    #[test]
    fn client_ip_trusts_only_proxy_seen_address() {
        // Loopback peer (via nginx): last XFF entry wins (proxy-appended),
        // earlier entries are client-controlled and ignored.
        assert_eq!(
            client_ip("127.0.0.1", true, Some("9.9.9.9, 8.8.8.8, 1.2.3.4")),
            "1.2.3.4"
        );
        assert_eq!(client_ip("127.0.0.1", true, None), "127.0.0.1");
        // Direct peer: XFF ignored entirely, even when present.
        assert_eq!(client_ip("5.6.7.8", false, Some("9.9.9.9")), "5.6.7.8");
        assert_eq!(client_ip("5.6.7.8", false, None), "5.6.7.8");
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
