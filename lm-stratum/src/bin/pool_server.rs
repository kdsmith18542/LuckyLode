//! `pool-server` — run the reference pool on a mock chain or a real node.
//!
//! ```text
//! pool-server --mock [--bind HOST:PORT] [--reward R] [--fees F]
//!             [--pool-target (HEX64|max)] [--net-target (HEX64|max)]
//! pool-server --node-rpc HOST:PORT [--rpc-user U] [--rpc-pass P] [--bind HOST:PORT]
//!             [--no-claim-jackpots] [--no-claim-pot] [--no-claim-faucet] [--no-claim-dev] [--no-claim-prize]
//! ```
//!
//! Node mode claims everything the node advertises by default (jackpot
//! bonus, pot splash, faucet/dev seed drips, consensus draw claims) — the exact pre-committed
//! amounts, every find submittable. The `--claim-*` flags are deprecated
//! aliases (accepted, no-op); use `--no-claim-*` to opt out. Draw divisors
//! are read from the node (`getjackpotinfo`); `--pot-divisor` /
//! `--jackpot-divisor` are regtest-only overrides.
//!
//! Mock mode (explicit `--mock`) serves an instant-block mock chain
//! (`max`/`max`, reward 1M) on 127.0.0.1:3333. Node mode talks to a running forked node
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
.muted{color:var(--dim);font-size:12px}
.gold{color:var(--gold)}
.twrap{overflow-x:auto}
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
<button data-tab="faq">Help</button>
<button data-tab="miner">My miner</button>
<a class="badge" href="https://luckylode.cc/" target="_blank" rel="noopener">luckylode.cc</a>
<a class="badge" href="https://luckylode.cc/explorer/" target="_blank" rel="noopener">explorer</a>
<a class="badge" href="https://luckylode.cc/faucet/" target="_blank" rel="noopener">faucet</a>
</nav>
<div class="err" id="stale" style="display:none"></div>
<section id="tab-dashboard" class="active">
<div class="grid" id="cards"><div class="card"><div class="k">connecting</div><div class="v">…</div></div></div>
<h2>Workers</h2>
<input type="search" id="wfilter" placeholder="filter by worker / payout address (Enter looks up idle workers)">
<div id="wlook" style="color:var(--dim);font-size:12px;margin:6px 0"></div>
<div class="twrap"><table><thead><tr><th></th><th>worker</th><th>device</th><th>60s</th><th>10m</th><th>1h</th><th>24h</th><th>shares</th><th>streak</th><th>division</th><th>rejects</th><th>last share</th></tr></thead><tbody id="workers"></tbody></table></div>
<h2>Division boards <span style="text-transform:none">(season = 30 days, effective work; declared device family, policed by rate)</span></h2>
<div id="divisions"><span style="color:var(--dim)">no ranked workers yet</span></div>
<h2>Blocks</h2>
<div class="twrap"><table><thead><tr><th>height</th><th>finder</th><th>block reward</th><th>jackpot bonus</th><th>pot splash</th><th>prize inflow</th><th>draw claim</th><th>pplns payouts</th></tr></thead><tbody id="blocks"></tbody></table></div>
<h2>Payouts</h2>
<p class="muted">Sent payouts with txids. Your own due/paid balances live on the <b>My miner</b> tab — worker token on token pools, worker-name lookup on open pools.</p>
<div class="twrap"><table><thead><tr><th>time</th><th>worker</th><th>amount</th><th>txid</th></tr></thead><tbody id="payouts"></tbody></table></div><div class="muted" id="pool-totals"></div>
</section>
<section id="tab-start">
<h2>Mine to the official pool</h2>
<p>Lucky Lode is CPU/GPU-first. The reference miner <code>lm-miner</code> speaks Stratum V1 to this pool (V2/Noise on the next port — same command with <code>--v2</code>):</p>
<pre id="cmd">&hellip;</pre>
<p>First you need two things: <b>your own <span id="gs-ticker">tLKYL</span> address</b> (<span id="gs-net">testnet</span> prefixes <code id="gs-prefixes">m, n, 2</code> — the desktop wallet or <code>luckyd getnewaddress</code>), as the worker name &mdash; that is where the pool pays you; and <b>testnet coins to nothing</b> &mdash; mining needs no stake, but grab startup funds from the <a href="https://luckylode.cc/faucet/" target="_blank" rel="noopener">faucet</a> if you want to move coins around. Every share is verified against the pool's pre-committed job; nothing is withheld.</p>
<h2>Options worth knowing</h2>
<table><thead><tr><th>flag</th><th>what it does</th></tr></thead><tbody>
<tr><td><code>--device cpu|opencl|auto</code></td><td>hash on CPU threads or an OpenCL GPU (auto prefers the GPU when present)</td></tr>
<tr><td><code>--threads N</code></td><td>CPU thread count (default: all logical cores)</td></tr>
<tr><td><code>--duty N</code></td><td>run N% of the time &mdash; thermal/power friendly (e.g. 50)</td></tr>
<tr><td><code>--max-temp C</code></td><td>pause when the package/GPU sensor reaches C (80 recommended)</td></tr>
<tr><td><code>--max-power W</code></td><td>NVIDIA board power limit in watts (needs driver permission; empty = driver default)</td></tr>
<tr><td><code>--job-timeout S</code></td><td>reconnect if no new job arrives within S seconds</td></tr>
<tr><td><code>--password PASS</code></td><td>only on closed pools: must match the operator's pool password or your worker token</td></tr>
</tbody></table>
<p>Stratum endpoint: <code id="stratum">:3346</code> (plaintext; V2 on the next port up). <span id="gs-worthless">Testnet coins have no value &mdash; this is the public burn-in.</span></p>
</section>
<section id="tab-faq">
<h2>Help &amp; FAQ</h2>
<dl>
<dt>What is a share vs a block?</dt>
<dd>A share proves work against the pool target; the pool verifies every one against the current job. When a share also beats the network target it is a block &mdash; the pool submits it for you.</dd>
<dt>Why do some shares get rejected?</dt>
<dd>Usually stale (arrived after a job rotation) or duplicate (nonce already submitted). A few are normal; a rising reject rate means connectivity or clock trouble.</dd>
<dt>How do payouts work?</dt>
<dd>Each block's value is split linear pro-rata over the last 2,000 shares (PPLNS) and sent from the pool wallet to your worker address once the coins mature and the balance clears the payout threshold. Jackpot bonuses and pot splashes instead use square-root weights with a 25% per-miner cap, so one big rig can't take a whole windfall. Sent payouts are listed on the dashboard; your own due/paid balance is on the My miner tab (token pools) or the worker lookup (open pools).</dd>
<dt>What are the jackpot and the Lucky Pot?</dt>
<dd>Every PoW block carries a pre-committed draw: <code>SHA256D(parent hash + height)</code> &mdash; the ticket is fixed before any grinding, so nothing can be withheld. Jackpot: roughly 1 in 100 blocks &mdash; the block's reward gains a reserve bonus (up to 5× the block reward). Pot: roughly 1 in 1000 blocks with 10/25/50% tiers and a pity timer that forces a splash after a long dry run. Both land only on PoW finds, and the whole block (bonus included) is split among contributors by PPLNS.</dd>
<dt>What is a Luckynode Draw?</dt>
<dd>Every epoch the chain picks a winner from registered <code>ln-*</code> identities, and the block found at the boundary pays the winner straight from the prize fund — no pool custody involved. Epochs are 3 blocks on testnet (2,880 on mainnet). This pool attaches the claim automatically on every block it mines; see the draw-claim column in Blocks. Registering a name and heartbeating once per epoch are small transactions on the node (the wallet's Luckynodes tab does both); the old pool-run registry is retired.</dd>
<dt>Where do testnet coins come from?</dt>
<dd>The <a href="https://luckylode.cc/faucet/" target="_blank" rel="noopener">faucet</a> drips startup funds to any address (once per address per day). Mining itself needs no stake — point the miner at your address and go; payouts arrive once the coins mature (100 blocks) and clear the pool's payout threshold.</dd>
<dt>Is this real money?</dt>
<dd id="faq-testnet">No. This is the Lucky Lode public testnet; coins are worthless by design. The point is to break things before mainnet.</dd>
</dl>

<h2>API</h2>
<p>Endpoints are relative to this page's path, so they work behind <code>/pool/</code> too:</p>
<table><thead><tr><th>endpoint</th><th>returns</th></tr></thead><tbody>
  <tr><td><code>GET api/stats</code></td><td>pool, workers, blocks, payouts, game state, division boards (JSON) + current-epoch draw status (<code>draw</code>: epoch/budget/remaining/winner) — per-worker owed/paid are NOT here; see My miner</td></tr>
<tr><td><code>GET api/divisions</code></td><td>season division boards per device class (30-day effective work; ranks, streaks, attestation flags)</td></tr>
<tr><td><code>GET api/worker/&lt;miner&gt;</code></td><td>one worker's public row, alive or idle (streak + division rank survive workerstats age-out; 404 when never seen) — owed/paid are included on open pools, and only with the matching worker token (<code>Authorization: Bearer</code>) when the pool runs a workers file</td></tr>
<tr><td><code>GET api/miner</code> with <code>Authorization: Bearer &lt;token&gt;</code></td><td>your own row + payout settings (worker-token auth — the token is a header, never a URL)</td></tr>
<tr><td><code>POST api/payout/settings</code></td><td>set/clear your payout address + threshold: <code>{token,address,threshold}</code></td></tr>
<tr><td><code>POST api/attest</code></td><td>optional device label for the division boards: <code>{worker,provider:"self",detail?,token?}</code> — a self-declared hint, not a security check</td></tr>
</tbody></table>
<p>The dashboard polls <code>api/stats</code> every 5 s; miners, bots and dashboards are welcome to do the same.</p>
</section>

<section id="tab-miner">
<h2>My miner</h2>
<p>Paste your <b>worker token</b> (token pools: the operator's <code>--workers-file</code> entry for your worker) or your <b>worker name / address</b> (open pools, where every row is public). Tokens unlock payout settings; a worker name is read-only — on an open pool payouts go straight to the worker name, so make it your payout address. Secrets typed here never leave this page except to this pool's own API.</p>
<div style="display:flex;gap:8px;flex-wrap:wrap;align-items:center;margin:10px 0">
  <input id="mtoken" type="password" placeholder="worker token or worker name" style="max-width:280px">
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
  <p style="color:var(--dim);font-size:12px">Payouts send automatically once your balance reaches the threshold. Leave the address empty (or the threshold at 0) to use the defaults. The threshold is a decimal coin amount (empty clears the override); anything else is rejected, not silently cleared.</p>
</div>
<div id="mopen" style="display:none;margin-top:12px">
  <p style="color:var(--dim);font-size:12px">Open pool, no worker tokens: your worker name <b>is</b> the payout destination (point it at your address), the pool default threshold applies, and these numbers are public to anyone who asks for this worker. Custom payout settings need a token pool.</p>
</div>
</section>
<footer id="note"></footer>
<div class="err" id="err"></div>
<script>
const esc=v=>String(v??'-').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
let TICK='tLKYL';
const coin=v=>typeof v==='number'?(v/1e8).toLocaleString(undefined,{maximumFractionDigits:8})+' '+TICK:'-';
const coin8=v=>typeof v==='number'?(v/1e8).toLocaleString(undefined,{maximumFractionDigits:8})+' '+TICK:'-';
 const hs=v=>{v=Number(v)||0;if(v>=1e6)return (v/1e6).toFixed(2)+' MH/s';if(v>=1e3)return (v/1e3).toFixed(1)+' kH/s';return v.toFixed(0)+' H/s';};
 const num=v=>{v=Number(v)||0;if(v>=1e6)return (v/1e6).toFixed(2)+'M';if(v>=1e3)return (v/1e3).toFixed(1)+'k';return v.toFixed(2);};
const ago=s=>{if(!s)return '-';const d=Math.max(0,Math.floor(Date.now()/1000-s));if(d<60)return d+'s';if(d<3600)return Math.floor(d/60)+'m';return Math.floor(d/3600)+'h';};
const tstr=t=>new Date(t*1000).toLocaleTimeString();
const device=a=>{const p=String(a||'').split(' ').filter(Boolean);return p.length>1?p.slice(1).join(' '):(p[0]||'-');};
// Tabs + miner lookup + endpoint fill. The active tab lives in the URL
// hash so Help/Getting started/My miner link and survive reloads.
const tabBtns=[...document.querySelectorAll('nav button')];
function showTab(name){
  if(!tabBtns.some(b=>b.dataset.tab===name))name='dashboard';
  tabBtns.forEach(b=>b.classList.toggle('active',b.dataset.tab===name));
  document.querySelectorAll('section').forEach(s=>s.classList.toggle('active',s.id==='tab-'+name));
}
tabBtns.forEach(b=>b.addEventListener('click',()=>{showTab(b.dataset.tab);try{location.hash=b.dataset.tab;}catch(e){}}));
showTab((location.hash||'').replace('#',''));
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
    const r=await fetch('api/worker/'+encodeURIComponent(q),{cache:'no-store',headers:mtok?{Authorization:'Bearer '+mtok}:{}});
    if(!r.ok){
      box.textContent=r.status===403
        ?q+': hidden on this token pool — paste your worker token in My miner'
        :r.status===404?q+': never seen on this pool':'lookup failed (pool error '+r.status+')';
      return;
    }
    const w=await r.json();
    box.textContent=`${w.name||w.miner} · ${w.online?'online':'idle'+(w.last_seen?' (last seen '+ago(w.last_seen)+')':'')} · streak ${w.streak||0}d (best ${w.best_streak||0}) · ${w.division_class?('division '+w.division_class+' #'+w.division_rank+'/'+w.division_size):'unranked this season'} · owed ${coin(w.owed)} · paid ${coin(w.paid)}`;
  }catch(err){box.textContent='lookup failed: '+err;}
});
let lastOk=0;
async function tick(){
 try{
  const j=await (await fetch('api/stats',{cache:'no-store'})).json();
  lastOk=Date.now();
  document.getElementById('stale').style.display='none';
  const c=[]; const add=(k,v)=>c.push(`<div class="card"><div class="k">${k}</div><div class="v">${esc(v)}</div></div>`);
  if(j.jackpot) add('height', j.jackpot.height);
  if(j.mining) add('difficulty', num(j.mining.difficulty));
  add('pool hashrate 10m', hs(j.pool.hashrate10m));
  add('workers online / seen', `${j.pool.online}/${j.pool.workers}`);
  add('shares', j.pool.shares); add('rejects', j.pool.rejects);
  add('blocks found', j.pool.blocks); add('prize fund', coin((j.jackpot||{}).prize));
  if(j.jackpot){add('jackpot reserve', coin(j.jackpot.reserve)); add('lucky pot', coin(j.jackpot.pot)); add('pot drought', j.jackpot.drought+' / '+(j.jackpot.potdrought||4320)+' blocks');}
  if(j.draw&&j.draw.epoch>0){add('draw epoch', j.draw.epoch); add('draw budget', coin(j.draw.budget)); add('draw remaining', coin(j.draw.remaining)); add('draw winner', (j.draw.winner||'').slice(0,12)+'…');}
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
    ? (j.payout.exec ? (j.payout.threshold>0 ? `payouts: EXEC ≥ ${coin(j.payout.threshold)}` : 'payouts: EXEC (every balance)') : `payouts: dry-run ≥ ${coin(j.payout.threshold)}`)
    : '';
  document.getElementById('updated').textContent='updated '+new Date().toLocaleTimeString();
  document.getElementById('workers').innerHTML=(j.workers||[]).map(w=>
    `<tr class="${w.online?'':'off'}"><td><span class="dot"></span></td><td>${esc(w.name||w.miner)}</td><td>${esc(device(w.agent))}</td><td>${hs(w.rate60)}</td><td>${hs(w.rate10m)}</td><td>${hs(w.rate1h)}</td><td>${hs(w.rate24h)}</td><td>${esc(w.shares)}</td><td>${w.streak?`${esc(w.streak)}d${w.best_streak>w.streak?` <span class="dim">(best ${esc(w.best_streak)})</span>`:''}`:'—'}</td><td>${w.division_class?esc(w.division_class)+' #'+esc(w.division_rank)+'/'+esc(w.division_size)+(w.attested?' <span class="badge">attested</span>':''):'—'}</td><td>${esc(w.rejects)}</td><td>${ago(w.last_seen)}</td></tr>`).join('')
    || '<tr><td colspan="12">no workers yet</td></tr>';
  applyWF();
  const divisions=(j.divisions&&j.divisions.boards)||[];
  // Full boards render as tables; empty families collapse to one line so
  // three empty tables don't dominate the page. The API caps boards at
  // 20 rows, so say "top 20 of N" when the board is longer.
  const boardHtml=b=>{
    const rows=(b.entries||[]).map(e=>
      `<tr><td>${esc(e.rank)}</td><td>${esc(e.name||e.miner)}</td><td>${num(e.eff_work)}</td><td>${esc(e.days_active)}d</td><td>${e.streak?esc(e.streak)+'d':'—'}</td><td>${e.attested?esc((e.attestation&&e.attestation.provider)||'attested'):'—'}</td></tr>`).join('');
    const cap=(b.entries||[]).length>=20&&(b.size||0)>20?`top 20 of ${esc(b.size)}`:`${esc(b.size)}`;
    return `<h3 style="margin:14px 0 6px;color:var(--gold);font-size:13px">${esc(b.class)} <span class="dim">· ${cap} ranked</span></h3>`+
      `<table><thead><tr><th>rank</th><th>worker</th><th>eff. work</th><th>days</th><th>streak</th><th>device attest</th></tr></thead><tbody>${rows||'<tr><td colspan="6">no qualifying workers this season</td></tr>'}</tbody></table>`;
  };
  const fullDivs=divisions.filter(b=>(b.entries||[]).length>0);
  const emptyDivs=divisions.filter(b=>(b.entries||[]).length===0);
  document.getElementById('divisions').innerHTML=
    fullDivs.map(boardHtml).join('')
    +(emptyDivs.length?`<p class="dim">no qualifying workers this season: ${emptyDivs.map(b=>esc(b.class)).join(', ')}</p>`:'')
    ||'<span style="color:var(--dim)">no ranked workers yet</span>';
  document.getElementById('blocks').innerHTML=(j.last_blocks||[]).map(b=>
    `<tr><td>${esc(b.height)}</td><td>${esc(b.finder_name||b.finder)}</td><td>${coin(b.value)}</td><td>${b.bonus>0?coin(b.bonus):'–'}</td><td>${b.pot>0?coin(b.pot):'–'}</td><td>${b.prize_inflow>0?coin(b.prize_inflow):'–'}</td><td>${b.prize_claim>0?coin(b.prize_claim)+' → '+esc((b.prize_winner||'').slice(0,12))+'…':'–'}</td><td>${b.pplns_total>0?coin(b.pplns_total)+' ('+esc(b.pplns_count)+')':'–'}</td></tr>`).join('')
    || '<tr><td colspan="8">no blocks yet</td></tr>';
  document.getElementById('payouts').innerHTML=(j.payouts||[]).map(p=>
    `<tr><td>${tstr(p.time)}</td><td>${esc(p.name)}</td><td>${coin(p.amount)}</td><td>${esc(p.txid)}</td></tr>`).join('')
    || '<tr><td colspan="4">no payouts sent yet</td></tr>';
  if(j.payout_totals){document.getElementById('pool-totals').textContent=
    `pool lifetime: ${coin(j.payout_totals.coinbases)} mined · ${coin(j.payout_totals.paid)} paid · ${coin(j.payout_totals.owed)} owed · fees ${coin(j.payout_totals.fees)}`;}
  const mode=j.mode||'';
  const claimNote = (mode.includes('pot') || mode.includes('jackpot'))
    ? 'Claim mode: draws are pre-committed per height (SHA256D of the parent hash); every find lands — no withholding.'
    : '';
  document.getElementById('note').innerHTML =
    'Official Lucky Lode reference pool · <span id="foot-net">'+esc(NET.name)+'</span> · no operator fee (1% miner + 0.5pp pool dev slice applies at mainnet). '
    + claimNote;
  document.getElementById('err').textContent='';
 }catch(e){
  const el=document.getElementById('stale');
  el.style.display='block';
  el.textContent='pool API unreachable'+(lastOk?(' — showing data from '+new Date(lastOk).toLocaleTimeString()):' — retrying…');
  document.getElementById('err').textContent='stats fetch failed: '+e;
 }
}
// My miner: token-authenticated stats + payout settings, with an
// open-pool fallback. A token Bearer against api/miner unlocks the
// settings form; otherwise the input is tried as a worker name against
// the public api/worker row (read-only — open pools have no tokens and
// payout settings stay pool defaults). Failure order matters: a token
// pool 403s the worker lookup for strangers, so report the token error.
let mtok='';
function minerCards(rows){
  return '<div class="grid">'+rows+'</div>';
}
function statCard(k,v,cls){return '<div class="card"><div class="k">'+k+'</div><div class="v'+(cls?' '+cls:'')+'">'+v+'</div></div>';}
async function loadMiner(keepMsg){
  const input=document.getElementById('mtoken').value.trim();
  const msg=document.getElementById('mmsg');
  if(!input){msg.textContent='enter your worker token or worker name';return;}
  // Token path first: the only route that unlocks payout settings.
  try{
    const r=await fetch('api/miner',{cache:'no-store',headers:{Authorization:'Bearer '+input}});
    const j=await r.json();
    if(r.ok&&!j.error){
      mtok=input;
      const m=j.miner;
      document.getElementById('mstats').innerHTML=minerCards(
        statCard('worker',esc(m.name))+
        statCard('device',esc(device(m.agent)))+
        statCard('60s / 10m',hs(m.rates[0])+' · '+hs(m.rates[1]))+
        statCard('shares / rejects',esc(m.shares)+' / '+esc(m.rejects))+
        statCard('owed',coin8(m.owed),'gold')+
        statCard('paid',coin8(m.paid)));
      document.getElementById('maddr').value=j.settings.address||'';
      document.getElementById('mthr').value=j.settings.threshold_coins||'';
      document.getElementById('msettings').style.display='block';
      document.getElementById('mopen').style.display='none';
      msg.textContent=keepMsg||('loaded · pool default threshold '+j.defaults.threshold_coins+' '+TICK);
      return;
    }
  }catch(e){/* fall through to the worker-name path */}
  // Open-pool path: public row, no settings.
  try{
    const r=await fetch('api/worker/'+encodeURIComponent(input),{cache:'no-store'});
    const j=await r.json();
    if(r.ok&&!j.error){
      mtok='';
      const div=j.division_class?('division '+esc(j.division_class)+' #'+esc(j.division_rank)+'/'+esc(j.division_size)):'unranked this season';
      const seen=j.online?'online':(j.last_seen?('idle (last seen '+ago(j.last_seen)+')'):'idle');
      document.getElementById('mstats').innerHTML=minerCards(
        statCard('worker',esc(j.name||j.miner))+
        statCard('device',esc(device(j.agent)))+
        statCard('60s / 10m',hs(j.rate60)+' · '+hs(j.rate10m))+
        statCard('shares / rejects',esc(j.shares)+' / '+esc(j.rejects))+
        statCard('streak',j.streak?(esc(j.streak)+'d'+(j.best_streak>j.streak?' (best '+esc(j.best_streak)+'d)':'')):'—')+
        statCard('division',div)+
        statCard('status',seen)+
        statCard('owed',coin8(j.owed),'gold')+
        statCard('paid',coin8(j.paid)));
      document.getElementById('msettings').style.display='none';
      document.getElementById('mopen').style.display='block';
      msg.textContent=keepMsg||('loaded '+(j.name||j.miner)+' · open pool (read-only)');
      return;
    }
    msg.textContent=r.status===403
      ?'this pool needs a worker token for that name — paste the token instead'
      :'never seen on this pool — check the worker name';
  }catch(e){msg.textContent='lookup failed: '+e;}
  document.getElementById('mstats').innerHTML='';
  document.getElementById('msettings').style.display='none';
  document.getElementById('mopen').style.display='none';
}
async function saveMinerSettings(){
  const msg=document.getElementById('mmsg');
  if(!mtok){msg.textContent='payout settings need a worker token (open-pool rows are read-only)';return;}
  const thr=document.getElementById('mthr').value.trim();
  // Validate client-side: the API treats junk as "clear", which would
  // silently drop a threshold on a typo.
  if(thr&&!/^\d+(\.\d{1,8})?$/.test(thr)){msg.textContent='threshold must be a decimal coin amount (empty clears it)';return;}
  try{
    const r=await fetch('api/payout/settings',{method:'POST',headers:{'Content-Type':'application/json'},
      body:JSON.stringify({token:mtok,address:document.getElementById('maddr').value.trim(),threshold:thr})});
    const j=await r.json();
    if(!r.ok||j.error)throw new Error(j.error||r.statusText);
    await loadMiner(j.cleared?'settings cleared — defaults apply':'saved');
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
    // Seasonal division boards (doc 03 §3.3): per-family ranks over the
    // 30-day season window, plus the per-worker rank shortcut the app
    // tiles read. The division is the miner's declared device family from
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
    let boards = lm_pool::ledger::rank_by_division(resolve_divisions(
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
            // Division rank within the worker's device family (null when the
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
                "division_class": lg.map(|l| l.0.clone()),
                "division_rank": lg.map(|l| l.1),
                "division_size": lg.map(|l| l.2),
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
    let payout_totals = server.payout_totals();
    let payouts_json: Vec<serde_json::Value> = ctx
        .history
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
                "prize_inflow": b.prize_inflow,
                "prize_claim": b.prize_claim,
                "prize_winner": b.prize_winner,
                "faucet": b.faucet,
                "dev": b.dev,
                "pplns_total": total,
                "pplns_count": b.payouts.len(),
            })
        })
        .collect();
    let d = server.latest_draw();
    let draw_json = serde_json::json!({
        "epoch": d.epoch,
        "budget": d.budget,
        "remaining": d.remaining,
        "winner": d.winner,
    });
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
            "workers": workers.len(),
            "online": online,
            "hashrate60": totals[0],
            "hashrate10m": totals[1],
            "hashrate1h": totals[2],
            "hashrate24h": totals[3],
        },
        "workers": workers_json,
        "payout_totals": {
            "owed": payout_totals.0,
            "paid": payout_totals.1,
            "fees": payout_totals.2,
            "coinbases": server.credited_total(),
        },
        "payouts": payouts_json,
        "last_blocks": last,
        "divisions": {
            "window_secs": lm_pool::ledger::SEASON_WINDOW_SECS,
            "window_days": lm_pool::ledger::SEASON_WINDOW_SECS / 86_400,
            "boards": division_boards_json(&boards, &names_map, &attestations, now, 20),
        },
        // Per-height prize inflow credited, including staker/peer finds
        // the pool did not mine (the explorer's per-block game view).
        "prize": {
            "credits": server
                .prize_credits()
                .into_iter()
                .map(|(height, amount)| serde_json::json!({"height": height, "amount": amount}))
                .collect::<Vec<_>>(),
        },
        // Current-epoch Luckynode Draw advisory (display only — claims
        // are spliced from the same node data at template build).
        // Epoch 0 = no advisory (mock provider or pre-draw node).
        "draw": draw_json,
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
    obj.to_string()
}

/// Resolve display divisions: the miner's declared device family from its
/// connect-time agent string decides, the trailing-24h rate polices
/// (doc 03 §3.6 — self-declared + statistically policed). Unlabeled or
/// contradicted miners fall back to the rate-implied family.
fn resolve_divisions(
    board: Vec<lm_pool::ledger::BoardEntry>,
    agents: &HashMap<&str, &str>,
) -> Vec<lm_pool::ledger::BoardEntry> {
    board
        .into_iter()
        .map(|mut e| {
            let agent = agents.get(e.miner.as_str()).copied().unwrap_or("");
            e.class = lm_pool::division::division_for(lm_pool::division::family_of(agent), e.hps);
            e
        })
        .collect()
}

/// Per-family division boards as JSON (doc 03 §3.3: phones race phones).
/// `cap` limits entries per family (0 = uncapped). Fresh attestation
/// verdicts decorate rows (`attested` + the verdict's provider/time).
fn division_boards_json(
    boards: &[(lm_pool::division::DeviceFamily, Vec<(u32, lm_pool::ledger::BoardEntry)>)],
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
                "size": rows.len(),
                "entries": entries,
            })
        })
        .collect()
}

/// `GET /api/divisions` — the full seasonal division boards (all families, up
/// to 100 entries each), same shape as the `divisions` object on
/// `/api/stats`.
fn divisions_json<P: TemplateProvider + 'static>(server: &RunningServer<P>) -> String {
    let now = now_secs();
    // Live + restored agents: idle miners keep their declared family.
    let stored_agents = server.agents();
    let agent_map: HashMap<&str, &str> = stored_agents
        .iter()
        .map(|(m, a)| (m.as_str(), a.as_str()))
        .collect();
    let boards = lm_pool::ledger::rank_by_division(resolve_divisions(
        server.leaderboard(lm_pool::ledger::SEASON_WINDOW_SECS, now),
        &agent_map,
    ));
    let names_map: HashMap<String, String> = server.names().into_iter().collect();
    let attestations = server.attestations();
    serde_json::json!({
        "window_secs": lm_pool::ledger::SEASON_WINDOW_SECS,
        "window_days": lm_pool::ledger::SEASON_WINDOW_SECS / 86_400,
        "boards": division_boards_json(&boards, &names_map, &attestations, now, 100),
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
        "division_class": lg.map(|l| l.0.clone()),
        "division_rank": lg.map(|l| l.1),
        "division_size": lg.map(|l| l.2),
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
/// idle: live rates when connected, streak + division rank + balances
/// from the ledger either way (streaks survive retention pruning and
/// workerstats age-out). The name is a URL path segment (percent-encoded;
/// capped at 128 chars). Money is token-gated: on an open pool (no
/// `--workers-file`) the full row is public; when the pool runs worker
/// tokens, owed/paid are served only to the matching token (Bearer) and
/// anything else gets 403. Stats (rates, streak, division) stay public
/// either way. 404 when the pool has never seen the name.
fn worker_api<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
    miner_seg: &str,
    authorization: Option<&str>,
) -> (&'static str, String) {
    let miner: String = pct_decode(miner_seg).chars().take(128).collect();
    if miner.is_empty() || miner.contains('/') {
        return (
            "400 Bad Request",
            serde_json::json!({"error": "missing worker name"}).to_string(),
        );
    }
    if server.worker_tokens_active() {
        let authed = authorization
            .and_then(|a| a.strip_prefix("Bearer ").or_else(|| a.strip_prefix("bearer ")))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .and_then(|t| server.miner_for_token(t));
        if authed.as_deref() != Some(miner.as_str()) {
            return (
                "403 Forbidden",
                serde_json::json!({"error": "worker token required (see your own row on the My miner tab)"})
                    .to_string(),
            );
        }
    }
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
    let boards = lm_pool::ledger::rank_by_division(resolve_divisions(
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
                "error": "unsupported provider; only a self-declared label is accepted"
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
fn read_line_capped(
    reader: &mut impl Read,
    cap: usize,
    deadline: std::time::Instant,
) -> std::io::Result<String> {
    let mut out: Vec<u8> = Vec::with_capacity(128);
    let mut b = [0u8; 1];
    loop {
        if std::time::Instant::now() > deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request deadline",
            ));
        }
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
    // Total-request deadline: the per-read timeouts above reset with
    // every byte, so a slowloris at one byte per nine seconds held a
    // thread forever. One request gets 30 seconds, all reads included.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut body = String::new();
    let mut authorization = String::new();
    let line;
    {
        let mut reader = BufReader::new(stream.try_clone()?);
        line = read_line_capped(&mut reader, 8192, deadline)?;
        // Headers: only Content-Length matters (POST bodies for payout
        // settings / attest APIs); the whole header section is capped (lines and
        // bytes) so neither count nor size can grow without bound.
        let mut content_len = 0usize;
        let mut header_lines = 0usize;
        let mut header_bytes = 0usize;
        loop {
            let h = read_line_capped(&mut reader, 8192, deadline)?;
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
        ("GET", "/api/divisions") => ("200 OK", "application/json", divisions_json(server)),
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
            let (status, json) =
                worker_api(server, &p["/api/worker/".len()..], Some(authorization.as_str()));
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
    /// Per-miner payout settings (address/threshold overrides,.
    #[serde(default)]
    payout_settings: lm_stratum::server::PayoutSettings,
    /// Per-worker device-attestation verdicts (doc 03 §3.6). `default`
    /// so state files written before this field still load.
    #[serde(default)]
    attestations: lm_stratum::server::Attestations,
    /// Lifetime miner-owed coinbase accepted (normal value + jackpot
    /// bonuses + pot splashes). Backs the /api/stats reconciliation
    /// identity `coinbases == owed + paid + fees`. `default` so state
    /// files written before this field still load — in that case the
    /// loader seeds it from the restored block records (exact while
    /// the retained block history is complete).
    #[serde(default)]
    credited_total: u64,
    /// Per-height prize inflow actually credited (height, sats),
    /// including staker/peer finds that never enter the block history.
    /// `default` so older state files still load; `alias` covers
    /// pre-rename files (`league_credits`).
    #[serde(default, alias = "league_credits")]
    prize_credits: Vec<(u64, u64)>,
    /// In-flight faucet claims (height, amount) — unmatured coinbase
    /// outputs the wallet cannot see yet. Survives restarts so a reboot
    /// does not re-claim a full drip on the next template.
    /// `default` so old state files still load.
    #[serde(default)]
    faucet_unmatured: Vec<(u64, u64)>,
    /// Last-known connect-time agents (miner, agent): idle miners keep
    /// their declared division family across restarts instead of falling
    /// back to the rate-implied one. `default` so old files still load.
    #[serde(default)]
    agents: Vec<(String, String)>,
    /// Share ledger (raw shares + day rollup + comeback state): streaks,
    /// division boards and PPLNS windows survive pool restarts. `default`
    /// so old state files still load.
    #[serde(default)]
    shares: lm_pool::ledger::Ledger,
}

/// State-file version. v2 was the luckynode registry ID-binding fix
/// (P1 takeover fix, `register_bound`): the registry is deleted with the
/// pool-era draw loop, so no registry content restores anymore — old
/// files' draw/registry/prize_pool keys are ignored on load, everything
/// else still loads.
const STATE_VERSION: u32 = 2;

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
/// Game identity, derived ONLY from the verified node chain.
/// `--network` is a display label for the dashboard/API; it must never
/// change cadence, divisors, or claims.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum GameChain {
    Mainnet,
    Testnet,
}

/// Resolve game identity from the node's `getblockchaininfo.chain`.
/// Fail closed on unknown or unreachable identity: defaulting to
/// testnet could run a mainnet pool on a 3-block draw epoch (or vice
/// versa). NOTE: regtest reports chain "main" (upstream label quirk)
/// and therefore maps to mainnet cadence — the pre-cull default too.
fn resolve_game_chain(node_chain: Option<&str>) -> Result<GameChain, String> {
    match node_chain {
        Some("main") => Ok(GameChain::Mainnet),
        Some("test") => Ok(GameChain::Testnet),
        Some(other) => Err(format!(
            "unrecognized node chain {other:?}; refusing to guess game parameters"
        )),
        None => Err("cannot determine node chain identity (getblockchaininfo failed); refusing to guess game parameters".to_string()),
    }
}

/// --force-fresh recovery: move the state file aside (never delete —
/// the operator may need its balances) and boot with an empty ledger.
/// Pure + unit-tested; the only impure step is the rename itself.
fn force_fresh_target(path: &std::path::Path) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".pre-fresh-{stamp}"));
    std::path::PathBuf::from(s)
}

fn apply_force_fresh(state_file: &Option<std::path::PathBuf>) {
    let Some(path) = state_file.as_ref() else {
        eprintln!("note: --force-fresh with no --state-file is a no-op");
        return;
    };
    if !path.exists() {
        return;
    }
    let aside = force_fresh_target(path);
    match std::fs::rename(path, &aside) {
        Ok(()) => eprintln!(
            "warning: --force-fresh moved {} aside to {} — starting with an empty ledger",
            path.display(),
            aside.display()
        ),
        Err(e) => {
            eprintln!("error: --force-fresh could not move {} aside: {e}", path.display());
            std::process::exit(1);
        }
    }
}

/// In-flight faucet claims restored from the state file, read BEFORE the
/// provider connects — so the FIRST template's top-up math already
/// accounts for them, instead of waiting for the first refresh (up to
/// 2 s of miners grinding an over-claimed template). Missing or corrupt
/// file = empty (fresh pool). `load_state` re-applies the same values
/// afterwards; the double load is idempotent.
fn read_unmatured(path: &Option<std::path::PathBuf>) -> Vec<(u64, u64)> {
    let Some(p) = path else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(p) else {
        return Vec::new();
    };
    serde_json::from_str::<PoolState>(&raw)
        .map(|st| st.faucet_unmatured)
        .unwrap_or_default()
}

fn load_state<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
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
            server.restore_credited(resolve_credited_total(st.credited_total, &st.blocks));
            server.restore_prize_credits(&st.prize_credits);
            let unmatured = st.faucet_unmatured.len();
            server.restore_faucet_unmatured(st.faucet_unmatured);
            let settings = st.payout_settings.len();
            server.restore_payout_settings(st.payout_settings);
            server.restore_attestations(st.attestations);
            let n = st.blocks.len();
            server.restore_blocks(st.blocks);
            let (owed, paid, fees) = server.payout_totals();
            println!(
                "state loaded from {}: {n} blocks, owed {owed}, paid {paid}, fees {fees}{}",
                path.display(),
                if settings > 0 {
                    format!(", {settings} payout setting(s)")
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

/// Serialize concurrent saves (periodic 10 s tick vs post-payout save):
/// both write through the same `<state>.tmp` name, so without a lock one
/// can rename the tmp file away between the other's create and rename —
/// and that rename then fails with ENOENT, silently skipping a
/// post-payout save (seen live exactly once; self-heals on the next tick,
/// but a crash inside the 10 s window would double-pay).
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Atomic snapshot write (tmp + rename) so a crash mid-save never leaves
/// a truncated state file.
fn save_state<P: TemplateProvider + 'static>(
    server: &RunningServer<P>,
    path: &Path,
) {
    let _guard = SAVE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let st = PoolState {
        version: STATE_VERSION,
        ledger: server.ledger_snapshot(),
        names: server.names(),
        blocks: server.blocks(),
        payout_settings: server.payout_settings(),
        attestations: server.attestations(),
        credited_total: server.credited_total(),
        prize_credits: server.prize_credits(),
        faucet_unmatured: server.faucet_unmatured(),
        agents: server.agents(),
        shares: server.shares_snapshot(),
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
    path: PathBuf,
) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        save_state(&server, &path);
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
    path: Option<PathBuf>,
) {
    loop {
        if TERM.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(path) = &path {
                save_state(&server, path);
                println!("shutdown: state saved to {} — exiting", path.display());
            } else {
                println!("shutdown: no state file configured — exiting");
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
    state: Option<PathBuf>,
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
        // spendable balance (partials included), the rest waits.
        let mut budget = match rpc.call("getbalance", serde_json::json!([])) {
            Ok(v) => coins_to_base(&v).saturating_sub(FEE_BUFFER),
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
                    if let Some(path) = &state {
                        save_state(&server, path);
                    }
                    budget = budget.saturating_sub(pay);
                    let mut h = history.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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

const USAGE: &str = "usage:
  pool-server --mock [--bind HOST:PORT] [--reward R] [--fees F] [--pool-password PASS] [--pool-target (HEX64|max)] [--net-target (HEX64|max)]
    (mock chain: instant blocks for dev/tests — never expose publicly; node mode needs --node-rpc, never bare flags)
  pool-server --node-rpc HOST:PORT [--rpc-user U] [--rpc-pass P | --rpc-pass-file PATH | LUCKY_POOL_RPC_PASS=...] [--pool-password PASS | --pool-password-file PATH] [--bind HOST:PORT]
  [--no-claim-jackpots] [--no-claim-pot] [--no-claim-faucet] [--no-claim-dev] [--no-claim-prize]   (all claims ON by default; these opt out)
  both forms accept [--http HOST:PORT] to serve the live dashboard (/ and /api/stats)
  node mode accepts [--payout-threshold V] [--payout-interval SECS] [--payout-exec]
  [--pot-divisor N] [--jackpot-divisor N]   (regtest-only overrides; otherwise read from the node)
  [--network testnet|mainnet] is a display label only (dashboard/API tickers);
  game parameters always come from the verified node chain
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
  file to intentionally clear. [--force-fresh] moves a present-or-corrupt
  state file aside (*.pre-fresh-TIMESTAMP, never deleted) and boots empty.

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
draws (node mode): the consensus Luckynode Draw claim is spliced from the
node's prize-draw advisories (exact remaining budget to the winner's
registered script; old nodes have no fields and stay claim-free). The
claim pays a third party, never the miner. Pass --no-claim-prize to opt
out.
payouts (node mode): balances at/above --payout-threshold
(base units) are sent from the node wallet every --payout-interval seconds
(default 60); WITHOUT --payout-exec the loop only logs what it would send.
Worker names double as payout addresses (chain prefixes: testnet m/n/2,
mainnet L — the old R-only wording was wrong).";

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
    // SIGTERM/SIGINT = clean stop: the handlers only flip the atomic; the
    // term_watcher thread (spawned per server) does the final state save.
    // (SIGINT used to take the default terminate path and lose up to 10 s
    // of ledger/block changes despite the "Ctrl-C to stop" banner.)
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_sigterm as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("pool_server {}", env!("CARGO_PKG_VERSION"));
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
    let mut claim_prize = true;
    // Instant-block mock chain for dev/tests. Explicit opt-in: bare flags
    // used to boot the mock silently, which is how a pool ends up mining
    // worthless blocks while the operator thinks it is on a node.
    let mut mock = false;
    let mut http: Option<String> = None;
    let mut claim_pot_tier = 10u64; // obsolete no-op (kept for old scripts)
    let mut pot_divisor: Option<u64> = None; // None = read from the node
    let mut jackpot_divisor: Option<u64> = None; // None = read from the node
    let mut claim_pot_pity = false; // obsolete no-op (kept for old scripts)
    let mut payout_threshold = 0u64;
    // Display label for user-facing strings only (dashboard, API
    // tickers). Game parameters (epoch, divisors, claims) ALWAYS come
    // from the verified node chain — never from this flag.
    let mut network: Option<String> = None;
    let mut payout_interval = 60u64;
    let mut payout_exec = false;
    let mut state_file: Option<PathBuf> = None;
    // Start fresh even if the state file exists or is corrupt: the old
    // file is moved aside (never deleted), then the pool boots with an
    // empty ledger. This is the documented recovery from a corrupt
    // state file ("restore the file or start with --force-fresh").
    let mut force_fresh = false;
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
            "--claim-prize" => {
                // Deprecated alias, see --claim-jackpots.
                claim_prize = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--mock" => {
                mock = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--no-claim-prize" => {
                claim_prize = false;
                i += 1; // valueless flag: do not skip the next argument
                continue;
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
            "--state-file" => {
                state_file = Some(PathBuf::from(next("state-file").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })));
            }
            "--force-fresh" => {
                force_fresh = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
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
                let v = next("pool-fee-pp")
                    .and_then(|v| {
                        v.parse::<u16>()
                            .map_err(|_| "--pool-fee-pp expects 0-10000".to_string())
                    })
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    });
                if v > 10_000 {
                    // Past 100% the fee pot exceeds the block: distributable
                    // saturates to 0 and the dev slice overpays from the
                    // node wallet. Reject, don't clamp.
                    eprintln!("error: --pool-fee-pp expects 0-10000, got {v}");
                    std::process::exit(2);
                }
                pool_fee_pp = v;
            }
            "--dev-fee-addr" => {
                dev_fee_addr = Some(next("dev-fee-addr").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }));
            }
            "--dev-fee-pp" => {
                let v = next("dev-fee-pp")
                    .and_then(|v| {
                        v.parse::<u16>()
                            .map_err(|_| "--dev-fee-pp expects 0-10000".to_string())
                    })
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    });
                if v > 10_000 {
                    eprintln!("error: --dev-fee-pp expects 0-10000, got {v}");
                    std::process::exit(2);
                }
                dev_fee_pp = v;
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
    if force_fresh {
        apply_force_fresh(&state_file);
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
            if jackpot_info.is_some() {
                println!(
                    "draw divisors from node: jackpot {} / pot {}",
                    resolved_jackpot_divisor, resolved_pot_divisor
                );
            } else {
                println!(
                    "node did not answer getjackpotinfo; using default divisors jackpot {} / pot {}",
                    resolved_jackpot_divisor, resolved_pot_divisor
                );
            }
        }
        let node_chain = probe
            .call("getblockchaininfo", serde_json::json!([]))
            .ok()
            .and_then(|i| i.get("chain").and_then(|v| v.as_str()).map(str::to_string));
        let game_chain = match resolve_game_chain(node_chain.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        // Display label only: flag wins when given, else the node chain.
        let chain_label = match game_chain {
            GameChain::Mainnet => "mainnet",
            GameChain::Testnet => "testnet",
        };
        let display_network = match network {
            Some(v) => v,
            None => chain_label.to_string(),
        };
        let cfg = lm_stratum::node_provider::ProviderConfig::new(
            claim_jackpots,
            claim_pot,
            claim_faucet,
            claim_dev,
            claim_prize,
            resolved_jackpot_divisor,
            resolved_pot_divisor,
        )
        .with_restored_unmatured(read_unmatured(&state_file));
        let provider = match NodeProvider::connect_with_config(
            &node,
            &rpc_user,
            &rpc_pass,
            cfg,
        ) {
            Ok(p) => p,
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
        let token_count = worker_tokens.as_ref().map(|m| m.len());
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
        let claims = match claim_prize {
            false => claims,
            true => format!("{claims}, serving consensus draw claims"),
        };
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
        // Operator status: the three things a restart must never leave
        // ambiguous (chain, auth mode, state durability).
        println!("chain: {chain_label} (node {node})");
        println!(
            "auth: {}",
            match (&token_count, &pool_password) {
                (Some(n), _) => format!("{n} worker token(s)"),
                (None, Some(_)) => "pool password".to_string(),
                (None, None) => "open".to_string(),
            }
        );
        println!(
            "state: {}",
            match &state_file {
                Some(p) => format!("file {}", p.display()),
                None => "memory (a restart loses balances)".to_string(),
            }
        );
        if let Some(path) = &state_file {
            if let Err(e) = load_state(&server, path) {
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
                let p = path.clone();
                move || state_saver(srv, p)
            });
        }
        // Always watch for SIGTERM (the handler is installed at startup).
        std::thread::spawn({
            let srv = Arc::clone(&server);
            let p = state_file.clone();
            move || term_watcher(srv, p)
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
                network: display_network.clone(),
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
            let payout_state = state_file.clone();
            std::thread::spawn(move || payout_loop(srv, node_cfg, cfg, hist, payout_state));
        }
    } else if !mock {
        eprintln!(
            "error: no --node-rpc: pass --mock for the instant-block mock chain (dev/tests only)\n{USAGE}"
        );
        return std::process::ExitCode::from(2);
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
        let token_count = worker_tokens.as_ref().map(|m| m.len());
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
        // Loud on purpose: the mock mines worthless blocks by design.
        eprintln!("MOCK CHAIN — not connected to a node; blocks are worthless");
        println!("chain: mock (instant blocks, no node)");
        println!(
            "auth: {}",
            match (&token_count, &pool_password) {
                (Some(n), _) => format!("{n} worker token(s)"),
                (None, Some(_)) => "pool password".to_string(),
                (None, None) => "open".to_string(),
            }
        );
        println!(
            "state: {}",
            match &state_file {
                Some(p) => format!("file {}", p.display()),
                None => "memory (a restart loses balances)".to_string(),
            }
        );
        if payout_threshold > 0 || payout_exec {
            eprintln!("note: payouts need node mode (--node-rpc); flags ignored on the mock chain");
        }
        if let Some(path) = &state_file {
            if let Err(e) = load_state(&server, path) {
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
                move || state_saver(srv, p)
            });
        }
        // Always watch for SIGTERM (the handler is installed at startup).
        std::thread::spawn({
            let srv = Arc::clone(&server);
            let p = state_file.clone();
            move || term_watcher(srv, p)
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

    /// Pre-rename state files (written before the Divisions/Prize Fund
    /// rename) must load: per-block `league_overflow` maps onto the new
    /// field name via serde alias, and the retired pool-era top-level
    /// `league_pool` money key plus per-height `league_credits` are
    /// silently dropped (consensus pays winners directly now — doc 06
    /// §6.7; the pool keeps no prize balance). Per-block inflow and the
    /// display-feed credits still load: the explorer's per-block game
    /// view depends on them. This exact shape failed the live cutover
    /// restart once — pinned here so it can never regress.
    #[test]
    fn pre_rename_state_file_loads_via_aliases() {
        let old = r#"{"version":2,"ledger":{"owed":{"m1":5},"paid":{},"fees":0},
          "names":[],"blocks":[{"height":3,"finder":"m1","value":100,"bonus":0,"pot":0,
          "league_overflow":77,"faucet":0,"dev":0,"payouts":[]}],
          "league_pool":1234,"credited_total":100,"league_credits":[[3,77]],
          "agents":[],"shares":{"shares":[],"days":{},"last_seen":{},"comeback":{}}}"#;
        let st: PoolState = serde_json::from_str(old).expect("pre-rename state loads");
        assert_eq!(st.prize_credits, vec![(3u64, 77u64)]);
        assert_eq!(st.blocks[0].prize_inflow, 77);
        // New spelling round-trips too (no pool-era money keys: the pool
        // holds no prize balance since the consensus cutover).
        let new = serde_json::to_string(&st).expect("serialize");
        let back: PoolState = serde_json::from_str(&new).expect("reload");
        assert_eq!(back.blocks[0].prize_inflow, 77);
        assert!(!new.contains("prize_pool"));
        assert!(!new.contains("league_pool"));
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
        assert_eq!(row["division_class"], "cpu");
        assert_eq!(row["division_rank"], 4);
        assert_eq!(row["owed"], 50);
        assert_eq!(row["paid"], 500);
        // Offline: no live row, but the streak + rank + balances survive.
        let row = worker_row_json("w2", None, &streaks, &ranks, 10, 0, "", None, 200).unwrap();
        assert_eq!(row["online"], false);
        assert!(row["last_seen"].is_null());
        assert_eq!(row["streak"], 3);
        assert_eq!(row["division_rank"], 9);
        assert_eq!(row["name"], "w2"); // no registered name → the miner id
        // Never seen: None (the route 404s).
        assert!(worker_row_json("ghost", None, &streaks, &ranks, 0, 0, "", None, 200).is_none());
    }

    #[test]
    fn state_file_roundtrips_credited_total_and_seeds_it_from_blocks() {
        use lm_stratum::server::BlockRecord;
        // A stored counter survives the round trip untouched.
        let st = PoolState {
            version: STATE_VERSION,
            ledger: lm_pool::payout::PayoutLedger::default(),
            names: Vec::new(),
            blocks: Vec::new(),
            payout_settings: lm_stratum::server::PayoutSettings::new(),
            attestations: lm_stratum::server::Attestations::new(),
            credited_total: 9_999_999,
            prize_credits: Vec::new(),
            faucet_unmatured: Vec::new(),
            agents: Vec::new(),
            shares: lm_pool::ledger::Ledger::new(),
        };
        let back: PoolState =
            serde_json::from_str(&serde_json::to_string(&st).expect("serialize")).expect("deserialize");
        assert_eq!(back.credited_total, 9_999_999);
        // Pre-counter files (no credited_total key) default to 0 and get
        // seeded from the restored block records instead.
        let seeded = resolve_credited_total(
            0,
            &[
                BlockRecord { height: 1, finder: "a".to_string(), value: 1_000_000, bonus: 100_000, pot: 0, prize_inflow: 0, faucet: 0, dev: 0, prize_claim: 0, prize_winner: String::new(), payouts: vec![] },
                BlockRecord { height: 2, finder: "a".to_string(), value: 2_000_000, bonus: 0, pot: 300_000, prize_inflow: 0, faucet: 0, dev: 0, prize_claim: 0, prize_winner: String::new(), payouts: vec![] },
            ],
        );
        assert_eq!(seeded, 3_400_000);
        // An explicit stored value always wins, even with blocks present.
        assert_eq!(resolve_credited_total(42, &[
            BlockRecord { height: 1, finder: "a".to_string(), value: 1_000_000, bonus: 0, pot: 0, prize_inflow: 0, faucet: 0, dev: 0, prize_claim: 0, prize_winner: String::new(), payouts: vec![] },
        ]), 42);
        // Fresh pool: nothing to seed from.
        assert_eq!(resolve_credited_total(0, &[]), 0);
    }

    #[test]
    fn force_fresh_moves_state_aside_never_deletes() {
        // The corrupt-state error message names --force-fresh, so it must
        // exist and must not destroy the file it refuses to boot from.
        let dir = std::env::temp_dir().join(format!("pf-force-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let path = dir.join("pool-state.json");
        std::fs::write(&path, r#"{"version":2}"#).expect("write state");
        // No state file: no-op, no crash.
        apply_force_fresh(&None);
        apply_force_fresh(&Some(dir.join("missing.json")));
        // Present file: moved aside with a timestamp suffix, payload intact.
        apply_force_fresh(&Some(path.clone()));
        assert!(!path.exists(), "original must be gone");
        let mut moved: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p != &path)
            .collect();
        assert_eq!(moved.len(), 1, "exactly one backup: {moved:?}");
        let kept = moved.pop().expect("backup");
        assert!(
            kept.to_string_lossy().contains(".pre-fresh-"),
            "timestamped suffix: {kept:?}"
        );
        assert_eq!(std::fs::read_to_string(&kept).expect("read backup"), r#"{"version":2}"#);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_file_ignores_retired_pool_era_money_keys() {
        // The consensus cutover deleted the pool-side prize balance: old
        // state files carrying `prize_pool` / `league_pool` must still
        // load, with the money keys silently dropped (consensus pays
        // winners directly — doc 06 §6.7).
        let old = r#"{"version":2,"ledger":{"owed":{},"fees":0,"paid":{}},"names":[],"blocks":[],
          "prize_pool":260317,"league_pool":1234,"credited_total":0,"prize_credits":[],
          "faucet_unmatured":[],"agents":[],"shares":{"shares":[],"days":{},"last_seen":{},"comeback":{}}}"#;
        let back: PoolState = serde_json::from_str(old).expect("old pool-era file loads");
        assert_eq!(back.credited_total, 0);
        // ... and new files never carry them.
        let st = PoolState {
            version: STATE_VERSION,
            ledger: lm_pool::payout::PayoutLedger::default(),
            names: Vec::new(),
            blocks: Vec::new(),
            payout_settings: lm_stratum::server::PayoutSettings::new(),
            attestations: lm_stratum::server::Attestations::new(),
            credited_total: 1_234_567,
            prize_credits: vec![(290u64, 1_841_270u64)],
            faucet_unmatured: vec![(12u64, 1_000u64)],
            agents: vec![("w1".to_string(), "lm-miner/0.2 cpu".to_string())],
            shares: lm_pool::ledger::Ledger::new(),
        };
        let json = serde_json::to_string(&st).expect("serialize");
        assert!(!json.contains("prize_pool") && !json.contains("league_pool"));
        let back: PoolState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.credited_total, 1_234_567);
        assert_eq!(back.prize_credits, vec![(290u64, 1_841_270u64)]);
        assert_eq!(back.faucet_unmatured, vec![(12u64, 1_000u64)]);
        // Old state files (pre-prize_pool / pre-attestations /
        // pre-unmatured) still load.
        let old = r#"{"version":1,"ledger":{"owed":{},"fees":0,"paid":{}},"names":[],"blocks":[]}"#;
        let back: PoolState = serde_json::from_str(old).expect("old file");
        assert_eq!(back.credited_total, 0); // pre-counter files load
        assert!(back.attestations.is_empty());
        assert!(back.faucet_unmatured.is_empty());
        // A shares snapshot round-trips share data (streaks survive a
        // restart through the state file).
        let mut led = lm_pool::ledger::Ledger::new();
        led.submit(lm_pool::ledger::ShareRec { miner: "w1".to_string(), ts: 100, work: 10, overnight: false });
        led.submit(lm_pool::ledger::ShareRec { miner: "w1".to_string(), ts: 100 + 86_400, work: 10, overnight: false });
        let st2 = PoolState {
            version: STATE_VERSION,
            ledger: lm_pool::payout::PayoutLedger::default(),
            names: Vec::new(),
            blocks: Vec::new(),
            payout_settings: lm_stratum::server::PayoutSettings::new(),
            attestations: lm_stratum::server::Attestations::new(),
            credited_total: 0,
            prize_credits: Vec::new(),
            faucet_unmatured: Vec::new(),
            agents: Vec::new(),
            shares: led,
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
            version: STATE_VERSION,
            ledger: lm_pool::payout::PayoutLedger::default(),
            names: Vec::new(),
            blocks: Vec::new(),
            payout_settings: lm_stratum::server::PayoutSettings::new(),
            attestations: lm_stratum::server::Attestations::new(),
            credited_total: 9_999,
            prize_credits: Vec::new(),
            faucet_unmatured: vec![(7u64, 500u64)],
            agents: Vec::new(),
            shares: lm_pool::ledger::Ledger::new(),
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
    fn division_boards_json_ranks_within_family_and_flags_fresh_attestations() {
        use lm_pool::division::DeviceFamily;
        use lm_pool::ledger::BoardEntry;
        let entry = |miner: &str, class: DeviceFamily, work: u64| BoardEntry {
            miner: miner.to_string(),
            class,
            eff_work: work,
            days_active: 3,
            streak_days: 1,
            hps: 1,
        };
        let boards = lm_pool::ledger::rank_by_division(vec![
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
        let out = division_boards_json(&boards, &names, &atts, 200, 20);
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
        let stale = division_boards_json(
            &boards,
            &names,
            &atts,
            100 + lm_stratum::server::ATTESTATION_TTL_SECS + 1,
            20,
        );
        assert_eq!(stale[0]["entries"][0]["attested"], false);
        // The per-family cap holds.
        let capped = division_boards_json(&boards, &names, &atts, 200, 1);
        assert_eq!(capped[0]["entries"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn resolve_game_chain_uses_node_not_flag() {
        // Game identity comes ONLY from the verified node chain.
        assert_eq!(resolve_game_chain(Some("main")), Ok(GameChain::Mainnet));
        assert_eq!(resolve_game_chain(Some("test")), Ok(GameChain::Testnet));
        assert!(resolve_game_chain(Some("bogus")).is_err());
        assert!(resolve_game_chain(None).is_err());
    }

    #[test]
    fn resolve_divisions_uses_labels_and_polices_by_rate() {
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
            class: lm_pool::division::DeviceFamily::Phone, // placeholder, resolved below
            eff_work: 1,
            days_active: 1,
            streak_days: 0,
            hps,
        };
        let resolved = resolve_divisions(
            vec![
                entry("honest-arm", 1_500),
                entry("honest-gpu", 21_000),
                entry("lying-farm", 600_000),
                entry("silent", 15_000),
            ],
            &agents,
        );
        let division = |m: &str| {
            resolved
                .iter()
                .find(|e| e.miner == m)
                .expect("row")
                .class
                .as_str()
                .to_string()
        };
        assert_eq!(division("honest-arm"), "phone");
        assert_eq!(division("honest-gpu"), "gpu");
        // A farm cannot hide behind a cpu label.
        assert_eq!(division("lying-farm"), "farm");
        // An unlabeled miner falls back to the rate-implied family.
        assert_eq!(division("silent"), "cpu");
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

    /// Money is token-gated on token pools: an open pool serves the full
    /// worker row, a `--workers-file` pool 403s strangers. Stats (rates,
    /// streak, division) stay public either way — the gate only guards
    /// owed/paid reads.
    #[test]
    fn worker_api_money_needs_matching_token_on_token_pools() {
        let mut tokens = HashMap::new();
        tokens.insert("w1".to_string(), "tok1".to_string());
        let gated = spawn_server(
            MockProvider::new([0xff; 32], [0xff; 32], 1_000, 0),
            "127.0.0.1:0",
            None,
            Some(tokens),
            None,
        )
        .expect("spawn");
        // Authorize once over the wire so the token resolves to w1.
        let mut c = lm_stratum::client::PoolClient::connect(gated.addr()).expect("connect");
        assert!(c.subscribe("probe/0.1").expect("subscribe"));
        assert!(c.authorize_with("w1", "tok1").expect("authorize"));
        // No token, wrong token: 403 (money stays hidden).
        let (status, body) = worker_api(&gated, "w1", None);
        assert_eq!(status, "403 Forbidden");
        assert!(body.contains("token"));
        let (status, _) = worker_api(&gated, "w1", Some("Bearer nope"));
        assert_eq!(status, "403 Forbidden");
        // The matching token gets the row (with its money fields).
        let (status, body) = worker_api(&gated, "w1", Some("Bearer tok1"));
        assert_eq!(status, "200 OK");
        assert!(body.contains("\"owed\""));
        // Unknown workers also 403 on token pools (the gate runs before
        // the existence check, so there is no known/unknown oracle).
        let (status, _) = worker_api(&gated, "ghost", Some("Bearer tok1"));
        assert_eq!(status, "403 Forbidden");
        gated.stop();
        // Open pools keep the old public behavior.
        let open = spawn_server(
            MockProvider::new([0xff; 32], [0xff; 32], 1_000, 0),
            "127.0.0.1:0",
            None,
            None,
            None,
        )
        .expect("spawn");
        let mut c = lm_stratum::client::PoolClient::connect(open.addr()).expect("connect");
        assert!(c.subscribe("probe/0.1").expect("subscribe"));
        assert!(c.authorize_with("w1", "x").expect("authorize"));
        let (status, body) = worker_api(&open, "w1", None);
        assert_eq!(status, "200 OK");
        assert!(body.contains("\"owed\""));
        open.stop();
    }

    #[test]
    fn my_miner_falls_back_to_worker_lookup() {
        // Token pools gate /api/miner; open pools have no tokens at all.
        // The tab tries the token route first, then the public worker
        // row (read-only — settings stay token-only), and validates the
        // threshold client-side instead of letting junk silently clear it.
        assert!(INDEX_HTML.contains("fetch('api/miner'"));
        assert!(INDEX_HTML.contains("fetch('api/worker/'+encodeURIComponent(input)"));
        assert!(INDEX_HTML.contains("id=\"mopen\""));
        assert!(INDEX_HTML.contains("/^\\d+(\\.\\d{1,8})?$/"));
        assert!(INDEX_HTML.contains("open-pool rows are read-only"));
    }

    #[test]
    fn dashboard_escapes_untrusted_strings() {
        // Worker names and subscribe agents are arbitrary client input
        // rendered into innerHTML: the esc() helper must entity-escape,
        // and markup-building spots must not bypass it. A regression
        // here is stored XSS on the public dashboard.
        let esc = "const esc=v=>String(v??'-').replace(/[&<>\"']/g,";
        assert!(
            INDEX_HTML.contains(esc),
            "dashboard esc() must entity-escape &<>\"'"
        );
        // One payouts table: a duplicate id leaves the second table
        // permanently empty (and is invalid HTML).
        assert_eq!(INDEX_HTML.matches("id=\"payouts\"").count(), 1);
        // The footer carries markup, so it must use innerHTML (textContent
        // would print the raw <span>).
        assert!(INDEX_HTML.contains("getElementById('note').innerHTML"));
        // Wide tables scroll instead of breaking phone layouts.
        assert!(INDEX_HTML.contains("class=\"twrap\""));
        assert!(INDEX_HTML.contains(".twrap{overflow-x:auto}"));
        // Classes the markup references must exist.
        assert!(INDEX_HTML.contains(".muted{"));
        assert!(INDEX_HTML.contains(".gold{"));
        // The empty-workers row spans the full 12-column table.
        assert!(INDEX_HTML.contains("colspan=\"12\""));
        // Resilience: tab state in the URL hash, a stale-data banner, and
        // honest lookup errors (403/404 distinguished, not "never seen").
        assert!(INDEX_HTML.contains("location.hash"));
        assert!(INDEX_HTML.contains("id=\"stale\""));
        assert!(INDEX_HTML.contains("showing data from"));
        assert!(INDEX_HTML.contains("hidden on this token pool"));
        // Boards collapse when empty and say "top 20 of N" when capped.
        assert!(INDEX_HTML.contains("no qualifying workers this season: "));
        assert!(INDEX_HTML.contains("top 20 of "));
        // Cards name what they count.
        assert!(INDEX_HTML.contains("workers online / seen"));
    }
}
