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
use lm_miner::pool_loop::{Backend, Event, Outcome, PoolConfig};
use lm_miner::thermal::MiningPolicy;
use lm_stratum::node_provider::{assemble_block, build_template, SeedClaim};
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
    // The agent carries the device family for pool-side division labels
    // (doc 03 §3.6): the wallet declares what it actually mines on.
    let agent = match &backend {
        Backend::Gpu(_) => "lucky-lode-wallet/0.1 opencl",
        Backend::Cpu { .. } => "lucky-lode-wallet/0.1 cpu",
    }
    .to_string();
    {
        let mut s = status.lock().expect("s");
        s.device = device_label.clone();
    }
    log_line(&log, format!("mining {device_label} → {pool} as {worker}"));

    let cfg = PoolConfig {
        pool: pool.clone(),
        worker: worker.clone(),
        agent: Some(agent),
        // The wallet always authorized with an empty password; open pools
        // ignore it either way.
        password: String::new(),
        threads: threads.max(1),
        chunk,
        max_temp,
        duty: duty.min(100) as i64,
        // Template rotations are ~60 s apart on testnet; a 10 s idle
        // window reconnects pointlessly (seen live). The CLI default 30 s
        // only reconnects when genuinely starved.
        job_timeout_secs: 30,
        tls: None,
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
            Event::JobAdopted { job, height } => {
                log_line(log, format!("job {job} height {height}"));
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

/// Solo mining against the wallet's own node: GBT templates, claim-aware
/// coinbases (jackpot + pot + prize, exactly like the internal miner —
/// seed drips stay pool-claimed), no pool in the loop (item 51).
///
/// Rewards land wherever the node's GBT coinbase pays — the caller must
/// verify that is one of this wallet's addresses (see
/// `template_beneficiary`) and refuse otherwise, or solo finds would pay
/// a stranger. Requires an unlocked wallet (HD scan for the check).
#[allow(clippy::too_many_arguments)]
pub fn start_solo(
    node: lm_stratum::rpc::RpcClient,
    node_label: String,
    device: String,
    threads: usize,
    duty: u32,
    max_temp: i64,
    gpu_platform: usize,
    gpu_device: usize,
    gpu_batch: usize,
    max_power: u32,
    seed: [u8; 64],
    net: lm_keys::chain::Network,
    recv_next: u32,
    change_next: u32,
) -> Result<Miner, String> {
    // Surface a bad thermal/duty config before spawning anything.
    let _ = MiningPolicy::new(max_temp, duty.min(100) as i64)?;
    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(Mutex::new(Status {
        running: true,
        device: device.clone(),
        pool: format!("solo {node_label}"),
        worker: String::new(),
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
        .name("wallet-solo-miner".to_string())
        .spawn(move || {
            run_solo(
                s,
                st,
                lg,
                node,
                device,
                threads,
                duty,
                max_temp,
                gpu_platform,
                gpu_device,
                gpu_batch,
                max_power,
                seed,
                net,
                recv_next,
                change_next,
            )
        })
        .map_err(|e| format!("cannot spawn mining thread: {e}"))?;
    Ok(Miner { stop, status, log })
}

/// The beneficiary check: the GBT coinbase's normal output must pay one
/// of this wallet's addresses, or solo finds would pay a stranger (node
/// wallet, `-mineraddress` leftovers, someone else's reserve key).
/// Returns the matched address for display. Fail closed with the exact
/// script and the one-line fix.
fn template_beneficiary(
    gbt: &serde_json::Value,
    txs: &[Vec<u8>],
    seed: &[u8; 64],
    net: lm_keys::chain::Network,
    recv_next: u32,
    change_next: u32,
) -> Result<String, String> {
    use lm_stratum::tx::output_scripts;
    let scripts =
        output_scripts(&txs[0]).map_err(|e| format!("coinbase parse: {e:?}"))?;
    let normal = scripts.first().cloned().unwrap_or_default();
    let normal_hex: String = normal.iter().map(|b| format!("{b:02x}")).collect();
    let height = gbt.get("height").and_then(|v| v.as_u64()).unwrap_or(0);
    for (chain, used) in [(0u32, recv_next), (1u32, change_next)] {
        let count = used.saturating_add(crate::wallet::GAP);
        for d in crate::wallet::derive_range(seed, net, chain, 0, count)
            .map_err(|e| format!("derive: {e}"))?
        {
            if d.script_hex.eq_ignore_ascii_case(&normal_hex) {
                return Ok(d.address);
            }
        }
    }
    Err(format!(
        "node coinbase at height {height} pays {normal_hex} — not one of this wallet's addresses. \
         Solo rewards would not land here. Point the node at your wallet first (Miner tab → point node payouts at my wallet, then restart the node), or start the managed node with -mineraddress=<receive> -minetolocalwallet=0."
    ))
}

/// GBT template refresh cadence: fresh fees matter, but every poll is an
/// RPC round trip (the dashboards poll on 5 s too).
const SOLO_REFRESH_SECS: u64 = 5;

/// Sleep `max`, aborting early when `stop` is set. Returns false when the
/// wait was cut short.
fn sleep_stop(max: std::time::Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + max;
    while Instant::now() < end {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200).min(end.saturating_duration_since(Instant::now())));
    }
    !stop.load(std::sync::atomic::Ordering::Relaxed)
}

#[allow(clippy::too_many_arguments)]
fn run_solo(
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<Status>>,
    log: Arc<Mutex<VecDeque<String>>>,
    node: lm_stratum::rpc::RpcClient,
    device: String,
    threads: usize,
    duty: u32,
    max_temp: i64,
    gpu_platform: usize,
    gpu_device: usize,
    gpu_batch: usize,
    max_power: u32,
    seed: [u8; 64],
    net: lm_keys::chain::Network,
    recv_next: u32,
    change_next: u32,
) {
    let policy = match MiningPolicy::new(max_temp, duty.min(100) as i64) {
        Ok(p) => p,
        Err(e) => {
            set_error(&status, &log, Some(e));
            status.lock().expect("s").running = false;
            return;
        }
    };
    // Same device handling as pool mode: OpenCL handles never cross the
    // thread boundary, so the backend opens here.
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
    log_line(&log, format!("solo mining {device_label} (claim-aware templates, no pool)"));

    let started = Instant::now();
    let mut hashes: u64 = 0;
    let mut hashrate: f64 = 0.0;
    let mut accepted: u64 = 0;
    let mut rejected: u64 = 0;
    let mut nonce: u64 = rand_nonce();
    let mut tpl: Option<(lm_stratum::provider::JobTemplate, Vec<Vec<u8>>)> = None;
    let mut last_fetch = Instant::now()
        .checked_sub(std::time::Duration::from_secs(SOLO_REFRESH_SECS + 1))
        .unwrap_or_else(Instant::now);

    macro_rules! snapshot {
        ($last:expr) => {{
            let mut s = status.lock().expect("s");
            let (t, p) = match &backend {
                Backend::Gpu(_) => lm_miner::gpu::gpu_status(),
                Backend::Cpu { .. } => (lm_miner::thermal::read_temp_c(), None),
            };
            s.hashrate = hashrate;
            s.hashes = hashes;
            s.accepted = accepted;
            s.rejected = rejected;
            s.uptime_secs = started.elapsed().as_secs();
            s.temp_c = t;
            s.power_w = p;
            if let Some(l) = $last.or_else(|| log.lock().expect("log").back().cloned()) {
                s.last_line = l;
            }
        }};
    }

    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        // Fresh template on new tip or every few seconds (mempool fees).
        let tip: Option<u64> = node
            .call("getblockcount", serde_json::json!([]))
            .ok()
            .and_then(|v| v.as_u64());
        let need = match (&tip, &tpl) {
            (Some(h), Some((t, _))) => {
                *h != t.height || last_fetch.elapsed().as_secs() >= SOLO_REFRESH_SECS
            }
            _ => true,
        };
        if need {
            match fetch_template(&node, &seed, net, recv_next, change_next) {
                Ok((t, txs, beneficiary, bonus, pot, prize)) => {
                    if Some(t.height) != tpl.as_ref().map(|(old, _)| old.height) {
                        log_line(
                            &log,
                            format!(
                                "template height {} (bonus {} pot {} prize {} to {})",
                                t.height, bonus, pot, prize, beneficiary
                            ),
                        );
                        {
                            let mut s = status.lock().expect("s");
                            s.worker = beneficiary;
                        }
                    }
                    tpl = Some((t, txs));
                    last_fetch = Instant::now();
                }
                Err(e) => {
                    set_error(&status, &log, Some(e));
                    if !sleep_stop(std::time::Duration::from_secs(5), &stop) {
                        break;
                    }
                    continue;
                }
            }
        }
        let (t, txs) = match tpl.clone() {
            Some(v) => v,
            None => {
                if !sleep_stop(std::time::Duration::from_secs(1), &stop) {
                    break;
                }
                continue;
            }
        };
        let t0 = Instant::now();
        let sol = match backend.search(&t.header, t.height, &t.net_target, nonce, chunk, &policy) {
            Ok(s) => s,
            Err(e) => {
                set_error(&status, &log, Some(format!("search: {e}")));
                break;
            }
        };
        let dt = t0.elapsed().as_secs_f64().max(0.001);
        hashes = hashes.saturating_add(chunk);
        let inst = chunk as f64 / dt;
        hashrate = if hashrate == 0.0 { inst } else { hashrate * 0.7 + inst * 0.3 };
        nonce = nonce.wrapping_add(chunk);
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if let Some(sol) = sol {
            use lm_stratum::v1::hex_of;
            let hex = hex_of(&assemble_block(&t.header, sol.nonce, &txs));
            match node.call("submitblock", serde_json::json!([hex])) {
                Ok(v) if v.is_null() => {
                    accepted += 1;
                    log_line(&log, format!("block found: height {}", t.height));
                }
                Ok(v) => {
                    rejected += 1;
                    log_line(&log, format!("block rejected: {v}"));
                }
                Err(e) => {
                    rejected += 1;
                    log_line(&log, format!("block rejected: {e}"));
                }
            }
        }
        snapshot!(None);
    }
    let mut s = status.lock().expect("s");
    s.running = false;
    if s.error.is_none() {
        log_line(&log, format!("stopped ({accepted} blocks found)"));
    }
}

/// Fetch GBT and build the claim-aware template (jackpot + pot + prize,
/// exactly like the internal miner — seed drips stay pool-claimed).
/// Fails closed when the coinbase does not pay this wallet. Returns the
/// template, its txs, the matched beneficiary address, and the claim
/// amounts for logging.
#[allow(clippy::type_complexity)]
fn fetch_template(
    node: &lm_stratum::rpc::RpcClient,
    seed: &[u8; 64],
    net: lm_keys::chain::Network,
    recv_next: u32,
    change_next: u32,
) -> Result<
    (
        lm_stratum::provider::JobTemplate,
        Vec<Vec<u8>>,
        String,
        u64,
        u64,
        u64,
    ),
    String,
> {
    let gbt: serde_json::Value = node.call(
        "getblocktemplate",
        serde_json::json!([{"capabilities": []}]),
    )?;
    let jackpot_divisor = gbt
        .get("jackpotdivisor")
        .and_then(|v| v.as_u64())
        .unwrap_or(100)
        .max(1);
    let pot_divisor = gbt
        .get("potdivisor")
        .and_then(|v| v.as_u64())
        .unwrap_or(1000)
        .max(1);
    let (tpl, txs, bonus, pot, _coinbase, _pity, _faucet, _dev, prize) =
        build_template(
            &gbt,
            true,
            true,
            jackpot_divisor,
            pot_divisor,
            &SeedClaim::default(),
            true,
        )?;
    let beneficiary =
        template_beneficiary(&gbt, &txs, seed, net, recv_next, change_next)?;
    Ok((tpl, txs, beneficiary, bonus, pot, prize.amount))
}

/// Random nonce start (avoids every solo miner grinding 0..N).
fn rand_nonce() -> u64 {
    use rand::RngCore;
    rand::thread_rng().next_u64()
}

/// Minimal legacy coinbase paying `script` once (test helper): version,
/// one null-input vin, one vout, zero locktime.
#[cfg(test)]
fn test_coinbase(script: &[u8]) -> Vec<u8> {
    let mut tx = vec![1u8, 0, 0, 0, 1];
    tx.extend_from_slice(&[0u8; 32]); // prev txid
    tx.extend_from_slice(&[0xffu8; 4]); // prev vout
    tx.push(3); // scriptSig len
    tx.extend_from_slice(&[0x51, 0x01, 0x01]); // height 1
    tx.extend_from_slice(&[0xffu8; 4]); // sequence
    tx.push(1); // one output
    tx.extend_from_slice(&152000000i64.to_le_bytes());
    assert!(script.len() < 0xfd);
    tx.push(script.len() as u8);
    tx.extend_from_slice(script);
    tx.extend_from_slice(&[0u8; 4]); // locktime
    tx
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
    fn solo_beneficiary_accepts_own_coinbase() {
        // The template coinbase pays the wallet's own address: solo may
        // proceed, and the matched address is reported for the log.
        let seed = [7u8; 64];
        let net = lm_keys::chain::Network::Regtest;
        let d0 = crate::wallet::derive_range(&seed, net, 0, 0, 1).expect("derive").remove(0);
        let d1 = crate::wallet::derive_range(&seed, net, 1, 0, 1).expect("derive").remove(0);
        let hexd = |h: &str| {
            (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect::<Vec<u8>>()
        };
        let gbt = serde_json::json!({"height": 5u64});
        for d in [&d0, &d1] {
            let cb = test_coinbase(&hexd(&d.script_hex));
            let got = template_beneficiary(&gbt, &[cb], &seed, net, 0, 0).expect("match");
            assert_eq!(got, d.address);
        }
    }

    #[test]
    fn solo_beneficiary_refuses_stranger_coinbase() {
        // Fail closed with the fix: the coinbase must pay this wallet.
        let seed = [7u8; 64];
        let net = lm_keys::chain::Network::Regtest;
        let stranger = "76a914ba38cfe49e436bb2a43898083a98becc148c204488ac";
        let hexd = |h: &str| {
            (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect::<Vec<u8>>()
        };
        let gbt = serde_json::json!({"height": 5u64});
        let cb = test_coinbase(&hexd(stranger));
        let err = template_beneficiary(&gbt, &[cb], &seed, net, 0, 0).expect_err("must refuse");
        assert!(err.contains("not one of this wallet"), "fail-closed reason, got: {err}");
        assert!(err.contains("-mineraddress"), "must name the fix, got: {err}");
    }

    /// End-to-end smoke: connect to a live local pool, mine a bit, stop.
    /// Ignored by default — run with `-- --ignored` while a pool serves
    /// 127.0.0.1:3356 (open auth).
    #[test]
    #[ignore]
    fn live_smoke_against_local_pool() {        let m = start(
            "127.0.0.1:3356".into(),
            "wallet-smoke".into(),
            "cpu".into(),
            1,
            100,
            95, // guard effectively off: this box's Tctl spikes to 80 under load
            0,
            0,
            512,
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

    /// Solo end-to-end on a throwaway regtest node (ignored in CI;
    /// run explicitly): the wallet grinds its own GBT templates with
    /// claim-aware coinbases and lands blocks paying the app wallet —
    /// no pool anywhere in the loop. Spawns `luckyd` itself
    /// (LUCKYD_BIN or PATH), so the only external requirement is the
    /// built node binary; temp datadir + fixed test ports, cleaned up
    /// afterwards.
    #[test]
    #[ignore]
    fn solo_mines_claim_aware_blocks_to_app_wallet() {
        use lm_stratum::rpc::RpcClient;
        use std::io::Write;

        struct Guard {
            child: Option<std::process::Child>,
            dir: std::path::PathBuf,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                if let Some(mut c) = self.child.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }

        let bin = std::env::var("LUCKYD_BIN").ok().or_else(|| {
            crate::node_proc::find_luckyd()
                .into_iter()
                .next()
        }).expect("luckyd binary needed (LUCKYD_BIN or PATH)");
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("lucky-solo-test-{pid}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let conf = "\
rpcuser=solouser\nrpcpassword=solopass0123456789abcdef\nrpcport=60451\nport=19461\n\
server=1\ntxindex=1\nrpcworkqueue=256\nrpcallowip=127.0.0.1\nrpchost=127.0.0.1\n\
listen=0\nac_algo=luckyhash\nac_veruspos=50\nac_cc=1\n\
launchsystemid=i3UXS5QPRQGNRDDqVnyWTnmFCTHDbzmsYk\n\
parentid=i3UXS5QPRQGNRDDqVnyWTnmFCTHDbzmsYk\n\
systemid=i3UXS5QPRQGNRDDqVnyWTnmFCTHDbzmsYk\n\
startblock=0\nendblock=0\ngatewayconverterissuance=0\nac_supply=0\n\
ac_halving=1,1051920,0\nac_decay=100000000,0,0\nac_reward=0,1600000000,100000000\n\
ac_eras=3\nac_end=10080,4217760,0\nac_options=264,264,264\n\
lucky-coinbase-maturity=1\n";
        std::fs::write(dir.join("LUCKYLODE.conf"), conf).expect("conf");
        // App wallet: deterministic test seed; the node coinbase must pay
        // it (verified by start_solo before grinding).
        let seed = [9u8; 64];
        let net = lm_keys::chain::Network::Regtest;
        let addr = crate::wallet::current_receive(&seed, net, 0).expect("addr");
        let args = crate::node_proc::build_args(
            dir.to_str().expect("dir"),
            "solouser",
            "solopass0123456789abcdef",
            60451,
            &format!("-regtest -mineraddress={addr} -minetolocalwallet=0"),
        );
        let child = crate::node_proc::spawn(&bin, &args).expect("spawn node");
        let guard = Guard { child: Some(child), dir: dir.clone() };
        let rpc = RpcClient::new("127.0.0.1:60451", "solouser", "solopass0123456789abcdef");
        let h0 = crate::node_proc::wait_for_rpc(&rpc, dir.to_str().expect("dir"), std::time::Duration::from_secs(120))
            .expect("node rpc up");
        assert_eq!(h0, 0, "fresh datadir must start at genesis (port clash?)");

        let m = start_solo(
            rpc.clone(),
            "127.0.0.1:60451".to_string(),
            "auto".to_string(),
            4,
            100,
            90,
            0,
            0,
            512,
            0,
            seed,
            net,
            0,
            0,
        )
        .expect("solo start");
        // Wait for at least two blocks (claim logic runs on real heights;
        // the draw needs a boundary — h1..h2 suffice for the plumbing).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(360);
        let mut tip = 0u64;
        while std::time::Instant::now() < deadline {
            if let Ok(v) = rpc.call("getblockcount", serde_json::json!([])) {
                tip = v.as_u64().unwrap_or(0);
                if tip >= 2 {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        m.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_secs(6));
        assert!(tip >= 2, "solo miner should have mined blocks, tip={tip}");
        let s = m.status.lock().expect("s").clone();
        assert!(s.hashes > 0, "hashes should grow: {s:?}");
        assert!(s.accepted >= 2, "blocks should land: {s:?}");
        assert!(s.error.is_none(), "no error expected: {s:?}");
        // The coinbase pays the app wallet (proves the -mineraddress path
        // and the beneficiary gate end to end).
        for h in 1..=tip {
            let bh: String = serde_json::from_str::<serde_json::Value>(
                &rpc.call("getblockhash", serde_json::json!([h])).expect("hash").to_string(),
            )
            .expect("hash str")
            .as_str()
            .expect("hash")
            .to_string();
            let blk: serde_json::Value =
                rpc.call("getblock", serde_json::json!([bh, 2])).expect("block");
            let addrs: Vec<String> = blk["tx"][0]["vout"][0]["scriptPubKey"]["addresses"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect();
            assert!(
                addrs.iter().any(|a| a == &addr),
                "h{h} coinbase must pay the app wallet, got {addrs:?}"
            );
        }
        drop(guard);
        let _ = std::io::stdout().flush();
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
