//! Embedded pool miner: mine from the wallet without the CLI.
//!
//! Reuses the reference stack in-process — `lm-miner`'s CPU search /
//! `GpuSearcher` and `lm-stratum`'s `PoolClient` — with the same
//! thermal/duty policy as `lm-miner pool`. One thread owns the pool
//! connection + search loop; status/log snapshots are shared for the UI.

use lm_miner::gpu::GpuSearcher;
use lm_miner::thermal::MiningPolicy;
use lm_stratum::client::PoolClient;
use std::collections::VecDeque;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Nonces per search call. Small enough that Stop is responsive (a few
/// hundred ms on GPU, a few seconds on CPU), large enough to amortize.
const CHUNK: u64 = 1024;
const LOG_CAP: usize = 128;

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
    std::thread::spawn(move || {
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
    });
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
    // cpu = threads only.
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
    let device_label = match &gpu {
        Some(g) => format!("gpu: {}", g.name()),
        None => format!("cpu: {} threads", threads.max(1)),
    };
    {
        let mut s = status.lock().expect("s");
        s.device = device_label.clone();
    }
    log_line(&log, format!("mining {device_label} → {pool} as {worker}"));
    if gpu.is_some() {
        log_line(
            &log,
            format!("gpu platform {gpu_platform} device {gpu_device}, batch {gpu_batch} ({} MiB VRAM)", gpu_batch),
        );
        if max_power > 0 {
            match lm_miner::gpu::apply_power_limit(max_power) {
                Ok(()) => log_line(&log, format!("gpu power limit set to {max_power} W")),
                Err(e) => log_line(&log, format!("gpu power limit {max_power} W not applied: {e}")),
            }
        }
    }

    let started = Instant::now();
    let mut hashes: u64 = 0;
    let mut accepted: u64 = 0;
    let mut rejected: u64 = 0;
    let mut duplicates: u64 = 0;
    let mut rate_win: VecDeque<(Instant, u64)> = VecDeque::new();

    'reconnect: while !stop.load(Ordering::Relaxed) {
        let addr = match pool.to_socket_addrs().ok().and_then(|mut it| it.next()) {
            Some(a) => a,
            None => {
                set_error(&status, &log, Some(format!("cannot resolve pool address '{pool}'")));
                sleep_stop(&stop, Duration::from_secs(5));
                continue;
            }
        };
        // TLS (review fix 2026-10-18): the reference pool serves
        // stratum+TLS on request — same roots the CLI miner uses.
        let mut client = if tls {
            let host = pool.split(':').next().unwrap_or(&pool);
            match PoolClient::connect_tls(
                addr,
                host,
                None,
                false,
                std::time::Duration::from_secs(10),
            ) {
                Ok(c) => c,
                Err(e) => {
                    set_error(&status, &log, Some(format!("pool TLS connect: {e}")));
                    sleep_stop(&stop, Duration::from_secs(3));
                    continue;
                }
            }
        } else {
            match PoolClient::connect(addr) {
                Ok(c) => c,
                Err(e) => {
                    set_error(&status, &log, Some(format!("pool connect: {e}")));
                    sleep_stop(&stop, Duration::from_secs(3));
                    continue;
                }
            }
        };
        if let Err(e) = client
            .subscribe("lucky-lode-wallet/0.1")
            .and_then(|_| client.authorize_with(&worker, ""))
        {
            set_error(&status, &log, Some(format!("pool authorize: {e}")));
            sleep_stop(&stop, Duration::from_secs(3));
            continue;
        }
        set_error(&status, &log, None);
        log_line(&log, "connected".to_string());

        let mut current: Option<lm_stratum::v1::Job> = None;
        let mut pos: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            // Fresh jobs first; same-header rotations keep the search
            // position so already-tried nonces are not re-ground.
            loop {
                match client.poll_job() {
                    Ok(Some(j)) => {
                        let same_header = current
                            .as_ref()
                            .map(|c| c.header == j.header)
                            .unwrap_or(false);
                        if !same_header {
                            pos = 0;
                            log_line(&log, format!("job {} height {}", j.id, j.height));
                        }
                        current = Some(j);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        set_error(&status, &log, Some(format!("pool connection lost: {e}")));
                        sleep_stop(&stop, Duration::from_secs(3));
                        continue 'reconnect;
                    }
                }
            }
            if current.is_none() {
                match client.read_job(Duration::from_secs(10)) {
                    Ok(Some(j)) => {
                        current = Some(j);
                        pos = 0;
                        log_line(&log, "job (after wait)".to_string());
                    }
                    Ok(None) => {}
                    Err(e) => {
                        set_error(&status, &log, Some(format!("pool connection lost: {e}")));
                        sleep_stop(&stop, Duration::from_secs(3));
                        continue 'reconnect;
                    }
                }
            }
            let Some(job) = current.clone() else { continue };

            let found = match &gpu {
                Some(g) => g.search(&job.header, job.height, &job.pool_target, pos, CHUNK, &policy),
                None => Ok(lm_miner::search_with_policy(
                    &job.header,
                    job.height,
                    &job.pool_target,
                    pos,
                    CHUNK,
                    threads.max(1),
                    &policy,
                )),
            };
            match found {
                Ok(Some(sol)) => {
                    hashes += sol.nonce.saturating_sub(pos) + 1;
                    match client.submit(&worker, &job.id, sol.nonce) {
                        Ok((true, _)) => {
                            accepted += 1;
                            log_line(&log, format!("share accepted: job={} nonce={}", job.id, sol.nonce));
                        }
                        Ok((false, err)) => {
                            let dup = err
                                .as_ref()
                                .map(|(_, t)| t.contains("duplicate"))
                                .unwrap_or(false);
                            if dup {
                                duplicates += 1;
                            } else {
                                rejected += 1;
                            }
                            log_line(&log, format!("share rejected: job={} {:?}", job.id, err));
                        }
                        Err(e) => {
                            set_error(&status, &log, Some(format!("submit: {e}")));
                            sleep_stop(&stop, Duration::from_secs(3));
                            continue 'reconnect;
                        }
                    }
                    pos = sol.nonce + 1;
                }
                Ok(None) => {
                    hashes += CHUNK;
                    pos += CHUNK;
                }
                Err(e) => {
                    set_error(&status, &log, Some(format!("gpu: {e}")));
                    sleep_stop(&stop, Duration::from_secs(3));
                    continue 'reconnect;
                }
            }

            // 60 s rolling rate + status snapshot.
            let now = Instant::now();
            rate_win.push_back((now, hashes));
            while rate_win
                .front()
                .map(|(t, _)| now.duration_since(*t) > Duration::from_secs(60))
                .unwrap_or(false)
            {
                rate_win.pop_front();
            }
            let rate = match rate_win.front() {
                Some((t0, h0)) if now > *t0 => {
                    (hashes - h0) as f64 / now.duration_since(*t0).as_secs_f64()
                }
                _ => 0.0,
            };
            let mut s = status.lock().expect("s");
            s.hashrate = rate;
            s.hashes = hashes;
            s.accepted = accepted;
            s.rejected = rejected;
            s.duplicates = duplicates;
            s.uptime_secs = started.elapsed().as_secs();
            let (t, p) = match &gpu {
                Some(_) => {
                    let (t, p) = lm_miner::gpu::gpu_status();
                    (t, p)
                }
                None => (lm_miner::thermal::read_temp_c(), None),
            };
            s.temp_c = t;
            s.power_w = p;
            if let Some(l) = log.lock().expect("log").back() {
                s.last_line = l.clone();
            }
        }
    }
    log_line(&log, "miner stopped".to_string());
    status.lock().expect("s").running = false;
}

/// Sleep that wakes early when Stop is pressed.
fn sleep_stop(stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
