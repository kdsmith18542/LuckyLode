//! Testnet faucet: drip small amounts to miners' addresses (runbook §5).
//!
//! Serves a branded form at `/` and `GET /api/drip?address=ADDR`. Each
//! address may drip once per `--per-address-secs` (default 24 h). Amounts
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
         [--amount COINS] [--per-address-secs N] [--state-file PATH]

defaults: rpc-user=u, rpc-pass=p, amount=1.0, per-address-secs=86400.
Testnet only. The wallet is funded from the consensus faucet reserve
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
  Funded by the consensus faucet reserve — never by hand.</span>
</div>
<div class="card">
  <label for="a">your testnet address</label>
  <form id="f" action="api/drip" method="get">
    <input id="a" name="address" placeholder="m… — from the wallet app or `luckyd getnewaddress`" spellcheck="false" autocomplete="off">
    <button id="go" type="submit">Drip</button>
  </form>
  <div id="out"></div>
</div>
<footer>
  A pool worker address works too — drip to the same address you mine with.<br>
  API: <code>GET api/drip?address=ADDR</code> &rarr; JSON <code>{ok,amount,txid}</code>; 429 while rate-limited.<br>
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
    const r=await fetch('api/drip?address='+encodeURIComponent(a),{cache:'no-store'});
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

#[derive(Default)]
struct Limits {
    /// address -> last drip unix time
    last: HashMap<String, u64>,
}

impl Limits {
    fn load(path: &Path) -> Self {
        let mut l = Self::default();
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(m) = serde_json::from_str::<HashMap<String, u64>>(&text) {
                l.last = m;
            }
        }
        l
    }

    fn save(&self, path: &Path) {
        if let Ok(text) = serde_json::to_string(&self.last) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    /// Seconds until `address` may drip again (0 = now).
    fn wait(&self, address: &str, period: u64, t: u64) -> u64 {
        match self.last.get(address) {
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

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
            return Some(
                v.replace('+', " ")
                    .chars()
                    .collect::<String>(),
            );
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
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let line = req.lines().next().unwrap_or("");
    let path = line.split_whitespace().nth(1).unwrap_or("/");

    if path == "/" || path.starts_with("/?") {
        let amount = if ctx.amount.fract() == 0.0 {
            format!("{}", ctx.amount as i64)
        } else {
            format!("{}", ctx.amount)
        };
        respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            &PAGE.replace("__AMOUNT__", &amount),
        );
        return;
    }

    if let Some(rest) = path.strip_prefix("/api/drip") {
        let raw = query_param(rest, "address").unwrap_or_default();
        let address = url_decode(&raw).trim().to_string();
        if address.is_empty() || address.len() > 64 {
            respond(&mut stream, "400 Bad Request", "application/json", r#"{"ok":false,"error":"missing address"}"#);
            return;
        }
        // Validate via the node (right chain, well-formed).
        match ctx.rpc.call("validateaddress", serde_json::json!([address])) {
            Ok(v) if v.get("isvalid").and_then(|b| b.as_bool()).unwrap_or(false) => {}
            _ => {
                respond(&mut stream, "400 Bad Request", "application/json", r#"{"ok":false,"error":"invalid address for this chain"}"#);
                return;
            }
        }
        let t = now();
        let mut limits = ctx.limits.lock().expect("limits");
        let wait = limits.wait(&address, ctx.period, t);
        if wait > 0 {
            let body = format!(r#"{{"ok":false,"error":"rate-limited","retry_in_secs":{wait}}}"#);
            respond(&mut stream, "429 Too Many Requests", "application/json", &body);
            return;
        }
        match ctx
            .rpc
            .call("sendtoaddress", serde_json::json!([address, ctx.amount]))
        {
            Ok(txid) => {
                limits.last.insert(address.clone(), t);
                if let Some(path) = &ctx.state_file {
                    limits.save(path);
                }
                let txid = txid.as_str().unwrap_or("");
                let body = format!(
                    r#"{{"ok":true,"address":"{}","amount":{},"txid":"{}"}}"#,
                    html_escape(&address),
                    ctx.amount,
                    txid
                );
                respond(&mut stream, "200 OK", "application/json", &body);
            }
            Err(e) => {
                let body = format!(r#"{{"ok":false,"error":"send failed: {}"}}"#, html_escape(&e));
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
    let mut node_rpc: Option<String> = None;
    let mut rpc_user = "u".to_string();
    let mut rpc_pass = "p".to_string();
    let mut bind: Option<String> = None;
    let mut amount = 1.0f64;
    let mut period = 86_400u64;
    let mut state_file: Option<PathBuf> = None;
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
                if amount <= 0.0 {
                    eprintln!("error: --amount must be positive");
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
                    })
            }
            "--state-file" => {
                state_file = Some(PathBuf::from(next("state-file").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })))
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
    let limits = state_file
        .as_deref()
        .map(Limits::load)
        .unwrap_or_default();
    let ctx = Arc::new(Ctx {
        rpc: RpcClient::new(&node_rpc, &rpc_user, &rpc_pass),
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
