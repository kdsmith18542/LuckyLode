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
    error_response, notify, response, worker_id, work_for_share, Job, JobRegistry, Message, Verdict,
};
use crate::vardiff::{target_for_work, Vardiff};
use lm_pool::ledger::{Ledger, MinerAgg, ShareRec, PPLNS_WINDOW_SECS as LEDGER_PPLNS};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One found block, as the pool paid it.
#[derive(Debug, Clone)]
pub struct BlockRecord {
    pub height: u64,
    pub finder: u64,
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
    /// When this job was issued; jobs rotate on a timer even without a
    /// height change (real-pool behavior: fresh curtime, fresh template).
    /// Height-only rotation starved miners between blocks (live-caught:
    /// a claim-mode test miner waited 300s for a job that never came
    /// because no block landed, so no new job was ever issued).
    rotated: std::time::Instant,
}

/// Job freshness window: rotate every N seconds regardless of height.
pub const JOB_ROTATE_SECS: u64 = 30;

struct Shared<P> {
    provider: Mutex<P>,
    ledger: Mutex<Ledger>,
    blocks: Mutex<Vec<BlockRecord>>,
    stats: Mutex<Stats>,
    broadcast: Mutex<JobState>,
    job_counter: AtomicU64,
    shutdown: AtomicBool,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Poll the template; on height change issue a new job and bump the version.
/// Returns the current job (after a possible rotation).
fn poll_template<P: TemplateProvider>(shared: &Shared<P>) -> Job {
    let tpl = shared.provider.lock().expect("provider").current();
    let mut bc = shared.broadcast.lock().expect("broadcast");
    let stale = bc.rotated.elapsed() >= std::time::Duration::from_secs(JOB_ROTATE_SECS);
    if tpl.height != bc.job.height || stale {
        let id = shared.job_counter.fetch_add(1, Ordering::SeqCst);
        bc.job = Job {
            id: format!("{id:08x}"),
            header: tpl.header,
            height: tpl.height,
            pool_target: tpl.pool_target,
            net_target: tpl.net_target,
        };
        bc.version += 1;
        bc.rotated = std::time::Instant::now();
    }
    bc.job.clone()
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
    let mut registry = JobRegistry::new(4);
    let mut authorized: Option<(String, u64)> = None;
    // Per-connection vardiff: template rotations reset the override target;
    // retargets derive conn-local jobs (same height, fresh id, clean=false).
    let mut vardiff = Vardiff::new();
    let mut _vd_target: Option<([u8; 32], u64)> = None; // (pool_target, template height)
    let mut seen_version = 0u64;
    let mut sent_height = 0u64;
    let mut line = String::new();

    // Seed the registry so the first job is evaluable even before broadcast.
    let first = poll_template(&shared);
    registry.issue(first.clone());

    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            break;
        }
        // Broadcast check first: fresh jobs must reach miners promptly.
        let cur = poll_template(&shared);
        let cur_version = shared.broadcast.lock().expect("broadcast").version;
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
                let (verdict, submit) = registry.evaluate(&job_id, &nonce_hex);
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
                                shared.blocks.lock().expect("blocks").push(BlockRecord {
                                    height: s.job.height,
                                    finder: miner_id,
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

    /// Signal shutdown and wait for the accept loop (connection threads exit
    /// on their next 100 ms tick).
    pub fn stop(mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
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
        broadcast: Mutex::new(JobState { job, version: 1, rotated: std::time::Instant::now() }),
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
                Err(_) => break,
            }
        }
    });
    Ok(RunningServer { addr: local, shared, accept: Some(accept) })
}
