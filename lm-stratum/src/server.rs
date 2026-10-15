//! Threaded reference pool server: miners in, ledger + money out.
//!
//! One OS thread per connection plus one accept thread; all shared state
//! behind short-lived mutexes, job broadcasts polled (no async runtime by
//! design — this must build and run on a miniPC with nothing but rustc).
//!
//! Per-submit flow: job lookup → dupe check → real LuckyHash verification
//! against the pool target → ledger share (work = expected max-target
//! hashes) → on network-target hit, authoritative chain submission via the
//! [`TemplateProvider`](crate::provider::TemplateProvider): reserve update,
//! league-pool credit, jackpot split over the PPLNS window, block record.
//! A share the chain rejects (height raced) is still an accepted share —
//! the pool's check is advisory, the chain's authoritative, and only the
//! chain mints money.

use crate::provider::TemplateProvider;
use crate::v1::{
    error_response, notify, response, work_for_share, worker_id, Dedupe, Job, JobRegistry, Message,
    Verdict,
};
use crate::v2::{
    from_frame, to_frame, LuckyJob, LuckySubmit, OpenMiningChannelError, OpenStandardMiningChannel,
    OpenStandardMiningChannelSuccess, SetTarget, SetupConnectionSuccess, SubmitSharesError,
    SubmitSharesSuccess, MSG_CLOSE_CHANNEL, MSG_LUCKY_SUBMIT, MSG_OPEN_STANDARD_MINING_CHANNEL,
    PROTOCOL_MINING,
};
use crate::v2_session::{accept_setup_connection, SessionError, V2Session};
use crate::vardiff::{target_for_work, Vardiff};
use crate::workerstats::{WorkerSnapshot, WorkerStats};
use lm_pool::ledger::{Ledger, MinerAgg, ShareRec, PPLNS_WINDOW_SECS as LEDGER_PPLNS};
use lm_pool::payout::PayoutLedger;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Background template refresh cadence. Matches the connection read-tick
/// granularity so new jobs reach miners as promptly as before; the
/// provider's own due-check (NodeProvider::REFRESH_INTERVAL) still gates
/// actual node polls.
const REFRESH_TICK_MS: u64 = 100;

/// One found block, as the pool paid it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockRecord {
    pub height: u64,
    pub finder: u64,
    /// Net block value the pool received (normal coinbase, no claims).
    pub value: u64,
    pub bonus: u64,
    /// Pot splash the coinbase claimed (0 without a pot event).
    pub pot: u64,
    pub league_overflow: u64,
    /// PPLNS split of every claimed amount (jackpot + pot), per miner.
    pub payouts: Vec<(u64, u64)>,
}
/// Retained block history (finding G spirit: the dashboard must never
/// OOM the pool; the cap is deep enough for every UI window).
pub const BLOCK_HISTORY_CAP: usize = 1000;

/// Pool counters.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub shares: u64,
    pub rejects: u64,
    pub blocks: u64,
}

struct JobState {
    job: Job,
    version: u64,
}

struct Shared<P> {
    provider: Mutex<P>,
    ledger: Mutex<Ledger>,
    blocks: Mutex<VecDeque<BlockRecord>>,
    stats: Mutex<Stats>,
    broadcast: Mutex<JobState>,
    /// Pool-wide work-identity dedupe: one share can be replayed on a
    /// fresh connection (empty per-connection job window) and each
    /// connection's registry must reject it, so the memory lives here and
    /// is locked around each evaluate — not inside the registries.
    dedupe: Mutex<Dedupe>,
    /// Per-worker dashboard stats (accepted/rejected, rolling hashrate,
    /// liveness) — internal/17 §17.1.
    workers: Mutex<WorkerStats>,
    /// Per-miner value accounting (internal/17 §17.4): what each miner
    /// earned from blocks (pro-rata) and splits, minus what paid out.
    balances: Mutex<PayoutLedger>,
    /// Per-miner payout settings (address/threshold overrides, internal/17).
    payout_settings: Mutex<PayoutSettings>,
    /// Pool fee in per-myriad, applied when blocks credit balances.
    pool_fee: Mutex<u16>,
    /// Dev-fee slice (miner id, per-myriad of the block), taken from the
    /// operator fee pot via `allocate_dev` — docs/04 §4.2. `None` = off.
    dev_fee: Mutex<Option<(u64, u16)>>,
    pool_password: Option<String>,
    /// Per-worker tokens (worker -> token). When set, it is authoritative:
    /// only listed workers with a matching token may authorize. Otherwise
    /// `pool_password` applies (open when unset).
    worker_tokens: Option<HashMap<String, String>>,
    /// TLS server config; `None` = plaintext wire (default).
    tls: Option<Arc<rustls::ServerConfig>>,
    job_counter: AtomicU64,
    shutdown: AtomicBool,
}

/// Per-miner payout settings (internal/17): an optional payout address
/// override (the default is the worker name, which doubles as an address)
/// and an optional payout threshold override (the default is the pool's
/// global threshold).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MinerSettings {
    /// Payout address override (validated before storing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Payout threshold override, base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u64>,
}

/// Per-miner payout settings keyed by miner id (persisted with the pool
/// state file).
pub type PayoutSettings = std::collections::BTreeMap<u64, MinerSettings>;

/// One miner's dashboard row + value totals (internal/17).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MinerRow {
    pub miner: u64,
    pub name: String,
    pub agent: String,
    pub shares: u64,
    pub rejects: u64,
    pub online: bool,
    pub last_seen: u64,
    /// Rolling hashrate over 60s/10m/1h/24h (H/s).
    pub rates: [f64; 4],
    /// Value owed but not yet paid (base units).
    pub owed: u64,
    /// Lifetime paid (base units).
    pub paid: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse a workers file: one `worker:token` per line; `#` comments and
/// blank lines ignored. Worker names are matched exactly (they double as
/// payout addresses); the token is everything after the first colon.
pub fn parse_workers_file(content: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for (n, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (worker, token) = line
            .split_once(':')
            .ok_or_else(|| format!("line {}: expected worker:token", n + 1))?;
        let (worker, token) = (worker.trim(), token.trim());
        if worker.is_empty() || token.is_empty() {
            return Err(format!("line {}: empty worker or token", n + 1));
        }
        if out.insert(worker.to_string(), token.to_string()).is_some() {
            return Err(format!("line {}: duplicate worker '{worker}'", n + 1));
        }
    }
    Ok(out)
}

/// Authorization decision. A per-worker token map (when configured) is
/// authoritative: the worker must exist and the token must match. Otherwise
/// the shared pool password applies (open when unset).
fn auth_ok(
    tokens: Option<&HashMap<String, String>>,
    password: Option<&str>,
    worker: &str,
    pass: &str,
) -> bool {
    if let Some(tokens) = tokens {
        return tokens.get(worker).map(|t| t == pass).unwrap_or(false);
    }
    match password {
        Some(pw) => pw == pass,
        None => true,
    }
}

/// Read the current broadcast job + version. Hot path for connection
/// threads: never touches the provider (finding D — they used to poll it
/// inline on every tick, serializing all miners behind one node RPC).
fn broadcast_job<P>(shared: &Shared<P>) -> (Job, u64) {
    let bc = shared.broadcast.lock().expect("broadcast");
    (bc.job.clone(), bc.version)
}

/// Background refresh loop: poll the provider and publish new jobs. The
/// ONLY thread that calls `provider.current()` after startup — a slow
/// node RPC stalls this thread, not the miners. Submits still take the
/// provider lock (serialized by design: last-write-wins would
/// double-submit a block), but nothing waits on a refresh anymore.
fn refresh_loop<P: TemplateProvider + 'static>(shared: Arc<Shared<P>>) {
    while !shared.shutdown.load(Ordering::Relaxed) {
        let tpl = shared.provider.lock().expect("provider").current();
        {
            // The broadcast guard must drop BEFORE the tick sleep: holding
            // it across the sleep starves every connection thread (release
            // caught it — a fast provider re-locks within nanoseconds, so
            // a waiting handler can lose the race for minutes; the e2e
            // suite timed out at 10 s per RPC).
            let mut bc = shared.broadcast.lock().expect("broadcast");
            // A new job exists only when the TEMPLATE BYTES change (height
            // or header). Timer-based rotation duplicated jobs whenever the
            // node's template cadence was slower than the timer: two
            // "different" jobs shared one header, so the miner re-found the
            // same nonce for each and the pool counted duplicate work as
            // separate shares (live-caught: nonce 20796 on jobs 1f/20/21).
            if tpl.height != bc.job.height || tpl.header != bc.job.header {
                let id = shared.job_counter.fetch_add(1, Ordering::SeqCst);
                bc.job = Job {
                    id: format!("{id:08x}"),
                    header: tpl.header,
                    height: tpl.height,
                    pool_target: tpl.pool_target,
                    net_target: tpl.net_target,
                };
                bc.version += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(REFRESH_TICK_MS));
    }
}

/// Shared money path for an accepted block-finding share (V1 and V2 call
/// this — one code path mints money): chain submission is authoritative,
/// then the league credit, the jackpot/pot PPLNS splits, per-miner value
/// accounting, and the block record. A share the chain rejects (height
/// raced) is still an accepted share — the pool's check is advisory, the
/// chain's authoritative, and only the chain mints money.
fn apply_block_found<P: TemplateProvider + 'static>(
    shared: &Shared<P>,
    miner_id: u64,
    job: &Job,
    nonce: u64,
) {
    let outcome = shared
        .provider
        .lock()
        .expect("provider")
        .submit_block(&job.header, job.height, nonce);
    if outcome.accepted {
        let now = now_secs();
        let mut ledger = shared.ledger.lock().expect("ledger");
        ledger.credit_league(outcome.league_overflow);
        // Both claimed amounts split over the PPLNS window (each with the
        // reference sqrt/cap rules); the combined list is what pays out.
        let mut payouts = ledger.jackpot_payout(outcome.bonus, LEDGER_PPLNS, now);
        payouts.extend(ledger.jackpot_payout(outcome.pot, LEDGER_PPLNS, now));
        // Per-miner value accounting (internal/17 §17.4): the normal block
        // value by PPLNS weight plus the block's own split payouts.
        let weights: Vec<(u64, u64)> = ledger
            .aggregate(LEDGER_PPLNS, now)
            .iter()
            .map(|a| (a.miner, a.eff_work))
            .collect();
        let mut balances = shared.balances.lock().expect("balances");
        let fee = *shared.pool_fee.lock().expect("pool fee");
        balances.credit_block(outcome.coinbase, &weights, fee);
        balances.credit_split(&payouts);
        if let Some((dev_id, pp)) = *shared.dev_fee.lock().expect("dev fee") {
            let dev = outcome.coinbase as u128 * u64::from(pp) as u128
                / lm_pool::payout::FEE_DENOM_PERMYRIAD as u128;
            let moved = balances.allocate_dev(dev_id, dev as u64);
            if moved > 0 {
                eprintln!(
                    "dev fee: {} -> miner {} ({} blocks paid)",
                    moved,
                    dev_id,
                    shared.blocks.lock().expect("blocks").len()
                );
            }
        }
        drop(balances);
        let mut blocks = shared.blocks.lock().expect("blocks");
        blocks.push_back(BlockRecord {
            height: job.height,
            finder: miner_id,
            value: outcome.coinbase,
            bonus: outcome.bonus,
            pot: outcome.pot,
            league_overflow: outcome.league_overflow,
            payouts,
        });
        while blocks.len() > BLOCK_HISTORY_CAP {
            blocks.pop_front();
        }
        drop(blocks);
        shared.stats.lock().expect("stats").blocks += 1;
    }
}

fn handle_conn<P: TemplateProvider + 'static>(stream: TcpStream, shared: Arc<Shared<P>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    // Kill Nagle: `writeln!` issues body and newline as two writes, and
    // Nagle + delayed ACK stretch the gap to ~100 ms — right at the read
    // tick, which is how half-submits got lost.
    let _ = stream.set_nodelay(true);
    let io = match crate::tls::accept(stream, shared.tls.as_ref()) {
        Ok(io) => io,
        Err(_) => return,
    };
    let mut stream = BufReader::new(io);
    // 16 jobs at 30s rotation = 8 minutes of job lifetime: a slow find
    // (p=1/65536 has a long tail) must still be submittable.
    let mut registry = JobRegistry::new(16);
    let mut authorized: Option<(String, u64)> = None;
    // Agent string from subscribe (params[0]); labels the worker's device
    // on the dashboard.
    let mut agent = String::new();
    // Per-connection vardiff: template rotations reset the override target;
    // retargets derive conn-local jobs (same height, fresh id, clean=false).
    let mut vardiff = Vardiff::new();
    let mut _vd_target: Option<([u8; 32], u64)> = None; // (pool_target, template height)
    let mut seen_version = 0u64;
    let mut sent_height = 0u64;
    let mut line = String::new();

    // Seed the registry so the first job is evaluable even before broadcast.
    let (first, _) = broadcast_job(&shared);
    registry.issue(first);

    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Broadcast check first: fresh jobs must reach miners promptly.
        let (cur, cur_version) = broadcast_job(&shared);
        if authorized.is_some() && cur_version != seen_version {
            seen_version = cur_version;
            _vd_target = Some((cur.pool_target, cur.height));
            registry.issue(cur.clone());
            let clean = cur.height != sent_height;
            sent_height = cur.height;
            let msg = notify(&cur, clean);
            if writeln!(stream.get_mut(), "{msg}").is_err() {
                break;
            }
        }
        // Partial reads MUST survive the tick timeout: a message split
        // across it (TCP segmentation, Nagle) was silently discarded by the
        // old clear-then-read pattern — half a submit got eaten, the miner
        // never got a response and reconnected (live-caught with pcaps:
        // submit body ACKed, pool silent 10 s, then FIN). Keep accumulating
        // in `line` until a full line arrives.
        match stream.read_line(&mut line) {
            Ok(0) => break, // peer closed
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
        let msg_line = std::mem::take(&mut line);
        let trimmed = msg_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Message = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(_) => {
                let _ = writeln!(
                    stream.get_mut(),
                    "{}",
                    error_response(&serde_json::Value::Null, -32700, "parse error")
                );
                continue;
            }
        };
        let id = msg.id.clone().unwrap_or(serde_json::Value::Null);
        let method = msg.method.as_deref().unwrap_or("");
        match method {
            "mining.subscribe" => {
                if let Some(a) = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                {
                    agent = a.to_string();
                }
                let _ = writeln!(
                    stream.get_mut(),
                    "{}",
                    response(&id, serde_json::json!([["mining.notify", "sid"], "", 0]))
                );
            }
            "mining.authorize" => {
                let arr = msg.params.as_ref().and_then(|p| p.as_array());
                let worker = arr
                    .and_then(|a| a.first())
                    .and_then(|w| w.as_str())
                    .unwrap_or("anonymous")
                    .to_string();
                let pass = arr
                    .and_then(|a| a.get(1))
                    .and_then(|w| w.as_str())
                    .unwrap_or("");
                if !auth_ok(
                    shared.worker_tokens.as_ref(),
                    shared.pool_password.as_deref(),
                    &worker,
                    pass,
                ) {
                    let _ = writeln!(
                        stream.get_mut(),
                        "{}",
                        error_response(&id, -32600, "invalid credentials")
                    );
                    continue;
                }
                let mid = worker_id(&worker);
                shared.workers.lock().expect("workers").authorized(
                    mid,
                    &worker,
                    &agent,
                    now_secs(),
                );
                authorized = Some((worker, mid));
                _vd_target = Some((cur.pool_target, cur.height));
                // Immediately push the current job (clean start).
                registry.issue(cur.clone());
                seen_version = cur_version;
                sent_height = cur.height;
                let _ = writeln!(stream.get_mut(), "{}", response(&id, serde_json::Value::Bool(true)));
                let msg = notify(&cur, true);
                if writeln!(stream.get_mut(), "{msg}").is_err() {
                    break;
                }
            }
            "mining.submit" => {
                let Some((worker_name, miner_id)) = authorized.clone() else {
                    let _ = writeln!(stream.get_mut(), "{}", error_response(&id, -32600, "authorize first"));
                    continue;
                };
                let arr = msg.params.as_ref().and_then(|p| p.as_array());
                let (job_id, nonce_hex) = match arr {
                    Some(a) if a.len() >= 3 => (
                        a[1].as_str().unwrap_or("").to_string(),
                        a[2].as_str().unwrap_or("").to_string(),
                    ),
                    _ => {
                        let _ = writeln!(
                            stream.get_mut(),
                            "{}",
                            error_response(&id, -32600, "submit needs [worker, job, nonce]")
                        );
                        continue;
                    }
                };
                // Worker field is informational; the authorized identity pays.
                let _ = worker_name;
                let (verdict, submit) = {
                    let mut dedupe = shared.dedupe.lock().expect("dedupe");
                    registry.evaluate(&job_id, &nonce_hex, &mut dedupe)
                };
                let (accept, err) = verdict.response();
                match (accept, submit) {
                    (true, Some(s)) => {
                        let now = now_secs();
                        let work = work_for_share(&s.job.pool_target);
                        shared.ledger.lock().expect("ledger").submit(ShareRec {
                            miner: miner_id,
                            ts: now,
                            work,
                            overnight: false,
                        });
                        shared.stats.lock().expect("stats").shares += 1;
                        shared
                            .workers
                            .lock()
                            .expect("workers")
                            .accepted(miner_id, work, now);
                        // Vardiff: adapt this connection's pool target, same template.
                        if let Some(new_work) = vardiff.record(now, work) {
                            let new_target = target_for_work(new_work);
                            _vd_target = Some((new_target, s.job.height));
                            let nid = shared.job_counter.fetch_add(1, Ordering::SeqCst);
                            let derived = Job {
                                id: format!("{nid:08x}"),
                                header: s.job.header.clone(),
                                height: s.job.height,
                                pool_target: new_target,
                                net_target: s.job.net_target,
                            };
                            registry.issue(derived.clone());
                            let _ = writeln!(stream.get_mut(), "{}", notify(&derived, false));
                        }
                        if matches!(verdict, Verdict::Accept { block: true }) {
                            apply_block_found(&shared, miner_id, &s.job, s.nonce);
                        }
                        let _ =
                            writeln!(stream.get_mut(), "{}", response(&id, serde_json::Value::Bool(true)));
                    }
                    _ => {
                        shared.stats.lock().expect("stats").rejects += 1;
                        shared
                            .workers
                            .lock()
                            .expect("workers")
                            .rejected(miner_id, now_secs());
                        let (code, text) = err.unwrap_or((-32600, "invalid submit".to_string()));
                        let _ = writeln!(stream.get_mut(), "{}", error_response(&id, code, &text));
                    }
                }
            }
            "" => {
                let _ = writeln!(stream.get_mut(), "{}", error_response(&id, -32600, "missing method"));
            }
            other => {
                let _ = writeln!(
                    stream.get_mut(),
                    "{}",
                    error_response(&id, -32601, &format!("unknown method {other}"))
                );
            }
        }
    }
}

/// Issue a LuckyJob for a V1 template (fresh job id, shared registry
/// window) with an explicit pool target override, and return it. The
/// broadcast path passes the template target (or the vardiff override for
/// the current height); retargets pass the new target with `clean: false`.
/// Job ids stay unique per connection through the shared counter.
fn issue_lucky_job(
    counter: &AtomicU64,
    registry: &mut JobRegistry,
    channel_id: u32,
    template: &Job,
    pool_target: [u8; 32],
    clean: bool,
) -> LuckyJob {
    let job_id = counter.fetch_add(1, Ordering::SeqCst) as u32;
    registry.issue(Job {
        id: format!("v2-{job_id:08x}"),
        header: template.header.clone(),
        height: template.height,
        pool_target,
        net_target: template.net_target,
    });
    LuckyJob {
        channel_id,
        job_id,
        height: template.height,
        header: template.header.clone(),
        pool_target,
        net_target: template.net_target,
        clean,
    }
}

/// Send the current broadcast template as a [`LuckyJob`] when it is newer
/// than `seen_version`. `vd_target` is the vardiff override, honored only
/// while it names the current height (a new template resets the channel
/// to the template target, mirroring V1). Returns the new
/// `(version, sent_height)` after a successful send; `None` when already
/// current or the send failed. The V1 [`JobRegistry`] is reused with
/// `v2-{job_id:08x}` ids so staleness, cross-connection dupe rejection
/// and PoW evaluation are the exact V1 rules — one validation path for
/// both wires.
fn send_lucky_job<P: TemplateProvider + 'static, S: Read + Write>(
    session: &mut V2Session<S>,
    shared: &Shared<P>,
    registry: &mut JobRegistry,
    channel_id: u32,
    seen_version: u64,
    sent_height: u64,
    vd_target: Option<([u8; 32], u64)>,
) -> Option<(u64, u64)> {
    let (cur, cur_version) = broadcast_job(shared);
    if cur_version == seen_version {
        return None;
    }
    let clean = cur.height != sent_height;
    let target = match vd_target {
        Some((t, h)) if h == cur.height => t,
        _ => cur.pool_target,
    };
    let job = issue_lucky_job(
        &shared.job_counter,
        registry,
        channel_id,
        &cur,
        target,
        clean,
    );
    session.send(&to_frame(&job)).ok()?;
    Some((cur_version, cur.height))
}

/// V2 connection handler (behind `--v2-bind`): Noise NX handshake, version
/// negotiation, channel open, then [`LuckyJob`] dispatch and
/// [`LuckySubmit`] validation. Auth mirrors V1 (`auth_ok`): the worker name
/// is `OpenStandardMiningChannel.user_identity` and the pool password /
/// worker token rides in `SetupConnection.device_id` (the only free-form
/// client-chosen string in the V2 handshake). One standard channel per
/// connection for this milestone. Vardiff mirrors V1 (same controller,
/// same 20 s/share target): retargets send core `SetTarget` plus a fresh
/// same-header job, and a new template resets the channel to the template
/// target.
fn handle_v2_conn<P: TemplateProvider + 'static>(
    stream: TcpStream,
    shared: Arc<Shared<P>>,
    authority: ([u8; 32], [u8; 32]),
    cert_validity: Duration,
) {
    let _ = stream.set_nodelay(true);
    // Handshake bound: a port scan that connects and says nothing must not
    // park a thread forever.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut session = match V2Session::accept(stream, authority, cert_validity) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = session.set_read_timeout(Some(Duration::from_millis(100)));
    // Version negotiation: mining protocol, must span version 2.
    let setup = match accept_setup_connection(&mut session, |s| {
        if s.protocol != PROTOCOL_MINING || s.max_version < 2 || s.min_version > 2 {
            Err((0, "unsupported protocol/version (need mining v2)".to_string()))
        } else {
            Ok(SetupConnectionSuccess {
                used_version: 2,
                flags: 0,
            })
        }
    }) {
        Ok(s) => s,
        Err(_) => return,
    };
    // Channel open + auth.
    let open_frame = match session.recv() {
        Ok(f) => f,
        Err(_) => return,
    };
    if open_frame.msg_type != MSG_OPEN_STANDARD_MINING_CHANNEL {
        return;
    }
    let open = match from_frame::<OpenStandardMiningChannel>(&open_frame) {
        Ok(o) => o,
        Err(_) => return,
    };
    if !auth_ok(
        shared.worker_tokens.as_ref(),
        shared.pool_password.as_deref(),
        &open.user_identity,
        &setup.device_id,
    ) {
        let _ = session.send(&to_frame(&OpenMiningChannelError {
            request_id: open.request_id,
            error_code: "invalid credentials".to_string(),
        }));
        return;
    }
    let miner_id = worker_id(&open.user_identity);
    shared.workers.lock().expect("workers").authorized(
        miner_id,
        &open.user_identity,
        &format!("v2 {} {}", setup.vendor, setup.hardware_version),
        now_secs(),
    );
    let channel_id = shared.job_counter.fetch_add(1, Ordering::SeqCst) as u32;
    let (cur, _) = broadcast_job(&shared);
    if session
        .send(&to_frame(&OpenStandardMiningChannelSuccess {
            request_id: open.request_id,
            channel_id,
            target: cur.pool_target,
            extranonce_prefix: Vec::new(),
            group_channel_id: 0,
        }))
        .is_err()
    {
        return;
    }
    // 16 jobs of registry window, matching the V1 connection lifetime.
    let mut registry = JobRegistry::new(16);
    let mut seen_version = 0u64;
    let mut sent_height = 0u64;
    // Per-channel vardiff (same controller and pacing as V1) plus the
    // active target override (target, template height).
    let mut vardiff = Vardiff::new();
    let mut vd_target: Option<([u8; 32], u64)> = None;
    // The broadcast always has a job (seeded at startup); a failed first
    // send means a dead peer.
    match send_lucky_job(
        &mut session,
        &shared,
        &mut registry,
        channel_id,
        seen_version,
        sent_height,
        vd_target,
    ) {
        Some((v, h)) => {
            seen_version = v;
            sent_height = h;
        }
        None => return,
    }

    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Fresh jobs first: a rotation between the last recv and this
        // submit must reach the miner before its ack does.
        if let Some((v, h)) = send_lucky_job(
            &mut session,
            &shared,
            &mut registry,
            channel_id,
            seen_version,
            sent_height,
            vd_target,
        ) {
            seen_version = v;
            sent_height = h;
        }
        // Timeout ticks are the job-poll heartbeat (same 100 ms cadence as
        // V1's read tick). Sends are single write_all calls on both sides,
        // so a timeout means silence, not a trickled frame — nothing
        // partial is lost by continuing here.
        let frame = match session.recv() {
            Ok(f) => f,
            Err(SessionError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => break,
        };
        match frame.msg_type {
            MSG_LUCKY_SUBMIT => {
                let sub = match from_frame::<LuckySubmit>(&frame) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                if sub.channel_id != channel_id {
                    continue;
                }
                let id = format!("v2-{:08x}", sub.job_id);
                let nonce_hex = format!("{:016x}", sub.nonce);
                let (verdict, submit) = {
                    let mut dedupe = shared.dedupe.lock().expect("dedupe");
                    registry.evaluate(&id, &nonce_hex, &mut dedupe)
                };
                let (accept, err) = verdict.response();
                match (accept, submit) {
                    (true, Some(s)) => {
                        let now = now_secs();
                        let work = work_for_share(&s.job.pool_target);
                        shared.ledger.lock().expect("ledger").submit(ShareRec {
                            miner: miner_id,
                            ts: now,
                            work,
                            overnight: false,
                        });
                        shared.stats.lock().expect("stats").shares += 1;
                        shared
                            .workers
                            .lock()
                            .expect("workers")
                            .accepted(miner_id, work, now);
                        let ack = SubmitSharesSuccess {
                            channel_id,
                            last_sequence_number: sub.sequence_number,
                            new_submits_accepted_count: 1,
                            new_shares_sum: work,
                        };
                        if session.send(&to_frame(&ack)).is_err() {
                            break;
                        }
                        // Vardiff: adapt this channel's pool target on the
                        // same template (mirrors V1). The miner keeps its
                        // nonce cursor: same header, fresh job id.
                        if let Some(new_work) = vardiff.record(now, work) {
                            let new_target = target_for_work(new_work);
                            vd_target = Some((new_target, s.job.height));
                            if session
                                .send(&to_frame(&SetTarget {
                                    channel_id,
                                    maximum_target: new_target,
                                }))
                                .is_err()
                            {
                                break;
                            }
                            let job = issue_lucky_job(
                                &shared.job_counter,
                                &mut registry,
                                channel_id,
                                &s.job,
                                new_target,
                                false,
                            );
                            if session.send(&to_frame(&job)).is_err() {
                                break;
                            }
                        }
                        if matches!(verdict, Verdict::Accept { block: true }) {
                            apply_block_found(&shared, miner_id, &s.job, s.nonce);
                        }
                    }
                    _ => {
                        shared.stats.lock().expect("stats").rejects += 1;
                        shared
                            .workers
                            .lock()
                            .expect("workers")
                            .rejected(miner_id, now_secs());
                        let (_, code) = err.unwrap_or((-32600, "invalid submit".to_string()));
                        let nack = SubmitSharesError {
                            channel_id,
                            sequence_number: sub.sequence_number,
                            error_code: code,
                        };
                        if session.send(&to_frame(&nack)).is_err() {
                            break;
                        }
                    }
                }
            }
            MSG_CLOSE_CHANNEL => break,
            // Ignore anything else (future extensions must not kill the
            // connection).
            _ => continue,
        }
    }
}

/// A running reference pool.
pub struct RunningServer<P: TemplateProvider + 'static> {
    addr: SocketAddr,
    shared: Arc<Shared<P>>,
    accept: Option<JoinHandle<()>>,
    refresh: Option<JoinHandle<()>>,
    /// V2 (Noise) accept loop; `None` until [`RunningServer::spawn_v2_listener`].
    v2_accept: Option<JoinHandle<()>>,
}

impl<P: TemplateProvider + 'static> RunningServer<P> {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn stats(&self) -> Stats {
        self.stats_clone()
    }

    fn stats_clone(&self) -> Stats {
        self.shared.stats.lock().expect("stats").clone()
    }

    pub fn blocks(&self) -> Vec<BlockRecord> {
        self.shared
            .blocks
            .lock()
            .expect("blocks")
            .iter()
            .cloned()
            .collect()
    }

    /// Ledger snapshot for `--state-file` persistence (internal/17).
    pub fn ledger_snapshot(&self) -> PayoutLedger {
        self.shared.balances.lock().expect("balances").clone()
    }

    /// Restore a persisted ledger (replaces the fresh one).
    pub fn restore_ledger(&self, ledger: PayoutLedger) {
        *self.shared.balances.lock().expect("balances") = ledger;
    }

    /// Persisted worker-name map (miner id → payout address).
    pub fn names(&self) -> Vec<(u64, String)> {
        self.shared.workers.lock().expect("workers").names()
    }

    /// Restore persisted worker names (payout destinations survive a
    /// restart even before miners reconnect).
    pub fn restore_names(&self, names: &[(u64, String)]) {
        self.shared
            .workers
            .lock()
            .expect("workers")
            .restore_names(names);
    }

    /// Restore persisted block history; the lifetime counter follows so
    /// the dashboard reads continuously across restarts.
    pub fn restore_blocks(&self, blocks: Vec<BlockRecord>) {
        let n = blocks.len() as u64;
        let mut b = self.shared.blocks.lock().expect("blocks");
        b.clear();
        b.extend(blocks);
        drop(b);
        let mut st = self.shared.stats.lock().expect("stats");
        st.blocks = st.blocks.max(n);
    }

    pub fn aggregate(&self, window_secs: u64, now: u64) -> Vec<MinerAgg> {
        self.shared
            .ledger
            .lock()
            .expect("ledger")
            .aggregate(window_secs, now)
    }

    pub fn league_balance(&self) -> u64 {
        self.shared.ledger.lock().expect("ledger").league_balance()
    }

    /// Per-worker snapshots for the dashboard/API (internal/17 §17.1).
    pub fn workers(&self, now: u64) -> Vec<WorkerSnapshot> {
        self.shared.workers.lock().expect("workers").snapshot(now)
    }

    /// All miner balances (owed) for the dashboard/API, miner-ascending.
    /// Payout selection itself lives in the payout path.
    pub fn balances(&self) -> Vec<(u64, u64)> {
        self.shared
            .balances
            .lock()
            .expect("balances")
            .select_due(1)
            .into_iter()
            .collect()
    }

    /// Debit after a successful on-chain send.
    pub fn record_paid(&self, miner: u64, amount: u64) {
        self.shared
            .balances
            .lock()
            .expect("balances")
            .record_paid(miner, amount);
    }

    /// Lifetime paid to one miner (stats reconciliation).
    pub fn paid(&self, miner: u64) -> u64 {
        self.shared.balances.lock().expect("balances").paid(miner)
    }

    /// One miner's row (worker stats + owed/paid). `None` when unknown.
    #[must_use]
    pub fn miner_row(&self, miner: u64, now: u64) -> Option<MinerRow> {
        let snap = self
            .shared
            .workers
            .lock()
            .expect("workers")
            .snapshot(now)
            .into_iter()
            .find(|w| w.miner == miner)?;
        let bal = self.shared.balances.lock().expect("balances");
        Some(MinerRow {
            miner,
            name: snap.name,
            agent: snap.agent,
            shares: snap.shares,
            rejects: snap.rejects,
            online: snap.online,
            last_seen: snap.last_seen,
            rates: snap.rates,
            owed: bal.owed(miner),
            paid: bal.paid(miner),
        })
    }

    /// Per-miner payout settings snapshot (persisted with the state file).
    #[must_use]
    pub fn payout_settings(&self) -> PayoutSettings {
        self.shared.payout_settings.lock().expect("settings").clone()
    }

    /// Replace the payout settings (state restore).
    pub fn restore_payout_settings(&self, settings: PayoutSettings) {
        *self.shared.payout_settings.lock().expect("settings") = settings;
    }

    /// Set one miner's payout settings (replaces the whole entry).
    pub fn set_payout_settings(&self, miner: u64, settings: MinerSettings) {
        self.shared
            .payout_settings
            .lock()
            .expect("settings")
            .insert(miner, settings);
    }

    /// Remove one miner's settings entirely. Returns whether one existed.
    pub fn clear_payout_settings(&self, miner: u64) -> bool {
        self.shared
            .payout_settings
            .lock()
            .expect("settings")
            .remove(&miner)
            .is_some()
    }

    /// Miner id owning a worker token (`--workers-file`), for the
    /// miner-facing HTTP endpoints. `None` when auth is off or the token is
    /// unknown.
    #[must_use]
    pub fn miner_for_token(&self, token: &str) -> Option<u64> {
        if token.is_empty() {
            return None;
        }
        let tokens = self.shared.worker_tokens.as_ref()?;
        let worker = tokens
            .iter()
            .find(|(_, t)| t.as_str() == token)
            .map(|(w, _)| w.as_str())?;
        self.shared
            .workers
            .lock()
            .expect("workers")
            .miner_for_name(worker)
    }

    /// `(owed_total, paid_total, fees)` — the payout reconciliation triple.
    pub fn payout_totals(&self) -> (u64, u64, u64) {
        self.shared.balances.lock().expect("balances").totals()
    }

    /// Pool fee in per-myriad for block credits (0 = none).
    pub fn set_pool_fee(&self, fee_permyriad: u16) {
        *self.shared.pool_fee.lock().expect("pool fee") = fee_permyriad;
    }

    /// Dev-fee slice (docs/04 §4.2): `pp` per-myriad of each block's
    /// coinbase moves from the operator fee pot to `addr`'s balance. The
    /// address is registered as a payout name so the payout loop pays it
    /// like any worker. No-op when `addr` is empty or `pp` is 0.
    pub fn set_dev_fee(&self, addr: &str, pp: u16) {
        if addr.is_empty() || pp == 0 {
            return;
        }
        let id = worker_id(addr);
        *self.shared.dev_fee.lock().expect("dev fee") = Some((id, pp));
        self.restore_names(&[(id, addr.to_string())]);
    }

    /// Signal shutdown and wait for the accept loop (connection threads exit
    /// on their next 100 ms tick) and the refresh loop.
    pub fn stop(mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
        if let Some(h) = self.v2_accept.take() {
            let _ = h.join();
        }
        if let Some(h) = self.refresh.take() {
            let _ = h.join();
        }
    }

    /// Serve Stratum V2 (Noise NX + Lucky jobs) on `addr`, sharing this
    /// server's provider, ledger, broadcast, dedupe and balances — one
    /// money path for V1 and V2 miners. Behind the pool's `--v2-bind`
    /// flag; V1 stays the default wire.
    ///
    /// # Errors
    /// When `addr` cannot be bound.
    pub fn spawn_v2_listener(
        &mut self,
        addr: &str,
        authority: ([u8; 32], [u8; 32]),
        cert_validity: Duration,
    ) -> std::io::Result<SocketAddr> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let shared = Arc::clone(&self.shared);
        let handle = std::thread::spawn(move || {
            while !shared.shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let s = shared.clone();
                        std::thread::spawn(move || {
                            handle_v2_conn(stream, s, authority, cert_validity)
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => {
                        eprintln!("pool(v2): accept error (continuing): {e}");
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        });
        self.v2_accept = Some(handle);
        Ok(local)
    }
}

impl RunningServer<crate::provider::MockProvider> {
    pub fn chain_height(&self) -> u64 {
        self.shared.provider.lock().expect("provider").height()
    }

    pub fn reserve(&self) -> u64 {
        self.shared.provider.lock().expect("provider").reserve()
    }

    pub fn chain_blocks(&self) -> u64 {
        self.shared.provider.lock().expect("provider").blocks()
    }
}

/// Bind on `addr` (use `127.0.0.1:0` for an ephemeral test port) and serve.
pub fn spawn_server<P: TemplateProvider + 'static>(
    provider: P,
    addr: &str,
    password: Option<String>,
    worker_tokens: Option<HashMap<String, String>>,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> std::io::Result<RunningServer<P>> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    let tpl = provider.current();
    let job = Job {
        id: "00000000".to_string(),
        header: tpl.header,
        height: tpl.height,
        pool_target: tpl.pool_target,
        net_target: tpl.net_target,
    };
    let shared = Arc::new(Shared {
        provider: Mutex::new(provider),
        ledger: Mutex::new(Ledger::new()),
        blocks: Mutex::new(VecDeque::new()),
        stats: Mutex::new(Stats::default()),
        broadcast: Mutex::new(JobState { job, version: 1 }),
        dedupe: Mutex::new(Dedupe::default()),
        workers: Mutex::new(WorkerStats::default()),
        balances: Mutex::new(PayoutLedger::default()),
        payout_settings: Mutex::new(PayoutSettings::new()),
        pool_fee: Mutex::new(0),
        dev_fee: Mutex::new(None),
        pool_password: password,
        worker_tokens,
        tls,
        job_counter: AtomicU64::new(1),
        shutdown: AtomicBool::new(false),
    });
    let accept_shared = shared.clone();
    let accept = std::thread::spawn(move || {
        while !accept_shared.shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let s = accept_shared.clone();
                    std::thread::spawn(move || handle_conn(stream, s));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    // A transient accept error (EMFILE, ECONNABORTED, ...)
                    // must not deafen the pool: the kernel completes
                    // handshakes into the listen backlog without the
                    // application, so exiting this loop would turn every
                    // later connect into a silent hang — the client
                    // connects, then times out with no server thread ever
                    // spawned. Log it and keep serving instead.
                    eprintln!("pool: accept error (continuing): {e}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    });
    let refresh_shared = shared.clone();
    let refresh = std::thread::spawn(move || refresh_loop(refresh_shared));
    Ok(RunningServer {
        addr: local,
        shared,
        accept: Some(accept),
        refresh: Some(refresh),
        v2_accept: None,
    })
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    fn tokens() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("worker-a".to_string(), "tok-a".to_string());
        m.insert("worker-b".to_string(), "tok-b".to_string());
        m
    }

    #[test]
    fn tokens_are_authoritative_when_set() {
        let t = tokens();
        assert!(auth_ok(Some(&t), None, "worker-a", "tok-a"));
        assert!(!auth_ok(Some(&t), None, "worker-a", "wrong"));
        assert!(!auth_ok(Some(&t), None, "unknown", "tok-a"));
        assert!(!auth_ok(Some(&t), None, "worker-a", ""));
        // Even a shared password does not bypass the token allowlist.
        assert!(!auth_ok(Some(&t), Some("shared"), "worker-a", "shared"));
    }

    #[test]
    fn shared_password_fallback_and_open_mode() {
        assert!(auth_ok(None, Some("pw"), "anyone", "pw"));
        assert!(!auth_ok(None, Some("pw"), "anyone", "nope"));
        assert!(auth_ok(None, None, "anyone", ""));
        assert!(auth_ok(None, None, "anyone", "anything"));
    }

    #[test]
    fn workers_file_parses_and_validates() {
        let m = parse_workers_file(
            "# comment\n\nworker-a: tok-a\nworker-b:tok:b\n",
        )
        .expect("parse");
        assert_eq!(m.get("worker-a").map(String::as_str), Some("tok-a"));
        // Token keeps colons after the first separator.
        assert_eq!(m.get("worker-b").map(String::as_str), Some("tok:b"));

        assert!(parse_workers_file("no-colon-line").is_err());
        assert!(parse_workers_file("worker:").is_err());
        assert!(parse_workers_file(":token").is_err());
        assert!(parse_workers_file("w:1\nw:2").is_err());
    }

    /// Regression (live-caught with pcaps on testnet): a message split
    /// across the 100 ms read tick — submit body, then the newline after a
    /// Nagle-sized gap — must still be answered. The old clear-then-read
    /// pattern discarded the partial line, the miner never got a response,
    /// timed out and reconnected in a loop.
    #[test]
    fn split_line_across_read_tick_is_not_discarded() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let provider = crate::provider::MockProvider::new(
            [0xffu8; 32],
            [0x00u8; 32],
            1_000_000,
            0,
        );
        let server = spawn_server(provider, "127.0.0.1:0", None, None, None)
            .expect("spawn");
        let stream = TcpStream::connect(server.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let mut r = BufReader::new(stream.try_clone().expect("clone"));
        let mut w = stream;

        w.write_all(
            b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[]}\n",
        )
        .expect("subscribe");
        w.write_all(
            b"{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"split\",\"\"]}\n",
        )
        .expect("authorize");

        // Drain up to the authorize response (notify may follow it).
        let mut saw_authorize = false;
        let mut line = String::new();
        while !saw_authorize {
            line.clear();
            let n = r.read_line(&mut line).expect("read");
            assert!(n > 0, "server closed before authorize response");
            saw_authorize = line.contains("\"id\":2");
        }

        // The bug: body and newline in separate writes >100 ms apart.
        w.write_all(
            b"{\"id\":3,\"method\":\"mining.submit\",\"params\":[\"split\",\"x\",\"0000000000000001\"]}",
        )
        .expect("submit body");
        std::thread::sleep(Duration::from_millis(150));
        w.write_all(b"\n").expect("submit newline");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) => panic!("server closed before submit response"),
                Ok(_) => {
                    if line.contains("\"id\":3") {
                        return; // answered — bug fixed
                    }
                }
                Err(_) => break, // timeout
            }
        }
        panic!("split submit got no response within 3 s (partial line discarded)");
    }

    /// Miner-facing auth + payout settings (internal/17): a worker token
    /// resolves to the miner id only after authorize, and settings
    /// round-trip through the server.
    #[test]
    fn miner_token_maps_to_the_worker_and_settings_round_trip() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let provider = crate::provider::MockProvider::new(
            [0xffu8; 32],
            [0x00u8; 32],
            1_000_000,
            0,
        );
        let mut toks = HashMap::new();
        toks.insert("worker-a".to_string(), "tok-a".to_string());
        let server = spawn_server(provider, "127.0.0.1:0", None, Some(toks), None)
            .expect("spawn");
        // No worker has connected: the token cannot resolve yet.
        assert_eq!(server.miner_for_token("tok-a"), None);
        assert_eq!(server.miner_for_token(""), None);

        let stream = TcpStream::connect(server.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let mut r = BufReader::new(stream.try_clone().expect("clone"));
        let mut w = stream;
        w.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[]}\n")
            .expect("subscribe");
        w.write_all(
            b"{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"worker-a\",\"tok-a\"]}\n",
        )
        .expect("authorize");
        let mut saw_authorize = false;
        let mut line = String::new();
        while !saw_authorize {
            line.clear();
            let n = r.read_line(&mut line).expect("read");
            assert!(n > 0, "server closed before authorize response");
            saw_authorize = line.contains("\"id\":2");
        }
        let miner = server
            .miner_for_token("tok-a")
            .expect("token resolves to the miner");
        assert!(server.miner_for_token("wrong").is_none());

        // Settings round-trip + clear.
        server.set_payout_settings(
            miner,
            MinerSettings {
                address: Some("addr-x".to_string()),
                threshold: Some(42),
            },
        );
        let s = server.payout_settings();
        assert_eq!(
            s.get(&miner).and_then(|s| s.address.as_deref()),
            Some("addr-x")
        );
        assert_eq!(s.get(&miner).and_then(|s| s.threshold), Some(42));
        assert!(server.clear_payout_settings(miner));
        assert!(!server.clear_payout_settings(miner));
        assert!(server.payout_settings().is_empty());

        // The miner row is visible once connected.
        let row = server.miner_row(miner, now_secs()).expect("row");
        assert_eq!(row.name, "worker-a");
        assert_eq!(row.shares, 0);
        assert_eq!(row.owed, 0);
        assert_eq!(row.paid, 0);
        server.stop();
    }

    /// V2 vardiff (mirrors V1 pacing): 8 fast shares on a static template
    /// retarget the channel harder via core `SetTarget` plus a fresh
    /// same-header job carrying the new target.
    #[test]
    fn v2_vardiff_retargets_with_set_target_and_job() {
        use crate::v2::{
            LuckyJob, LuckySubmit, OpenStandardMiningChannel,
            OpenStandardMiningChannelSuccess, SetTarget, SetupConnection, SubmitSharesSuccess,
            MSG_LUCKY_JOB, MSG_SET_TARGET, MSG_SUBMIT_SHARES_SUCCESS, PROTOCOL_MINING,
        };
        use std::net::TcpStream;
        use std::time::Duration;

        // Tight net target: shares accept, never blocks → height static.
        let provider =
            crate::provider::MockProvider::new([0xffu8; 32], [0x00u8; 32], 1_000_000, 0);
        let mut server = spawn_server(provider, "127.0.0.1:0", None, None, None).expect("spawn");
        let authority = crate::v2_session::generate_authority();
        let v2_addr = server
            .spawn_v2_listener("127.0.0.1:0", authority, Duration::from_secs(3600))
            .expect("v2 listen");
        let stream = TcpStream::connect(v2_addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut session =
            crate::v2_session::V2Session::connect(stream, Some(authority.0)).expect("handshake");
        crate::v2_session::setup_connection(
            &mut session,
            &SetupConnection {
                protocol: PROTOCOL_MINING,
                min_version: 2,
                max_version: 2,
                flags: 0,
                endpoint_host: "x".to_string(),
                endpoint_port: 1,
                vendor: "t".to_string(),
                hardware_version: "cpu".to_string(),
                firmware: "0.2".to_string(),
                device_id: "x".to_string(),
            },
        )
        .expect("negotiate");
        session
            .send(&crate::v2::to_frame(&OpenStandardMiningChannel {
                request_id: 1,
                user_identity: "vd-miner".to_string(),
                nominal_hash_rate: 0.0,
                max_target: [0xffu8; 32],
            }))
            .expect("open");
        let opened = crate::v2::from_frame::<OpenStandardMiningChannelSuccess>(
            &session.recv().expect("open success"),
        )
        .expect("decode open");
        let channel_id = opened.channel_id;
        let first_target = opened.target;
        let job_frame = session.recv().expect("job");
        assert_eq!(job_frame.msg_type, MSG_LUCKY_JOB);
        let job = LuckyJob::from_frame(&job_frame).expect("decode job");
        assert_eq!(job.pool_target, first_target);
        // 8 fast shares, distinct nonces; the ack always arrives before
        // any retarget message (the server sends it first).
        for i in 0..8u64 {
            session
                .send(&crate::v2::to_frame(&LuckySubmit {
                    channel_id,
                    job_id: job.job_id,
                    sequence_number: i as u32,
                    nonce: i,
                }))
                .expect("submit");
            let ack_frame = session.recv().expect("ack");
            assert_eq!(ack_frame.msg_type, MSG_SUBMIT_SHARES_SUCCESS, "submit {i}");
            let ack = SubmitSharesSuccess::from_frame(&ack_frame).expect("decode ack");
            assert_eq!(ack.last_sequence_number, i as u32);
        }
        // Retarget: SetTarget first, then a same-header job with the target.
        let st_frame = session.recv().expect("settarget");
        assert_eq!(st_frame.msg_type, MSG_SET_TARGET);
        let st = SetTarget::from_frame(&st_frame).expect("decode target");
        assert_eq!(st.channel_id, channel_id);
        assert!(
            st.maximum_target < first_target,
            "8 instant shares must step harder"
        );
        let job2_frame = session.recv().expect("job2");
        assert_eq!(job2_frame.msg_type, MSG_LUCKY_JOB);
        let job2 = LuckyJob::from_frame(&job2_frame).expect("decode job2");
        assert_eq!(job2.header, job.header, "same template, new target");
        assert_eq!(job2.pool_target, st.maximum_target);
        assert!(!job2.clean, "same height is not clean");
        server.stop();
    }
}
