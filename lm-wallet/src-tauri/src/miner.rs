//! Embedded pool miner: mine from the wallet without the CLI.
//!
//! One loop for every surface now: this drives `lm-miner::pool_loop::run`
//! — the same runtime the CLI and the Android app use — with the wallet's
//! status/log snapshot as the event sink. That brings the GPU pool cadence
//! (65 k-nonce chunks for the pipelined submit, post-submit settle,
//! backoff reconnects, dev-fee plumbing off) along with it; the GPU
//! backend opens on this thread (`GpuSearcher` holds raw OpenCL handles,
//! so it never crosses a thread boundary).

use lm_miner::gpu::GpuSearcher;
use lm_miner::pool_loop::{Backend, Event, Outcome, PoolConfig, TlsCfg};
use lm_miner::thermal::MiningPolicy;
use std::collections::VecDeque;
use std::net::ToSocketAddrs;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const LOG_CAP: usize = 128;

/// Nonces per search call, per device (stop responsiveness vs pipeline
/// depth): the GPU amortizes a deep queue (~0.8 s per chunk at the
/// pipelined rate), the CPU keeps Stop near-instant.
const GPU_CHUNK: u64 = 16_384;
const CPU_CHUNK: u64 = 2_048;

/// Handle held in app state while the miner runs.
pub struct Miner {
    pub stop: Arc<AtomicBool>,
    pub status: Arc<Mutex<Status>>,
    pub log: Arc<Mutex<VecDeque<String>>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Status {
    pub running: bool,
    pub device: String,
    pub pool: String,
    pub worker: String,
    pub hashrate: f64,
    pub hashes: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub duplicates: u64,
    pub uptime_secs: u64,
    pub last_line: String,
    pub error: Option<String>,
    /// Card sensor on GPU runs, package sensor on CPU runs (−1 = n/a).
    pub temp_c: i32,
    /// Board power in W when reported (NVIDIA via nvidia-smi).
    pub power_w: Option<u32>,
}

fn log_line(log: &Mutex<VecDeque<String>>, line: String) {
    let mut l = log.lock().expect("miner log");
    if l.len() >= LOG_CAP {
        l.pop_front();
    }
    l.push_back(line);
}

fn set_error(status: &Mutex<Status>, log: &Mutex<VecDeque<String>>, err: Option<String>) {
    if let Some(e) = &err {
        log_line(log, format!("error: {e}"));
    }
    status.lock().expect("miner status").error = err;
}

/// Start mining. `device` = `cpu` | `opencl` | `auto`; `duty` = percent of
/// wall time spent hashing (0–100); `max_temp` = pause threshold in °C.
/// GPU knobs: `gpu_platform`/`gpu_device` pick the card, `gpu_batch` =
/// Argon2 instances per launch (1 MiB VRAM each; 0 → 512), `max_power` =
/// NVIDIA board limit in W via `nvidia-smi -pl` (0 = leave alone).
#[allow(clippy::too_many_arguments)]
pub fn start(
    pool: String,
    worker: String,
    device: String,
    threads: usize,
    duty: u32,
    max_temp: i64,
    gpu_platform: usize,
    gpu_device: usize,
    gpu_batch: usize,
    tls: bool,
    max_power: u32,
) -> Result<Miner, String> {
    if worker.trim().is_empty() {
        return Err("set a payout address (worker name) first".to_string());
    }
    // Surface a bad thermal/duty config before spawning anything.
    let _ = MiningPolicy::new(max_temp, duty.min(100) as i64)?;
    if pool.to_socket_addrs().ok().and_then(|mut it| it.next()).is_none() {
        return Err(format!("cannot resolve pool address '{pool}'"));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(Status {
        running: true,
        device: device.clone(),
        pool: pool.clone(),
        worker: worker.clone(),
        hashrate: 0.0,
        hashes: 0,
        accepted: 0,
        rejected: 0,
        duplicates: 0,
        uptime_secs: 0,
        last_line: String::new(),
        error: None,
        temp_c: -1,
        power_w: None,
    }));
    let log: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));

    let (s, st, lg) = (Arc::clone(&stop), Arc::clone(&status), Arc::clone(&log));
    std::thread::Builder::new()
        .name("wallet-miner".to_string())
        .spawn(move || {
            run(
                s,
                st,
                lg,
                pool,
                worker,
                device,
                threads,
                duty,
                max_temp,
                gpu_platform,
                gpu_device,
                gpu_batch,
                tls,
                max_power,
            )
        })
        .map_err(|e| format!("cannot spawn mining thread: {e}"))?;
    Ok(Miner { stop, status, log })
}

#[allow(clippy::too_many_arguments)]
fn run(
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    log: Arc<Mutex<VecDeque<String>>>,
    pool: String,
    worker: String,
    device: String,
    threads: usize,
    duty: u32,
    max_temp: i64,
    gpu_platform: usize,
    gpu_device: usize,
    gpu_batch: usize,
    tls: bool,
    max_power: u32,
) {
    let policy = match MiningPolicy::new(max_temp, duty.min(100) as i64) {
        Ok(p) => p,
        Err(e) => {
            set_error(&status, &log, Some(e));
            status.lock().expect("s").running = false;
            return;
        }
    };
    // Device resolution: opencl = hard requirement, auto = prefer GPU,
    // cpu = threads only. Opened HERE — the searcher holds raw OpenCL
    // handles and must not cross the thread boundary.
    let gpu_batch = if gpu_batch == 0 { 512 } else { gpu_batch };
    let gpu = match device.as_str() {
        "opencl" => match GpuSearcher::open(gpu_platform, gpu_device, gpu_batch) {
            Ok(g) => Some(g),
            Err(e) => {
                set_error(&status, &log, Some(format!("opencl: {e}")));
                status.lock().expect("s").running = false;
                return;
            }
        },
        "auto" => GpuSearcher::open(gpu_platform, gpu_device, gpu_batch).ok(),
        _ => None,
    };
    let (backend, chunk) = match gpu {
        Some(g) => {
            log_line(
                &log,
                format!("gpu platform {gpu_platform} device {gpu_device}, batch {gpu_batch} ({gpu_batch} MiB VRAM)"),
            );
            if max_power > 0 {
                match lm_miner::gpu::apply_power_limit(max_power) {
                    Ok(()) => log_line(&log, format!("gpu power limit set to {max_power} W")),
                    Err(e) => log_line(&log, format!("gpu power limit {max_power} W not applied: {e}")),
                }
            }
            (Backend::Gpu(g), GPU_CHUNK)
        }
        None => (Backend::Cpu { threads: threads.max(1) }, CPU_CHUNK),
    };
    let device_label = match &backend {
        Backend::Gpu(g) => format!("gpu: {}", g.name()),
        Backend::Cpu { threads } => format!("cpu: {threads} threads"),
    };
    {
        let mut s = status.lock().expect("s");
        s.device = device_label.clone();
    }
    log_line(&log, format!("mining {device_label} → {pool} as {worker}"));

    let cfg = PoolConfig {
        pool: pool.clone(),
        worker: worker.clone(),
        agent: Some("lucky-lode-wallet/0.1".to_string()),
        // The wallet always authorized with an empty password; open pools
        // ignore it either way.
        password: String::new(),
        threads: threads.max(1),
        chunk,
        max_temp,
        duty: duty.min(100) as i64,
        job_timeout_secs: 10,
        tls: tls.then_some(TlsCfg { server_name: None, ca_pem: None, insecure: false }),
        report_secs: 5,
        ..PoolConfig::new(String::new(), String::new())
    };

    let started = Instant::now();
    let counters = Arc::new(Mutex::new(Counters::default()));
    let c2 = Arc::clone(&counters);
    let is_gpu = matches!(&backend, Backend::Gpu(_));
    // The sink owns its own Arc clones; the outer scope keeps the originals.
    let (st_sink, log_sink) = (Arc::clone(&status), Arc::clone(&log));
    let mut sink = move |ev: Event| {
        let mut c = c2.lock().expect("counters");
        let (status, log) = (&st_sink, &log_sink);
        match &ev {
            Event::Connected { worker, detail, .. } => {
                set_error(status, log, None);
                log_line(log, format!("connected as '{worker}' ({detail})"));
            }
            Event::ShareAccepted { job, nonce } => {
                c.accepted += 1;
                log_line(log, format!("share accepted: job={job} nonce={nonce}"));
            }
            Event::ShareRejected { job, error } => {
                if error.contains("duplicate") {
                    c.duplicates += 1;
                } else {
                    c.rejected += 1;
                }
                log_line(log, format!("share rejected: job={job} ({error})"));
            }
            Event::Rate { hps, hashes, .. } => {
                c.hashes = c.hashes.saturating_add(*hashes);
                c.hashrate = *hps;
            }
            Event::Reconnect { reason, wait_secs } => {
                let line = if *wait_secs > 0 {
                    format!("{reason}; retrying in {wait_secs}s")
                } else {
                    reason.clone()
                };
                set_error(status, log, Some(line));
            }
            Event::Idle { timeout_secs, .. } => {
                log_line(log, format!("no job within {timeout_secs}s; reconnecting"));
            }
            Event::JobExhausted { job } => {
                log_line(log, format!("job {job} exhausted, waiting for a new template"));
            }
            Event::DevFee { .. } | Event::Note(_) => {}
        }
        // Status snapshot on every event (the loop reports at least every
        // few seconds; gpu_status is cached 2 s so this stays cheap).
        let mut s = status.lock().expect("s");
        let (t, p) = if is_gpu {
            lm_miner::gpu::gpu_status()
        } else {
            (lm_miner::thermal::read_temp_c(), None)
        };
        s.hashrate = c.hashrate;
        s.hashes = c.hashes;
        s.accepted = c.accepted;
        s.rejected = c.rejected;
        s.duplicates = c.duplicates;
        s.uptime_secs = started.elapsed().as_secs();
        s.temp_c = t;
        s.power_w = p;
        if let Some(l) = log.lock().expect("log").back() {
            s.last_line = l.clone();
        }
    };

    let outcome = lm_miner::pool_loop::run(&cfg, &backend, &policy, &stop, &mut sink);
    let (closing, failed) = match outcome {
        Outcome::SessionComplete { accepted, submitted } => {
            (format!("session complete: {accepted}/{submitted} shares accepted"), None)
        }
        Outcome::Stopped { accepted, submitted } => {
            (format!("stopped ({accepted}/{submitted} accepted)"), None)
        }
        Outcome::Failed { error } => (format!("stopped: {error}"), Some(error)),
    };
    log_line(&log, closing);
    if let Some(error) = failed {
        set_error(&status, &log, Some(error));
    }
    status.lock().expect("s").running = false;
}

#[derive(Default)]
struct Counters {
    hashes: u64,
    hashrate: f64,
    accepted: u64,
    rejected: u64,
    duplicates: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// End-to-end smoke: connect to a live local pool, mine a bit, stop.
    /// Ignored by default — run with `-- --ignored` while a pool serves
    /// 127.0.0.1:3356 (open auth).
    #[test]
    #[ignore]
    fn live_smoke_against_local_pool() {
        let m = start(
            "127.0.0.1:3356".into(),
            "wallet-smoke".into(),
            "cpu".into(),
            1,
            100,
            95, // guard effectively off: this box's Tctl spikes to 80 under load
            0,
            0,
            512,
            false,
            0,
        )
        .expect("start");
        std::thread::sleep(Duration::from_secs(20));
        let lg: Vec<String> = m.log.lock().expect("log").iter().cloned().collect();
        println!("cpu smoke log: {lg:?}");
        let s = m.status.lock().expect("s").clone();
        assert!(s.running, "miner should be running: {s:?}");
        assert!(s.hashes > 0, "hashes should grow: {s:?}");
        assert!(s.error.is_none(), "no error expected: {s:?}");
        m.stop.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(12));
        let s = m.status.lock().expect("s").clone();
        assert!(!s.running, "miner should stop: {s:?}");
        println!("smoke ok: {s:?}");
    }

    /// GPU path smoke: open platform 0 / device 0 with a 512 batch and
    /// prove hashes flow + the card sensor reports. Needs the local pool
    /// and an OpenCL GPU; run with `-- --ignored` in release.
    #[test]
    #[ignore]
    fn live_smoke_gpu_against_local_pool() {
        let m = start(
            "127.0.0.1:3356".into(),
            "wallet-smoke-gpu".into(),
            "opencl".into(),
            1,
            100,
            80,
            0,
            0,
            512,
            false,
            0,
        )
        .expect("start");
        std::thread::sleep(Duration::from_secs(20));
        let s = m.status.lock().expect("s").clone();
        assert!(s.running, "gpu miner should be running: {s:?}");
        assert!(s.device.starts_with("gpu:"), "should be on the GPU: {s:?}");
        assert!(s.hashes > 0, "hashes should grow: {s:?}");
        assert!(s.temp_c > 0, "gpu sensor should report: {s:?}");
        m.stop.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_secs(12));
        let s = m.status.lock().expect("s").clone();
        assert!(!s.running, "gpu miner should stop: {s:?}");
        println!("gpu smoke ok: {s:?}");
    }
}
