//! `pool-server` — run the reference pool on a mock chain or a real node.
//!
//! ```text
//! pool-server [--bind HOST:PORT] [--reward R] [--fees F]
//!             [--pool-target (HEX64|max)] [--net-target (HEX64|max)]
//! pool-server --node-rpc HOST:PORT [--rpc-user U] [--rpc-pass P] [--bind HOST:PORT]
//!             [--no-claim-jackpots] [--no-claim-pot] [--no-claim-faucet] [--no-claim-dev]
//! ```
//!
//! Node mode claims everything the node advertises by default (jackpot
//! bonus, pot splash, faucet/dev seed drips) — the exact pre-committed
//! amounts, every find submittable. The `--claim-*` flags are deprecated
//! aliases (accepted, no-op); use `--no-claim-*` to opt out. Draw divisors
//! are read from the node (`getjackpotinfo`); `--pot-divisor` /
//! `--jackpot-divisor` are regtest-only overrides.
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
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Live dashboard HTML (self-contained; polls `api/stats` relative to the
/// page, so it works under any reverse-proxy path prefix, e.g. /pool/).
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
.card .v{font-size:20px;margin-top:6px;color:var(--green);overflow-wrap:anywhere}
table{border-collapse:collapse;width:100%}
th,td{text-align:left;padding:6px 10px;border-bottom:1px solid #21262d;white-space:nowrap}
th{color:var(--dim);font-weight:normal;font-size:12px;text-transform:uppercase}
tr.off td{color:#484f58}
.dot{display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--green);margin-right:6px}
tr.off .dot{background:#484f58}
.err{color:var(--red);font-size:12px;margin-top:10px}
nav{display:flex;gap:6px;flex-wrap:wrap;align-items:center;margin:0 0 18px}
nav button{background:none;border:1px solid var(--line);border-radius:8px;color:var(--dim);padding:5px 12px;font:inherit;font-size:13px;cursor:pointer}
nav button.active{color:var(--gold);border-color:var(--gold)}
section{display:none}
section.active{display:block}
a{color:var(--green)}
a.badge{text-decoration:none}
code,pre{background:#0b0f14;border:1px solid var(--line);border-radius:6px}
code{padding:1px 5px;font-size:13px}
pre{padding:10px 12px;overflow-x:auto;font-size:13px;line-height:1.45}
dl{line-height:1.6;margin:0}
dt{color:var(--gold);margin-top:12px}
dd{margin:4px 0 0 0}
p{line-height:1.6}
input[type=search]{background:var(--card);border:1px solid var(--line);border-radius:8px;color:var(--fg);font:inherit;padding:6px 10px;width:min(320px,100%);margin-bottom:8px}
input[type=password],input[type=text]{background:var(--card);border:1px solid var(--line);border-radius:8px;color:var(--fg);font:inherit;padding:6px 10px}
.tabbtn{background:none;border:1px solid var(--line);border-radius:8px;color:var(--fg);padding:6px 12px;font:inherit;font-size:13px;cursor:pointer}
.tabbtn:hover{border-color:var(--gold);color:var(--gold)}
footer{margin-top:26px;color:var(--dim);font-size:12px;line-height:1.5}
</style></head><body>
<header>
  <h1>Lucky Lode &mdash; official pool</h1>
  <span class="badge" id="mode">&hellip;</span>
  <span class="badge" id="payoutmode"></span>
  <span class="badge" id="updated"></span>
</header>
<nav>
  <button data-tab="dashboard" class="active">Dashboard</button>
  <button data-tab="start">Getting started</button>
  <button data-tab="faq">FAQ</button>
  <button data-tab="api">API</button>
  <button data-tab="miner">My miner</button>
  <a class="badge" href="https://luckylode.cc/" target="_blank" rel="noopener">luckylode.cc</a>
  <a class="badge" href="https://luckylode.cc/explorer/" target="_blank" rel="noopener">explorer</a>
  <a class="badge" href="https://luckylode.cc/faucet/" target="_blank" rel="noopener">faucet</a>
</nav>
<section id="tab-dashboard" class="active">
<div class="grid" id="cards"></div>
<h2>Workers</h2>
<input type="search" id="wfilter" placeholder="filter by worker / payout address (Enter looks up idle workers)">
<div id="wlook" style="color:var(--dim);font-size:12px;margin:6px 0"></div>
<table><thead><tr><th></th><th>worker</th><th>device</th><th>60s</th><th>10m</th><th>1h</th><th>24h</th><th>shares</th><th>streak</th><th>league</th><th>rejects</th><th>last share</th></tr></thead><tbody id="workers"></tbody></table>
<h2>League boards <span style="text-transform:none">(season = 30 days, effective work; declared device family, policed by rate)</span></h2>
<div id="league"><span style="color:var(--dim)">no ranked workers yet</span></div>
<h2>Blocks</h2>
<table><thead><tr><th>height</th><th>finder</th><th>block reward</th><th>jackpot bonus</th><th>pot splash</th><th>league inflow</th><th>pplns payouts</th></tr></thead><tbody id="blocks"></tbody></table>
<h2>Balances due</h2>
<table><thead><tr><th>worker</th><th>owed</th><th>paid</th></tr></thead><tbody id="balances"></tbody></table><div class="muted" id="balances-foot"></div>
<h2>Payouts</h2>
<table><thead><tr><th>time</th><th>worker</th><th>amount</th><th>txid</th></tr></thead><tbody id="payouts"></tbody></table>
<h2>Luckynodes</h2>
<div id="luckynodes"><span style="color:var(--dim)">luckynode draws off (start the pool with --luckynode)</span></div>
</section>
<section id="tab-start">
<h2>Mine to the official pool</h2>
<p>Lucky Lode is CPU/GPU-first. The reference miner <code>lm-miner</code> speaks Stratum V1 to this pool:</p>
<pre id="cmd">&hellip;</pre>
<p>Use <b>your own <span id="gs-ticker">tLKYL</span> address</b> (<span id="gs-net">testnet</span> prefixes <code id="gs-prefixes">m, n, 2</code>) as the worker name &mdash; that is where the pool pays you. Every share is verified against the pool's pre-committed job; nothing is withheld.</p>
<h2>Options worth knowing</h2>
<table><thead><tr><th>flag</th><th>what it does</th></tr></thead><tbody>
<tr><td><code>--device cpu|opencl|auto</code></td><td>hash on CPU threads or an OpenCL GPU (auto prefers the GPU when present)</td></tr>
<tr><td><code>--threads N</code></td><td>CPU thread count (default: physical cores)</td></tr>
<tr><td><code>--duty N</code></td><td>run N% of the time &mdash; thermal/power friendly (e.g. 50)</td></tr>
<tr><td><code>--max-temp C</code></td><td>pause when the package/GPU sensor reaches C (80 recommended)</td></tr>
<tr><td><code>--job-timeout S</code></td><td>reconnect if no new job arrives within S seconds</td></tr>
</tbody></table>
<p>Stratum endpoint: <code id="stratum">:3346</code> (plaintext; TLS available on request). <span id="gs-worthless">Testnet coins have no value &mdash; this is the public burn-in.</span></p>
</section>
<section id="tab-faq">
<h2>FAQ</h2>
<dl>
<dt>What is a share vs a block?</dt>
<dd>A share proves work against the pool target; the pool verifies every one against the current job. When a share also beats the network target it is a block &mdash; the pool submits it for you.</dd>
<dt>Why do some shares get rejected?</dt>
<dd>Usually stale (arrived after a job rotation) or duplicate (nonce already submitted). A few are normal; a rising reject rate means connectivity or clock trouble.</dd>
<dt>How do payouts work?</dt>
<dd>Block rewards are credited with PPLNS weights (sqrt-weighted, 25% cap) and sent from the pool wallet to your worker address once mature and above the payout threshold. Balances due and sent payouts are on the dashboard.</dd>
<dt>What are the jackpot and the Lucky Pot?</dt>
<dd>Every PoW block carries a pre-committed draw: <code>SHA256D(parent hash + height)</code> &mdash; the ticket is fixed before any grinding, so nothing can be withheld. Jackpot: draw below 2^256/100 (~1%) &mdash; the block's reward gains a reserve bonus (up to 5× the block reward). Pot: draw below 2^256/1000 with 10/25/50% tiers and a pity timer that forces a splash after a long dry run. Both land only on PoW finds, and the whole block (bonus included) is split among contributors by PPLNS.</dd>
<dt>What is a Luckynode?</dt>
<dd>A staker running the <code>luckynode</code> daemon with the required bond balance, heartbeating to stay active. Luckynodes cosign draws and enter the Luckynode Draw &mdash; a raffle every ~48 h that is the stakers' game-layer luck. PoW finders keep the jackpot and the Lucky Pot. Serious nodes designate a locked bond UTXO at registration (spending it evicts them immediately) instead of relying on a drifting balance.</dd>
<dt>Is this real money?</dt>
<dd id="faq-testnet">No. This is the Lucky Lode public testnet; coins are worthless by design. The point is to break things before mainnet.</dd>
</dl>
</section>
<section id="tab-api">
<h2>API</h2>
<p>Endpoints are relative to this page's path, so they work behind <code>/pool/</code> too:</p>
<table><thead><tr><th>endpoint</th><th>returns</th></tr></thead><tbody>
<tr><td><code>GET api/stats</code></td><td>pool, workers, blocks, balances, payouts, game state, league boards (JSON)</td></tr>
<tr><td><code>GET api/league</code></td><td>season league boards per device class (30-day effective work; ranks, streaks, attestation flags)</td></tr>
<tr><td><code>GET api/worker/&lt;miner&gt;</code></td><td>one worker's public row, alive or idle (streak + league rank + owed/paid survive workerstats age-out; 404 when never seen)</td></tr>
<tr><td><code>GET api/luckynodes</code></td><td>luckynode registry + draw state</td></tr>
<tr><td><code>GET api/luckynode/pending</code></td><td>draws/events awaiting cosigns</td></tr>
<tr><td><code>POST api/luckynode/register</code></td><td>register a luckynode: <code>{id,address,pubkey,msg,sig,ts}</code> (msg must rebuild canonically; sig proves payout-address ownership via node <code>verifymessage</code>)</td></tr>
<tr><td><code>POST api/luckynode/heartbeat</code></td><td>signed liveness: <code>{id,timestamp,sig}</code></td></tr>
<tr><td><code>POST api/luckynode/notarize</code></td><td>cosign a draw: <code>{draw_id,id,sig}</code></td></tr>
<tr><td><code>GET api/miner</code> with <code>Authorization: Bearer &lt;token&gt;</code></td><td>your own row + payout settings (worker-token auth — the token is a header, never a URL)</td></tr>
<tr><td><code>POST api/payout/settings</code></td><td>set/clear your payout address + threshold: <code>{token,address,threshold}</code></td></tr>
<tr><td><code>POST api/attest</code></td><td>record a device-attestation verdict (doc 03 §3.6): <code>{worker,provider:"self",detail?,token?}</code> — verdict-only, shown on league boards; a real Play Integrity verifier replaces the self-labeled stub at mainnet</td></tr>
</tbody></table>
<p>The dashboard polls <code>api/stats</code> every 5 s; miners, bots and dashboards are welcome to do the same.</p>
</section>
<section id="tab-miner">
<h2>My miner</h2>
<p>Paste your worker token (the token from your pool password / the operator's <code>--workers-file</code>) to see your own stats and set where and when you get paid. The token never leaves this page except to this pool's own API.</p>
<div style="display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin:10px 0">
  <input id="mtoken" type="password" placeholder="worker token" style="max-width:280px">
  <button class="tabbtn" id="mload">Load my miner</button>
  <span id="mmsg" style="color:var(--dim)"></span>
</div>
<div id="mstats"></div>
<div id="msettings" style="display:none;margin-top:12px">
  <h3 style="margin:6px 0">Payout settings</h3>
  <div style="display:flex;gap:10px;flex-wrap:wrap;align-items:center">
    <label>payout address <input id="maddr" placeholder="default: your worker name" style="max-width:300px"></label>
    <label>threshold (<span class="mtick">tLKYL</span>) <input id="mthr" placeholder="pool default" style="max-width:130px"></label>
    <button class="tabbtn" id="msave">Save</button>
  </div>
  <p style="color:var(--dim);font-size:12px">Payouts send automatically once your balance reaches the threshold. Leave the address empty (or the threshold at 0) to use the defaults.</p>
</div>
</section>
<footer id="note"></footer>
<div class="err" id="err"></div>
<script>
const esc=v=>String(v??'-');
let TICK='tLKYL';
const coin=v=>typeof v==='number'?(v/1e8).toLocaleString(undefined,{maximumFractionDigits:8})+' '+TICK:'-';
const coin8=v=>typeof v==='number'?(v/1e8).toLocaleString(undefined,{maximumFractionDigits:8})+' '+TICK:'-';
 const hs=v=>{v=Number(v)||0;if(v>=1e6)return (v/1e6).toFixed(2)+' MH/s';if(v>=1e3)return (v/1e3).toFixed(1)+' kH/s';return v.toFixed(0)+' H/s';};
 const num=v=>{v=Number(v)||0;if(v>=1e6)return (v/1e6).toFixed(2)+'M';if(v>=1e3)return (v/1e3).toFixed(1)+'k';return v.toFixed(2);};
const ago=s=>{if(!s)return '-';const d=Math.max(0,Math.floor(Date.now()/1000-s));if(d<60)return d+'s';if(d<3600)return Math.floor(d/60)+'m';return Math.floor(d/3600)+'h';};
const tstr=t=>new Date(t*1000).toLocaleTimeString();
const device=a=>{const p=String(a||'').split(' ').filter(Boolean);return p.length>1?p.slice(1).join(' '):(p[0]||'-');};
// Tabs + miner lookup + endpoint fill.
document.querySelectorAll('nav button').forEach(b=>b.addEventListener('click',()=>{
  document.querySelectorAll('nav button').forEach(x=>x.classList.toggle('active',x===b));
  document.querySelectorAll('section').forEach(s=>s.classList.toggle('active',s.id==='tab-'+b.dataset.tab));
}));
const stratum=location.hostname+':3346';
document.getElementById('stratum').textContent=stratum;
document.getElementById('cmd').textContent='lm-miner pool --pool '+stratum+' --worker <your '+TICK+' payout address> --device auto';
const wf=document.getElementById('wfilter');
const applyWF=()=>{const q=wf.value.trim().toLowerCase();
  document.querySelectorAll('#workers tr').forEach(tr=>{
    tr.style.display=(!q||tr.textContent.toLowerCase().includes(q))?'':'none';});};
wf.addEventListener('input',applyWF);
// Enter in the filter looks the worker up directly (finds idle miners
// whose rows aged out of the live table via api/worker/<miner>).
wf.addEventListener('keydown',async e=>{
  if(e.key!=='Enter')return;
  const q=wf.value.trim();
  const box=document.getElementById('wlook');
  if(!q){box.textContent='';return;}
  box.textContent='looking up '+q+' …';
  try{
    const r=await fetch('api/worker/'+encodeURIComponent(q),{cache:'no-store'});
    if(!r.ok){box.textContent=q+': never seen on this pool';return;}
    const w=await r.json();
    box.textContent=`${w.name||w.miner} · ${w.online?'online':'idle'+(w.last_seen?' (last seen '+ago(w.last_seen)+')':'')} · streak ${w.streak||0}d (best ${w.best_streak||0}) · ${w.league_class?('league '+w.league_class+' #'+w.league_rank+'/'+w.league_size):'unranked this season'} · owed ${coin(w.owed)} · paid ${coin(w.paid)}`;
  }catch(err){box.textContent='lookup failed: '+err;}
});
async function tick(){
 try{
  const j=await (await fetch('api/stats',{cache:'no-store'})).json();
  const c=[]; const add=(k,v)=>c.push(`<div class="card"><div class="k">${k}</div><div class="v">${esc(v)}</div></div>`);
  if(j.jackpot) add('height', j.jackpot.height);
  if(j.mining) add('difficulty', num(j.mining.difficulty));
  add('pool hashrate 10m', hs(j.pool.hashrate10m));
  add('workers', `${j.pool.online}/${j.pool.workers}`);
  add('shares', j.pool.shares); add('rejects', j.pool.rejects);
  add('blocks found', j.pool.blocks); add('league pool', coin(j.pool.league_pool));
  if(j.jackpot){add('jackpot reserve', coin(j.jackpot.reserve)); add('lucky pot', coin(j.jackpot.pot)); add('pot drought', j.jackpot.drought+' / '+(j.jackpot.potdrought||4320)+' blocks');}
  // Faucet/dev reserves and stake supply are NOT pool cards: this page is the
  // miners' view (hashrate, payouts, winnable game layer). Treasury numbers
  // live on the faucet page (faucet reserve) and the explorer game page
  // (dev reserve); stake supply lives on the explorer home. All three stay
  // in /api/stats for tooling.
  const NET=(j.network&&j.network.ticker)?j.network:{ticker:'tLKYL',name:'testnet',prefixes:'m, n, 2'};
  TICK=NET.ticker;
  const setT=(id,v)=>{const e=document.getElementById(id);if(e)e.textContent=v;};
  setT('gs-ticker',NET.ticker); setT('gs-net',NET.name); setT('gs-prefixes',NET.prefixes); setT('foot-net',NET.name);
  document.querySelectorAll('.mtick').forEach(e=>e.textContent=NET.ticker);
  document.getElementById('cmd').textContent='lm-miner pool --pool '+location.hostname+':3346 --worker <your '+NET.ticker+' payout address> --device auto';
  setT('faq-testnet',NET.name==='mainnet'
    ? 'Yes — mainnet coins are real value. This pool is the official reference pool.'
    : 'No. This is the Lucky Lode public testnet; coins are worthless by design. The point is to break things before mainnet.');
  setT('gs-worthless',NET.name==='mainnet'
    ? 'Mainnet coins are real value.'
    : 'Testnet coins have no value — this is the public burn-in.');
  // (no stake-supply card — see the note above the jackpot cards)
  document.getElementById('cards').innerHTML=c.join('');
  document.getElementById('mode').textContent=j.mode||'';
  document.getElementById('payoutmode').textContent=j.payout
    ? (j.payout.exec ? `payouts: EXEC ≥ ${coin(j.payout.threshold)} ${NET.ticker}` : `payouts: dry-run ≥ ${coin(j.payout.threshold)} ${NET.ticker}`)
    : '';
  document.getElementById('updated').textContent='updated '+new Date().toLocaleTimeString();
  document.getElementById('workers').innerHTML=(j.workers||[]).map(w=>
    `<tr class="${w.online?'':'off'}"><td><span class="dot"></span></td><td>${esc(w.name||w.miner)}</td><td>${esc(device(w.agent))}</td><td>${hs(w.rate60)}</td><td>${hs(w.rate10m)}</td><td>${hs(w.rate1h)}</td><td>${hs(w.rate24h)}</td><td>${esc(w.shares)}</td><td>${w.streak?`${esc(w.streak)}d${w.best_streak>w.streak?` <span class="dim">(best ${esc(w.best_streak)})</span>`:''}`:'—'}</td><td>${w.league_class?esc(w.league_class)+' #'+esc(w.league_rank)+'/'+esc(w.league_size)+(w.attested?' <span class="badge">attested</span>':''):'—'}</td><td>${esc(w.rejects)}</td><td>${ago(w.last_seen)}</td></tr>`).join('')
    || '<tr><td colspan="11">no workers yet</td></tr>';
  applyWF();
  const league=(j.league&&j.league.boards)||[];
  document.getElementById('league').innerHTML=league.map(b=>{
    const rows=(b.entries||[]).map(e=>
      `<tr><td>${esc(e.rank)}</td><td>${esc(e.name||e.miner)}</td><td>${num(e.eff_work)}</td><td>${esc(e.days_active)}d</td><td>${e.streak?esc(e.streak)+'d':'—'}</td><td>${e.attested?esc((e.attestation&&e.attestation.provider)||'attested'):'—'}</td></tr>`).join('')
      || '<tr><td colspan="6">no qualifying workers this season</td></tr>';
    return `<h3 style="margin:14px 0 6px;color:var(--gold);font-size:13px">${esc(b.class)}${b.prize_eligible?'':' <span class="dim">(no prizes)</span>'} <span class="dim">· ${esc(b.size)} ranked</span></h3>`+
      `<table><thead><tr><th>rank</th><th>worker</th><th>eff. work</th><th>days</th><th>streak</th><th>device attest</th></tr></thead><tbody>${rows}</tbody></table>`;
  }).join('')||'<span style="color:var(--dim)">no ranked workers yet</span>';
  document.getElementById('blocks').innerHTML=(j.last_blocks||[]).map(b=>
    `<tr><td>${esc(b.height)}</td><td>${esc(b.finder_name||b.finder)}</td><td>${coin(b.value)}</td><td>${b.bonus>0?coin(b.bonus):'–'}</td><td>${b.pot>0?coin(b.pot):'–'}</td><td>${b.league_overflow>0?coin(b.league_overflow):'–'}</td><td>${b.pplns_total>0?coin(b.pplns_total)+' ('+esc(b.pplns_count)+')':'–'}</td></tr>`).join('')
    || '<tr><td colspan="7">no blocks yet</td></tr>';
  document.getElementById('balances').innerHTML=(j.balances||[]).map(b=>
    `<tr><td>${esc(b.name||b.miner)}</td><td>${coin(b.owed)}</td><td>${coin(b.paid)}</td></tr>`).join('')
    || '<tr><td colspan="3">nothing due</td></tr>';
  if(j.payout_totals){document.getElementById('balances-foot').textContent=
    `total owed ${coin(j.payout_totals.owed)} ${NET.ticker} · total paid ${coin(j.payout_totals.paid)} ${NET.ticker} · tx fees ${coin(j.payout_totals.fees)} ${NET.ticker}`;}
  document.getElementById('payouts').innerHTML=(j.payouts||[]).map(p=>
    `<tr><td>${tstr(p.time)}</td><td>${esc(p.name)}</td><td>${coin(p.amount)}</td><td>${esc(p.txid)}</td></tr>`).join('')
    || '<tr><td colspan="4">no payouts sent yet</td></tr>';
  if(j.luckynode){
    const L=j.luckynode;
    const draws=(L.recent_draws||[]).map(d=>
      `<tr><td>${esc(d.epoch)}</td><td>${esc(d.height)}</td><td>${esc((d.winner||'').slice(0,12))}&hellip;</td><td>${coin8(d.budget)}</td><td>${esc(d.cosigns||0)}</td><td>${esc(d.txid)}</td></tr>`).join('')
      || '<tr><td colspan="6">no draws yet</td></tr>';
    document.getElementById('luckynodes').innerHTML=
      `<div class="grid"><div class="card"><div class="k">nodes</div><div class="v">${esc(L.nodes)}</div></div>`+
      `<div class="card"><div class="k">active</div><div class="v">${esc(L.active)}</div></div>`+
      `<div class="card"><div class="k">pending cosigns</div><div class="v">${esc(L.pending_cosigns)}</div></div></div>`+
      `<table><thead><tr><th>epoch</th><th>height</th><th>winner</th><th>budget</th><th>cosigns</th><th>txid</th></tr></thead><tbody>${draws}</tbody></table>`;
  }
  const mode=j.mode||'';
  const claimNote = (mode.includes('pot') || mode.includes('jackpot'))
    ? 'Claim mode: draws are pre-committed per height (SHA256D of the parent hash); every find lands — no withholding.'
    : '';
  document.getElementById('note').textContent =
    'Official Lucky Lode reference pool · <span id="foot-net">testnet</span> · no operator fee (1% miner + 0.5pp pool dev slice applies at mainnet). '
    + claimNote;
  document.getElementById('err').textContent='';
 }catch(e){document.getElementById('err').textContent='stats fetch failed: '+e;}
}
// My miner: token-authenticated stats + payout settings.
let mtok='';
async function loadMiner(){
  mtok=document.getElementById('mtoken').value.trim();
  const msg=document.getElementById('mmsg');
  if(!mtok){msg.textContent='enter your worker token';return;}
  try{
    const r=await fetch('api/miner',{cache:'no-store',headers:{Authorization:'Bearer '+mtok}});
    const j=await r.json();
    if(!r.ok||j.error)throw new Error(j.error||r.statusText);
    const m=j.miner;
    document.getElementById('mstats').innerHTML='<div class="grid">'+
      '<div class="card"><div class="k">worker</div><div class="v">'+esc(m.name)+'</div></div>'+
      '<div class="card"><div class="k">device</div><div class="v">'+esc(device(m.agent))+'</div></div>'+
      '<div class="card"><div class="k">60s / 10m</div><div class="v">'+hs(m.rates[0])+' · '+hs(m.rates[1])+'</div></div>'+
      '<div class="card"><div class="k">shares / rejects</div><div class="v">'+esc(m.shares)+' / '+esc(m.rejects)+'</div></div>'+
      '<div class="card"><div class="k">owed</div><div class="v gold">'+coin8(m.owed)+'</div></div>'+
      '<div class="card"><div class="k">paid</div><div class="v">'+coin8(m.paid)+'</div></div></div>';
    document.getElementById('maddr').value=j.settings.address||'';
    document.getElementById('mthr').value=j.settings.threshold_coins||'';
    document.getElementById('msettings').style.display='block';
    msg.textContent='loaded · pool default threshold '+j.defaults.threshold_coins+' tLKYL';
  }catch(e){msg.textContent=e.message;
    document.getElementById('mstats').innerHTML='';
    document.getElementById('msettings').style.display='none';}
}
async function saveMinerSettings(){
  const msg=document.getElementById('mmsg');
  try{
    const r=await fetch('api/payout/settings',{method:'POST',headers:{'Content-Type':'application/json'},
      body:JSON.stringify({token:mtok,address:document.getElementById('maddr').value.trim(),threshold:document.getElementById('mthr').value.trim()})});
    const j=await r.json();
    if(!r.ok||j.error)throw new Error(j.error||r.statusText);
    msg.textContent=j.cleared?'settings cleared — defaults apply':'saved';
    await loadMiner();
  }catch(e){msg.textContent=e.message;}
}
document.getElementById('mload').onclick=loadMiner;
document.getElementById('msave').onclick=saveMinerSettings;
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
    // miner id -> payout address (persisted names), for block finders.
    let names_map: HashMap<String, String> = server.names().into_iter().collect();
    // Full-history streak state (day rollup; survives raw-log pruning).
    let streak_map: HashMap<String, lm_pool::ledger::StreakState> = server
        .streaks(now)
        .into_iter()
        .map(|s| (s.miner.clone(), s))
        .collect();
    // Seasonal league boards (doc 03 §3.3): per-family ranks over the
    // 30-day season window, plus the per-worker rank shortcut the app
    // tiles read. The league is the miner's declared device family from
    // its connect-time agent string, policed by the trailing rate —
    // labels decide, hashrate polices (doc 03 §3.6). The agent map covers
    // live AND restored (idle) miners, so an offline worker keeps its
    // declared family instead of falling back to the rate-implied one.
    // Fresh attestation verdicts decorate the rows.
    let stored_agents = server.agents();
    let agent_map: HashMap<&str, &str> = stored_agents
        .iter()
        .map(|(m, a)| (m.as_str(), a.as_str()))
        .collect();
    let boards = lm_pool::ledger::rank_by_class(resolve_leagues(
        server.leaderboard(lm_pool::ledger::SEASON_WINDOW_SECS, now),
        &agent_map,
    ));
    let mut rank_map: HashMap<String, (String, u32, u32)> = HashMap::new();
    for (class, rows) in &boards {
        let class_size = rows.len() as u32;
        for (rank, e) in rows {
            rank_map.insert(e.miner.clone(), (class.as_str().to_string(), *rank, class_size));
        }
    }
    let attestations = server.attestations();
    let fresh_attestation = |miner: &str| -> Option<lm_stratum::server::Attestation> {
        attestations
            .get(miner)
            .filter(|a| now.saturating_sub(a.time) <= lm_stratum::server::ATTESTATION_TTL_SECS)
            .cloned()
    };
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
            // Streak tiles come from the full-history day rollup (0 when
            // the miner has no streak or it lapsed via the 48 h rule).
            let st = streak_map.get(&w.miner);
            let (days, streak, best) = match st {
                Some(s) => (s.days_active, s.streak_days, s.best_streak_days),
                None => (0, 0, 0),
            };
            // League rank within the worker's device class (null when the
            // worker has no in-window shares).
            let lg = rank_map.get(&w.miner);
            let att = fresh_attestation(&w.miner);
            serde_json::json!({
                "miner": w.miner,
                "name": w.name,
                "agent": w.agent,
                "shares": w.shares,
                "rejects": w.rejects,
                "last_seen": w.last_seen,
                "online": w.online,
                "days_active": days,
                "streak": streak,
                "best_streak": best,
                "league_class": lg.map(|l| l.0.clone()),
                "league_rank": lg.map(|l| l.1),
                "league_size": lg.map(|l| l.2),
                "attested": att.is_some(),
                "attestation": att.map(|a| serde_json::json!({
                    "provider": a.provider,
                    "time": a.time,
                })),
                "rate60": w.rates[0],
                "rate10m": w.rates[1],
                "rate1h": w.rates[2],
                "rate24h": w.rates[3],
            })
        })
        .collect();
    let by_name: HashMap<String, String> = workers.iter().map(|w| (w.miner.clone(), w.name.clone())).collect();
    let payout_totals = server.payout_totals();
    let balances_json: Vec<serde_json::Value> = server
        .balances()
        .into_iter()
        .map(|(miner, owed)| {
            serde_json::json!({
                "miner": miner,
                "name": by_name.get(&miner).cloned().unwrap_or_default(),
                "owed": owed,
                "paid": server.paid(&miner),
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
            let finder_name = names_map
                .get(&b.finder)
                .cloned()
                .unwrap_or_else(|| format!("miner {}", b.finder));
            serde_json::json!({
                "height": b.height,
                "finder": b.finder,
                "finder_name": finder_name,
                "bonus": b.bonus,
                "pot": b.pot,
                "value": b.value,
                "league_overflow": b.league_overflow,
                "faucet": b.faucet,
                "dev": b.dev,
                "pplns_total": total,
                "pplns_count": b.payouts.len(),
            })
        })
        .collect();
    let mut obj = serde_json::json!({
        "mode": mode,
        "network": {
            "name": ctx.network,
            "ticker": if ctx.network == "mainnet" { "LKYL" } else { "tLKYL" },
            "prefixes": if ctx.network == "mainnet" { "L" } else { "m, n, 2" },
        },
        "payout": {
            "exec": ctx.exec,
            "threshold": ctx.threshold,
        },
        "pool": {
            "shares": s.shares,
            "rejects": s.rejects,
            "blocks": s.blocks,
            "block_rejects": s.block_rejects,
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
        "payout_totals": {
            "owed": payout_totals.0,
            "paid": payout_totals.1,
            "fees": payout_totals.2,
            "coinbases": server.credited_total(),
        },
        "payouts": payouts_json,
        "last_blocks": last,
        "league": {
            "window_secs": lm_pool::ledger::SEASON_WINDOW_SECS,
            "window_days": lm_pool::ledger::SEASON_WINDOW_SECS / 86_400,
            "boards": league_boards_json(&boards, &names_map, &attestations, now, 20),
            // Per-height inflow credited, including staker/peer finds the
            // pool did not mine (the explorer's per-block game view).
            "credits": server
                .league_credits()
                .into_iter()
                .map(|(height, amount)| serde_json::json!({"height": height, "amount": amount}))
                .collect::<Vec<_>>(),
        },
    });
    if let Some((addr, user, pass)) = node {
        let rpc = RpcClient::new(addr, user, pass);
        if let Ok(v) = rpc.call("getjackpotinfo", serde_json::json!([])) {
            obj["jackpot"] = v;
        }
        if let Ok(v) = rpc.call("getmininginfo", serde_json::json!([])) {
            obj["mining"] = v;
        }
        // Node-side staking state (the pool mines PoW; staking lives in
        // the node's wallet). PUBLIC surface: on/off + chain supply only.
        // Operator-wallet shape (eligible outputs/balance, unlock time) is
        // deliberately NOT published — it maps an idle hot wallet. The wallet serves it to the node owner.
        {
            let coins = |v: Option<&serde_json::Value>| -> u64 {
                (v.and_then(|x| x.as_f64()).unwrap_or(0.0) * 100_000_000.0).round() as u64
            };
            obj["staking"] = serde_json::json!({
                "staking_on": obj.get("mining").and_then(|m| m.get("staking")).cloned().unwrap_or(serde_json::Value::Null),
                "staking_supply": coins(obj.get("mining").and_then(|m| m.get("stakingsupply"))),
            });
        }
    }
    if let Some(shared) = &ctx.luckynode {
        let now = now_secs();
        let core = shared.core.lock().expect("luckynode");
        let active = core.registry.active(now, shared.cfg.hb_window);
        let nodes: Vec<serde_json::Value> = core
            .registry
            .all_nodes()
            .iter()
            .map(|n| {
                serde_json::json!({
                    "id": n.id, "address": n.address,
                    "heartbeats": n.heartbeats, "last_heartbeat": n.last_heartbeat,
                    "bond_ok": n.bond_ok,
                })
            })
            .collect();
        let draws: Vec<serde_json::Value> = core
            .recent_draws
            .iter()
            .rev()
            .map(|d| {
                serde_json::json!({
                    "epoch": d.epoch, "height": d.height, "winner": d.winner,
                    "budget": d.budget, "txid": d.txid, "cosigns": d.cosigns,
                    "seed": d.seed_hash, "time": d.time,
                })
            })
            .collect();
        // Only events still awaiting their first cosign count as pending;
        // cosigned entries stay in the list as evidence.
        let pending: usize = core
            .pending
            .iter()
            .filter(|p| p.cosigns.is_empty())
            .count();
        obj["luckynode"] = serde_json::json!({
            "enabled": true,
            "nodes": nodes.len(),
            "active": active.len(),
            "pending_cosigns": pending,
            "recent_draws": draws,
            "config": {"bond": shared.cfg.bond, "epoch": shared.cfg.epoch},
        });
    }
    obj.to_string()
}

/// Resolve display leagues: the miner's declared device family from its
/// connect-time agent string decides, the trailing-24h rate polices
/// (doc 03 §3.6 — self-declared + statistically policed). Unlabeled or
/// contradicted miners fall back to the rate-implied family.
fn resolve_leagues(
    board: Vec<lm_pool::ledger::BoardEntry>,
    agents: &HashMap<&str, &str>,
) -> Vec<lm_pool::ledger::BoardEntry> {
    board
        .into_iter()
        .map(|mut e| {
            let agent = agents.get(e.miner.as_str()).copied().unwrap_or("");
            e.class = lm_pool::league::league_for(lm_pool::league::family_of(agent), e.hps);
            e
        })
        .collect()
}

/// Per-family league boards as JSON (doc 03 §3.3: phones race phones).
/// `cap` limits entries per family (0 = uncapped). Fresh attestation
/// verdicts decorate rows (`attested` + the verdict's provider/time).
fn league_boards_json(
    boards: &[(lm_pool::league::DeviceFamily, Vec<(u32, lm_pool::ledger::BoardEntry)>)],
    names: &HashMap<String, String>,
    attestations: &lm_stratum::server::Attestations,
    now: u64,
    cap: usize,
) -> Vec<serde_json::Value> {
    boards
        .iter()
        .map(|(class, rows)| {
            let entries: Vec<serde_json::Value> = rows
                .iter()
                .take(if cap == 0 { usize::MAX } else { cap })
                .map(|(rank, e)| {
                    let att = attestations.get(&e.miner).filter(|a| {
                        now.saturating_sub(a.time) <= lm_stratum::server::ATTESTATION_TTL_SECS
                    });
                    serde_json::json!({
                        "rank": rank,
                        "miner": e.miner,
                        "name": names.get(&e.miner).cloned().unwrap_or_else(|| e.miner.clone()),
                        "eff_work": e.eff_work,
                        "days_active": e.days_active,
                        "streak": e.streak_days,
                        "attested": att.is_some(),
                        "attestation": att.map(|a| serde_json::json!({
                            "provider": a.provider,
                            "time": a.time,
                        })),
                    })
                })
                .collect();
            serde_json::json!({
                "class": class.as_str(),
                "prize_eligible": class.prize_eligible(),
                "size": rows.len(),
                "entries": entries,
            })
        })
        .collect()
}

/// `GET /api/league` — the full seasonal league boards (all families, up
/// to 100 entries each), same shape as the `league` object on
/// `/api/stats`.
fn league_json<P: TemplateProvider + 'static>(server: &RunningServer<P>) -> String {
    let now = now_secs();
    // Live + restored agents: idle miners keep their declared family.
    let stored_agents = server.agents();
    let agent_map: HashMap<&str, &str> = stored_agents
        .iter()
        .map(|(m, a)| (m.as_str(), a.as_str()))
        .collect();
    let boards = lm_pool::ledger::rank_by_class(resolve_leagues(
        server.leaderboard(lm_pool::ledger::SEASON_WINDOW_SECS, now),
        &agent_map,
    ));
    let names_map: HashMap<String, String> = server.names().into_iter().collect();
    let attestations = server.attestations();
    serde_json::json!({
        "window_secs": lm_pool::ledger::SEASON_WINDOW_SECS,
        "window_days": lm_pool::ledger::SEASON_WINDOW_SECS / 86_400,
        "boards": league_boards_json(&boards, &names_map, &attestations, now, 100),
    })
    .to_string()
}

/// One worker's public row for `GET /api/worker/<miner>` (pure: feeds
/// the offline streak tiles — miners whose workerstats rows aged out
/// are still in the ledger rollup). Returns `None` when the pool has
/// never seen the miner (no live row, no streak, no balance, no name,
/// no attestation) so the route can 404 unknown workers instead of
/// handing out an empty row.
#[allow(clippy::too_many_arguments)]
fn worker_row_json(
    miner: &str,
    live: Option<&lm_stratum::workerstats::WorkerSnapshot>,
    streaks: &HashMap<String, lm_pool::ledger::StreakState>,
    ranks: &HashMap<String, (String, u32, u32)>,
    owed: u64,
    paid: u64,
    name: &str,
    attestation: Option<lm_stratum::server::Attestation>,
    now: u64,
) -> Option<serde_json::Value> {
    let st = streaks.get(miner);
    let lg = ranks.get(miner);
    let seen = live.is_some() || st.is_some() || owed > 0 || paid > 0 || !name.is_empty() || attestation.is_some();
    if !seen {
        return None;
    }
    let (days, streak, best) = match st {
        Some(s) => (s.days_active, s.streak_days, s.best_streak_days),
        None => (0, 0, 0),
    };
    Some(serde_json::json!({
        "miner": miner,
        "name": if name.is_empty() { miner.to_string() } else { name.to_string() },
        "online": live.map(|w| w.online).unwrap_or(false),
        "last_seen": live.map(|w| w.last_seen),
        "agent": live.map(|w| w.agent.clone()),
        "shares": live.map(|w| w.shares).unwrap_or(0),
        "rejects": live.map(|w| w.rejects).unwrap_or(0),
        "rate60": live.map(|w| w.rates[0]),
        "rate10m": live.map(|w| w.rates[1]),
        "rate1h": live.map(|w| w.rates[2]),
        "rate24h": live.map(|w| w.rates[3]),
        "days_active": days,
        "streak": streak,
        "best_streak": best,
        "league_class": lg.map(|l| l.0.clone()),
        "league_rank": lg.map(|l| l.1),
        "league_size": lg.map(|l| l.2),
        "owed": owed,
        "paid": paid,
        "attested": attestation.is_some(),
        "attestation": attestation.map(|a| serde_json::json!({
            "provider": a.provider,
            "time": a.time,
        })),
        "as_of": now,
    }))
}

/// `GET /api/worker/<miner>` — the public row for one worker, alive or
/// idle: live rates when connected, streak + league rank + balances
/// from the ledger either way (streaks survive retention pruning and
/// workerstats age-out). The name is a URL path segment (percent-encoded;
/// capped at 128 chars); worker names are public (they already appear on
/// `/api/stats`), so no auth. 404 when the pool has never seen the name.
fn worker_api<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
    miner_seg: &str,
) -> (&'static str, String) {
    let miner: String = pct_decode(miner_seg).chars().take(128).collect();
    if miner.is_empty() || miner.contains('/') {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "missing worker name"}).to_string(),
        );
    }
    let now = now_secs();
    let workers = server.workers(now);
    let live = workers.into_iter().find(|w| w.miner == miner);
    let streaks: HashMap<String, lm_pool::ledger::StreakState> = server
        .streaks(now)
        .into_iter()
        .map(|s| (s.miner.clone(), s))
        .collect();
    // Live + restored agents: idle miners keep their declared family.
    let stored_agents = server.agents();
    let agent_map: HashMap<&str, &str> = stored_agents
        .iter()
        .map(|(m, a)| (m.as_str(), a.as_str()))
        .collect();
    let boards = lm_pool::ledger::rank_by_class(resolve_leagues(
        server.leaderboard(lm_pool::ledger::SEASON_WINDOW_SECS, now),
        &agent_map,
    ));
    let mut ranks: HashMap<String, (String, u32, u32)> = HashMap::new();
    for (class, rows) in &boards {
        let size = rows.len() as u32;
        for (rank, e) in rows {
            ranks.insert(e.miner.clone(), (class.as_str().to_string(), *rank, size));
        }
    }
    let owed = server
        .balances()
        .into_iter()
        .find(|(m, _)| m == &miner)
        .map(|(_, o)| o)
        .unwrap_or(0);
    let paid = server.paid(&miner);
    let names: HashMap<String, String> = server.names().into_iter().collect();
    let name = names.get(&miner).cloned().unwrap_or_default();
    let attestation = server
        .attestations()
        .get(&miner)
        .filter(|a| now.saturating_sub(a.time) <= lm_stratum::server::ATTESTATION_TTL_SECS)
        .cloned();
    match worker_row_json(
        &miner,
        live.as_ref(),
        &streaks,
        &ranks,
        owed,
        paid,
        &name,
        attestation,
        now,
    ) {
        Some(v) => ("200 OK", v.to_string()),
        None => (
            "404 Not Found",
            serde_json::json!({"error": "unknown worker"}).to_string(),
        ),
    }
}

/// `POST /api/attest` — record a device-attestation verdict (doc 03 §3.6,
/// doc 07 §7.4: verdict-only, no identity). Body:
/// `{worker, provider, detail?, token?}`; the caller may also send the
/// token as `Authorization: Bearer <token>`. When the pool runs a
/// `--workers-file`, the token is required and decides the worker (nobody
/// can attest for someone else); on an open pool the submitted worker
/// name groups the verdict. Only the self-labeled `"self"` provider is
/// accepted until a real Play Integrity verifier is configured — the
/// record is a display flag, not a security boundary.
fn attest_api<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
    authorization: Option<&str>,
    body: &str,
) -> (&'static str, String) {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": format!("bad json: {e}")}).to_string(),
            )
        }
    };
    let provider = req.get("provider").and_then(|v| v.as_str()).unwrap_or("");
    if provider != "self" {
        return (
            "400 Bad Request",
            serde_json::json!({
                "error": "unsupported provider; only 'self' is accepted until a Play Integrity verifier is configured"
            })
            .to_string(),
        );
    }
    let bearer = authorization
        .and_then(|a| a.strip_prefix("Bearer ").or_else(|| a.strip_prefix("bearer ")))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let body_token = req
        .get("token")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let worker = req
        .get("worker")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let miner = if server.worker_tokens_active() {
        match bearer.or(body_token).and_then(|t| server.miner_for_token(t)) {
            Some(m) => m,
            None => {
                return (
                    "403 Forbidden",
                    serde_json::json!({"error": "unknown token"}).to_string(),
                )
            }
        }
    } else if worker.is_empty() {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "missing worker (or send a token)"}).to_string(),
        );
    } else {
        worker
    };
    let detail: String = req
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect();
    let now = now_secs();
    server.set_attestation(
        miner.clone(),
        lm_stratum::server::Attestation {
            provider: provider.to_string(),
            time: now,
            detail,
        },
    );
    (
        "200 OK",
        serde_json::json!({
            "ok": true,
            "worker": miner,
            "provider": provider,
            "time": now,
            "verified": false,
        })
        .to_string(),
    )
}

/// Bounded line read for HTTP: a header line must never be able to
/// allocate without limit, and the caller's read timeout bounds how long
/// a stalled peer can hold the connection.
fn read_line_capped(reader: &mut impl Read, cap: usize) -> std::io::Result<String> {
    let mut out: Vec<u8> = Vec::with_capacity(128);
    let mut b = [0u8; 1];
    loop {
        if reader.read(&mut b)? == 0 {
            break; // EOF
        }
        if b[0] == b'\n' {
            break;
        }
        out.push(b[0]);
        if out.len() > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line too long",
            ));
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn http_reply<P: TemplateProvider + 'static>(
    stream: &mut TcpStream,
    server: &RunningServer<P>,
    ctx: &HttpCtx,
) -> std::io::Result<()> {
    // Deadlines: a slowloris gets seconds, not forever.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
    let mut body = String::new();
    let mut authorization = String::new();
    let line;
    {
        let mut reader = BufReader::new(stream.try_clone()?);
        line = read_line_capped(&mut reader, 8192)?;
        // Headers: only Content-Length matters (POST bodies for the
        // luckynode API); the whole header section is capped (lines and
        // bytes) so neither count nor size can grow without bound.
        let mut content_len = 0usize;
        let mut header_lines = 0usize;
        let mut header_bytes = 0usize;
        loop {
            let h = read_line_capped(&mut reader, 8192)?;
            header_lines += 1;
            header_bytes += h.len();
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if header_lines > 128 || header_bytes > 32 * 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "header section too large",
                ));
            }
            if let Some(v) = h
                .strip_prefix("Content-Length:")
                .or_else(|| h.strip_prefix("content-length:"))
            {
                content_len = v.trim().parse().unwrap_or(0);
            }
            if let Some(v) = h
                .strip_prefix("Authorization:")
                .or_else(|| h.strip_prefix("authorization:"))
            {
                authorization = v.trim().to_string();
            }
        }
        if content_len > 0 && content_len <= (1 << 20) {
            let mut buf = vec![0u8; content_len];
            reader.read_exact(&mut buf)?;
            body = String::from_utf8_lossy(&buf).into_owned();
        }
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");
    // Split any query string off; routes match on the bare path.
    // Query strings are parsed away (nothing routes on them — tokens
    // travel in the Authorization header, never the URL).
    let path = match path.split_once('?') {
        Some((p, _)) => p,
        None => path,
    };
    let node = ctx.node.as_ref();
    let (status, ctype, resp) = match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => (
            "200 OK",
            "text/html; charset=utf-8",
            INDEX_HTML.to_string(),
        ),
        ("GET", "/api/stats") => (
            "200 OK",
            "application/json",
            stats_json(server, node, &ctx.mode, ctx),
        ),
        ("GET", "/api/luckynodes") => ("200 OK", "application/json", luckynode_list(ctx)),
        ("GET", "/api/league") => ("200 OK", "application/json", league_json(server)),
        ("GET", "/api/luckynode/pending") => {
            ("200 OK", "application/json", luckynode_pending(ctx))
        }
        ("POST", "/api/luckynode/register") => {
            let (ok, msg) = luckynode_register(ctx, &body);
            (
                if ok { "200 OK" } else { "400 Bad Request" },
                "application/json",
                msg,
            )
        }
        ("POST", "/api/luckynode/heartbeat") => {
            let (ok, msg) = luckynode_heartbeat(ctx, &body);
            (
                if ok { "200 OK" } else { "400 Bad Request" },
                "application/json",
                msg,
            )
        }
        ("POST", "/api/luckynode/notarize") => {
            let (ok, msg) = luckynode_notarize(ctx, &body);
            (
                if ok { "200 OK" } else { "400 Bad Request" },
                "application/json",
                msg,
            )
        }
        ("GET", "/api/miner") => {
            let (status, json) = miner_api(server, ctx, Some(&authorization));
            (status, "application/json", json)
        }
        ("POST", "/api/miner") => {
            let (status, json) = miner_api(server, ctx, Some(&body));
            (status, "application/json", json)
        }
        ("POST", "/api/payout/settings") => {
            let (status, json) = payout_settings_api(server, &body);
            (status, "application/json", json)
        }
        ("POST", "/api/attest") => {
            let (status, json) = attest_api(server, Some(&authorization), &body);
            (status, "application/json", json)
        }
        ("GET", p) if p.starts_with("/api/worker/") => {
            let (status, json) = worker_api(server, &p["/api/worker/".len()..]);
            (status, "application/json", json)
        }
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found\n".to_string(),
        ),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{resp}",
        resp.len()
    )?;
    stream.flush()
}

/// `GET /api/miner` with `Authorization: Bearer <token>` (or
/// `POST /api/miner` with `{"token": …}`) — the token owner's row +
/// payout settings. Tokens come from `--workers-file`; without one the
/// route is closed. The token never travels in the query string (it
/// would land in logs and proxies).
fn miner_api<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
    ctx: &HttpCtx,
    auth: Option<&str>,
) -> (&'static str, String) {
    let bearer = auth
        .and_then(|a| a.strip_prefix("Bearer ").or_else(|| a.strip_prefix("bearer ")))
        .map(str::trim)
        .map(str::to_string);
    let token = match bearer {
        Some(t) if !t.is_empty() => t,
        _ => match auth.and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok()) {
            Some(v) => v
                .get("token")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            None => String::new(),
        },
    };
    if token.is_empty() {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "missing token (Authorization: Bearer … or POST {token})"}).to_string(),
        );
    }
    let Some(miner) = server.miner_for_token(&token) else {
        return (
            "403 Forbidden",
            serde_json::json!({"error": "unknown token"}).to_string(),
        );
    };
    let Some(row) = server.miner_row(&miner, now_secs()) else {
        return (
            "404 Not Found",
            serde_json::json!({"error": "miner has not connected yet"}).to_string(),
        );
    };
    let s = server
        .payout_settings()
        .get(&miner)
        .cloned()
        .unwrap_or_default();
    (
        "200 OK",
        serde_json::json!({
            "miner": row,
            "settings": {
                "address": s.address,
                "threshold": s.threshold,
                "threshold_coins": s.threshold.map(lm_pool::payout::format_coins),
            },
            "defaults": {
                "threshold": ctx.threshold,
                "threshold_coins": lm_pool::payout::format_coins(ctx.threshold),
            },
        })
        .to_string(),
    )
}

/// `POST /api/payout/settings` — set or clear the caller's payout address
/// and threshold. Body: `{token, address?, threshold?}`; an empty address
/// or a zero/absent threshold clears that override.
fn payout_settings_api<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
    body: &str,
) -> (&'static str, String) {
    let req: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                "400 Bad Request",
                serde_json::json!({"error": format!("bad json: {e}")}).to_string(),
            )
        }
    };
    let token = req.get("token").and_then(|v| v.as_str()).unwrap_or("");
    let Some(miner) = server.miner_for_token(token) else {
        return (
            "403 Forbidden",
            serde_json::json!({"error": "unknown token"}).to_string(),
        );
    };
    let current = server
        .payout_settings()
        .get(&miner)
        .cloned()
        .unwrap_or_default();
    let address = match req.get("address").and_then(|v| v.as_str()) {
        Some("") => None,
        Some(a) => {
            if !lm_pool::payout::valid_payout_address(a) {
                return (
                    "400 Bad Request",
                    serde_json::json!({"error": "not a valid payout address"}).to_string(),
                );
            }
            Some(a.to_string())
        }
        None => current.address,
    };
    let threshold = match req.get("threshold") {
        Some(v) => {
            let t = coins_to_base(v);
            (t > 0).then_some(t)
        }
        None => current.threshold,
    };
    let settings = lm_stratum::server::MinerSettings { address, threshold };
    let cleared = settings == lm_stratum::server::MinerSettings::default();
    if cleared {
        server.clear_payout_settings(&miner);
    } else {
        server.set_payout_settings(miner.clone(), settings.clone());
    }
    (
        "200 OK",
        serde_json::json!({
            "miner": miner,
            "cleared": cleared,
            "settings": {
                "address": settings.address,
                "threshold": settings.threshold,
                "threshold_coins": settings.threshold.map(lm_pool::payout::format_coins),
            },
        })
        .to_string(),
    )
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
    // Thread per connection with a hard cap: the dashboard used to
    // serve sequentially, so one stalled peer froze /api/stats for
    // everyone. Over-cap peers get a 503 and are closed.
    let active: Arc<std::sync::atomic::AtomicUsize> =
        Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for s in listener.incoming().flatten() {
        if active.load(std::sync::atomic::Ordering::SeqCst) >= 64 {
            // Loud: the one time this fired in production (2026-10-19)
            // every API route was already dead — the counter never drains
            // when handler threads wedge, so log it instead of failing
            // silently into per-client 503s.
            eprintln!("http: connection cap hit, 503 for peer {}", s.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".to_string()));
            let mut s = s;
            let _ = writeln!(
                s,
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            continue;
        }
        active.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let server = Arc::clone(&server);
        let ctx = ctx.clone();
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            let mut s = s;
            let _ = http_reply(&mut s, &server, &ctx);
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
}

/// One sent payout, kept in a bounded list for the API.
#[derive(Debug, Clone)]
struct PayoutRecord {
    time: u64,
    miner: String,
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
    luckynode: Option<Arc<LuckynodeShared>>,
    /// Chain label for the dashboard.
    network: String,
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

/// Persisted pool state (`--state-file`): payout ledger, worker-name map
/// and block history. Versioned so a future format can migrate or refuse
/// cleanly.
#[derive(serde::Serialize, serde::Deserialize)]
struct PoolState {
    version: u32,
    ledger: lm_pool::payout::PayoutLedger,
    names: Vec<(String, String)>,
    blocks: Vec<lm_stratum::server::BlockRecord>,
    /// Luckynode draw history (node mode; empty otherwise). `default` so
    /// state files written before this field still load.
    #[serde(default)]
    recent_draws: Vec<DrawRecord>,
    /// Draws/events awaiting cosigns (node mode; empty otherwise).
    #[serde(default)]
    pending: Vec<PendingCosign>,
    /// Luckynode registry snapshot (tenure survives pool restarts).
    #[serde(default)]
    luckynodes: Vec<lm_luckynode::Node>,
    /// Per-miner payout settings (address/threshold overrides,.
    #[serde(default)]
    payout_settings: lm_stratum::server::PayoutSettings,
    /// Per-worker device-attestation verdicts (doc 03 §3.6). `default`
    /// so state files written before this field still load.
    #[serde(default)]
    attestations: lm_stratum::server::Attestations,
    /// League prize-pool balance (feeds the Luckynode draw budgets).
    /// `default` so state files written before this field still load.
    #[serde(default)]
    league_pool: u64,
    /// Lifetime miner-owed coinbase accepted (normal value + jackpot
    /// bonuses + pot splashes). Backs the /api/stats reconciliation
    /// identity `coinbases == owed + paid + fees`. `default` so state
    /// files written before this field still load — in that case the
    /// loader seeds it from the restored block records (exact while
    /// the retained block history is complete).
    #[serde(default)]
    credited_total: u64,
    /// Per-height league inflow actually credited (height, sats),
    /// including staker/peer finds that never enter the block history.
    /// `default` so older state files still load.
    #[serde(default)]
    league_credits: Vec<(u64, u64)>,
    /// In-flight faucet claims (height, amount) — unmatured coinbase
    /// outputs the wallet cannot see yet. Survives restarts so a reboot
    /// does not re-claim a full drip on the next template.
    /// `default` so old state files still load.
    #[serde(default)]
    faucet_unmatured: Vec<(u64, u64)>,
    /// Last-known connect-time agents (miner, agent): idle miners keep
    /// their declared league family across restarts instead of falling
    /// back to the rate-implied one. `default` so old files still load.
    #[serde(default)]
    agents: Vec<(String, String)>,
    /// Share ledger (raw shares + day rollup + comeback state): streaks,
    /// league boards and PPLNS windows survive pool restarts. League
    /// *money* still round-trips through `league_pool` above; the skipped
    /// `Ledger::league_pool` is not authoritative. `default` so old
    /// state files still load.
    #[serde(default)]
    shares: lm_pool::ledger::Ledger,
}

/// Resolve the lifetime credited-coinbase total on state-file load.
/// Pre-counter files carry 0 with a full block history: seed from the
/// restored block records (exact while retained history is complete —
/// the cap is 1000 blocks, noted on BLOCK_HISTORY_CAP). A fresh pool
/// has no blocks, so the seed is 0 as well.
fn resolve_credited_total(
    stored: u64,
    blocks: &[lm_stratum::server::BlockRecord],
) -> u64 {
    if stored == 0 && !blocks.is_empty() {
        blocks.iter().map(|b| b.value + b.bonus + b.pot).sum()
    } else {
        stored
    }
}

/// Load `--state-file` into a freshly spawned server (missing file = a
/// fresh pool; a corrupt file warns and starts fresh rather than refusing
/// to boot — balances are re-derivable from the chain only in theory, so
/// the warning is loud).
fn load_state<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
    luckynode: Option<&Arc<LuckynodeShared>>,
    path: &Path,
) -> Result<(), String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    match serde_json::from_str::<PoolState>(&raw) {
        Ok(st) => {
            server.restore_ledger(st.ledger);
            server.restore_names(&st.names);
            server.restore_agents(&st.agents);
            server.restore_shares(st.shares);
            server.restore_league(st.league_pool);
            server.restore_credited(resolve_credited_total(st.credited_total, &st.blocks));
            server.restore_league_credits(&st.league_credits);
            let unmatured = st.faucet_unmatured.len();
            server.restore_faucet_unmatured(st.faucet_unmatured);
            let settings = st.payout_settings.len();
            server.restore_payout_settings(st.payout_settings);
            server.restore_attestations(st.attestations);
            let n = st.blocks.len();
            server.restore_blocks(st.blocks);
            let (owed, paid, fees) = server.payout_totals();
            let (mut draws, mut pend) = (0usize, 0usize);
            let mut lnodes = 0usize;
            if let Some(ln) = luckynode {
                let mut core = ln.core.lock().expect("luckynode");
                draws = st.recent_draws.len();
                pend = st.pending.len();
                lnodes = st.luckynodes.len();
                core.recent_draws = st.recent_draws.into();
                core.pending = st.pending;
                core.registry.restore(st.luckynodes);
            }
            println!(
                "state loaded from {}: {n} blocks, owed {owed}, paid {paid}, fees {fees}, league {} {}{}",
                path.display(),
                lm_pool::payout::format_coins(st.league_pool),
                if settings > 0 {
                    format!(", {settings} payout setting(s)")
                } else {
                    String::new()
                },
                if luckynode.is_some() {
                    format!(", {draws} draws, {pend} pending cosigns, {lnodes} luckynodes")
                } else if unmatured > 0 {
                    format!(", {unmatured} in-flight faucet claim(s)")
                } else {
                    String::new()
                }
            );
        }
        Err(e) => {
            // A corrupt ledger must NOT silently
            // "start fresh" — that zeroes balances and double-pays. Move
            // the file aside and refuse to boot; the operator decides.
            let corrupt = path.with_extension("corrupt");
            let _ = std::fs::rename(path, &corrupt);
            return Err(format!(
                "state file {} is unreadable ({e}); moved to {} — refusing to start with a zeroed ledger. Restore the file or start with --force-fresh.",
                path.display(),
                corrupt.display()
            ));
        }
    }
    Ok(())
}

/// Atomic snapshot write (tmp + rename) so a crash mid-save never leaves
/// a truncated state file.
fn save_state<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
    luckynode: Option<&Arc<LuckynodeShared>>,
    path: &Path,
) {
    let (recent_draws, pending, luckynodes) = match luckynode {
        Some(ln) => {
            let core = ln.core.lock().expect("luckynode");
            (
                core.recent_draws.iter().cloned().collect(),
                core.pending.clone(),
                core.registry.all_nodes().iter().map(|n| (*n).clone()).collect(),
            )
        }
        None => (Vec::new(), Vec::new(), Vec::new()),
    };
    let st = PoolState {
        version: 1,
        ledger: server.ledger_snapshot(),
        names: server.names(),
        blocks: server.blocks(),
        recent_draws,
        pending,
        payout_settings: server.payout_settings(),
        attestations: server.attestations(),
        league_pool: server.league_balance(),
        credited_total: server.credited_total(),
        league_credits: server.league_credits(),
        faucet_unmatured: server.faucet_unmatured(),
        agents: server.agents(),
        shares: server.shares_snapshot(),
        luckynodes,
    };
    let bytes = match serde_json::to_vec(&st) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("warning: state serialize failed: {e}");
            return;
        }
    };
    let tmp = path.with_extension("tmp");
    // Durability: write → fsync file → rename → fsync dir.
    // Without the syncs a power loss can leave a truncated/empty state
    // file after a "successful" rename.
    let result = std::fs::File::create(&tmp).and_then(|mut f| {
        use std::io::Write;
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Some(dir) = path.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        eprintln!("warning: cannot write state file {}: {e}", path.display());
    }
}

/// Periodic saver: the ledger moves on every block and payout, so a
/// bounded 10 s loss window is the crash story.
fn state_saver<P: TemplateProvider + 'static>(
    server: Arc<RunningServer<P>>,
    luckynode: Option<Arc<LuckynodeShared>>,
    path: PathBuf,
) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        save_state(&server, luckynode.as_ref(), &path);
    }
}

/// SIGTERM arrives with the ledger up to 10 s stale: save once, then
/// exit. The handler itself only flips an atomic (async-signal safety);
/// this watcher thread does the actual saving. Spawned with or without a
/// state file — the handler is installed unconditionally, so without a
/// watcher a SIGTERM to a no-state-file pool would be silently swallowed.
static TERM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn on_sigterm(_sig: libc::c_int) {
    TERM.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn term_watcher<P: TemplateProvider + 'static>(
    server: Arc<RunningServer<P>>,
    luckynode: Option<Arc<LuckynodeShared>>,
    path: Option<PathBuf>,
) {
    loop {
        if TERM.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(path) = &path {
                save_state(&server, luckynode.as_ref(), path);
                println!("SIGTERM: state saved to {} — exiting", path.display());
            } else {
                println!("SIGTERM: no state file configured — exiting");
            }
            std::process::exit(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Background payout loop: every interval, send every
/// balance at/above the threshold whose worker name is a payout address,
/// via the node wallet (`sendtoaddress`). Sends debit balances only when
/// the RPC reports success; anything else (invalid name, RPC error) keeps
/// accruing for a later round. In dry-run mode nothing is sent and
/// nothing debits — the log shows what would have moved.
/// Miners due for a payout: balance at or above their own threshold, else
/// the pool's global one payout settings).
fn due_miners(
    balances: Vec<(String, u64)>,
    settings: &lm_stratum::server::PayoutSettings,
    global_threshold: u64,
) -> Vec<(String, u64)> {
    balances
        .into_iter()
        .filter(|(miner, v)| {
            let t = settings
                .get(miner.as_str())
                .and_then(|s| s.threshold)
                .unwrap_or(global_threshold);
            *v > 0 && *v >= t
        })
        .collect()
}

/// Payout destination for one miner: their own address override, else the
/// worker name (which doubles as an address).
fn payout_destination(
    miner: &str,
    settings: &lm_stratum::server::PayoutSettings,
) -> String {
    // The worker name IS the payout identity —
    // it doubles as the address unless the miner set an override.
    settings
        .get(miner)
        .and_then(|s| s.address.clone())
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| miner.to_string())
}

fn payout_loop<P: TemplateProvider + 'static>(
    server: Arc<RunningServer<P>>,
    node: (String, String, String),
    cfg: PayoutCfg,
    history: Arc<Mutex<VecDeque<PayoutRecord>>>,
    // Persist immediately after each real send:
    // the old 10 s snapshot window could restore a spent balance after a
    // crash and pay the same miner twice.
    state: Option<(PathBuf, Option<Arc<LuckynodeShared>>)>,
) {
    // Kept for tx fees: never drain the wallet to the last satoshi.
    const FEE_BUFFER: u64 = 100_000;
    let rpc = RpcClient::new(&node.0, &node.1, &node.2);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(cfg.interval_secs.max(10)));
        let settings = server.payout_settings();
        let due: Vec<(String, u64)> =
            due_miners(server.balances(), &settings, cfg.threshold);
        if due.is_empty() {
            continue;
        }
        // Spendable cap: owed keeps growing with immature blocks while the
        // wallet only holds mature funds — pay in due order up to the
        // spendable balance (partials included), the rest waits. The league
        // prize pool is reserved for Luckynode Draws, so miner payouts may
        // never eat it (draws debit the ledger; this keeps the wallet
        // honest against that claim).
        let league_reserve = server.league_balance();
        let mut budget = match rpc.call("getbalance", serde_json::json!([])) {
            Ok(v) => coins_to_base(&v)
                .saturating_sub(FEE_BUFFER)
                .saturating_sub(league_reserve),
            Err(e) => {
                eprintln!("payout: getbalance failed: {e}; holding this round");
                continue;
            }
        };
        for (miner, amount) in due {
            // Payout destination: the miner's own override, else the worker
            // name (which doubles as an address).
            let name = payout_destination(&miner, &settings);
            if !lm_pool::payout::valid_payout_address(&name) {
                println!(
                    "payout: miner {miner} owed {} but '{name}' is not a payout address; holding",
                    lm_pool::payout::format_coins(amount)
                );
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
                    server.record_paid(&miner, pay);
                    if let Some((path, ln)) = &state {
                        save_state(&server, ln.as_ref(), path);
                    }
                    budget = budget.saturating_sub(pay);
                    let mut h = history.lock().expect("payout history");
                    h.push_back(PayoutRecord {
                        time: now_secs(),
                        miner: miner.clone(),
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

/// Luckynode config (CLI; pool-side v1 — no consensus).
#[derive(Debug, Clone)]
struct LuckynodeCfg {
    /// Payout address must hold at least this (base units) to qualify.
    bond: u64,
    /// Blocks per draw epoch (spec 2880 ~48h; tests override shorter).
    epoch: u64,
    /// Basis points of trailing league inflow funding each draw (spec ~500).
    slice_bps: u64,
    /// Heartbeat trailing window for active service (secs; spec open).
    hb_window: u64,
    /// Tenure reset gap: a longer dark spell restarts tenure (secs).
    reset_gap: u64,
    /// Real sendtoaddress vs log-only.
    exec: bool,
    /// Tick interval secs.
    interval: u64,
}

/// A draw (or event) posted for luckynode cosigning (notarization duty).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PendingCosign {
    draw_id: String,
    kind: String,
    height: u64,
    block_hash: String,
    detail: String,
    cosigns: Vec<(String, String)>,
    created_at: u64,
}

/// A completed Luckynode Draw §34).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DrawRecord {
    epoch: u64,
    height: u64,
    seed_hash: String,
    winner: String,
    /// Winner's payout address, captured at draw time.
    #[serde(default)]
    winner_addr: String,
    budget: u64,
    txid: String,
    time: u64,
    /// Cosigns received for this draw (notarization evidence; absent in
    /// old state files → 0).
    #[serde(default)]
    cosigns: u32,
}

/// Pool-side luckynode state behind ONE mutex (shared by the HTTP
/// threads and the draw loop). Single lock by design: the registry,
/// pending cosigns and draw records are always read/written together,
/// and three fine-grained mutexes deadlocked the pool in production
/// (2026-10-19: every HTTP thread parked in lock_contended, all API
/// routes 503 — ABBA between the cosign path's pending→registry→draws
/// order and the list path's registry→draws order). Contention is
/// trivial (a few ops/sec), so one coarse lock is strictly correct.
/// Never hold it across RPC (snapshot IDs first, re-lock to write).
struct LuckynodeCore {
    registry: lm_luckynode::Registry,
    pending: Vec<PendingCosign>,
    recent_draws: VecDeque<DrawRecord>,
}

/// Pool-side luckynode state, shared by the HTTP thread and the draw loop.
struct LuckynodeShared {
    core: Mutex<LuckynodeCore>,
    /// Highest epoch already drawn (None = none yet). Startup marks
    /// already-completed epochs as drawn (no backfill); only new
    /// boundaries draw — restart-safe without persistence.
    last_epoch: Mutex<Option<u64>>,
    /// League balance at the last draw (trailing inflow = now - snapshot).
    league_snapshot: Mutex<u64>,
    /// First-tick init done (last_epoch + snapshot set once).
    init_done: Mutex<bool>,
    cfg: LuckynodeCfg,
}

/// One node's bond inputs, snapshotted under the luckynode lock so the
/// blocking RPC checks run unlocked (never hold luckynode state over
/// network I/O).
struct BondCheck {
    id: String,
    addr: String,
    owned: bool,
    bond_utxo: Option<(String, u32)>,
}

/// Bond balance for one address (base units). Preferred: the node's
/// address index (`getaddressbalance` — counts all confirmed outputs
/// including immature coinbases; needs `-addressindex=1`). Fallback:
/// wallet `listunspent` (mature/spendable only on chains that hide
/// immature coinbases there). Bond addresses live in the node wallet.
fn bond_balance(rpc: &RpcClient, address: &str) -> u64 {
    if let Ok(v) = rpc.call(
        "getaddressbalance",
        serde_json::json!([{"addresses": [address]}]),
    ) {
        if let Some(bal) = v.get("balance").and_then(|b| b.as_i64()) {
            if bal >= 0 {
                return bal as u64;
            }
        }
    }
    // Fallback path (no address index): scan wallet UTXOs.
    let Ok(v) = rpc.call("listunspent", serde_json::json!([0])) else {
        return 0;
    };
    let mut sum = 0u64;
    for u in v.as_array().cloned().unwrap_or_default() {
        let addr = u.get("address").and_then(|a| a.as_str()).unwrap_or("");
        if addr != address {
            continue;
        }
        let amt = u.get("amount").and_then(|a| a.as_f64()).unwrap_or(0.0);
        if amt > 0.0 {
            sum += (amt * 100_000_000.0).round() as u64;
        }
    }
    sum
}

/// One luckynode tick: bond checks, epoch draws, winner payment.
/// Runs in its own thread (see luckynode_loop), never on a hot path.
fn luckynode_tick<P: TemplateProvider + 'static>(
    server: &Arc<RunningServer<P>>,
    rpc: &RpcClient,
    shared: &Arc<LuckynodeShared>,
    history: &Arc<Mutex<VecDeque<PayoutRecord>>>,
) {
    let now = now_secs();
    let cfg = &shared.cfg;
    let height = match rpc.call("getblockcount", serde_json::json!([])) {
        Ok(v) => v.as_u64().unwrap_or(0),
        Err(e) => {
            eprintln!("luckynode: getblockcount failed: {e}");
            return;
        }
    };
    if cfg.epoch == 0 {
        return;
    }
    // Bond verification (pool-side; the node holds the funds). Per
    // address via the address index (falls back to wallet listunspent).
    // Snapshot under the lock, release it across the blocking RPC calls,
    // re-lock only to stamp verdicts (never hold luckynode state over
    // network I/O — that stalled every HTTP thread in production once).
    let checks: Vec<BondCheck> = {
        let core = shared.core.lock().expect("luckynode");
        core.registry
            .all_nodes()
            .iter()
            .map(|n| BondCheck {
                id: n.id.clone(),
                addr: n.address.clone(),
                owned: n.owned,
                bond_utxo: n.bond_utxo.clone(),
            })
            .collect()
    };
    let mut verdicts: Vec<(String, bool)> = Vec::with_capacity(checks.len());
    for c in checks {
        // Bond verdict: funded AND ownership-proven at registration.
        let mut ok = bond_balance(rpc, &c.addr) >= cfg.bond && c.owned;
        // Locked-collateral check: a designated bond UTXO must still
        // be unspent — spending it evicts immediately, even if the
        // address balance was topped up elsewhere. gettxout needs no
        // address index, so third-party nodes can bond too.
        if let Some((txid, vout)) = c.bond_utxo {
            let spent = match rpc.call(
                "gettxout",
                serde_json::json!([txid, vout, true]),
            ) {
                Ok(v) => v.is_null(),
                Err(_) => true, // fail closed on RPC trouble
            };
            if spent {
                ok = false;
            }
        }
        verdicts.push((c.id, ok));
    }
    {
        let mut core = shared.core.lock().expect("luckynode");
        for (id, ok) in verdicts {
            core.registry.set_bond(&id, ok, now);
        }
    }
    // Epoch draws: epoch E completes at height (E+1)*epoch (seeded by the
    // hash of block E*epoch). Startup marks already-completed epochs as
    // drawn (no backfill); only new boundaries draw — restart-safe.
    let cur_epoch = height / cfg.epoch;
    {
        let mut done = shared.init_done.lock().expect("init");
        if !*done {
            *done = true;
            let mut last = shared.last_epoch.lock().expect("last epoch");
            // No backfill: epochs already completable at startup are
            // marked drawn; E=0 (or the current one) draws at its boundary.
            *last = if height >= cfg.epoch {
                Some(cur_epoch.saturating_sub(1))
            } else {
                None
            };
            let mut snap = shared.league_snapshot.lock().expect("snapshot");
            *snap = server.league_balance();
            println!(
                "luckynode: live (bond {:.8} coins epoch {} slice {}bps); next draw at height {}",
                cfg.bond as f64 / 100_000_000.0,
                cfg.epoch,
                cfg.slice_bps,
                (cur_epoch + 1) * cfg.epoch
            );
            // Reconcile state written before the `cosigns` field existed:
            // stamp draw records from already-cosigned pending entries.
            {
                let mut guard = shared.core.lock().expect("luckynode");
                let LuckynodeCore { pending, recent_draws, .. } = &mut *guard;
                for p in pending.iter() {
                    if p.cosigns.is_empty() {
                        continue;
                    }
                    if let Some(d) = recent_draws
                        .iter_mut()
                        .find(|d| format!("draw-{}", d.epoch) == p.draw_id)
                    {
                        d.cosigns = p.cosigns.len() as u32;
                    }
                }
            }
        }
    }
    // Draw every completable epoch past `last` (normally exactly one).
    // Completable at height h (h >= epoch): E <= h/epoch - 1.
    loop {
        if height < cfg.epoch {
            break;
        }
        let next = {
            let last = shared.last_epoch.lock().expect("last epoch");
            let highest_done = height / cfg.epoch - 1;
            match *last {
                Some(l) if l < highest_done => l + 1,
                None => 0,
                _ => break,
            }
        };
        // `None` with highest_done == 0 draws E=0; with height < epoch we
        // broke above, so highest_done >= 0 always holds here.
        run_draw(server, rpc, shared, history, next, height, now);
        *shared.last_epoch.lock().expect("last epoch") = Some(next);
    }
    // Held draw payments: the wallet was empty or the coinbases were
    // immature when the draw fired (testnet ramp). Retry whenever there
    // is spendable balance; update the record on success. Addresses come
    // from the registry, so records predating any address field still pay.
    if cfg.exec {
        let spendable = rpc
            .call("getbalance", serde_json::json!([]))
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if spendable > 0.0 {
            let held: Vec<(String, u64, u64, String)> = {
                let core = shared.core.lock().expect("luckynode");
                core.recent_draws
                    .iter()
                    .filter(|d| {
                        d.txid.starts_with("held:") && d.budget > 0 && !d.winner.is_empty()
                    })
                    .map(|d| {
                        (d.winner.clone(), d.budget, d.epoch, d.winner_addr.clone())
                    })
                    .collect()
            };
            for (winner_id, budget, epoch, saved_addr) in held {
                // Prefer the address captured at draw time (survives a
                // pool restart); fall back to the live registry.
                let addr = if !saved_addr.is_empty() {
                    saved_addr
                } else {
                    let core = shared.core.lock().expect("luckynode");
                    core.registry
                        .all_nodes()
                        .iter()
                        .find(|n| n.id == winner_id)
                        .map(|n| n.address.clone())
                        .unwrap_or_default()
                };
                if addr.is_empty() {
                    continue;
                }
                let coins = lm_pool::payout::format_coins(budget);
                match rpc.call("sendtoaddress", serde_json::json!([addr, coins])) {
                    Ok(v) => {
                        let t = v.as_str().unwrap_or("").to_string();
                        println!(
                            "luckynode: held draw {epoch} for {winner_id} ({addr}) paid {coins} tx {t}"
                        );
                        server.debit_league(budget);
                        {
                            let mut h = history.lock().expect("payout history");
                            h.push_back(PayoutRecord {
                                time: now,
                                miner: "luckynode-draw".to_string(),
                                name: format!("luckynode-draw:{addr}"),
                                amount: budget,
                                txid: t.clone(),
                            });
                            while h.len() > 100 {
                                h.pop_front();
                            }
                        }
                        let mut core = shared.core.lock().expect("luckynode");
                        for d in core.recent_draws.iter_mut() {
                            if d.epoch == epoch && d.txid.starts_with("held:") {
                                d.txid = t.clone();
                            }
                        }
                    }
                    Err(e) => {
                        let mut core = shared.core.lock().expect("luckynode");
                        for d in core.recent_draws.iter_mut() {
                            if d.epoch == epoch && d.txid.starts_with("held:") {
                                d.txid = format!("held: {e}");
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Run the Luckynode Draw for epoch `e` §34).
/// Default Luckynode Draw slice: 10% of trailing league inflow
/// (raised from 5% on 2026-10-17 — the draw is the only luckynode income).
/// Pinned by `draw_budget_gates` so a silent revert fails CI.
const DEFAULT_LUCKYNODE_SLICE_BPS: u64 = 1000;

/// Draw budget for a trailing league inflow at `slice_bps` basis points.
/// Pure + floor (u128 intermediate): the exact cut the draw can spend.
fn draw_budget(inflow: u64, slice_bps: u64) -> u64 {
    ((inflow as u128 * slice_bps as u128) / 10_000) as u64
}

fn run_draw<P: TemplateProvider + 'static>(
    server: &Arc<RunningServer<P>>,
    rpc: &RpcClient,
    shared: &Arc<LuckynodeShared>,
    history: &Arc<Mutex<VecDeque<PayoutRecord>>>,
    e: u64,
    height: u64,
    now: u64,
) {
    let cfg = &shared.cfg;
    let seed_h = match rpc.call("getblockhash", serde_json::json!([e * cfg.epoch])) {
        Ok(v) => v.as_str().unwrap_or("").to_string(),
        Err(e) => {
            eprintln!("luckynode: draw {e}: getblockhash failed: {e}");
            return;
        }
    };
    let Ok(seed_bytes) = lm_luckynode::notarize::unhex(&seed_h) else {
        eprintln!("luckynode: draw {e}: bad seed hash {seed_h}");
        return;
    };
    let seed: [u8; 32] = match seed_bytes.try_into() {
        Ok(b) => b,
        Err(_) => {
            eprintln!("luckynode: draw {e}: seed not 32 bytes");
            return;
        }
    };
    let (entrants, winner_addr) = {
        let core = shared.core.lock().expect("luckynode");
        let bonded = core.registry.draw_eligibles(now, cfg.hb_window);
        let ent: Vec<(String, u64)> = bonded
            .iter()
            .map(|n| {
                (
                    n.id.clone(),
                    lm_luckynode::tenure_weight_millis(core.registry.tenure_secs(n, now)),
                )
            })
            .collect();
        let refs: Vec<(&str, u64)> = ent.iter().map(|(id, w)| (id.as_str(), *w)).collect();
        let winner = lm_luckynode::draw_winner(&refs, &seed).unwrap_or("").to_string();
        let addr = bonded
            .iter()
            .find(|n| n.id == winner)
            .map(|n| n.address.clone())
            .unwrap_or_default();
        (ent.len(), (winner, addr))
    };
    // Budget: slice of trailing league inflow (snapshot difference), capped
    // at the credited league balance — a draw may only spend what the pool
    // actually credited to the league ledger, never miner-owed funds.
    let league_now = server.league_balance();
    let mut snap = shared.league_snapshot.lock().expect("snapshot");
    let inflow = league_now.saturating_sub(*snap);
    *snap = league_now;
    drop(snap);
    let slice = draw_budget(inflow, cfg.slice_bps);
    let budget = slice.min(league_now);
    let draw_id = format!("draw-{e}");
    let (winner, winner_addr) = winner_addr;
    let txid = if winner.is_empty() {
        "none (no eligible luckynodes)".to_string()
    } else if budget == 0 {
        "none (no league inflow)".to_string()
    } else if !cfg.exec {
        println!(
            "luckynode: draw {e} winner {winner} ({winner_addr}) budget {} (dry-run, not sent)",
            lm_pool::payout::format_coins(budget)
        );
        "dry-run".to_string()
    } else {
        let coins = lm_pool::payout::format_coins(budget);
        match rpc.call("sendtoaddress", serde_json::json!([winner_addr, coins])) {
            Ok(v) => {
                let t = v.as_str().unwrap_or("").to_string();
                println!(
                    "luckynode: draw {e} winner {winner} ({winner_addr}) paid {coins} tx {t}"
                );
                // Debit the league ledger only when the coins actually left
                // the wallet (a held draw keeps its claim until a retry pays).
                server.debit_league(budget);
                {
                    let mut h = history.lock().expect("payout history");
                    h.push_back(PayoutRecord {
                        time: now,
                        miner: "luckynode-draw".to_string(),
                        name: format!("luckynode-draw:{winner_addr}"),
                        amount: budget,
                        txid: t.clone(),
                    });
                    while h.len() > 100 {
                        h.pop_front();
                    }
                }
                t
            }
            Err(e) => {
                eprintln!("luckynode: draw {e} pay {winner_addr} {coins} failed: {e} (held)");
                format!("held: {e}")
            }
        }
    };
    if !winner.is_empty() {
        println!(
            "luckynode: draw {e} @height {height} seed {seed_h} entrants {entrants} winner {winner} budget {budget} tx {txid}"
        );
    } else {
        println!("luckynode: draw {e} @height {height}: {txid}");
    }
    {
        let mut core = shared.core.lock().expect("luckynode");
        core.recent_draws.push_back(DrawRecord {
            epoch: e,
            height,
            seed_hash: seed_h.clone(),
            winner: winner.clone(),
            winner_addr: winner_addr.clone(),
            budget,
            txid: txid.clone(),
            time: now,
            cosigns: 0,
        });
        while core.recent_draws.len() > 20 {
            core.recent_draws.pop_front();
        }
        // Post for cosigning (notarization duty): luckynodes cosign the
        // result; anyone can recompute the draw from the seed + registry.
        core.pending.push(PendingCosign {
            draw_id: draw_id.clone(),
            kind: "luckynode-draw".to_string(),
            height,
            block_hash: seed_h,
            detail: format!("epoch {e} winner {winner} budget {budget} tx {txid}"),
            cosigns: Vec::new(),
            created_at: now,
        });
        while core.pending.len() > 20 {
            core.pending.remove(0);
        }
    }
}

fn luckynode_shared(ctx: &HttpCtx) -> Option<std::sync::Arc<LuckynodeShared>> {
    ctx.luckynode.clone()
}

fn luckynode_disabled() -> String {
    serde_json::json!({"ok": false, "error": "luckynodes not enabled (start with --luckynode)"})
    .to_string()
}

/// Percent-decode a single URL path segment (std only; `+` stays a
/// literal plus — path segments never use form encoding). Invalid or
/// overlong sequences decode to U+FFFD; callers cap the length.
fn pct_decode(seg: &str) -> String {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// GET /api/luckynodes: registry snapshot + draw state.
fn luckynode_list(ctx: &HttpCtx) -> String {
    let Some(shared) = luckynode_shared(ctx) else {
        return luckynode_disabled();
    };
    let now = now_secs();
    let core = shared.core.lock().expect("luckynode");
    let nodes: Vec<serde_json::Value> = core
        .registry
        .all_nodes()
        .iter()
            .map(|n| {
                serde_json::json!({
                    "id": n.id, "address": n.address,
                    "heartbeats": n.heartbeats, "last_heartbeat": n.last_heartbeat,
                    "bond_ok": n.bond_ok,
                    "bond_utxo": n.bond_utxo.as_ref().map(|(t, v)| format!("{t}:{v}")),
                    "tenure_secs": core.registry.tenure_secs(n, now),
                    "weight_millis": lm_luckynode::tenure_weight_millis(core.registry.tenure_secs(n, now)),
                })
            })
        .collect();
    let active: Vec<String> = core
        .registry
        .active(now, shared.cfg.hb_window)
        .iter()
        .map(|n| n.id.clone())
        .collect();
    let draws: Vec<serde_json::Value> = core
        .recent_draws
        .iter()
        .map(|d| {
            serde_json::json!({
                "epoch": d.epoch, "height": d.height, "seed": d.seed_hash,
                "winner": d.winner, "budget": d.budget, "txid": d.txid, "time": d.time,
                "cosigns": d.cosigns,
            })
        })
        .collect();
    serde_json::json!({
        "ok": true, "nodes": nodes, "active": active,
        "config": {"bond": shared.cfg.bond, "epoch": shared.cfg.epoch,
                   "slice_bps": shared.cfg.slice_bps, "hb_window": shared.cfg.hb_window},
        "recent_draws": draws,
    })
    .to_string()
}

/// GET /api/luckynode/pending: draws/events awaiting cosigns.
fn luckynode_pending(ctx: &HttpCtx) -> String {
    let Some(shared) = luckynode_shared(ctx) else {
        return luckynode_disabled();
    };
    let core = shared.core.lock().expect("luckynode");
    let list: Vec<serde_json::Value> = core
        .pending
        .iter()
        .map(|p| {
            serde_json::json!({
                "draw_id": p.draw_id, "kind": p.kind, "height": p.height,
                "block_hash": p.block_hash, "detail": p.detail,
                "cosigns": p.cosigns.len(), "created_at": p.created_at,
            })
        })
        .collect();
    serde_json::json!(list).to_string()
}

/// POST /api/luckynode/register {id, address, pubkey, msg, sig, ts}: id must
/// equal the hex pubkey (32 bytes); address must be a payout address the
/// REGISTRAR controls — proven by a node-verified `verifymessage` signature
/// over the canonical `luckynode-register:v1:{id}:{address}:{ts}` (freshness
/// window like heartbeats). No proof → rejected (Sybil control).
fn luckynode_register(ctx: &HttpCtx, body: &str) -> (bool, String) {
    let Some(shared) = luckynode_shared(ctx) else {
        return (false, luckynode_disabled());
    };
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (false, serde_json::json!({"ok": false, "error": format!("bad json: {e}")}).to_string()),
    };
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
    let address = v.get("address").and_then(|x| x.as_str()).unwrap_or("");
    let pubkey = v.get("pubkey").and_then(|x| x.as_str()).unwrap_or("");
    let msg = v.get("msg").and_then(|x| x.as_str()).unwrap_or("");
    let sig = v.get("sig").and_then(|x| x.as_str()).unwrap_or("");
    let ts = v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0);
    if id.is_empty() || id != pubkey {
        return (false, serde_json::json!({"ok": false, "error": "id must equal the hex pubkey"}).to_string());
    }
    match lm_luckynode::notarize::unhex(pubkey) {
        Ok(b) if b.len() == 32 => {}
        _ => return (false, serde_json::json!({"ok": false, "error": "pubkey must be 32 hex bytes"}).to_string()),
    }
    if !lm_pool::payout::valid_payout_address(address) {
        return (false, serde_json::json!({"ok": false, "error": "address is not a payout address"}).to_string());
    }
    let now = now_secs();
    // Freshness window (anti-replay), same as heartbeats.
    if ts == 0 || now.saturating_sub(ts) > 300 || ts > now + 60 {
        return (false, serde_json::json!({"ok": false, "error": "stale timestamp"}).to_string());
    }
    // Canonical message: exact match prevents cross-context replay.
    let expect = format!("luckynode-register:v1:{id}:{address}:{ts}");
    if msg != expect || sig.is_empty() {
        return (false, serde_json::json!({"ok": false, "error": "message mismatch"}).to_string());
    }
    // Ownership proof via the node (fail closed if RPC unavailable).
    let owned = match &ctx.node {
        Some((url, user, pass)) => {
            let rpc = match RpcClient::from_url(url, user, pass) {
                Ok(r) => r,
                Err(e) => return (false, serde_json::json!({"ok": false, "error": format!("node rpc: {e}")}).to_string()),
            };
            match rpc.call("verifymessage", serde_json::json!([address, sig, msg])) {
                Ok(v) => v.as_bool().unwrap_or(false),
                Err(e) => return (false, serde_json::json!({"ok": false, "error": format!("verifymessage: {e}")}).to_string()),
            }
        }
        None => return (false, serde_json::json!({"ok": false, "error": "pool has no node connection"}).to_string()),
    };
    if !owned {
        return (false, serde_json::json!({"ok": false, "error": "payout address ownership not proven (sign the registration with the address's key)"}).to_string());
    }
    shared
        .core
        .lock()
        .expect("luckynode")
        .registry
        .register(id, address, now, true);
    // Optional designated bond UTXO (locked collateral): "txid:vout".
    // The pool verifies it is unspent every tick; spending it evicts the
    // node. Without one, the bond is a plain balance check (testnet).
    let mut bond_locked = false;
    if let Some(utxo) = v.get("bond_utxo").and_then(|x| x.as_str()) {
        if let Some((txid, vout)) = utxo.split_once(':') {
            if let Ok(vout) = vout.parse::<u32>() {
            if !txid.is_empty() && txid.len() == 64 {
                bond_locked = shared
                    .core
                    .lock()
                    .expect("luckynode")
                    .registry
                    .designate_bond(id, txid, vout);
            }
            }
        }
        if !bond_locked {
            return (false, serde_json::json!({"ok": false, "error": "bond_utxo must be txid:vout (64 hex + index)"}).to_string());
        }
    }
    (
        true,
        serde_json::json!({"ok": true, "id": id, "bond_pending": true, "bond_locked": bond_locked}).to_string(),
    )
}

/// POST /api/luckynode/heartbeat {id, timestamp, sig}: signed liveness.
fn luckynode_heartbeat(ctx: &HttpCtx, body: &str) -> (bool, String) {
    let Some(shared) = luckynode_shared(ctx) else {
        return (false, luckynode_disabled());
    };
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (false, serde_json::json!({"ok": false, "error": format!("bad json: {e}")}).to_string()),
    };
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
    let ts = v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
    let sig = v.get("sig").and_then(|x| x.as_str()).unwrap_or("");
    let now = now_secs();
    // Freshness window (anti-replay); the daemon heartbeats every ~60 s.
    if ts == 0 || now.saturating_sub(ts) > 300 || ts > now + 60 {
        return (false, serde_json::json!({"ok": false, "error": "stale timestamp"}).to_string());
    }
    let pk = match lm_luckynode::notarize::unhex(id) {
        Ok(b) if b.len() == 32 => b,
        _ => return (false, serde_json::json!({"ok": false, "error": "unknown node (register first)"}).to_string()),
    };
    let sigb = match lm_luckynode::notarize::unhex(sig) {
        Ok(b) if b.len() == 64 => b,
        _ => return (false, serde_json::json!({"ok": false, "error": "sig must be 64 hex bytes"}).to_string()),
    };
    if !lm_luckynode::notarize::verify(&pk, &lm_luckynode::notarize::heartbeat_msg(id, ts), &sigb) {
        return (false, serde_json::json!({"ok": false, "error": "bad signature"}).to_string());
    }
    let mut core = shared.core.lock().expect("luckynode");
    if core.registry.get(id).is_none() {
        return (false, serde_json::json!({"ok": false, "error": "unknown node (register first)"}).to_string());
    }
    core.registry.heartbeat(id, now, shared.cfg.reset_gap);
    (true, serde_json::json!({"ok": true}).to_string())
}

/// POST /api/luckynode/notarize {draw_id, id, sig}: cosign a draw result.
fn luckynode_notarize(ctx: &HttpCtx, body: &str) -> (bool, String) {
    let Some(shared) = luckynode_shared(ctx) else {
        return (false, luckynode_disabled());
    };
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return (false, serde_json::json!({"ok": false, "error": format!("bad json: {e}")}).to_string()),
    };
    let draw_id = v.get("draw_id").and_then(|x| x.as_str()).unwrap_or("");
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
    let sig = v.get("sig").and_then(|x| x.as_str()).unwrap_or("");
    if draw_id.is_empty() || id.is_empty() || sig.is_empty() {
        return (false, serde_json::json!({"ok": false, "error": "need draw_id, id, sig"}).to_string());
    }
    // Single lock for the whole cosign: the pending entry, the
    // registry lookup and the draw-record stamp used to take three
    // nested mutexes (pending→registry→draws) while the list path took
    // registry→draws — that ABBA wedged every HTTP thread in production.
    let mut guard = shared.core.lock().expect("luckynode");
    let LuckynodeCore { registry, pending, recent_draws } = &mut *guard;
    let Some(p) = pending.iter_mut().find(|p| p.draw_id == draw_id) else {
        return (false, serde_json::json!({"ok": false, "error": "unknown draw"}).to_string());
    };
    let Some(node) = registry.get(id) else {
        return (false, serde_json::json!({"ok": false, "error": "unknown node"}).to_string());
    };
    let pk = match lm_luckynode::notarize::unhex(&node.id) {
        Ok(b) if b.len() == 32 => b,
        _ => return (false, serde_json::json!({"ok": false, "error": "bad node key"}).to_string()),
    };
    let sigb = match lm_luckynode::notarize::unhex(sig) {
        Ok(b) if b.len() == 64 => b,
        _ => return (false, serde_json::json!({"ok": false, "error": "sig must be 64 hex bytes"}).to_string()),
    };
    let msg = lm_luckynode::notarize::cosign_msg(draw_id, &p.block_hash);
    if !lm_luckynode::notarize::verify(&pk, &msg, &sigb) {
        return (false, serde_json::json!({"ok": false, "error": "bad signature"}).to_string());
    }
    if !p.cosigns.iter().any(|(i, _)| i == id) {
        p.cosigns.push((id.to_string(), sig.to_string()));
    }
    // Stamp the draw record: the notarization count must outlive the
    // pending list (which is capped) and show up in the draw table.
    let want = p.draw_id.clone();
    let n = p.cosigns.len() as u32;
    if let Some(d) = recent_draws
        .iter_mut()
        .find(|d| format!("draw-{}", d.epoch) == want)
    {
        d.cosigns = n;
    }
    let total = p.cosigns.len();
    (
        true,
        serde_json::json!({"ok": true, "cosigns": total}).to_string(),
    )
}

/// Background luckynode loop: bond checks + epoch draws.
fn luckynode_loop<P: TemplateProvider + 'static>(
    server: Arc<RunningServer<P>>,
    node: (String, String, String),
    shared: Arc<LuckynodeShared>,
    history: Arc<Mutex<VecDeque<PayoutRecord>>>,
) {
    let rpc = RpcClient::new(&node.0, &node.1, &node.2);
    let interval = shared.cfg.interval.max(10);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(interval));
        luckynode_tick(&server, &rpc, &shared, &history);
    }
}

const USAGE: &str = "usage:
  pool-server [--bind HOST:PORT] [--reward R] [--fees F] [--pool-password PASS] [--pool-target (HEX64|max)] [--net-target (HEX64|max)]
  pool-server --node-rpc HOST:PORT [--rpc-user U] [--rpc-pass P] [--pool-password PASS] [--bind HOST:PORT]
  [--no-claim-jackpots] [--no-claim-pot] [--no-claim-faucet] [--no-claim-dev]   (all claims ON by default; these opt out)
  both forms accept [--http HOST:PORT] to serve the live dashboard (/ and /api/stats)
  node mode accepts [--payout-threshold V] [--payout-interval SECS] [--payout-exec]
  [--pot-divisor N] [--jackpot-divisor N]   (regtest-only overrides; otherwise read from the node)
  both modes accept [--pool-fee-pp N] (operator cut, per-myriad, default 0)
  and [--dev-fee-addr ADDR --dev-fee-pp N] (dev slice from the operator cut,
  per-myriad; docs/04 section 4.2)
  both modes accept [--workers-file PATH]: one `worker:token` per line;
  when set, only listed workers with a matching token may authorize
  (takes precedence over --pool-password)
  both modes accept [--v2-bind HOST:PORT] to serve Stratum V2 (Noise NX +
  Lucky jobs) alongside V1; [--v2-authority-key HEX64] pins a stable pool
  authority secret (default: an ephemeral one, printed at startup —
  miners pinning it must re-pin after every restart);
  [--v2-cert-validity SECS] sets the responder certificate lifetime
  (default 2592000 = 30 days); miners pass the pool's §4.7 URL key and
  their pool password/token as the V2 device id
  both modes accept [--tls-cert PATH --tls-key PATH] to serve TLS (stratum
  over TLS on the bound port; miners connect with --tls)
  both modes accept [--state-file PATH]: owed/paid ledger, worker names and
  block history persist across restarts (atomic save every 10 s); delete the
  file to intentionally clear
  node mode accepts [--luckynode] to run Luckynode draws v1):
  [--luckynode-bond V] [--luckynode-epoch N] [--luckynode-slice-bps BPS]
  [--luckynode-hb-window SECS] [--luckynode-exec] [--luckynode-interval SECS]
  (bond default 400 LKYL on all chains; epoch default 2880 mainnet, 3 on
  test+regtest; slice default 1000 = 10% of trailing league inflow; draws pay
  the winner via sendtoaddress when --luckynode-exec, else log only)

defaults: bind=127.0.0.1:3333; mock mode: reward=1000000, fees=0, both targets=max (instant blocks);
node mode: jobs from the node's getblocktemplate; claims ON by default (exact
advertised jackpotbonus / pot splash / faucet+dev drips — every find lands).
seed funds (node mode, internal/20): faucet tops up the faucet wallet
toward 1,000 tLKYL whenever a template advertises a nonzero faucetdrip (the
pool reads the wallet balance via listunspent — no addressindex needed);
dev is claimed on dev-epoch heights (every 100th block). Both splice exact
pinned-script outputs (node is source of truth); unclaimed amounts stay in
their reserves. Empty node pins (mainnet until the ceremony) disable both
legs. Pass --no-claim-faucet / --no-claim-dev to opt out.
payouts (node mode,: balances at/above --payout-threshold
(base units) are sent from the node wallet every --payout-interval seconds
(default 60); WITHOUT --payout-exec the loop only logs what it would send.
Worker names double as payout addresses (valid R-addresses only).";

fn parse_u64(s: &str, flag: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .map_err(|_| format!("--{flag} expects a non-negative integer, got '{s}'"))
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

/// Start the V2 listener when `--v2-bind` was given (both modes). A stable
/// `--v2-authority-key` keeps miner pins valid across restarts; without one
/// an ephemeral authority is generated and its URL key printed (miners must
/// re-pin after every restart). Flag errors exit the process, the same
/// posture as the other bind paths.
fn maybe_spawn_v2<P: TemplateProvider + 'static>(
    server: &mut RunningServer<P>,
    v2_bind: &Option<String>,
    v2_authority_key: &Option<String>,
    v2_cert_validity: u64,
) {
    use lm_stratum::v2_session::{authority_from_secret, authority_url_key, generate_authority};
    let Some(bind) = v2_bind else {
        return;
    };
    let authority = match v2_authority_key {
        Some(hex) => {
            let secret = parse_target(hex, "v2-authority-key").unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(2);
            });
            authority_from_secret(&secret).unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(1);
            })
        }
        None => {
            let authority = generate_authority();
            println!("v2: no --v2-authority-key: ephemeral authority (pins die on restart)");
            authority
        }
    };
    match server.spawn_v2_listener(
        bind,
        authority,
        std::time::Duration::from_secs(v2_cert_validity),
    ) {
        Ok(addr) => println!(
            "v2: Stratum V2 (Noise NX + Lucky jobs) on {addr}; authority URL key: {}",
            authority_url_key(&authority.0)
        ),
        Err(e) => {
            eprintln!("error: cannot bind V2 {bind}: {e}");
            std::process::exit(1);
        }
    }
}

fn main() -> std::process::ExitCode {
    // SIGTERM = clean stop: the handler only flips the atomic; the
    // term_watcher thread (spawned per server) does the final state save.
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
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
    // Precedence: --rpc-pass / --rpc-pass-file, then the environment
    // (LUCKY_POOL_RPC_PASS), then the public testnet default.
    let mut rpc_pass = std::env::var("LUCKY_POOL_RPC_PASS").unwrap_or_else(|_| "p".to_string());
    // Consensus claims are ON by default (industry norm: bitcoind/ckpool/SV2
    // bake consensus, flags are ops-only). The node advertises the exact
    // amounts; `--no-claim-*` opts out. `--claim-*` are deprecated aliases.
    let mut claim_jackpots = true;
    let mut claim_pot = true;
    let mut claim_faucet = true;
    let mut claim_dev = true;
    let mut http: Option<String> = None;
    let mut claim_pot_tier = 10u64; // obsolete no-op (kept for old scripts)
    let mut pot_divisor: Option<u64> = None; // None = read from the node
    let mut jackpot_divisor: Option<u64> = None; // None = read from the node
    let mut claim_pot_pity = false; // obsolete no-op (kept for old scripts)
    let mut payout_threshold = 0u64;
    // Chain label for user-facing strings. None = ask the node
    // (`getblockchaininfo.chain`); --network overrides.
    let mut network: Option<String> = None;
    let mut payout_interval = 60u64;
    let mut payout_exec = false;
    let mut state_file: Option<PathBuf> = None;
    let mut pool_password: Option<String> = None;
    let mut workers_file: Option<String> = None;
    let mut pool_fee_pp: u16 = 0;
    let mut dev_fee_addr: Option<String> = None;
    let mut dev_fee_pp: u16 = 0;
    let mut tls_cert: Option<String> = None;
    let mut tls_key: Option<String> = None;
    let mut v2_bind: Option<String> = None;
    let mut v2_authority_key: Option<String> = None;
    let mut v2_cert_validity = 2592000u64; // 30 days
    let mut luckynode = false;
    // None = default (bond 400 LKYL everywhere; epoch 2880 mainnet, 3 on
    // test+regtest, resolved after the node handshake). Explicit flags win.
    let mut luckynode_bond: Option<u64> = None;
    let mut luckynode_epoch: Option<u64> = None;
    let mut luckynode_slice_bps = DEFAULT_LUCKYNODE_SLICE_BPS;
    let mut luckynode_hb_window = 1800u64;
    let mut luckynode_reset_gap = 86400u64;
    let mut luckynode_exec = false;
    let mut luckynode_interval = 60u64;
    let mut i = 0;
    while i < args.len() {
        let next = |flag: &str| -> Result<String, String> {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("--{flag} needs a value"))
        };
        match args[i].as_str() {
            "--bind" => {
                bind = next("bind").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--reward" => {
                reward = parse_u64(
                    &next("reward").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "reward",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--fees" => {
                fees = parse_u64(
                    &next("fees").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "fees",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--pool-target" => {
                pool_target = parse_target(
                    &next("pool-target").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "pool-target",
                )
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
                // Deprecated alias: claims are on by default since the
                // pre-committed-draw era. Accepted so old scripts keep
                // running; use --no-claim-jackpots to opt out.
                claim_jackpots = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--no-claim-jackpots" => {
                claim_jackpots = false;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--claim-pot" => {
                // Deprecated alias, see --claim-jackpots.
                claim_pot = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--no-claim-pot" => {
                claim_pot = false;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--claim-faucet" => {
                // Deprecated alias, see --claim-jackpots.
                claim_faucet = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--no-claim-faucet" => {
                claim_faucet = false;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--claim-dev" => {
                // Deprecated alias, see --claim-jackpots.
                claim_dev = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--no-claim-dev" => {
                claim_dev = false;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--jackpot-divisor" => {
                jackpot_divisor = Some(parse_u64(
                    &next("jackpot-divisor").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "jackpot-divisor",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
                if jackpot_divisor == Some(0) {
                    eprintln!("error: --jackpot-divisor must be >= 1");
                    return std::process::ExitCode::from(2);
                }
            }
            "--claim-pot-pity" => {
                claim_pot_pity = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--luckynode" => {
                luckynode = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--luckynode-exec" => {
                luckynode_exec = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--luckynode-bond" => {
                let v = parse_u64(
                    &next("luckynode-bond").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "luckynode-bond",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
                // The flag is BASE UNITS. A sub-coin value is almost always
                // coins-by-mistake (400 = 400 sats) and silently neuters the
                // bond gate, so refuse it: 400 LKYL = 40000000000.
                if v != 0 && v < 100_000_000 {
                    eprintln!(
                        "error: --luckynode-bond is in base units; {} is less than one coin.\n\
                         Use 40000000000 for 400 LKYL (or 0 for no bond).",
                        v
                    );
                    std::process::exit(2);
                }
                luckynode_bond = Some(v);
            }
            "--network" => {
                let v = next("network").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
                if v != "testnet" && v != "mainnet" {
                    eprintln!("error: --network must be testnet|mainnet, got '{v}'");
                    std::process::exit(2);
                }
                network = Some(v);
            }
            "--luckynode-epoch" => {
                let v = parse_u64(
                    &next("luckynode-epoch").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "luckynode-epoch",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
                if v == 0 {
                    eprintln!("error: --luckynode-epoch must be >= 1");
                    return std::process::ExitCode::from(2);
                }
                luckynode_epoch = Some(v);
            }
            "--luckynode-slice-bps" => {
                luckynode_slice_bps = parse_u64(
                    &next("luckynode-slice-bps").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "luckynode-slice-bps",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
            }
            "--luckynode-hb-window" => {
                luckynode_hb_window = parse_u64(
                    &next("luckynode-hb-window").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "luckynode-hb-window",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
            }
            "--luckynode-interval" => {
                luckynode_interval = parse_u64(
                    &next("luckynode-interval").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "luckynode-interval",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
            }
            "--luckynode-reset-gap" => {
                luckynode_reset_gap = parse_u64(
                    &next("luckynode-reset-gap").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "luckynode-reset-gap",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
            }
            "--state-file" => {
                state_file = Some(PathBuf::from(next("state-file").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })));
            }
            "--claim-pot-tier" => {
                let v = next("claim-pot-tier").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
                claim_pot_tier = match v.as_str() {
                    "10" => 10,
                    "25" => 25,
                    "50" => 50,
                    other => {
                        eprintln!("error: --claim-pot-tier expects 10|25|50, got '{other}'");
                        return std::process::ExitCode::from(2);
                    }
                };
            }
            "--http" => {
                http = Some(next("http").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }))
            }
            "--pot-divisor" => {
                pot_divisor = Some(parse_u64(
                    &next("pot-divisor").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "pot-divisor",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
                if pot_divisor == Some(0) {
                    eprintln!("error: --pot-divisor must be >= 1");
                    return std::process::ExitCode::from(2);
                }
            }
            "--payout-threshold" => {
                payout_threshold = parse_u64(
                    &next("payout-threshold").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "payout-threshold",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--payout-interval" => {
                payout_interval = parse_u64(
                    &next("payout-interval").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "payout-interval",
                )
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
            "--rpc-pass-file" => {
                // Secrets off argv: a file (0600) or the environment beats
                // `ps`-visible command lines.
                let path = next("rpc-pass-file").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
                rpc_pass = std::fs::read_to_string(path.trim())
                    .unwrap_or_else(|e| {
                        eprintln!("error: cannot read rpc-pass-file: {e}");
                        std::process::exit(2);
                    })
                    .trim()
                    .to_string();
            }
            "--pool-password-file" => {
                let path = next("pool-password-file").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
                pool_password = Some(
                    std::fs::read_to_string(path.trim())
                        .unwrap_or_else(|e| {
                            eprintln!("error: cannot read pool-password-file: {e}");
                            std::process::exit(2);
                        })
                        .trim()
                        .to_string(),
                );
            }
            "--pool-password" => {
                pool_password = Some(next("pool-password").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
            }
            "--workers-file" => {
                workers_file = Some(next("workers-file").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
            }
            "--pool-fee-pp" => {
                pool_fee_pp = next("pool-fee-pp")
                    .and_then(|v| {
                        v.parse::<u16>()
                            .map_err(|_| "--pool-fee-pp expects 0-10000".to_string())
                    })
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    });
            }
            "--dev-fee-addr" => {
                dev_fee_addr = Some(next("dev-fee-addr").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
            }
            "--dev-fee-pp" => {
                dev_fee_pp = next("dev-fee-pp")
                    .and_then(|v| {
                        v.parse::<u16>()
                            .map_err(|_| "--dev-fee-pp expects 0-10000".to_string())
                    })
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    });
            }
            "--tls-cert" => {
                tls_cert = Some(next("tls-cert").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
            }
            "--tls-key" => {
                tls_key = Some(next("tls-key").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
            }
            "--v2-bind" => {
                v2_bind = Some(next("v2-bind").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
            }
            "--v2-authority-key" => {
                v2_authority_key = Some(next("v2-authority-key").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
            }
            "--v2-cert-validity" => {
                v2_cert_validity = parse_u64(
                    &next("v2-cert-validity").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "v2-cert-validity",
                )
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
                if v2_cert_validity == 0 {
                    eprintln!("error: --v2-cert-validity must be >= 1");
                    return std::process::ExitCode::from(2);
                }
            }
            "--net-target" => {
                net_target = parse_target(
                    &next("net-target").unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    }),
                    "net-target",
                )
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
        // Auto-discovery (industry norm: consensus baked, node is source of
        // truth). Divisors default to the node's effective values; explicit
        // flags are regtest-only overrides. Network defaults to the node's
        // chain; --network overrides for display only.
        let probe = RpcClient::new(&node, &rpc_user, &rpc_pass);
        let jackpot_info = probe.call("getjackpotinfo", serde_json::json!([])).ok();
        let resolved_pot_divisor = match pot_divisor {
            Some(v) => {
                if let Some(node_div) = jackpot_info
                    .as_ref()
                    .and_then(|i| i.get("potdivisor"))
                    .and_then(|v| v.as_u64())
                {
                    if node_div != v {
                        eprintln!(
                            "warning: --pot-divisor {v} != node effective divisor {node_div}; \
                             claims will disagree — remove the flag except on regtest"
                        );
                    }
                }
                v
            }
            None => jackpot_info
                .as_ref()
                .and_then(|i| i.get("potdivisor"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1000),
        };
        let resolved_jackpot_divisor = match jackpot_divisor {
            Some(v) => {
                if let Some(node_div) = jackpot_info
                    .as_ref()
                    .and_then(|i| i.get("jackpotdivisor"))
                    .and_then(|v| v.as_u64())
                {
                    if node_div != v {
                        eprintln!(
                            "warning: --jackpot-divisor {v} != node effective {node_div}; \
                             remove the flag except on regtest"
                        );
                    }
                }
                v
            }
            None => jackpot_info
                .as_ref()
                .and_then(|i| i.get("jackpotdivisor"))
                .and_then(|v| v.as_u64())
                .unwrap_or(100),
        };
        if pot_divisor.is_none() || jackpot_divisor.is_none() {
            println!(
                "draw divisors from node: jackpot {} / pot {}",
                resolved_jackpot_divisor, resolved_pot_divisor
            );
        }
        let node_chain = probe
            .call("getblockchaininfo", serde_json::json!([]))
            .ok()
            .and_then(|i| i.get("chain").and_then(|v| v.as_str()).map(str::to_string));
        let network_name = match network {
            Some(v) => v,
            None => match node_chain.as_deref() {
                Some("main") => "mainnet".to_string(),
                _ => "testnet".to_string(),
            },
        };
        // Luckynode bond/epoch unless explicitly flagged (bond 400 LKYL on
        // all chains — the live testnet runs mainnet-form bond since
        // 2026-10-20; epoch 2880 ≈48h mainnet, 3 on test+regtest).
        let is_main = network_name == "mainnet";
        let resolved_bond = luckynode_bond.unwrap_or(40_000_000_000u64);
        let resolved_epoch = luckynode_epoch.unwrap_or(if is_main { 2880 } else { 3 });
        let provider = match NodeProvider::connect_with_claims(
            &node,
            &rpc_user,
            &rpc_pass,
            claim_jackpots,
            claim_pot,
        ) {
            Ok(p) => p
                .with_pot_divisor(resolved_pot_divisor)
                .with_jackpot_divisor(resolved_jackpot_divisor)
                .with_claim_faucet(claim_faucet)
                .with_claim_dev(claim_dev),
            Err(e) => {
                eprintln!("error: cannot reach node RPC {node}: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        // Draws are pre-committed per height (doc 06 rev 1.2): claim
        // templates are built for jackpot/pot heights and EVERY find is
        // submittable. --claim-pot-tier/--claim-pot-pity are obsolete
        // (kept accepted for older scripts; they no longer change claims).
        let worker_tokens = match &workers_file {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(content) => match lm_stratum::server::parse_workers_file(&content) {
                    Ok(m) => {
                        println!("auth: {} worker token(s) from {path}", m.len());
                        Some(m)
                    }
                    Err(e) => {
                        eprintln!("error: {path}: {e}");
                        return std::process::ExitCode::from(1);
                    }
                },
                Err(e) => {
                    eprintln!("error: cannot read --workers-file {path}: {e}");
                    return std::process::ExitCode::from(1);
                }
            },
            None => None,
        };
        let tls = match (&tls_cert, &tls_key) {
            (Some(cert), Some(key)) => match lm_stratum::tls::server_config(
                std::path::Path::new(cert),
                std::path::Path::new(key),
            ) {
                Ok(cfg) => {
                    println!("tls: enabled (cert {cert})");
                    Some(cfg)
                }
                Err(e) => {
                    eprintln!("error: cannot load TLS cert/key: {e}");
                    return std::process::ExitCode::from(1);
                }
            },
            (None, None) => None,
            _ => {
                eprintln!("error: --tls-cert and --tls-key must be given together");
                return std::process::ExitCode::from(2);
            }
        };
        let mut server = match spawn_server(
            provider,
            &addr.to_string(),
            pool_password.clone(),
            worker_tokens,
            tls.clone(),
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot bind {addr}: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        if claim_pot_tier != 10 || claim_pot_pity {
            println!("note: --claim-pot-tier/--claim-pot-pity are obsolete (draws are pre-committed; tiers and pity are automatic)");
        }
        let pot_label = match claim_pot {
            false => "",
            true => ", claiming pot events (exact advertised splash)",
        };
        let seed_label = match (claim_faucet, claim_dev) {
            (true, true) => ", claiming faucet+dev seeds",
            (true, false) => ", claiming faucet seed",
            (false, true) => ", claiming dev seed",
            (false, false) => "",
        };
        let claims = match (claim_jackpots, claim_pot) {
            (true, true) => ", claiming jackpots",
            (true, false) => ", claiming jackpots",
            (false, true) => "",
            (false, false) => "",
        };
        let claims = format!("{claims}{pot_label}{seed_label}");
        let mode = match (claim_jackpots, claim_pot) {
            (true, true) => "claiming jackpots+pot",
            (true, false) => "claiming jackpots",
            (false, true) => "claiming pot",
            (false, false) => "normal mining",
        };
        println!(
            "pool on {} (node provider {}{claims}); Ctrl-C to stop",
            server.addr(),
            node
        );
        // Luckynodes v1, node mode only — needs node RPC).
        // Created before the state load so persisted draws/cosigns restore
        // into it.
        let luckynode_shared: Option<Arc<LuckynodeShared>> = if luckynode {
            Some(Arc::new(LuckynodeShared {
                core: Mutex::new(LuckynodeCore {
                    registry: lm_luckynode::Registry::new(),
                    pending: Vec::new(),
                    recent_draws: VecDeque::new(),
                }),
                last_epoch: Mutex::new(None),
                league_snapshot: Mutex::new(0),
                init_done: Mutex::new(false),
                cfg: LuckynodeCfg {
                    bond: resolved_bond,
                    epoch: resolved_epoch,
                    slice_bps: luckynode_slice_bps,
                    hb_window: luckynode_hb_window,
                    reset_gap: luckynode_reset_gap,
                    exec: luckynode_exec,
                    interval: luckynode_interval,
                },
            }))
        } else {
            None
        };
        if luckynode {
            println!(
                "luckynode draws on (bond {} epoch {} slice {}bps{})",
                resolved_bond,
                resolved_epoch,
                luckynode_slice_bps,
                if luckynode_exec {
                    ", EXEC: real sendtoaddress"
                } else {
                    ", dry-run: log only"
                }
            );
        }
        if let Some(path) = &state_file {
            if let Err(e) = load_state(&server, luckynode_shared.as_ref(), path) {
                eprintln!("pool: {e}");
                std::process::exit(1);
            }
        }
        if pool_fee_pp > 0 {
            server.set_pool_fee(pool_fee_pp);
            println!("pool fee: {pool_fee_pp} per-myriad");
        }
        if let Some(addr) = &dev_fee_addr {
            if dev_fee_pp == 0 {
                eprintln!("warning: --dev-fee-addr set but --dev-fee-pp is 0; dev accrues nothing");
            } else if pool_fee_pp == 0 {
                eprintln!("warning: dev fee configured but pool fee is 0; dev accrues nothing until --pool-fee-pp is set (the slice comes from the operator cut)");
            }
            server.set_dev_fee(addr, dev_fee_pp);
            println!("dev fee: {dev_fee_pp} per-myriad of each block to {addr}");
        }
        maybe_spawn_v2(
            &mut server,
            &v2_bind,
            &v2_authority_key,
            v2_cert_validity,
        );
        let server = Arc::new(server);
        if let Some(path) = &state_file {
            std::thread::spawn({
                let srv = Arc::clone(&server);
                let ln = luckynode_shared.clone();
                let p = path.clone();
                move || state_saver(srv, ln, p)
            });
        }
        // Always watch for SIGTERM (the handler is installed at startup).
        std::thread::spawn({
            let srv = Arc::clone(&server);
            let ln = luckynode_shared.clone();
            let p = state_file.clone();
            move || term_watcher(srv, ln, p)
        });
        let history = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(a) = http_addr {
            let srv = Arc::clone(&server);
            let ctx = HttpCtx {
                node: Some((node.clone(), rpc_user.clone(), rpc_pass.clone())),
                mode: mode.to_string(),
                exec: payout_exec,
                threshold: payout_threshold,
                history: Arc::clone(&history),
                luckynode: luckynode_shared.clone(),
                network: network_name.clone(),
            };
            std::thread::spawn(move || http_serve(a, srv, ctx));
        }
        // Payout loop: dry-run unless --payout-exec.
        // Spawned whenever payouts are armed — a 0 threshold means "pay
        // everything", not "disabled" (the old `threshold > 0` gate
        // silently skipped the loop for --payout-threshold 0).
        if payout_threshold > 0 || payout_exec {
            let srv = Arc::clone(&server);
            let cfg = PayoutCfg {
                threshold: payout_threshold,
                interval_secs: payout_interval,
                exec: payout_exec,
            };
            let node_cfg = (node.clone(), rpc_user.clone(), rpc_pass.clone());
            let hist = Arc::clone(&history);
            println!(
                "payouts: every {}s over {} base units ({})",
                payout_interval,
                payout_threshold,
                if payout_exec {
                    "EXEC: real sendtoaddress"
                } else {
                    "dry-run: log only"
                }
            );
            let payout_state =
                state_file.as_ref().map(|p| (p.clone(), luckynode_shared.clone()));
            std::thread::spawn(move || payout_loop(srv, node_cfg, cfg, hist, payout_state));
        }
        // Luckynode draw loop (own thread; needs node RPC, so node mode).
        if let Some(shared) = luckynode_shared {
            let srv = Arc::clone(&server);
            let node_cfg = (node.clone(), rpc_user.clone(), rpc_pass.clone());
            let hist = Arc::clone(&history);
            std::thread::spawn(move || luckynode_loop(srv, node_cfg, shared, hist));
        }
    } else {
        let tls = match (&tls_cert, &tls_key) {
            (Some(cert), Some(key)) => match lm_stratum::tls::server_config(
                std::path::Path::new(cert),
                std::path::Path::new(key),
            ) {
                Ok(cfg) => Some(cfg),
                Err(e) => {
                    eprintln!("error: cannot load TLS cert/key: {e}");
                    return std::process::ExitCode::from(1);
                }
            },
            (None, None) => None,
            _ => {
                eprintln!("error: --tls-cert and --tls-key must be given together");
                return std::process::ExitCode::from(2);
            }
        };
        let worker_tokens = match &workers_file {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(content) => match lm_stratum::server::parse_workers_file(&content) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        eprintln!("error: {path}: {e}");
                        return std::process::ExitCode::from(1);
                    }
                },
                Err(e) => {
                    eprintln!("error: cannot read --workers-file {path}: {e}");
                    return std::process::ExitCode::from(1);
                }
            },
            None => None,
        };
        let mut server = match spawn_server(
            MockProvider::new(pool_target, net_target, reward, fees),
            &addr.to_string(),
            pool_password.clone(),
            worker_tokens,
            tls,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot bind {addr}: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        println!(
            "pool on {} (reward={reward} fees={fees}); Ctrl-C to stop",
            server.addr()
        );
        if payout_threshold > 0 || payout_exec {
            eprintln!("note: payouts need node mode (--node-rpc); flags ignored on the mock chain");
        }
        if let Some(path) = &state_file {
            if let Err(e) = load_state(&server, None, path) {
                eprintln!("pool: {e}");
                std::process::exit(1);
            }
        }
        if pool_fee_pp > 0 {
            server.set_pool_fee(pool_fee_pp);
        }
        if let Some(addr) = &dev_fee_addr {
            server.set_dev_fee(addr, dev_fee_pp);
        }
        maybe_spawn_v2(
            &mut server,
            &v2_bind,
            &v2_authority_key,
            v2_cert_validity,
        );
        let server = Arc::new(server);
        if let Some(path) = &state_file {
            std::thread::spawn({
                let srv = Arc::clone(&server);
                let p = path.clone();
                move || state_saver(srv, None, p)
            });
        }
        // Always watch for SIGTERM (the handler is installed at startup).
        std::thread::spawn({
            let srv = Arc::clone(&server);
            let p = state_file.clone();
            move || term_watcher(srv, None, p)
        });
        let history = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(a) = http_addr {
            let srv = Arc::clone(&server);
            let ctx = HttpCtx {
                node: None,
                mode: "mock".to_string(),
                exec: false,
                threshold: 0,
                history: Arc::clone(&history),
                luckynode: None,
                network: network.clone().unwrap_or_else(|| "testnet".to_string()),
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

    #[test]
    fn luckynode_routes_share_one_lock_without_deadlock() {
        // Regression: the registry/pending/draws triple mutex deadlocked
        // production (all HTTP threads parked, every route 503). They now
        // live behind a single lock, so no lock order exists to invert —
        // this drives heartbeat + notarize + list + pending through the
        // real handlers, then hammers list/notarize from threads with a
        // bounded join (a re-split would hang CI instead of passing).
        use lm_luckynode::notarize;
        let secret = [7u8; 32];
        // Pinned ed25519 pubkey for the fixed test secret (libsodium
        // crypto_sign_seed_keypair; signatures are deterministic, so all
        // vectors below are stable).
        let id = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c".to_string();
        let shared = Arc::new(LuckynodeShared {
            core: Mutex::new(LuckynodeCore {
                registry: lm_luckynode::Registry::new(),
                pending: vec![PendingCosign {
                    draw_id: "draw-7".to_string(),
                    kind: "luckynode-draw".to_string(),
                    height: 21,
                    block_hash: "00".repeat(32),
                    detail: "epoch 7".to_string(),
                    cosigns: Vec::new(),
                    created_at: 1,
                }],
                recent_draws: VecDeque::from([DrawRecord {
                    epoch: 7,
                    height: 21,
                    seed_hash: "00".repeat(32),
                    winner: id.clone(),
                    winner_addr: "mx5tDPWsWxhC7EVkJXGCR14dUoMdps8fLy".to_string(),
                    budget: 100,
                    txid: "held: empty".to_string(),
                    time: 1,
                    cosigns: 0,
                }]),
            }),
            last_epoch: Mutex::new(None),
            league_snapshot: Mutex::new(0),
            init_done: Mutex::new(false),
            cfg: LuckynodeCfg {
                bond: 0,
                epoch: 3,
                slice_bps: 1000,
                hb_window: 3600,
                reset_gap: 3600,
                exec: false,
                interval: 60,
            },
        });
        {
            let mut guard = shared.core.lock().expect("luckynode");
            guard.registry.register(&id, "mx5tDPWsWxhC7EVkJXGCR14dUoMdps8fLy", 1000, true);
        }
        let ctx = HttpCtx {
            node: None,
            mode: "mock".to_string(),
            exec: false,
            threshold: 0,
            history: Arc::new(Mutex::new(VecDeque::new())),
            luckynode: Some(Arc::clone(&shared)),
            network: "test".to_string(),
        };
        let ts = now_secs();
        let hb_sig = notarize::hex(
            &notarize::sign(&secret, &notarize::heartbeat_msg(&id, ts)).expect("sign"),
        );
        let (ok, _) = luckynode_heartbeat(
            &ctx,
            &serde_json::json!({"id": id, "timestamp": ts, "sig": hb_sig}).to_string(),
        );
        assert!(ok, "heartbeat must accept a valid signature");
        let co_sig = notarize::hex(
            &notarize::sign(&secret, &notarize::cosign_msg("draw-7", &"00".repeat(32)))
                .expect("sign"),
        );
        let (ok, body) = luckynode_notarize(
            &ctx,
            &serde_json::json!({"draw_id": "draw-7", "id": id, "sig": co_sig}).to_string(),
        );
        assert!(ok, "notarize must accept a valid cosign: {body}");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["cosigns"], 1);
        // Duplicate cosign is idempotent, not double-counted.
        let (ok, _) = luckynode_notarize(
            &ctx,
            &serde_json::json!({"draw_id": "draw-7", "id": id, "sig": co_sig}).to_string(),
        );
        assert!(ok);
        let list: serde_json::Value =
            serde_json::from_str(&luckynode_list(&ctx)).expect("list json");
        assert_eq!(list["nodes"].as_array().map(|n| n.len()), Some(1));
        assert_eq!(list["recent_draws"][0]["cosigns"], 1, "stamp must land on the draw");
        let pend: serde_json::Value =
            serde_json::from_str(&luckynode_pending(&ctx)).expect("pending json");
        assert_eq!(pend.as_array().map(|p| p.len()), Some(1));
        // Concurrent hammering across the same routes (bounded join: a
        // reintroduced lock order would hang, and the timeout fails loud
        // instead of wedging CI forever).
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..8 {
            let tx = tx.clone();
            let ctx2 = HttpCtx {
                node: None,
                mode: "mock".to_string(),
                exec: false,
                threshold: 0,
                history: Arc::new(Mutex::new(VecDeque::new())),
                luckynode: Some(Arc::clone(&shared)),
                network: "test".to_string(),
            };
            let (id2, sig2) = (id.clone(), co_sig.clone());
            std::thread::spawn(move || {
                for _ in 0..50 {
                    let _ = luckynode_list(&ctx2);
                    let _ = luckynode_pending(&ctx2);
                    let _ = luckynode_notarize(
                        &ctx2,
                        &serde_json::json!({"draw_id": "draw-7", "id": id2, "sig": sig2})
                            .to_string(),
                    );
                }
                let _ = tx.send(());
            });
        }
        drop(tx);
        for _ in 0..8 {
            rx.recv_timeout(std::time::Duration::from_secs(30)).expect("threads must finish: lock deadlock?");
        }
    }

    #[test]
    fn draw_budget_gates() {
        // Pin the 10% default: the draw is the only
        // luckynode income; a silent revert to 5% must fail CI.
        assert_eq!(DEFAULT_LUCKYNODE_SLICE_BPS, 1000);
        assert_eq!(draw_budget(1_000_000, 1000), 100_000); // 10%
        assert_eq!(draw_budget(1_000_000, 500), 50_000); // old 5%, for contrast
        assert_eq!(draw_budget(922_000_000, 1000), 92_200_000); // spec scale
        // floor, never round up (a draw may not spend past the slice)
        assert_eq!(draw_budget(999, 1000), 99);
        assert_eq!(draw_budget(0, 1000), 0);
        // no overflow at u64-adjacent inflows
        assert_eq!(
            draw_budget(u64::MAX, 1000),
            (u64::MAX as u128 * 1000 / 10_000) as u64
        );
    }

    #[test]
    fn zero_global_threshold_makes_every_nonzero_balance_due() {
        use lm_stratum::server::PayoutSettings;
        // --payout-threshold 0 means "pay everything": every non-empty
        // balance is due (empty ones are skipped so they can't stall the
        // budget loop), with or without per-miner overrides.
        let settings = PayoutSettings::new();
        let due = due_miners(vec![("m1".to_string(), 5), ("m2".to_string(), 0), ("m3".to_string(), 999)], &settings, 0);
        assert_eq!(due, vec![("m1".to_string(), 5), ("m3".to_string(), 999)]);
    }

    #[test]
    fn due_selection_honours_per_miner_thresholds() {
        use lm_stratum::server::{MinerSettings, PayoutSettings};
        let mut settings = PayoutSettings::new();
        settings.insert(
            "m2".to_string(),
            MinerSettings {
                address: None,
                threshold: Some(1_000),
            },
        );
        let bals = vec![("m1".to_string(), 500u64), ("m2".to_string(), 500), ("m3".to_string(), 500)];
        // Global 100: 1 and 3 are due; 2's own threshold (1000) holds it.
        assert_eq!(
            due_miners(bals.clone(), &settings, 100),
            vec![("m1".to_string(), 500), ("m3".to_string(), 500)]
        );
        // Global 600: nobody reaches their effective threshold.
        assert!(due_miners(bals, &settings, 600).is_empty());
        // A per-miner threshold below the global one pays earlier.
        settings.insert(
            "m2".to_string(),
            MinerSettings {
                address: None,
                threshold: Some(50),
            },
        );
        assert_eq!(
            due_miners(vec![("m2".to_string(), 60)], &settings, 100),
            vec![("m2".to_string(), 60)]
        );
    }

    #[test]
    fn payout_destination_prefers_the_override() {
        use lm_stratum::server::{MinerSettings, PayoutSettings};
        let mut settings = PayoutSettings::new();
        settings.insert(
            "worker-name".to_string(),
            MinerSettings {
                address: Some("override-addr".to_string()),
                threshold: None,
            },
        );
        settings.insert(
            "worker-2".to_string(),
            MinerSettings {
                address: None,
                threshold: None,
            },
        );
        // The worker name is the payout identity: override wins when set,
        // otherwise the name itself is the destination.
        assert_eq!(payout_destination("worker-name", &settings), "override-addr");
        assert_eq!(payout_destination("worker-2", &settings), "worker-2");
        assert_eq!(payout_destination("unregistered", &settings), "unregistered");
    }

    #[test]
    fn pct_decode_handles_paths_without_panicking() {
        assert_eq!(pct_decode("plain-miner"), "plain-miner");
        assert_eq!(pct_decode("my%20miner"), "my miner");
        assert_eq!(pct_decode("a%2Fb"), "a/b"); // decoded; worker_api 400s slashes
        assert_eq!(pct_decode("100%25"), "100%");
        assert_eq!(pct_decode("%zz"), "%zz"); // invalid stays literal
        assert_eq!(pct_decode("trailing%"), "trailing%"); // no panic
        assert_eq!(pct_decode("short%2"), "short%2"); // no panic
        assert_eq!(pct_decode("a+b"), "a+b"); // plus stays a plus
    }

    #[test]
    fn worker_row_covers_online_offline_and_unknown() {
        use lm_pool::ledger::StreakState;
        use lm_stratum::workerstats::WorkerSnapshot;
        let live = WorkerSnapshot {
            miner: "w1".to_string(),
            name: "w1".to_string(),
            agent: "lm-miner/0.2 cpu".to_string(),
            shares: 7,
            rejects: 1,
            first_seen: 90,
            last_seen: 100,
            online: true,
            rates: [1.0, 2.0, 3.0, 4.0],
        };
        let mut streaks = HashMap::new();
        streaks.insert(
            "w1".to_string(),
            StreakState { miner: "w1".to_string(), days_active: 5, streak_days: 2, best_streak_days: 9 },
        );
        streaks.insert(
            "w2".to_string(),
            StreakState { miner: "w2".to_string(), days_active: 3, streak_days: 3, best_streak_days: 3 },
        );
        let mut ranks: HashMap<String, (String, u32, u32)> = HashMap::new();
        ranks.insert("w1".to_string(), ("cpu".to_string(), 4, 11));
        ranks.insert("w2".to_string(), ("cpu".to_string(), 9, 11));
        // Online: live rates + ledger streak/rank side by side.
        let row = worker_row_json("w1", Some(&live), &streaks, &ranks, 50, 500, "w1", None, 200).unwrap();
        assert_eq!(row["online"], true);
        assert_eq!(row["shares"], 7);
        assert_eq!(row["rate60"], 1.0);
        assert_eq!(row["streak"], 2);
        assert_eq!(row["best_streak"], 9);
        assert_eq!(row["league_class"], "cpu");
        assert_eq!(row["league_rank"], 4);
        assert_eq!(row["owed"], 50);
        assert_eq!(row["paid"], 500);
        // Offline: no live row, but the streak + rank + balances survive.
        let row = worker_row_json("w2", None, &streaks, &ranks, 10, 0, "", None, 200).unwrap();
        assert_eq!(row["online"], false);
        assert!(row["last_seen"].is_null());
        assert_eq!(row["streak"], 3);
        assert_eq!(row["league_rank"], 9);
        assert_eq!(row["name"], "w2"); // no registered name → the miner id
        // Never seen: None (the route 404s).
        assert!(worker_row_json("ghost", None, &streaks, &ranks, 0, 0, "", None, 200).is_none());
    }

    #[test]
    fn state_file_roundtrips_credited_total_and_seeds_it_from_blocks() {
        use lm_stratum::server::BlockRecord;
        // A stored counter survives the round trip untouched.
        let st = PoolState {
            version: 1,
            ledger: lm_pool::payout::PayoutLedger::default(),
            names: Vec::new(),
            blocks: Vec::new(),
            recent_draws: Vec::new(),
            pending: Vec::new(),
            payout_settings: lm_stratum::server::PayoutSettings::new(),
            attestations: lm_stratum::server::Attestations::new(),
            league_pool: 0,
            credited_total: 9_999_999,
            league_credits: Vec::new(),
            faucet_unmatured: Vec::new(),
            agents: Vec::new(),
            shares: lm_pool::ledger::Ledger::new(),
            luckynodes: Vec::new(),
        };
        let back: PoolState =
            serde_json::from_str(&serde_json::to_string(&st).expect("serialize")).expect("deserialize");
        assert_eq!(back.credited_total, 9_999_999);
        // Pre-counter files (no credited_total key) default to 0 and get
        // seeded from the restored block records instead.
        let seeded = resolve_credited_total(
            0,
            &[
                BlockRecord { height: 1, finder: "a".to_string(), value: 1_000_000, bonus: 100_000, pot: 0, league_overflow: 0, faucet: 0, dev: 0, payouts: vec![] },
                BlockRecord { height: 2, finder: "a".to_string(), value: 2_000_000, bonus: 0, pot: 300_000, league_overflow: 0, faucet: 0, dev: 0, payouts: vec![] },
            ],
        );
        assert_eq!(seeded, 3_400_000);
        // An explicit stored value always wins, even with blocks present.
        assert_eq!(resolve_credited_total(42, &[
            BlockRecord { height: 1, finder: "a".to_string(), value: 1_000_000, bonus: 0, pot: 0, league_overflow: 0, faucet: 0, dev: 0, payouts: vec![] },
        ]), 42);
        // Fresh pool: nothing to seed from.
        assert_eq!(resolve_credited_total(0, &[]), 0);
    }

    #[test]
    fn state_file_roundtrips_league_pool_and_defaults_old_files() {
        let st = PoolState {
            version: 1,
            ledger: lm_pool::payout::PayoutLedger::default(),
            names: Vec::new(),
            blocks: Vec::new(),
            recent_draws: Vec::new(),
            pending: Vec::new(),
            payout_settings: lm_stratum::server::PayoutSettings::new(),
            attestations: lm_stratum::server::Attestations::new(),
            league_pool: 260_317,
            credited_total: 1_234_567,
            league_credits: vec![(290u64, 1_841_270u64)],
            faucet_unmatured: vec![(12u64, 1_000u64)],
            agents: vec![("w1".to_string(), "lm-miner/0.2 cpu".to_string())],
            shares: lm_pool::ledger::Ledger::new(),
            luckynodes: Vec::new(),
        };
        let json = serde_json::to_string(&st).expect("serialize");
        let back: PoolState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.league_pool, 260_317);
        assert_eq!(back.credited_total, 1_234_567);
        assert_eq!(back.league_credits, vec![(290u64, 1_841_270u64)]);
        assert_eq!(back.faucet_unmatured, vec![(12u64, 1_000u64)]);
        assert_eq!(back.agents, vec![("w1".to_string(), "lm-miner/0.2 cpu".to_string())]);
        // Old state files (pre-league_pool / pre-attestations /
        // pre-unmatured) still load.
        let old = r#"{"version":1,"ledger":{"owed":{},"fees":0,"paid":{}},"names":[],"blocks":[]}"#;
        let back: PoolState = serde_json::from_str(old).expect("old file");
        assert_eq!(back.league_pool, 0);
        assert_eq!(back.credited_total, 0); // pre-counter files load
        assert!(back.attestations.is_empty());
        assert!(back.faucet_unmatured.is_empty());
        // A shares snapshot round-trips share data (streaks survive a
        // restart through the state file).
        let mut led = lm_pool::ledger::Ledger::new();
        led.submit(lm_pool::ledger::ShareRec { miner: "w1".to_string(), ts: 100, work: 10, overnight: false });
        led.submit(lm_pool::ledger::ShareRec { miner: "w1".to_string(), ts: 100 + 86_400, work: 10, overnight: false });
        let st2 = PoolState {
            version: 1,
            ledger: lm_pool::payout::PayoutLedger::default(),
            names: Vec::new(),
            blocks: Vec::new(),
            recent_draws: Vec::new(),
            pending: Vec::new(),
            payout_settings: lm_stratum::server::PayoutSettings::new(),
            attestations: lm_stratum::server::Attestations::new(),
            league_pool: 0,
            credited_total: 0,
            league_credits: Vec::new(),
            faucet_unmatured: Vec::new(),
            agents: Vec::new(),
            shares: led,
            luckynodes: Vec::new(),
        };
        let back2: PoolState = serde_json::from_str(&serde_json::to_string(&st2).expect("serialize")).expect("deserialize");
        let streaks = back2.shares.streaks(u64::MAX / 2);
        assert_eq!(streaks.len(), 1);
        assert_eq!(streaks[0].miner, "w1");
        assert_eq!(streaks[0].days_active, 2);
    }

    #[test]
    fn state_file_roundtrips_attestations() {
        let mut st = PoolState {
            version: 1,
            ledger: lm_pool::payout::PayoutLedger::default(),
            names: Vec::new(),
            blocks: Vec::new(),
            recent_draws: Vec::new(),
            pending: Vec::new(),
            payout_settings: lm_stratum::server::PayoutSettings::new(),
            attestations: lm_stratum::server::Attestations::new(),
            league_pool: 0,
            credited_total: 9_999,
            league_credits: Vec::new(),
            faucet_unmatured: vec![(7u64, 500u64)],
            agents: Vec::new(),
            shares: lm_pool::ledger::Ledger::new(),
            luckynodes: Vec::new(),
        };
        st.attestations.insert(
            "worker-a".to_string(),
            lm_stratum::server::Attestation {
                provider: "self".to_string(),
                time: 1_792_000_000,
                detail: "Pixel 8".to_string(),
            },
        );
        let json = serde_json::to_string(&st).expect("serialize");
        let back: PoolState = serde_json::from_str(&json).expect("deserialize");
        let a = back.attestations.get("worker-a").expect("kept");
        assert_eq![a.provider, "self"];
        assert_eq!(a.time, 1_792_000_000);
    }

    #[test]
    fn league_boards_json_ranks_within_family_and_flags_fresh_attestations() {
        use lm_pool::league::DeviceFamily;
        use lm_pool::ledger::BoardEntry;
        let entry = |miner: &str, class: DeviceFamily, work: u64| BoardEntry {
            miner: miner.to_string(),
            class,
            eff_work: work,
            days_active: 3,
            streak_days: 1,
            hps: 1,
        };
        let boards = lm_pool::ledger::rank_by_class(vec![
            entry("a1", DeviceFamily::Phone, 900),
            entry("a2", DeviceFamily::Phone, 500),
            entry("g1", DeviceFamily::Gpu, 100),
        ]);
        let mut names = HashMap::new();
        names.insert("a1".to_string(), "addr-a1".to_string());
        let mut atts = lm_stratum::server::Attestations::new();
        atts.insert(
            "a1".to_string(),
            lm_stratum::server::Attestation {
                provider: "self".to_string(),
                time: 100,
                detail: String::new(),
            },
        );
        // 30-day TTL: `now = 200` is fresh; the JSON carries the provider.
        let out = league_boards_json(&boards, &names, &atts, 200, 20);
        assert_eq!(out.len(), 4); // ladder order, farms present even if empty
        assert_eq!(out[0]["class"], "phone");
        assert_eq!(out[0]["size"], 2);
        let top = &out[0]["entries"][0];
        assert_eq!(top["rank"], 1);
        assert_eq!(top["name"], "addr-a1");
        assert_eq!(top["attested"], true);
        assert_eq!(top["attestation"]["provider"], "self");
        let second = &out[0]["entries"][1];
        assert_eq!(second["rank"], 2);
        assert_eq!(second["name"], "a2"); // no registered name → the miner id
        assert_eq!(second["attested"], false);
        assert_eq!(out[3]["class"], "farm");
        assert_eq!(out[3]["entries"].as_array().unwrap().len(), 0);
        // An expired verdict stops decorating rows.
        let stale = league_boards_json(
            &boards,
            &names,
            &atts,
            100 + lm_stratum::server::ATTESTATION_TTL_SECS + 1,
            20,
        );
        assert_eq!(stale[0]["entries"][0]["attested"], false);
        // The per-family cap holds.
        let capped = league_boards_json(&boards, &names, &atts, 200, 1);
        assert_eq!(capped[0]["entries"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn resolve_leagues_uses_labels_and_polices_by_rate() {
        use lm_pool::ledger::BoardEntry;
        let agents: HashMap<&str, &str> = [
            ("honest-arm", "lm-miner-mobile/0.1 android"),
            ("honest-gpu", "lm-miner/0.2 opencl"),
            ("lying-farm", "lm-miner/0.2 cpu"),
            ("silent", "mystery-miner/9.9"),
        ]
        .iter()
        .map(|(m, a)| (*m, *a))
        .collect();
        let entry = |miner: &str, hps: u64| BoardEntry {
            miner: miner.to_string(),
            class: lm_pool::league::DeviceFamily::Phone, // placeholder, resolved below
            eff_work: 1,
            days_active: 1,
            streak_days: 0,
            hps,
        };
        let resolved = resolve_leagues(
            vec![
                entry("honest-arm", 1_500),
                entry("honest-gpu", 21_000),
                entry("lying-farm", 600_000),
                entry("silent", 15_000),
            ],
            &agents,
        );
        let league = |m: &str| {
            resolved
                .iter()
                .find(|e| e.miner == m)
                .expect("row")
                .class
                .as_str()
                .to_string()
        };
        assert_eq!(league("honest-arm"), "phone");
        assert_eq!(league("honest-gpu"), "gpu");
        // A farm cannot hide behind a cpu label.
        assert_eq!(league("lying-farm"), "farm");
        // An unlabeled miner falls back to the rate-implied family.
        assert_eq!(league("silent"), "cpu");
    }

    #[test]
    fn attest_api_records_self_and_rejects_other_providers() {
        let server = spawn_server(
            MockProvider::new([0xff; 32], [0xff; 32], 1_000, 0),
            "127.0.0.1:0",
            None,
            None,
            None,
        )
        .expect("spawn");
        // Only the self-labeled provider exists until a verifier is wired.
        let (status, body) = attest_api(
            &server,
            None,
            r#"{"worker":"w1","provider":"play_integrity"}"#,
        );
        assert_eq!(status, "400 Bad Request");
        assert!(body.contains("unsupported provider"));
        // An open pool needs the worker name.
        let (status, _) = attest_api(&server, None, r#"{"provider":"self"}"#);
        assert_eq!(status, "400 Bad Request");
        // A self verdict records for the worker (latest wins).
        let (status, _) = attest_api(
            &server,
            None,
            r#"{"worker":"w1","provider":"self","detail":"Pixel 8"}"#,
        );
        assert_eq!(status, "200 OK");
        let att = server
            .attestation_for("w1", now_secs())
            .expect("recorded");
        assert_eq!(att.provider, "self");
        assert_eq!(att.detail, "Pixel 8");
        let (status, _) = attest_api(
            &server,
            None,
            r#"{"worker":"w1","provider":"self","detail":"Pixel 9"}"#,
        );
        assert_eq!(status, "200 OK");
        assert_eq!(
            server.attestation_for("w1", now_secs()).expect("kept").detail,
            "Pixel 9"
        );
        server.stop();
    }
}
