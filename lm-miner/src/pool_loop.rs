//! Pool-mining runtime shared by every Lucky Lode miner surface: the CLI
//! (`lm-miner pool`), and embedded consumers (the wallet's in-app miner,
//! the Android JNI facade). One loop, both wires (V1 JSON and V2 Noise),
//! identical grind semantics — chunked searches that adopt newer jobs,
//! nonce-cursor preservation across same-header vardiff rotations, the
//! post-submit settle, backoff reconnects, and the rotating dev fee.
//!
//! Embedding contract: [`run`] owns the loop on the caller's thread and
//! reports progress through an [`Event`] sink; callers that need a
//! start/stop handle use [`PoolMiner`], which spawns [`run`] on a CPU
//! backend and answers stop within one chunk (keep `chunk` small on
//! constrained devices). The CLI never sets the stop flag and maps
//! [`Outcome`] to exit codes, so the binary's behavior is the loop's
//! behavior.

use crate::thermal::MiningPolicy;
use crate::Solution;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(feature = "opencl")]
use crate::gpu::GpuSearcher;

/// TLS options for the pool connection (V1 only today).
#[derive(Debug, Clone, Default)]
pub struct TlsCfg {
    /// Server name to verify; defaults to the host part of `pool`.
    pub server_name: Option<String>,
    /// Extra CA (PEM) to trust on top of the Mozilla root store.
    pub ca_pem: Option<String>,
    /// Skip verification (tests only — never a public pool).
    pub insecure: bool,
}

/// Full pool-mining configuration. `agent: None` selects the default
/// advertisement below (binary name + crate version + device).
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub pool: String,
    pub worker: String,
    pub agent: Option<String>,
    pub password: String,
    pub threads: usize,
    pub start: u64,
    /// Nonce budget per job (the pool rotates templates anyway).
    pub count: u64,
    /// Grind granularity — the stop-flag and job-adoption granularity.
    pub chunk: u64,
    pub max_shares: u64,
    pub job_timeout_secs: u64,
    pub max_temp: i64,
    pub duty: i64,
    /// Exit on connection loss instead of reconnecting (scripted use).
    pub fail_fast: bool,
    pub tls: Option<TlsCfg>,
    pub v2: bool,
    pub v2_authority: Option<[u8; 32]>,
    /// Dev-fee percent of hash time (0 = off; needs `dev_address`).
    pub dev_fee_pct: f64,
    pub dev_address: Option<String>,
    /// Seconds between [`Event::Rate`] reports.
    pub report_secs: u64,
}

impl PoolConfig {
    /// Library defaults: same grind budget/timeout as the CLI, but no dev
    /// fee (embedded callers opt in explicitly).
    pub fn new(pool: String, worker: String) -> Self {
        Self {
            pool,
            worker,
            agent: None,
            password: "x".to_string(),
            threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            start: 0,
            count: 1_000_000,
            chunk: 65_536,
            max_shares: u64::MAX,
            job_timeout_secs: 30,
            max_temp: crate::thermal::DEFAULT_MAX_TEMP_C,
            duty: 100,
            fail_fast: false,
            tls: None,
            v2: false,
            v2_authority: None,
            dev_fee_pct: 0.0,
            dev_address: None,
            report_secs: 30,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.pool.is_empty() {
            return Err("pool address is required (HOST:PORT)".to_string());
        }
        if self.worker.is_empty() {
            return Err("worker must not be empty".to_string());
        }
        if self.threads == 0 {
            return Err("threads must be >= 1".to_string());
        }
        if self.count == 0 {
            return Err("count must be >= 1".to_string());
        }
        Ok(())
    }
}

/// Progress from the mining loop. Owned strings so sinks can stash them
/// (the mobile stats buffer, the wallet log) without borrowing the loop.
#[derive(Debug, Clone)]
pub enum Event {
    /// A session was established (or re-established after a drop).
    Connected { v2: bool, worker: String, detail: String },
    /// The dev-fee rotation is mining as a different worker.
    DevFee { worker: String },
    ShareAccepted { job: String, nonce: u64 },
    ShareRejected { job: String, error: String },
    /// Rolling hashrate over the last `window_secs`.
    Rate { hps: f64, hashes: u64, window_secs: u64 },
    /// The connection dropped (or never established); the loop sleeps
    /// `wait_secs` (0 = immediate) and reconnects unless fail-fast.
    Reconnect { reason: String, wait_secs: u64 },
    /// No job arrived within the timeout; the loop reconnects.
    Idle { timeout_secs: u64, accepted: u64, submitted: u64 },
    /// The nonce budget for a job ran out; waiting for a new template.
    JobExhausted { job: String },
    /// A job with a new header was adopted (same-header vardiff
    /// rotations keep the nonce cursor and stay quiet).
    JobAdopted { job: String, height: u64 },
    /// Everything else worth a line (fee notes, GPU status).
    Note(String),
}

/// Terminal state of the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `max_shares` reached (CLI exit 0).
    SessionComplete { accepted: u64, submitted: u64 },
    /// The stop flag was observed (embedded callers).
    Stopped { accepted: u64, submitted: u64 },
    /// fail-fast surfaced an error (CLI exit 1).
    Failed { error: String },
}

/// Grind engine: CPU threads or one GPU (OpenCL build only — the Android
/// facade builds without the `opencl` feature, so `Backend::Gpu` does not
/// exist there).
pub enum Backend {
    Cpu { threads: usize },
    #[cfg(feature = "opencl")]
    Gpu(GpuSearcher),
}

impl Backend {
    /// Agent device label (pool dashboards group by it).
    pub fn device(&self) -> &'static str {
        match self {
            Backend::Cpu { .. } => "cpu",
            #[cfg(feature = "opencl")]
            Backend::Gpu(_) => "opencl",
        }
    }

    /// The `<detail>` half of the session line ("4 threads" /
    /// "gpu: <name>, batch 512").
    pub fn detail(&self) -> String {
        match self {
            Backend::Cpu { threads } => format!("{threads} threads"),
            #[cfg(feature = "opencl")]
            Backend::Gpu(g) => format!("gpu: {}, batch {}", g.name(), g.batch()),
        }
    }

    /// One chunk of the grind (public: the CLI's `mine` path uses the
    /// same backend the pool loop does).
    pub fn search(
        &self,
        header: &[u8],
        height: u64,
        target: &[u8; 32],
        start: u64,
        count: u64,
        policy: &MiningPolicy,
    ) -> Result<Option<Solution>, String> {
        match self {
            Backend::Cpu { threads } => Ok(crate::search_with_policy(
                header, height, target, start, count, *threads, policy,
            )),
            #[cfg(feature = "opencl")]
            Backend::Gpu(g) => g.search(header, height, target, start, count, policy),
        }
    }
}

/// Sleep `max`, aborting early when `stop` is set. Returns false when
/// the wait was cut short.
fn sleep_stop(max: Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + max;
    while Instant::now() < end {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200).min(end - Instant::now()));
    }
    !stop.load(Ordering::Relaxed)
}

/// Resolve `HOST:PORT` to (host, socket addr). Hostnames are supported so
/// TLS can verify the certificate against the pool name.
pub fn resolve_pool(pool: &str) -> Result<(String, SocketAddr), String> {
    use std::net::ToSocketAddrs;
    let host = pool
        .rsplit_once(':')
        .map(|(h, _)| h.trim_start_matches('[').trim_end_matches(']').to_string())
        .ok_or_else(|| format!("pool expects HOST:PORT, got '{pool}'"))?;
    let addr = pool
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve '{pool}': {e}"))?
        .next()
        .ok_or_else(|| format!("no addresses for '{pool}'"))?;
    Ok((host, addr))
}

/// Dev-fee rotation (docs/04 §4.2): a percent of hash time mines to the
/// dev address. The pool pays the *authorized* identity, so the fee is
/// implemented by reconnecting as a different worker — never by
/// relabeling shares. Returns the worker to mine as right now.
fn dev_worker(cfg: &PoolConfig, t0: Instant) -> String {
    const PERIOD_SECS: u64 = 3600;
    let addr = cfg.dev_address.as_deref().unwrap_or("");
    if cfg.dev_fee_pct <= 0.0 || addr.is_empty() {
        return cfg.worker.clone();
    }
    let elapsed = Instant::now().duration_since(t0).as_secs();
    let dev_secs = (PERIOD_SECS as f64 * cfg.dev_fee_pct / 100.0) as u64;
    if dev_secs == 0 || elapsed % PERIOD_SECS >= dev_secs {
        cfg.worker.clone()
    } else {
        addr.to_string()
    }
}

/// Default agent advertisement (`lm-miner/<crate version>`), shared with
/// the CLI help/`--version` so build identity never disagrees with itself.
pub const DEFAULT_AGENT: &str = concat!("lm-miner/", env!("CARGO_PKG_VERSION"));

/// The resolved agent string: explicit, or the default with the device
/// suffix (pools label workers by it).
fn agent_of(cfg: &PoolConfig, backend: &Backend) -> String {
    match &cfg.agent {
        Some(a) => a.clone(),
        None => format!("{DEFAULT_AGENT} {}", backend.device()),
    }
}

/// Connect + subscribe + authorize in one step so the session loop can
/// re-establish a dropped connection with backoff (pool restarts are
/// routine in the live stack).
fn connect_session(
    addr: SocketAddr,
    agent: &str,
    worker: &str,
    password: &str,
    tls: Option<&TlsCfg>,
) -> Result<lm_stratum::client::PoolClient, String> {
    use lm_stratum::client::PoolClient;
    let mut client = match tls {
        Some(t) => PoolClient::connect_tls(
            addr,
            t.server_name.as_deref().unwrap_or_default(),
            t.ca_pem.as_deref().map(std::path::Path::new),
            t.insecure,
            Duration::from_secs(10),
        )
        .map_err(|e| e.to_string())?,
        None => PoolClient::connect(addr).map_err(|e| e.to_string())?,
    };
    match client.subscribe(agent) {
        Ok(true) => {}
        Ok(false) => return Err("pool refused subscribe".to_string()),
        Err(e) => {
            // The rustls handshake runs lazily on first I/O, so TLS
            // pointed at a plaintext port fails HERE — not in
            // connect_tls — when the server drops the ClientHello bytes
            // as a garbage line. Name the likely cause instead of
            // surfacing a bare EOF that reads like a network outage.
            return Err(if tls.is_some() {
                format!(
                    "subscribe failed over TLS: {e} (the pool probably does not serve TLS on this port — the reference pool's public stratum port is plaintext)"
                )
            } else {
                format!("subscribe failed: {e}")
            });
        }
    }
    match client.authorize_with(worker, password) {
        Ok(true) => {}
        Ok(false) => return Err(format!("pool refused authorize for '{worker}'")),
        Err(e) => return Err(format!("authorize failed: {e}")),
    }
    Ok(client)
}

/// Run the pool-mining loop until `max_shares`, a fail-fast error, or the
/// stop flag. One call = one thread: the loop owns the connection and
/// grind cadence exactly as `lm-miner pool` always has.
pub fn run(
    cfg: &PoolConfig,
    backend: &Backend,
    policy: &MiningPolicy,
    stop: &AtomicBool,
    sink: &mut dyn FnMut(Event),
) -> Outcome {
    if let Err(e) = cfg.validate() {
        return Outcome::Failed { error: e };
    }
    let (pool_host, addr) = match resolve_pool(&cfg.pool) {
        Ok(v) => v,
        Err(e) => return Outcome::Failed { error: e },
    };
    let agent = agent_of(cfg, backend);
    if cfg.v2 {
        run_v2(cfg, backend, policy, stop, sink, agent, pool_host, addr)
    } else {
        run_v1(cfg, backend, policy, stop, sink, agent, addr)
    }
}

/// V1 (JSON stratum) session loop.
fn run_v1(
    cfg: &PoolConfig,
    backend: &Backend,
    policy: &MiningPolicy,
    stop: &AtomicBool,
    sink: &mut dyn FnMut(Event),
    agent: String,
    addr: SocketAddr,
) -> Outcome {
    use lm_stratum::v1::Job;
    let mut submitted = 0u64;
    let mut accepted = 0u64;
    // Grind each job in chunks and adopt newer jobs as they arrive: one
    // search per rotation left the GPU idle ~80% of the time (the pool's
    // template rotates every ~20 s, the card finishes a search in ~4 s).
    let chunk = cfg.chunk.min(cfg.count);
    let mut hashed: u64 = 0;
    let mut report_at = Instant::now() + Duration::from_secs(cfg.report_secs.max(1));
    let t0 = Instant::now();
    // A dropped pool connection is a normal event (pool restart, box
    // reboot): reconnect with capped backoff and keep the session
    // counters instead of exiting — the miner is a service.
    let mut backoff = Duration::from_secs(1);
    let stopped = |accepted: u64, submitted: u64| Outcome::Stopped { accepted, submitted };
    'session: loop {
        if stop.load(Ordering::Relaxed) {
            return stopped(accepted, submitted);
        }
        let session_worker = dev_worker(cfg, t0);
        if session_worker != cfg.worker {
            sink(Event::DevFee { worker: session_worker.clone() });
        }
        let tls_name = cfg
            .tls
            .as_ref()
            .and_then(|t| t.server_name.clone())
            .or_else(|| cfg.pool.rsplit_once(':').map(|(h, _)| h.to_string()));
        let tls = cfg.tls.as_ref().map(|t| TlsCfg {
            server_name: Some(tls_name.clone().unwrap_or_default()),
            ca_pem: t.ca_pem.clone(),
            insecure: t.insecure,
        });
        let mut client = match connect_session(addr, &agent, &session_worker, &cfg.password, tls.as_ref()) {
            Ok(c) => c,
            Err(e) => {
                if cfg.fail_fast {
                    return Outcome::Failed { error: format!("cannot reach pool: {e}") };
                }
                sink(Event::Reconnect {
                    reason: format!("pool connect failed: {e}"),
                    wait_secs: backoff.as_secs(),
                });
                if !sleep_stop(backoff, stop) {
                    return stopped(accepted, submitted);
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue 'session;
            }
        };
        backoff = Duration::from_secs(1);
        sink(Event::Connected {
            v2: false,
            worker: session_worker.clone(),
            detail: backend.detail(),
        });
        let mut current: Option<Job> = None;
        let mut pos = cfg.start;
        loop {
            if stop.load(Ordering::Relaxed) {
                drop(client);
                return stopped(accepted, submitted);
            }
            if dev_worker(cfg, t0) != session_worker {
                continue 'session;
            }
            if submitted >= cfg.max_shares {
                return Outcome::SessionComplete { accepted, submitted };
            }
            if Instant::now() >= report_at {
                let secs = cfg.report_secs.max(1) as f64;
                sink(Event::Rate {
                    hps: hashed as f64 / secs,
                    hashes: hashed,
                    window_secs: secs as u64,
                });
                hashed = 0;
                report_at = Instant::now() + Duration::from_secs(cfg.report_secs.max(1));
            }
            // Non-blocking: pick up any newer job sitting in the buffer.
            // A vardiff retarget keeps the SAME header with a fresh job id —
            // keep the nonce position then, or the range gets re-ground and
            // the pool dedupe rejects the replayed shares (found in testing:
            // ~10% of shares were duplicate re-finds after rotations).
            loop {
                match client.poll_job() {
                    Ok(Some(j)) => {
                        if current.as_ref().map(|c| c.id != j.id).unwrap_or(true) {
                            let same_header = current
                                .as_ref()
                                .map(|c| c.header == j.header)
                                .unwrap_or(false);
                            if !same_header {
                                pos = cfg.start;
                                sink(Event::JobAdopted { job: j.id.clone(), height: j.height });
                            }
                            current = Some(j);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        if cfg.fail_fast {
                            return Outcome::Failed { error: format!("pool connection lost: {e}") };
                        }
                        sink(Event::Reconnect {
                            reason: format!("pool connection lost: {e}"),
                            wait_secs: 0,
                        });
                        continue 'session;
                    }
                }
            }
            if current.is_none() {
                match client.read_job(Duration::from_secs(cfg.job_timeout_secs)) {
                    Ok(Some(j)) => {
                        sink(Event::JobAdopted { job: j.id.clone(), height: j.height });
                        current = Some(j);
                        pos = cfg.start;
                    }
                    Ok(None) => {
                        if cfg.fail_fast {
                            return Outcome::Failed {
                                error: format!(
                                    "idle: no job within {}s ({accepted}/{submitted} accepted)",
                                    cfg.job_timeout_secs
                                ),
                            };
                        }
                        sink(Event::Idle {
                            timeout_secs: cfg.job_timeout_secs,
                            accepted,
                            submitted,
                        });
                        continue 'session;
                    }
                    Err(e) => {
                        if cfg.fail_fast {
                            return Outcome::Failed { error: format!("pool connection lost: {e}") };
                        }
                        sink(Event::Reconnect {
                            reason: format!("pool connection lost: {e}"),
                            wait_secs: 0,
                        });
                        continue 'session;
                    }
                }
            }
            let job = current.clone().expect("job set above");
            let job_end = cfg.start.saturating_add(cfg.count);
            if pos >= job_end {
                sink(Event::JobExhausted { job: job.id });
                current = None;
                pos = cfg.start;
                continue;
            }
            let take = chunk.min(job_end - pos);
            match backend.search(&job.header, job.height, &job.pool_target, pos, take, policy) {
                Ok(Some(sol)) => {
                    hashed += sol.nonce.saturating_sub(pos).saturating_add(1);
                    match client.submit(&session_worker, &job.id, sol.nonce) {
                        Ok((true, _)) => {
                            submitted += 1;
                            accepted += 1;
                            sink(Event::ShareAccepted { job: job.id.clone(), nonce: sol.nonce });
                        }
                        Ok((false, err)) => {
                            submitted += 1;
                            let err_text =
                                err.as_ref().map(|(_, t)| t.as_str()).unwrap_or_default();
                            if err_text.contains("stale") {
                                // The pool declared this job dead; drop it
                                // instead of grinding it to exhaustion (seen
                                // live on testnet: three stale rejects, then
                                // a full nonce budget of wasted work on the
                                // dead template before the idle reconnect).
                                current = None;
                            }
                            sink(Event::ShareRejected {
                                job: job.id.clone(),
                                error: err
                                    .map(|(c, t)| format!("{c} {t}"))
                                    .unwrap_or_default(),
                            });
                        }
                        Err(e) => {
                            if cfg.fail_fast {
                                return Outcome::Failed { error: format!("pool connection lost: {e}") };
                            }
                            sink(Event::Reconnect {
                                reason: format!("pool connection lost: {e}"),
                                wait_secs: 0,
                            });
                            continue 'session;
                        }
                    }
                    // Post-submit settle: an accepted block rotates the job
                    // within the pool's refresh tick, and grinding the stale
                    // job in the meantime submits shares the chain rejects.
                    // Poll briefly for the rotation before carrying on.
                    let mut rotated = false;
                    let settle = Instant::now() + Duration::from_millis(300);
                    while Instant::now() < settle {
                        match client.poll_job() {
                            Ok(Some(j)) => {
                                if current.as_ref().map(|c| c.id != j.id).unwrap_or(true) {
                                    let same_header = current
                                        .as_ref()
                                        .map(|c| c.header == j.header)
                                        .unwrap_or(false);
                                    current = Some(j);
                                    if !same_header {
                                        rotated = true;
                                    }
                                }
                                break;
                            }
                            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                            Err(e) => {
                                if cfg.fail_fast {
                                    return Outcome::Failed {
                                        error: format!("pool connection lost: {e}"),
                                    };
                                }
                                sink(Event::Reconnect {
                                    reason: format!("pool connection lost: {e}"),
                                    wait_secs: 0,
                                });
                                continue 'session;
                            }
                        }
                    }
                    pos = if rotated { cfg.start } else { sol.nonce.saturating_add(1) };
                }
                Ok(None) => {
                    hashed += take;
                    pos += take;
                }
                Err(e) => {
                    sink(Event::Note(format!("error: {e}")));
                    return Outcome::Failed { error: e };
                }
            }
        }
    }
}

/// V2 (Noise + Lucky extension) session loop — the same chunked grind
/// over [`LuckyJob`] templates with u64 submits. Dev-fee rotation, backoff
/// reconnects, counters and stop semantics mirror [`run_v1`]; the wire is
/// the only difference.
// The extra context (agent is unused on V1-style subscribe; host/port
// ride with the SetupConnection) mirrors run_v1's parameterization
// one-to-one.
#[allow(clippy::too_many_arguments)]
fn run_v2(
    cfg: &PoolConfig,
    backend: &Backend,
    policy: &MiningPolicy,
    stop: &AtomicBool,
    sink: &mut dyn FnMut(Event),
    agent: String,
    pool_host: String,
    addr: SocketAddr,
) -> Outcome {
    use lm_stratum::v2::{SetupConnection, LuckyJob, PROTOCOL_MINING};
    use lm_stratum::v2_client::V2Client;
    let setup = SetupConnection {
        protocol: PROTOCOL_MINING,
        min_version: 2,
        max_version: 2,
        flags: 0,
        endpoint_host: pool_host,
        endpoint_port: addr.port(),
        // The vendor field carries the configured agent verbatim (identity
        // + device family, e.g. "lm-miner-mobile/0.1 android"), so the
        // pool reads the same device label on both wires. `hardware_version`
        // stays the backend device ("cpu"/"opencl").
        vendor: agent.clone(),
        hardware_version: backend.device().to_string(),
        firmware: "0.2".to_string(),
        // The pool authenticates the worker against the same password /
        // worker-token map as V1; V2 has no password field, so the token
        // rides in the free-form device id.
        device_id: cfg.password.clone(),
    };
    let mut submitted = 0u64;
    let mut accepted = 0u64;
    let chunk = cfg.chunk.min(cfg.count);
    let mut hashed: u64 = 0;
    let mut report_at = Instant::now() + Duration::from_secs(cfg.report_secs.max(1));
    let mut backoff = Duration::from_secs(1);
    let t0 = Instant::now();
    let stopped = |accepted: u64, submitted: u64| Outcome::Stopped { accepted, submitted };
    'session: loop {
        if stop.load(Ordering::Relaxed) {
            return stopped(accepted, submitted);
        }
        let mut client = match V2Client::connect(addr, cfg.v2_authority, &setup) {
            Ok(c) => c,
            Err(e) => {
                if cfg.fail_fast {
                    return Outcome::Failed { error: format!("cannot reach pool (v2): {e}") };
                }
                sink(Event::Reconnect {
                    reason: format!("pool(v2) connect failed: {e}"),
                    wait_secs: backoff.as_secs(),
                });
                if !sleep_stop(backoff, stop) {
                    return stopped(accepted, submitted);
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue 'session;
            }
        };
        backoff = Duration::from_secs(1);
        let session_worker = dev_worker(cfg, t0);
        if session_worker != cfg.worker {
            sink(Event::DevFee { worker: session_worker.clone() });
        }
        let opened = match client.open_channel(1, &session_worker, 0.0, [0xffu8; 32]) {
            Ok(opened) => opened,
            Err(e) => {
                if cfg.fail_fast {
                    return Outcome::Failed { error: format!("pool refused V2 channel: {e}") };
                }
                sink(Event::Reconnect {
                    reason: format!("pool refused V2 channel: {e}"),
                    wait_secs: 0,
                });
                continue 'session;
            }
        };
        let channel_id = opened.channel_id;
        sink(Event::Connected {
            v2: true,
            worker: session_worker.clone(),
            detail: backend.detail(),
        });
        let mut current: Option<LuckyJob> = None;
        let mut pos = cfg.start;
        loop {
            if stop.load(Ordering::Relaxed) {
                drop(client);
                return stopped(accepted, submitted);
            }
            if dev_worker(cfg, t0) != session_worker {
                continue 'session;
            }
            if submitted >= cfg.max_shares {
                return Outcome::SessionComplete { accepted, submitted };
            }
            if Instant::now() >= report_at {
                let secs = cfg.report_secs.max(1) as f64;
                sink(Event::Rate {
                    hps: hashed as f64 / secs,
                    hashes: hashed,
                    window_secs: secs as u64,
                });
                hashed = 0;
                report_at = Instant::now() + Duration::from_secs(cfg.report_secs.max(1));
            }
            // Non-blocking: adopt newer jobs; a same-header rotation keeps
            // the nonce cursor (V1's duplicate-re-find lesson applies to
            // both wires).
            loop {
                match client.poll_job() {
                    Ok(Some(j)) => {
                        if current.as_ref().map(|c| c.job_id != j.job_id).unwrap_or(true) {
                            let same_header = current
                                .as_ref()
                                .map(|c| c.header == j.header)
                                .unwrap_or(false);
                            if !same_header {
                                pos = cfg.start;
                                sink(Event::JobAdopted { job: j.job_id.to_string(), height: j.height });
                            }
                            current = Some(j);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        if cfg.fail_fast {
                            return Outcome::Failed { error: format!("pool connection lost: {e}") };
                        }
                        sink(Event::Reconnect {
                            reason: format!("pool connection lost: {e}"),
                            wait_secs: 0,
                        });
                        continue 'session;
                    }
                }
            }
            if current.is_none() {
                match client.read_job(Duration::from_secs(cfg.job_timeout_secs)) {
                    Ok(Some(j)) => {
                        sink(Event::JobAdopted { job: j.job_id.to_string(), height: j.height });
                        current = Some(j);
                        pos = cfg.start;
                    }
                    Ok(None) => {
                        if cfg.fail_fast {
                            return Outcome::Failed {
                                error: format!(
                                    "idle: no job within {}s ({accepted}/{submitted} accepted)",
                                    cfg.job_timeout_secs
                                ),
                            };
                        }
                        sink(Event::Idle {
                            timeout_secs: cfg.job_timeout_secs,
                            accepted,
                            submitted,
                        });
                        continue 'session;
                    }
                    Err(e) => {
                        if cfg.fail_fast {
                            return Outcome::Failed { error: format!("pool connection lost: {e}") };
                        }
                        sink(Event::Reconnect {
                            reason: format!("pool connection lost: {e}"),
                            wait_secs: 0,
                        });
                        continue 'session;
                    }
                }
            }
            let job = current.clone().expect("job set above");
            let job_end = cfg.start.saturating_add(cfg.count);
            if pos >= job_end {
                sink(Event::JobExhausted { job: job.job_id.to_string() });
                current = None;
                pos = cfg.start;
                continue;
            }
            let take = chunk.min(job_end - pos);
            match backend.search(&job.header, job.height, &job.pool_target, pos, take, policy) {
                Ok(Some(sol)) => {
                    hashed += sol.nonce.saturating_sub(pos).saturating_add(1);
                    match client.submit(channel_id, job.job_id, sol.nonce) {
                        Ok((true, _)) => {
                            submitted += 1;
                            accepted += 1;
                            sink(Event::ShareAccepted { job: job.job_id.to_string(), nonce: sol.nonce });
                        }
                        Ok((false, err)) => {
                            submitted += 1;
                            let err_text = err.clone().unwrap_or_default();
                            if err_text.contains("stale") {
                                // Same defensive drop as V1: a stale verdict
                                // retires the job instead of wasting the
                                // rest of its nonce budget.
                                current = None;
                            }
                            sink(Event::ShareRejected {
                                job: job.job_id.to_string(),
                                error: err_text,
                            });
                        }
                        Err(e) => {
                            if cfg.fail_fast {
                                return Outcome::Failed { error: format!("pool connection lost: {e}") };
                            }
                            sink(Event::Reconnect {
                                reason: format!("pool connection lost: {e}"),
                                wait_secs: 0,
                            });
                            continue 'session;
                        }
                    }
                    // Post-submit settle: poll briefly for a rotation before
                    // carrying on (same stale-job avoidance as V1).
                    let mut rotated = false;
                    let settle = Instant::now() + Duration::from_millis(300);
                    while Instant::now() < settle {
                        match client.poll_job() {
                            Ok(Some(j)) => {
                                if current.as_ref().map(|c| c.job_id != j.job_id).unwrap_or(true) {
                                    let same_header = current
                                        .as_ref()
                                        .map(|c| c.header == j.header)
                                        .unwrap_or(false);
                                    if !same_header {
                                        sink(Event::JobAdopted { job: j.job_id.to_string(), height: j.height });
                                    }
                                    current = Some(j);
                                    if !same_header {
                                        rotated = true;
                                    }
                                }
                                break;
                            }
                            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                            Err(e) => {
                                if cfg.fail_fast {
                                    return Outcome::Failed {
                                        error: format!("pool connection lost: {e}"),
                                    };
                                }
                                sink(Event::Reconnect {
                                    reason: format!("pool connection lost: {e}"),
                                    wait_secs: 0,
                                });
                                continue 'session;
                            }
                        }
                    }
                    pos = if rotated { cfg.start } else { sol.nonce.saturating_add(1) };
                }
                Ok(None) => {
                    hashed += take;
                    pos += take;
                }
                Err(e) => {
                    sink(Event::Note(format!("error: {e}")));
                    return Outcome::Failed { error: e };
                }
            }
        }
    }
}

/// Live counters for a [`PoolMiner`] — what a UI needs between events.
#[derive(Debug, Clone, Default)]
pub struct RunStats {
    pub submitted: u64,
    pub accepted: u64,
    pub rejected: u64,
    /// True between Connected and the first drop after it.
    pub connected: bool,
    /// Rolling rate from the last [`Event::Rate`] (0 until then).
    pub hps: f64,
    /// The last event as a short human line (notification text).
    pub last: String,
}

/// A start/stop handle around [`run`] on a CPU backend: the embedded
/// miner (wallet, Android). `stop` is observed within one chunk — pass a
/// small `chunk` for tight teardown.
pub struct PoolMiner {
    stop: std::sync::Arc<AtomicBool>,
    stats: std::sync::Arc<Mutex<RunStats>>,
    join: Option<std::thread::JoinHandle<Outcome>>,
    started: std::time::SystemTime,
}

impl PoolMiner {
    /// Validate the config and spawn the loop on its own thread.
    pub fn spawn(cfg: PoolConfig) -> Result<Self, String> {
        cfg.validate()?;
        let policy = MiningPolicy::new(cfg.max_temp, cfg.duty)?;
        // CPU-only, and built inside the thread: `Backend::Gpu` holds raw
        // OpenCL handles (!Send) and cannot cross the spawn boundary.
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stats = std::sync::Arc::new(Mutex::new(RunStats {
            last: format!("starting: {} as '{}'", cfg.pool, cfg.worker),
            ..RunStats::default()
        }));
        let (stats2, stop2) = (stats.clone(), stop.clone());
        let join = std::thread::Builder::new()
            .name("lm-pool-miner".to_string())
            .spawn(move || {
                let cfg = cfg;
                let backend = Backend::Cpu { threads: cfg.threads };
                let mut sink = {
                    let stats_sink = stats2.clone();
                    move |ev: Event| update_stats(&stats_sink, &ev)
                };
                let out = run(&cfg, &backend, &policy, &stop2, &mut sink);
                if let Ok(mut s) = stats2.lock() {
                    s.connected = false;
                    let last = match &out {
                        Outcome::SessionComplete { accepted, submitted } => {
                            format!("session complete: {accepted}/{submitted} shares accepted")
                        }
                        Outcome::Stopped { accepted, submitted } => {
                            format!("stopped ({accepted}/{submitted} accepted)")
                        }
                        Outcome::Failed { error } => format!("stopped: {error}"),
                    };
                    s.last = last;
                }
                out
            })
            .map_err(|e| format!("cannot spawn mining thread: {e}"))?;
        Ok(Self {
            stop,
            stats,
            join: Some(join),
            started: std::time::SystemTime::now(),
        })
    }

    /// Signal the loop to stop (idempotent).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Signal stop and wait for the thread to finish; returns the loop's
    /// outcome when it was still running.
    pub fn stop_join(&mut self) -> Option<Outcome> {
        self.stop();
        self.join.take().map(|j| j.join().unwrap_or(Outcome::Stopped {
            accepted: 0,
            submitted: 0,
        }))
    }

    /// The stop flag is set but the thread may still be finishing a chunk.
    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// True until the looped thread exits (approximate; cheap).
    pub fn running(&self) -> bool {
        self.join.as_ref().map(|j| !j.is_finished()).unwrap_or(false)
    }

    pub fn stats(&self) -> RunStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn started_at(&self) -> std::time::SystemTime {
        self.started
    }
}

fn update_stats(stats: &Mutex<RunStats>, ev: &Event) {
    let mut s = match stats.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    match ev {
        Event::Connected { worker, detail, .. } => {
            s.connected = true;
            s.last = format!("mining as '{worker}' ({detail})");
        }
        Event::DevFee { worker } => {
            s.last = format!("dev fee: mining as '{worker}'");
        }
        Event::ShareAccepted { job, nonce } => {
            s.submitted += 1;
            s.accepted += 1;
            s.last = format!("share accepted (job {job}, nonce {nonce})");
        }
        Event::ShareRejected { job, error } => {
            s.submitted += 1;
            s.rejected += 1;
            s.last = format!("share rejected (job {job}): {error}");
        }
        Event::Rate { hps, .. } => {
            s.hps = *hps;
            s.last = format!("hashing at {hps:.0} H/s");
        }
        Event::Reconnect { reason, wait_secs } => {
            s.connected = false;
            s.last = if *wait_secs > 0 {
                format!("{reason}; retrying in {wait_secs}s")
            } else {
                reason.clone()
            };
        }
        Event::Idle { timeout_secs, .. } => {
            s.connected = false;
            s.last = format!("no job within {timeout_secs}s; reconnecting");
        }
        Event::JobExhausted { job } => {
            s.last = format!("job {job} exhausted, waiting for a new template");
        }
        Event::JobAdopted { job, height } => {
            s.last = format!("job {job} height {height}");
        }
        Event::Note(n) => s.last = n.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_pool() -> lm_stratum::server::RunningServer<lm_stratum::provider::MockProvider> {
        lm_stratum::server::spawn_server(
            lm_stratum::provider::MockProvider::instant(1_000_000),
            "127.0.0.1:0",
            None,
            None,
            None,
        )
        .expect("bind mock pool")
    }

    /// The embedded loop mines real shares through the same wire the CLI
    /// uses and reports them through the stats buffer.
    #[test]
    fn pool_miner_mines_to_max_shares() {
        let server = mock_pool();
        let cfg = PoolConfig {
            pool: server.addr().to_string(),
            worker: "embed".to_string(),
            threads: 1,
            chunk: 1024,
            max_shares: 2,
            job_timeout_secs: 10,
            report_secs: 1,
            ..PoolConfig::new(String::new(), String::new())
        };
        let mut miner = PoolMiner::spawn(cfg).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(60);
        let out = loop {
            if let Some(out) = {
                let done = miner.stats().accepted >= 2;
                if done {
                    miner.stop_join()
                } else {
                    None
                }
            } {
                break out;
            }
            assert!(Instant::now() < deadline, "no shares in 60s: {:?}", miner.stats());
            std::thread::sleep(Duration::from_millis(100));
        };
        // Either terminal is correct: with the stats poll racing the
        // loop, the stop flag can be observed a hair before the
        // max-shares check. The counts are the contract.
        match out {
            Outcome::SessionComplete { accepted, submitted }
            | Outcome::Stopped { accepted, submitted } => {
                assert_eq!((accepted, submitted), (2, 2));
            }
            other => panic!("expected complete or stopped, got {other:?}"),
        }
        assert!(miner.stats().accepted >= 2);
        assert!(
            miner.stats().last.contains("session complete")
                || miner.stats().last.contains("stopped (2/2"),
            "terminal line: {:?}",
            miner.stats().last
        );
        server.stop();
    }

    /// The stop flag is observed mid-session (between chunks / in the
    /// connect backoff) and teardown is clean.
    #[test]
    fn pool_miner_stop_is_observed() {
        let server = mock_pool();
        let cfg = PoolConfig {
            pool: server.addr().to_string(),
            worker: "embed-stop".to_string(),
            threads: 1,
            chunk: 256,
            job_timeout_secs: 5,
            ..PoolConfig::new(String::new(), String::new())
        };
        let mut miner = PoolMiner::spawn(cfg).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !miner.stats().connected {
            assert!(Instant::now() < deadline, "never connected: {:?}", miner.stats());
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(miner.running());
        match miner.stop_join() {
            Some(Outcome::Stopped { .. }) => {}
            Some(other) => panic!("expected stopped, got {other:?}"),
            None => panic!("stop_join returned None"),
        }
        assert!(!miner.running());
        assert!(!miner.stats().connected);
        server.stop();
    }

    /// fail-fast + a dead port surfaces the typed failure the CLI maps
    /// to exit 1.
    #[test]
    fn run_fail_fast_dead_pool_is_typed() {
        let probe = mock_pool();
        let addr = probe.addr().to_string();
        probe.stop();
        let cfg = PoolConfig {
            pool: addr,
            worker: "x".to_string(),
            fail_fast: true,
            ..PoolConfig::new(String::new(), String::new())
        };
        let policy = MiningPolicy::new(cfg.max_temp, cfg.duty).expect("policy");
        let backend = Backend::Cpu { threads: 1 };
        let stop = AtomicBool::new(false);
        let mut sink = |_ev: Event| {};
        match run(&cfg, &backend, &policy, &stop, &mut sink) {
            Outcome::Failed { error } => assert!(error.starts_with("cannot reach pool")),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    /// TLS pointed at a plaintext port must say so instead of a bare
    /// EOF: rustls handshakes lazily, so the failure surfaces inside
    /// subscribe when the server drops the ClientHello bytes. Live-caught
    /// 2026-10-23 (wallet miner with TLS ticked against the plaintext
    /// public stratum port).
    #[test]
    fn tls_against_plaintext_names_the_cause() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            drop(stream); // FIN with no bytes: a plaintext port hearing TLS
        });
        let tls = TlsCfg {
            server_name: Some("localhost".to_string()),
            insecure: true,
            ..TlsCfg::default()
        };
        let err = match connect_session(addr, "test-agent", "w", "x", Some(&tls)) {
            Ok(_) => panic!("TLS against a closed port must fail"),
            Err(e) => e,
        };
        assert!(err.contains("over TLS"), "names TLS: {err}");
        assert!(err.contains("plaintext"), "names plaintext: {err}");
        handle.join().expect("join");
    }

    /// Config validation rejects what the CLI rejects (worker/threads).
    #[test]
    fn config_validation_matches_the_cli() {
        let mut cfg = PoolConfig::new("127.0.0.1:1".to_string(), String::new());
        assert!(cfg.validate().is_err());
        cfg.worker = "w".to_string();
        assert!(cfg.validate().is_ok());
        cfg.threads = 0;
        assert!(cfg.validate().is_err());
        cfg.threads = 1;
        cfg.count = 0;
        assert!(cfg.validate().is_err());
    }
}
