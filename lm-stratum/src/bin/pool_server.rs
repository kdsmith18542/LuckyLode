//! `pool-server` — run the reference pool on a mock chain or a real node.
//!
//! ```text
//! pool-server [--bind HOST:PORT] [--reward R] [--fees F]
//!             [--pool-target (HEX64|max)] [--net-target (HEX64|max)]
//! pool-server --node-rpc HOST:PORT [--rpc-user U] [--rpc-pass P] [--bind HOST:PORT]
//!             [--claim-jackpots] [--claim-pot]
//! ```
//!
//! Mock mode (default) serves an instant-block mock chain (`max`/`max`,
//! reward 1M) on 127.0.0.1:3333. Node mode talks to a running forked node
//! via `getblocktemplate`/`submitblock` — jobs come from the real chain
//! and accepted shares advance it. Ctrl-C stops (connections drain on
//! their next tick).

use lm_stratum::node_provider::NodeProvider;
use lm_stratum::provider::{MockProvider, TemplateProvider};
use lm_stratum::rpc::RpcClient;
use lm_stratum::server::{spawn_server, RunningServer};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// Live dashboard HTML (self-contained; polls /api/stats every 5 s).
const INDEX_HTML: &str = r#"<!doctype html><html><head><meta charset="utf-8">
<title>Lucky Lode pool</title><meta name="viewport" content="width=device-width,initial-scale=1">
<style>
:root{--bg:#0d1117;--card:#161b22;--line:#30363d;--fg:#e6edf3;--dim:#8b949e;--green:#7ee787;--gold:#ffd166;--red:#f85149}
*{box-sizing:border-box}
body{background:var(--bg);color:var(--fg);font:14px ui-monospace,SFMono-Regular,Menlo,monospace;margin:0 auto;padding:24px;max-width:1180px}
header{display:flex;align-items:baseline;gap:12px;flex-wrap:wrap;margin-bottom:18px}
h1{font-size:20px;color:var(--gold);margin:0}
.badge{border:1px solid var(--line);border-radius:999px;padding:2px 10px;font-size:12px;color:var(--dim)}
.badge.claim{color:var(--gold);border-color:var(--gold)}
h2{font-size:12px;color:var(--dim);text-transform:uppercase;letter-spacing:.08em;margin:24px 0 8px}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px}
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:12px}
.card .k{color:var(--dim);font-size:12px;text-transform:uppercase;letter-spacing:.05em}
.card .v{font-size:20px;margin-top:6px;color:var(--green)}
table{border-collapse:collapse;width:100%}
th,td{text-align:left;padding:6px 10px;border-bottom:1px solid #21262d;white-space:nowrap}
th{color:var(--dim);font-weight:normal;font-size:12px;text-transform:uppercase}
tr.off td{color:#484f58}
.dot{display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--green);margin-right:6px}
tr.off .dot{background:#484f58}
.err{color:var(--red);font-size:12px;margin-top:10px}
footer{margin-top:26px;color:var(--dim);font-size:12px;line-height:1.5}
</style></head><body>
<header>
  <h1>Lucky Lode &mdash; pool</h1>
  <span class="badge" id="mode">&hellip;</span>
  <span class="badge" id="payoutmode"></span>
  <span class="badge" id="updated"></span>
</header>
<div class="grid" id="cards"></div>
<h2>Workers</h2>
<table><thead><tr><th></th><th>worker</th><th>device</th><th>60s</th><th>10m</th><th>1h</th><th>shares</th><th>rej</th><th>last share</th></tr></thead><tbody id="workers"></tbody></table>
<h2>Blocks</h2>
<table><thead><tr><th>height</th><th>finder</th><th>jackpot split</th><th>league</th><th>pplns payouts</th></tr></thead><tbody id="blocks"></tbody></table>
<h2>Balances due</h2>
<table><thead><tr><th>worker</th><th>owed</th></tr></thead><tbody id="balances"></tbody></table>
<h2>Payouts</h2>
<table><thead><tr><th>time</th><th>worker</th><th>amount</th><th>txid</th></tr></thead><tbody id="payouts"></tbody></table>
<footer id="note"></footer>
<div class="err" id="err"></div>
<script>
const esc=v=>String(v??'-');
const coin=v=>typeof v==='number'?(v/1e8).toLocaleString(undefined,{maximumFractionDigits:2}):'-';
const hs=v=>{v=Number(v)||0;if(v>=1e6)return (v/1e6).toFixed(2)+' MH/s';if(v>=1e3)return (v/1e3).toFixed(1)+' kH/s';return v.toFixed(0)+' H/s';};
const ago=s=>{if(!s)return '-';const d=Math.max(0,Math.floor(Date.now()/1000-s));if(d<60)return d+'s';if(d<3600)return Math.floor(d/60)+'m';return Math.floor(d/3600)+'h';};
const tstr=t=>new Date(t*1000).toLocaleTimeString();
const device=a=>{const p=String(a||'').split(' ');return p.length>1?p.slice(1).join(' '):'-';};
async function tick(){
 try{
  const j=await (await fetch('/api/stats',{cache:'no-store'})).json();
  const c=[]; const add=(k,v)=>c.push(`<div class="card"><div class="k">${k}</div><div class="v">${esc(v)}</div></div>`);
  if(j.jackpot) add('height', j.jackpot.height);
  if(j.mining) add('difficulty', j.mining.difficulty);
  add('pool 10m', hs(j.pool.hashrate10m));
  add('workers', `${j.pool.online}/${j.pool.workers}`);
  add('shares', j.pool.shares); add('rejects', j.pool.rejects);
  add('blocks found', j.pool.blocks); add('league pool', coin(j.pool.league_pool));
  if(j.jackpot){add('jackpot reserve', coin(j.jackpot.reserve)); add('lucky pot', coin(j.jackpot.pot)); add('drought', j.jackpot.drought+' / 4320');}
  document.getElementById('cards').innerHTML=c.join('');
  document.getElementById('mode').textContent=j.mode||'';
  document.getElementById('payoutmode').textContent=j.payout
    ? (j.payout.exec ? `payouts: EXEC ≥ ${j.payout.threshold}` : `payouts: dry-run ≥ ${j.payout.threshold}`)
    : '';
  document.getElementById('updated').textContent='updated '+new Date().toLocaleTimeString();
  document.getElementById('workers').innerHTML=(j.workers||[]).map(w=>
    `<tr class="${w.online?'':'off'}"><td><span class="dot"></span></td><td>${esc(w.name||w.miner)}</td><td>${esc(device(w.agent))}</td><td>${hs(w.rate60)}</td><td>${hs(w.rate10m)}</td><td>${hs(w.rate1h)}</td><td>${esc(w.shares)}</td><td>${esc(w.rejects)}</td><td>${ago(w.last_seen)}</td></tr>`).join('')
    || '<tr><td colspan="9">no workers yet</td></tr>';
  document.getElementById('blocks').innerHTML=(j.last_blocks||[]).map(b=>
    `<tr><td>${esc(b.height)}</td><td>${esc(b.finder)}</td><td>${coin(b.bonus)}</td><td>${coin(b.league_overflow)}</td><td>${coin(b.pplns_total)} (${esc(b.pplns_count)})</td></tr>`).join('')
    || '<tr><td colspan="5">no blocks yet</td></tr>';
  document.getElementById('balances').innerHTML=(j.balances||[]).map(b=>
    `<tr><td>${esc(b.name||b.miner)}</td><td>${coin(b.owed)}</td></tr>`).join('')
    || '<tr><td colspan="2">nothing due</td></tr>';
  document.getElementById('payouts').innerHTML=(j.payouts||[]).map(p=>
    `<tr><td>${tstr(p.time)}</td><td>${esc(p.name)}</td><td>${coin(p.amount)}</td><td>${esc(p.txid)}</td></tr>`).join('')
    || '<tr><td colspan="4">no payouts sent yet</td></tr>';
  const mode=j.mode||'';
  document.getElementById('note').textContent = mode.includes('pot')
    ? 'Claim mode: templates claim the pot floor, so non-event finds are withheld locally and a block lands only when a find is a pot event (digest < T/1000). Shares are still credited.'
    : (mode.includes('jackpot') ? 'Claim mode: a block lands only when a find wins the 1/100 jackpot draw.' : '');
  document.getElementById('err').textContent='';
 }catch(e){document.getElementById('err').textContent='stats fetch failed: '+e;}
}
tick(); setInterval(tick,5000);
</script></body></html>
"#;

fn stats_json<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
    node: Option<&(String, String, String)>,
    mode: &str,
    ctx: &HttpCtx,
) -> String {
    let s = server.stats();
    let now = now_secs();
    let workers = server.workers(now);
    let online = workers.iter().filter(|w| w.online).count();
    let mut totals = [0.0f64; 4];
    for w in &workers {
        for (i, r) in w.rates.iter().enumerate() {
            totals[i] += r;
        }
    }
    let workers_json: Vec<serde_json::Value> = workers
        .iter()
        .map(|w| {
            serde_json::json!({
                "miner": w.miner,
                "name": w.name,
                "agent": w.agent,
                "shares": w.shares,
                "rejects": w.rejects,
                "last_seen": w.last_seen,
                "online": w.online,
                "rate60": w.rates[0],
                "rate10m": w.rates[1],
                "rate1h": w.rates[2],
                "rate24h": w.rates[3],
            })
        })
        .collect();
    let by_name: HashMap<u64, String> =
        workers.iter().map(|w| (w.miner, w.name.clone())).collect();
    let balances_json: Vec<serde_json::Value> = server
        .balances()
        .into_iter()
        .map(|(miner, owed)| {
            serde_json::json!({
                "miner": miner,
                "name": by_name.get(&miner).cloned().unwrap_or_default(),
                "owed": owed,
            })
        })
        .collect();
    let payouts_json: Vec<serde_json::Value> = ctx
        .history
        .lock()
        .expect("payout history")
        .iter()
        .rev()
        .take(25)
        .map(|p| {
            serde_json::json!({
                "time": p.time,
                "miner": p.miner,
                "name": p.name,
                "amount": p.amount,
                "txid": p.txid,
            })
        })
        .collect();
    let last: Vec<serde_json::Value> = server
        .blocks()
        .iter()
        .rev()
        .take(10)
        .map(|b| {
            let total: u64 = b.payouts.iter().map(|(_, p)| *p).sum();
            serde_json::json!({
                "height": b.height,
                "finder": b.finder,
                "bonus": b.bonus,
                "league_overflow": b.league_overflow,
                "pplns_total": total,
                "pplns_count": b.payouts.len(),
            })
        })
        .collect();
    let mut obj = serde_json::json!({
        "mode": mode,
        "payout": {
            "exec": ctx.exec,
            "threshold": ctx.threshold,
        },
        "pool": {
            "shares": s.shares,
            "rejects": s.rejects,
            "blocks": s.blocks,
            "league_pool": server.league_balance(),
            "workers": workers.len(),
            "online": online,
            "hashrate60": totals[0],
            "hashrate10m": totals[1],
            "hashrate1h": totals[2],
            "hashrate24h": totals[3],
        },
        "workers": workers_json,
        "balances": balances_json,
        "payouts": payouts_json,
        "last_blocks": last,
    });
    if let Some((addr, user, pass)) = node {
        let rpc = RpcClient::new(addr, user, pass);
        if let Ok(v) = rpc.call("getjackpotinfo", serde_json::json!([])) {
            obj["jackpot"] = v;
        }
        if let Ok(v) = rpc.call("getmininginfo", serde_json::json!([])) {
            obj["mining"] = v;
        }
    }
    obj.to_string()
}

fn http_reply<P: TemplateProvider + 'static>(
    stream: &mut TcpStream,
    server: &RunningServer<P>,
    ctx: &HttpCtx,
) -> std::io::Result<()> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(stream.try_clone()?);
        reader.read_line(&mut line)?;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let node = ctx.node.as_ref();
    let (status, ctype, body) = match path {
        "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.to_string()),
        "/api/stats" => ("200 OK", "application/json", stats_json(server, node, &ctx.mode, ctx)),
        _ => ("404 Not Found", "text/plain; charset=utf-8", "not found\n".to_string()),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

/// Minimal read-only HTTP server for the dashboard (one request at a time —
/// it serves a status page, not a workload).
fn http_serve<P: TemplateProvider + 'static>(
    addr: SocketAddr,
    server: Arc<RunningServer<P>>,
    ctx: HttpCtx,
) {
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("http: cannot bind {addr}: {e}");
            return;
        }
    };
    println!("http stats on http://{addr}/");
    for mut s in listener.incoming().flatten() {
        let _ = http_reply(&mut s, &server, &ctx);
    }
}

/// One sent payout, kept in a bounded list for the API.
#[derive(Debug, Clone)]
struct PayoutRecord {
    time: u64,
    miner: u64,
    name: String,
    amount: u64,
    txid: String,
}

/// Payout loop configuration (CLI).
#[derive(Debug, Clone, Copy)]
struct PayoutCfg {
    /// Balances at/above this (base units) become payout candidates.
    /// 0 disables the payout loop entirely.
    threshold: u64,
    interval_secs: u64,
    /// False = dry run (log only, balances untouched). True = real
    /// `sendtoaddress` calls from the node wallet.
    exec: bool,
}

/// Dashboard/API context shared with the HTTP thread.
#[derive(Clone)]
struct HttpCtx {
    node: Option<(String, String, String)>,
    mode: String,
    exec: bool,
    threshold: u64,
    history: Arc<Mutex<VecDeque<PayoutRecord>>>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Wallet decimal (float or string) → base units. Fixed-point parse,
/// integer math only — the same rule as the send path in reverse.
fn coins_to_base(v: &serde_json::Value) -> u64 {
    if let Some(u) = v.as_u64() {
        return u.saturating_mul(100_000_000);
    }
    if let Some(i) = v.as_i64() {
        return (i.max(0) as u64).saturating_mul(100_000_000);
    }
    let s = if let Some(f) = v.as_f64() {
        format!("{f:.8}")
    } else if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        return 0;
    };
    let mut parts = s.splitn(2, '.');
    let whole: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let mut frac = parts.next().unwrap_or("").to_string();
    while frac.len() < 8 {
        frac.push('0');
    }
    let frac: u64 = frac[..frac.len().min(8)].parse().unwrap_or(0);
    whole.saturating_mul(100_000_000).saturating_add(frac)
}

/// Background payout loop (internal/17 §17.4): every interval, send every
/// balance at/above the threshold whose worker name is a payout address,
/// via the node wallet (`sendtoaddress`). Sends debit balances only when
/// the RPC reports success; anything else (invalid name, RPC error) keeps
/// accruing for a later round. In dry-run mode nothing is sent and
/// nothing debits — the log shows what would have moved.
fn payout_loop<P: TemplateProvider + 'static>(
    server: Arc<RunningServer<P>>,
    node: (String, String, String),
    cfg: PayoutCfg,
    history: Arc<Mutex<VecDeque<PayoutRecord>>>,
) {
    // Kept for tx fees: never drain the wallet to the last satoshi.
    const FEE_BUFFER: u64 = 100_000;
    let rpc = RpcClient::new(&node.0, &node.1, &node.2);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(cfg.interval_secs.max(10)));
        let due: Vec<(u64, u64)> = server
            .balances()
            .into_iter()
            .filter(|(_, v)| *v >= cfg.threshold)
            .collect();
        if due.is_empty() {
            continue;
        }
        // Spendable cap: owed keeps growing with immature blocks while the
        // wallet only holds mature funds — pay in due order up to the
        // spendable balance (partials included), the rest waits.
        let mut budget = match rpc.call("getbalance", serde_json::json!([])) {
            Ok(v) => coins_to_base(&v).saturating_sub(FEE_BUFFER),
            Err(e) => {
                eprintln!("payout: getbalance failed: {e}; holding this round");
                continue;
            }
        };
        let names: HashMap<u64, String> = server
            .workers(now_secs())
            .into_iter()
            .map(|w| (w.miner, w.name))
            .collect();
        for (miner, amount) in due {
            let name = names.get(&miner).cloned().unwrap_or_default();
            if !lm_pool::payout::valid_payout_address(&name) {
                println!("payout: miner {miner} owed {} but '{name}' is not a payout address; holding", lm_pool::payout::format_coins(amount));
                continue;
            }
            if !cfg.exec {
                println!(
                    "payout: dry-run would send {} to {name} (miner {miner})",
                    lm_pool::payout::format_coins(amount)
                );
                continue;
            }
            let pay = amount.min(budget);
            if pay == 0 {
                println!("payout: wallet budget exhausted this round; holding the rest");
                break;
            }
            let coins = lm_pool::payout::format_coins(pay);
            match rpc.call("sendtoaddress", serde_json::json!([name, coins])) {
                Ok(v) => {
                    let txid = v.as_str().unwrap_or("").to_string();
                    server.record_paid(miner, pay);
                    budget = budget.saturating_sub(pay);
                    let mut h = history.lock().expect("payout history");
                    h.push_back(PayoutRecord {
                        time: now_secs(),
                        miner,
                        name: name.clone(),
                        amount: pay,
                        txid: txid.clone(),
                    });
                    while h.len() > 100 {
                        h.pop_front();
                    }
                    println!("payout: sent {coins} to {name} (miner {miner}) tx {txid}");
                }
                Err(e) => {
                    eprintln!("payout: sendtoaddress {name} {coins} failed: {e} (balance held)");
                }
            }
        }
    }
}

const USAGE: &str = "usage:
  pool-server [--bind HOST:PORT] [--reward R] [--fees F] [--pool-target (HEX64|max)] [--net-target (HEX64|max)]
  pool-server --node-rpc HOST:PORT [--rpc-user U] [--rpc-pass P] [--bind HOST:PORT] [--claim-jackpots] [--claim-pot]
  both forms accept [--http HOST:PORT] to serve the live dashboard (/ and /api/stats)
  node mode accepts [--payout-threshold V] [--payout-interval SECS] [--payout-exec]

defaults: bind=127.0.0.1:3333; mock mode: reward=1000000, fees=0, both targets=max (instant blocks);
node mode: jobs from the node's getblocktemplate.
claims (node mode): --claim-jackpots serves coinbases claiming the advertised
jackpotbonus (submitted only on jackpot finds); --claim-pot claims the tier-A
floor of the credited pot (submitted only on pot events).
payouts (node mode, internal/17 §17.4): balances at/above --payout-threshold
(base units) are sent from the node wallet every --payout-interval seconds
(default 60); WITHOUT --payout-exec the loop only logs what it would send.
Worker names double as payout addresses (valid R-addresses only).";

fn parse_u64(s: &str, flag: &str) -> Result<u64, String> {
    s.parse::<u64>().map_err(|_| format!("--{flag} expects a non-negative integer, got '{s}'"))
}

fn parse_target(s: &str, flag: &str) -> Result<[u8; 32], String> {
    if s == "max" {
        return Ok([0xffu8; 32]);
    }
    if s.len() != 64 {
        return Err(format!("--{flag} expects 64 hex chars or 'max', got '{s}'"));
    }
    let mut t = [0u8; 32];
    for i in 0..32 {
        t[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("--{flag} expects hex, got '{s}'"))?;
    }
    Ok(t)
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    let mut bind = "127.0.0.1:3333".to_string();
    let mut reward = 1_000_000u64;
    let mut fees = 0u64;
    let mut pool_target = [0xffu8; 32];
    let mut net_target = [0xffu8; 32];
    let mut node_rpc: Option<String> = None;
    let mut rpc_user = "u".to_string();
    let mut rpc_pass = "p".to_string();
    let mut claim_jackpots = false;
    let mut claim_pot = false;
    let mut http: Option<String> = None;
    let mut payout_threshold = 0u64;
    let mut payout_interval = 60u64;
    let mut payout_exec = false;
    let mut i = 0;
    while i < args.len() {
        let next = |flag: &str| -> Result<String, String> {
            args.get(i + 1).cloned().ok_or_else(|| format!("--{flag} needs a value"))
        };
        match args[i].as_str() {
            "--bind" => bind = next("bind").unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(2);
            }),
            "--reward" => {
                reward = parse_u64(&next("reward").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "reward")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--fees" => {
                fees = parse_u64(&next("fees").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "fees")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--pool-target" => {
                pool_target = parse_target(&next("pool-target").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "pool-target")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
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
            "--claim-jackpots" => {
                claim_jackpots = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--claim-pot" => {
                claim_pot = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--http" => {
                http = Some(next("http").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }))
            }
            "--payout-threshold" => {
                payout_threshold = parse_u64(&next("payout-threshold").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "payout-threshold")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--payout-interval" => {
                payout_interval = parse_u64(&next("payout-interval").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "payout-interval")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--payout-exec" => {
                payout_exec = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--rpc-pass" => {
                rpc_pass = next("rpc-pass").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--net-target" => {
                net_target = parse_target(&next("net-target").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "net-target")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            other => {
                eprintln!("error: unknown flag '{other}'\n{USAGE}");
                return std::process::ExitCode::from(2);
            }
        }
        i += 2;
    }
    let addr: SocketAddr = match bind.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("error: --bind expects HOST:PORT, got '{bind}'");
            return std::process::ExitCode::from(2);
        }
    };
    let http_addr = match http {
        Some(a) => match a.parse::<SocketAddr>() {
            Ok(x) => Some(x),
            Err(_) => {
                eprintln!("error: --http expects HOST:PORT, got '{a}'");
                return std::process::ExitCode::from(2);
            }
        },
        None => None,
    };
    if let Some(node) = node_rpc {
        let provider = match NodeProvider::connect_with_claims(
            &node,
            &rpc_user,
            &rpc_pass,
            claim_jackpots,
            claim_pot,
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: cannot reach node RPC {node}: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        let server = match spawn_server(provider, &addr.to_string()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot bind {addr}: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        let claims = match (claim_jackpots, claim_pot) {
            (true, true) => ", claiming jackpots+pot",
            (true, false) => ", claiming jackpots",
            (false, true) => ", claiming pot",
            (false, false) => "",
        };
        let mode = match (claim_jackpots, claim_pot) {
            (true, true) => "claiming jackpots+pot",
            (true, false) => "claiming jackpots",
            (false, true) => "claiming pot",
            (false, false) => "normal mining",
        };
        println!("pool on {} (node provider {}{claims}); Ctrl-C to stop", server.addr(), node);
        let server = Arc::new(server);
        let history = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(a) = http_addr {
            let srv = Arc::clone(&server);
            let ctx = HttpCtx {
                node: Some((node.clone(), rpc_user.clone(), rpc_pass.clone())),
                mode: mode.to_string(),
                exec: payout_exec,
                threshold: payout_threshold,
                history: Arc::clone(&history),
            };
            std::thread::spawn(move || http_serve(a, srv, ctx));
        }
        // Payout loop (internal/17 §17.4): dry-run unless --payout-exec.
        if payout_threshold > 0 {
            let srv = Arc::clone(&server);
            let cfg = PayoutCfg { threshold: payout_threshold, interval_secs: payout_interval, exec: payout_exec };
            let node_cfg = (node.clone(), rpc_user.clone(), rpc_pass.clone());
            let hist = Arc::clone(&history);
            println!(
                "payouts: every {}s over {} base units ({})",
                payout_interval,
                payout_threshold,
                if payout_exec { "EXEC: real sendtoaddress" } else { "dry-run: log only" }
            );
            std::thread::spawn(move || payout_loop(srv, node_cfg, cfg, hist));
        }
    } else {
        let server =
            match spawn_server(MockProvider::new(pool_target, net_target, reward, fees), &addr.to_string())
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: cannot bind {addr}: {e}");
                    return std::process::ExitCode::from(1);
                }
            };
        println!("pool on {} (reward={reward} fees={fees}); Ctrl-C to stop", server.addr());
        if payout_threshold > 0 || payout_exec {
            eprintln!("note: payouts need node mode (--node-rpc); flags ignored on the mock chain");
        }
        let server = Arc::new(server);
        let history = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(a) = http_addr {
            let srv = Arc::clone(&server);
            let ctx = HttpCtx {
                node: None,
                mode: "mock".to_string(),
                exec: false,
                threshold: 0,
                history: Arc::clone(&history),
            };
            std::thread::spawn(move || http_serve(a, srv, ctx));
        }
    }
    // Park the main thread; the accept + connection threads do the work.
    // `stop` is exercised by the test suite; here we simply never call it.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coins_to_base_roundtrips_format_coins() {
        // Bare integers are whole coins (getbalance returns decimals; an
        // integer can only mean whole units of those).
        assert_eq!(coins_to_base(&serde_json::json!(0)), 0);
        assert_eq!(coins_to_base(&serde_json::json!(1)), 100_000_000);
        for v in [1u64, 76_000_000, 100_000_000, 23_809_723_786] {
            // f64 path is exact up to float precision.
            let as_float: f64 = v as f64 / 100_000_000.0;
            let back = coins_to_base(&serde_json::json!(as_float)) as i128;
            assert!((back - v as i128).abs() <= 2, "v={v}");
        }
        assert_eq!(coins_to_base(&serde_json::json!("1.99047620")), 199_047_620);
        assert_eq!(coins_to_base(&serde_json::json!(-1.0)), 0);
        assert_eq!(coins_to_base(&serde_json::json!(null)), 0);
    }
}
