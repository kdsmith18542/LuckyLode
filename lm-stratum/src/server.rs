//! Threaded reference pool server: miners in, ledger + money out.
//!
//! One OS thread per connection plus one accept thread; all shared state
//! behind short-lived mutexes, job broadcasts polled (no async runtime by
//! design — this must build and run on a miniPC with nothing but rustc).
//!
//! Per-submit flow: job lookup → dupe check → real EmberHash verification
//! against the pool target → ledger share (work = expected max-target
//! hashes) → on network-target hit, authoritative chain submission via the
//! [`TemplateProvider`](crate::provider::TemplateProvider): reserve update,
//! league-pool credit, jackpot split over the PPLNS window, block record.
//! A share the chain rejects (height raced) is still an accepted share —
//! the pool's check is advisory, the chain's authoritative, and only the
//! chain mints money.

use crate::provider::TemplateProvider;
use crate::v1::{
    error_response, notify, response, worker_id, work_for_share, Dedupe, Job, JobRegistry, Message,
    Verdict,
};
use crate::vardiff::{target_for_work, Vardiff};
use crate::workerstats::{WorkerSnapshot, WorkerStats};
use lm_pool::ledger::{Ledger, MinerAgg, ShareRec, PPLNS_WINDOW_SECS as LEDGER_PPLNS};
use lm_pool::payout::PayoutLedger;
use std::io::{BufRead, BufReader, Write};
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
#[derive(Debug, Clone)]
pub struct BlockRecord {
    pub height: u64,
    pub finder: u64,
    /// Net block value the pool received (normal coinbase, no claims).
    pub value: u64,
    pub bonus: u64,
    pub league_overflow: u64,
    pub payouts: Vec<(u64, u64)>,
}
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
    blocks: Mutex<Vec<BlockRecord>>,
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
    /// Pool fee in per-myriad, applied when blocks credit balances.
    pool_fee: Mutex<u16>,
    job_counter: AtomicU64,
    shutdown: AtomicBool,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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

fn handle_conn<P: TemplateProvider + 'static>(stream: TcpStream, shared: Arc<Shared<P>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;
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
            if writeln!(writer, "{msg}").is_err() {
                break;
            }
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // peer closed
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
                continue;
            }
            Err(_) => break,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Message = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(_) => {
                let _ = writeln!(writer, "{}", error_response(&serde_json::Value::Null, -32700, "parse error"));
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
                    writer,
                    "{}",
                    response(&id, serde_json::json!([["mining.notify", "sid"], "", 0]))
                );
            }
            "mining.authorize" => {
                let worker = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.as_array())
                    .and_then(|a| a.first())
                    .and_then(|w| w.as_str())
                    .unwrap_or("anonymous")
                    .to_string();
                let mid = worker_id(&worker);
                shared
                    .workers
                    .lock()
                    .expect("workers")
                    .authorized(mid, &worker, &agent, now_secs());
                authorized = Some((worker, mid));
                _vd_target = Some((cur.pool_target, cur.height));
                // Immediately push the current job (clean start).
                registry.issue(cur.clone());
                seen_version = cur_version;
                sent_height = cur.height;
                let _ = writeln!(writer, "{}", response(&id, serde_json::Value::Bool(true)));
                let msg = notify(&cur, true);
                if writeln!(writer, "{msg}").is_err() {
                    break;
                }
            }
            "mining.submit" => {
                let Some((worker_name, miner_id)) = authorized.clone() else {
                    let _ = writeln!(writer, "{}", error_response(&id, -32600, "authorize first"));
                    continue;
                };
                let arr = msg.params.as_ref().and_then(|p| p.as_array());
                let (job_id, nonce_hex) = match arr {
                    Some(a) if a.len() >= 3 => (
                        a[1].as_str().unwrap_or("").to_string(),
                        a[2].as_str().unwrap_or("").to_string(),
                    ),
                    _ => {
                        let _ = writeln!(writer, "{}", error_response(&id, -32600, "submit needs [worker, job, nonce]"));
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
                            let _ = writeln!(writer, "{}", notify(&derived, false));
                        }
                        if matches!(verdict, Verdict::Accept { block: true }) {
                            let outcome = shared
                                .provider
                                .lock()
                                .expect("provider")
                                .submit_block(&s.job.header, s.job.height, s.nonce);
                            if outcome.accepted {
                                let mut ledger = shared.ledger.lock().expect("ledger");
                                ledger.credit_league(outcome.league_overflow);
                                let payouts = ledger.jackpot_payout(outcome.bonus, LEDGER_PPLNS, now);
                                // Per-miner value accounting (internal/17 §17.4):
                                // the normal block value by PPLNS weight plus
                                // the block's own split payouts.
                                let weights: Vec<(u64, u64)> = ledger
                                    .aggregate(LEDGER_PPLNS, now)
                                    .iter()
                                    .map(|a| (a.miner, a.eff_work))
                                    .collect();
                                let mut balances = shared.balances.lock().expect("balances");
                                let fee = *shared.pool_fee.lock().expect("pool fee");
                                balances.credit_block(outcome.coinbase, &weights, fee);
                                balances.credit_split(&payouts);
                                drop(balances);
                                shared.blocks.lock().expect("blocks").push(BlockRecord {
                                    height: s.job.height,
                                    finder: miner_id,
                                    value: outcome.coinbase,
                                    bonus: outcome.bonus,
                                    league_overflow: outcome.league_overflow,
                                    payouts,
                                });
                                shared.stats.lock().expect("stats").blocks += 1;
                            }
                        }
                        let _ = writeln!(writer, "{}", response(&id, serde_json::Value::Bool(true)));
                    }
                    _ => {
                        shared.stats.lock().expect("stats").rejects += 1;
                        shared
                            .workers
                            .lock()
                            .expect("workers")
                            .rejected(miner_id, now_secs());
                        let (code, text) = err.unwrap_or(( -32600, "invalid submit".to_string()));
                        let _ = writeln!(writer, "{}", error_response(&id, code, &text));
                    }
                }
            }
            "" => {
                let _ = writeln!(writer, "{}", error_response(&id, -32600, "missing method"));
            }
            other => {
                let _ = writeln!(
                    writer,
                    "{}",
                    error_response(&id, -32601, &format!("unknown method {other}"))
                );
            }
        }
    }
}

/// A running reference pool.
pub struct RunningServer<P: TemplateProvider + 'static> {
    addr: SocketAddr,
    shared: Arc<Shared<P>>,
    accept: Option<JoinHandle<()>>,
    refresh: Option<JoinHandle<()>>,
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
        self.shared.blocks.lock().expect("blocks").clone()
    }

    pub fn aggregate(&self, window_secs: u64, now: u64) -> Vec<MinerAgg> {
        self.shared.ledger.lock().expect("ledger").aggregate(window_secs, now)
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
        self.shared.balances.lock().expect("balances").record_paid(miner, amount);
    }

    /// Pool fee in per-myriad for block credits (0 = none).
    pub fn set_pool_fee(&self, fee_permyriad: u16) {
        *self.shared.pool_fee.lock().expect("pool fee") = fee_permyriad;
    }

    /// Signal shutdown and wait for the accept loop (connection threads exit
    /// on their next 100 ms tick) and the refresh loop.
    pub fn stop(mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
        if let Some(h) = self.refresh.take() {
            let _ = h.join();
        }
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
        blocks: Mutex::new(Vec::new()),
        stats: Mutex::new(Stats::default()),
        broadcast: Mutex::new(JobState { job, version: 1 }),
        dedupe: Mutex::new(Dedupe::default()),
        workers: Mutex::new(WorkerStats::default()),
        balances: Mutex::new(PayoutLedger::default()),
        pool_fee: Mutex::new(0),
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
    Ok(RunningServer { addr: local, shared, accept: Some(accept), refresh: Some(refresh) })
}
