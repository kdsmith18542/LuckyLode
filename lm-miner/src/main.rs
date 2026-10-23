//! `lm-miner` — reference LuckyHash v1 miner binary.
//!
//! ```text
//! lm-miner bench [--threads N] [--hashes N] [--height H] [--header HEX] [--json]
//! lm-miner mine --header HEX --height H --target (HEX64 | nbits:XXXXXXXX | max)
//!                [--threads N] [--start N] [--count N]
//! lm-miner pool --pool HOST:PORT --worker NAME [--agent STR] [--threads N]
//!                [--start N] [--count N] [--max-shares N] [--job-timeout SECS]
//! ```
//!
//! Exit codes: 0 = bench done / solution found / pool session ended after
//! `--max-shares`, 1 = budget exhausted with no solution / pool idle past
//! `--job-timeout`, 2 = usage error. No third-party argument parser by design
//! (portable, offline-buildable, auditable) — flags are `--name value`.

use lm_miner::gpu::GpuSearcher;
use lm_miner::pool_loop::{Backend, Event, Outcome, PoolConfig, TlsCfg};
use lm_miner::thermal::MiningPolicy;
use lm_miner::{bench, gpu, opencl};
use lm_pow::difficulty::compact_to_target;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

/// Advertised agent string: the binary name plus the crate version, one
/// constant for the default, `--help`, and `--version` (they used to
/// disagree: help said 0.1 while the code sent 0.2 against crate 0.1.0).
/// Defined in the pool-loop library so embedders (wallet, Android) send
/// the same identity.
use lm_miner::pool_loop::DEFAULT_AGENT as AGENT;

const USAGE: &str = concat!(
    "usage:
  lm-miner bench [--threads N] [--hashes N] [--height H] [--header HEX] [--json] [--device auto|cpu|opencl] [--gpu-platform N] [--gpu-device N] [--gpu-batch N]
  lm-miner mine --header HEX --height H --target (HEX64 | nbits:XXXXXXXX | max) [--threads N] [--start N] [--count N] [--max-temp C] [--duty N] [--device auto|cpu|opencl] [--gpu-platform N] [--gpu-device N] [--gpu-batch N]
  lm-miner pool --pool HOST:PORT --worker NAME [--agent STR] [--threads N] [--start N] [--count N] [--max-shares N] [--job-timeout SECS] [--max-temp C] [--duty N] [--device auto|cpu|opencl] [--gpu-platform N] [--gpu-device N] [--gpu-batch N] [--password PASS] [--dev-fee-pct N] [--dev-address ADDR] [--tls [--tls-ca PEM] [--tls-server-name NAME] | --tls-insecure] [--v2 [--v2-authority URLKEY|HEX]]
  lm-miner gpu-test [--platform N] [--device N]

bench defaults: threads=auto, hashes=3/thread, height=1, 80-byte 0x00..0x4F header.
mine defaults: threads=auto, start=0, count=1000000.
pool defaults: agent=",
    concat!("lm-miner/", env!("CARGO_PKG_VERSION")),
    ", threads=auto, start=0, count=1000000/job, max-shares=unlimited, job-timeout=30s.
gpu-test: runs the frozen KAT vectors through the GPU (needs OpenCL hardware).
device: cpu (default), auto (OpenCL when present, else CPU), opencl (hard error if absent).
gpu knobs: --gpu-platform 0, --gpu-device 0, --gpu-batch 512 (Argon2 instances per launch, 1 MiB VRAM each), --max-power W (board power limit via nvidia-smi -pl; needs driver permission).
safety defaults: --max-temp 75 (pause above; 0 disables), --duty 100 (hash percent).
--max-temp reads the CPU package sensor on CPU runs and the CARD sensor (AMD hwmon / nvidia-smi) on GPU runs; a proportional band below the cap avoids pause/resume sawtooth. The rate line reports gpu temp/power.
exit: 0 found/done/session-complete, 1 exhausted/idle, 2 usage."
);

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn parse_u64(s: &str, flag: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .map_err(|_| format!("--{flag} expects a non-negative integer, got '{s}'"))
}

fn parse_usize(s: &str, flag: &str) -> Result<usize, String> {
    s.parse::<usize>()
        .map_err(|_| format!("--{flag} expects a non-negative integer, got '{s}'"))
}

fn parse_hex(s: &str, flag: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("--{flag} expects even-length hex, got '{s}'"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| format!("--{flag} expects hex, got '{s}'"))
        })
        .collect()
}

fn parse_target(s: &str) -> Result<[u8; 32], String> {
    if s == "max" {
        return Ok([0xffu8; 32]);
    }
    if let Some(nbits) = s.strip_prefix("nbits:") {
        let nbits = nbits.strip_prefix("0x").unwrap_or(nbits);
        let v = u32::from_str_radix(nbits, 16)
            .map_err(|_| format!("--target nbits expects 8 hex digits, got '{s}'"))?;
        return compact_to_target(v)
            .map(|t| t.to_big_endian())
            .ok_or_else(|| {
                format!("--target nbits value '{s}' is not a valid difficulty encoding")
            });
    }
    let b = parse_hex(s, "target")?;
    if b.len() != 32 {
        return Err(format!(
            "--target expects 64 hex chars (32 bytes big-endian), got {} bytes",
            b.len()
        ));
    }
    let mut t = [0u8; 32];
    t.copy_from_slice(&b);
    Ok(t)
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Backend selection for bench/mine/pool. Default is CPU: the GPU's own
/// thermal/power guards (doc 15 §15.5, step 3) are not wired yet, so GPU
/// mining stays opt-in until then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceSel {
    Cpu,
    Auto,
    OpenCl,
}

/// GPU knobs shared by bench/mine/pool.
#[derive(Debug, Clone, Copy)]
struct GpuCfg {
    sel: DeviceSel,
    platform: usize,
    device: usize,
    batch: usize,
    /// `--max-power W`: apply `nvidia-smi -pl W` when opening the GPU
    /// (needs driver permission; a failure warns and continues).
    max_power_w: u32,
}

impl Default for GpuCfg {
    fn default() -> Self {
        Self {
            sel: DeviceSel::Cpu,
            platform: 0,
            device: 0,
            batch: gpu::DEFAULT_BATCH,
            max_power_w: 0,
        }
    }
}

fn parse_device(s: &str) -> Result<DeviceSel, String> {
    match s {
        "cpu" => Ok(DeviceSel::Cpu),
        "auto" => Ok(DeviceSel::Auto),
        "opencl" => Ok(DeviceSel::OpenCl),
        other => Err(format!("--device expects auto|cpu|opencl, got '{other}'")),
    }
}

/// Handle the four GPU flags if `args[i]` is one; advances `i` past the
/// value. Returns false for any other flag so the caller's match runs.
fn try_parse_gpu_flag(args: &[String], i: &mut usize, cfg: &mut GpuCfg) -> Result<bool, String> {
    match args[*i].as_str() {
        "--device" => {
            *i += 1;
            cfg.sel = parse_device(args.get(*i).ok_or("--device needs a value")?)?;
        }
        "--gpu-platform" => {
            *i += 1;
            cfg.platform = parse_usize(
                args.get(*i).ok_or("--gpu-platform needs a value")?,
                "gpu-platform",
            )?;
        }
        "--gpu-device" => {
            *i += 1;
            cfg.device = parse_usize(
                args.get(*i).ok_or("--gpu-device needs a value")?,
                "gpu-device",
            )?;
        }
        "--max-power" => {
            *i += 1;
            cfg.max_power_w = parse_usize(
                args.get(*i).ok_or("--max-power needs a value")?,
                "max-power",
            )? as u32;
        }
        "--gpu-batch" => {
            *i += 1;
            cfg.batch = parse_usize(
                args.get(*i).ok_or("--gpu-batch needs a value")?,
                "gpu-batch",
            )?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Open the GPU when the selection asks for (or allows) it. `auto` falls
/// back to CPU with a note; `opencl` is a hard error when unavailable.
fn open_gpu(cfg: &GpuCfg) -> Result<Option<GpuSearcher>, String> {
    if cfg.batch == 0 {
        return Err("--gpu-batch must be >= 1".to_string());
    }
    let opened = match cfg.sel {
        DeviceSel::Cpu => return Ok(None),
        DeviceSel::Auto => match GpuSearcher::open(cfg.platform, cfg.device, cfg.batch) {
            Ok(g) => Some(g),
            Err(e) => {
                eprintln!("gpu: unavailable ({e}); using CPU");
                None
            }
        },
        DeviceSel::OpenCl => Some(
            GpuSearcher::open(cfg.platform, cfg.device, cfg.batch)
                .map_err(|e| format!("--device opencl: {e}"))?,
        ),
    };
    if let Some(g) = &opened {
        if cfg.max_power_w > 0 {
            apply_power_limit(cfg.max_power_w);
        }
        let (t, p) = gpu::gpu_status();
        match p {
            Some(w) => println!("gpu {}: {t}C, {w}W", g.name()),
            None => println!("gpu {}: {t}C", g.name()),
        }
    }
    Ok(opened)
}

/// Apply a board power limit (doc 15 §15.5). nvidia-smi needs driver
/// permission (often root); failure is a warning, never fatal — the
/// thermal guard still bounds the card.
fn apply_power_limit(watts: u32) {
    match lm_miner::gpu::apply_power_limit(watts) {
        Ok(()) => println!("gpu power limit set to {watts} W"),
        Err(e) => eprintln!("gpu power limit {watts} W not applied: {e}"),
    }
}

// The grind engine comes from the shared pool runtime now
// (`lm_miner::pool_loop::Backend`): CPU threads or one GPU, the same
// search every embedded miner uses.

/// Resolve the backend from options, announcing the GPU when one is used.
fn make_backend(cfg: &GpuCfg, threads: usize) -> Result<Backend, String> {
    match open_gpu(cfg)? {
        Some(g) => {
            println!("gpu: {} (batch {})", g.name(), g.batch());
            Ok(Backend::Gpu(g))
        }
        None => Ok(Backend::Cpu { threads }),
    }
}

struct BenchOpts {
    threads: usize,
    hashes: u64,
    height: u64,
    header: Vec<u8>,
    json: bool,
    gpu: GpuCfg,
}

struct MineOpts {
    header: Vec<u8>,
    height: u64,
    target: [u8; 32],
    threads: usize,
    start: u64,
    count: u64,
    max_temp: i64,
    duty: i64,
    gpu: GpuCfg,
}

fn parse_bench(args: &[String]) -> Result<BenchOpts, String> {
    let mut o = BenchOpts {
        threads: default_threads(),
        hashes: 3,
        height: 1,
        header: (0u8..80).collect(),
        json: false,
        gpu: GpuCfg::default(),
    };
    let mut i = 0;
    while i < args.len() {
        if try_parse_gpu_flag(args, &mut i, &mut o.gpu)? {
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--threads" => {
                i += 1;
                o.threads = parse_usize(args.get(i).ok_or("--threads needs a value")?, "threads")?;
            }
            "--hashes" => {
                i += 1;
                o.hashes = parse_u64(args.get(i).ok_or("--hashes needs a value")?, "hashes")?;
            }
            "--height" => {
                i += 1;
                o.height = parse_u64(args.get(i).ok_or("--height needs a value")?, "height")?;
            }
            "--header" => {
                i += 1;
                o.header = parse_hex(args.get(i).ok_or("--header needs a value")?, "header")?;
            }
            "--json" => o.json = true,
            other => return Err(format!("bench: unknown flag '{other}'")),
        }
        i += 1;
    }
    if o.threads == 0 {
        return Err("--threads must be >= 1".to_string());
    }
    if o.hashes == 0 {
        return Err("--hashes must be >= 1".to_string());
    }
    if o.header.is_empty() {
        return Err("--header must not be empty".to_string());
    }
    Ok(o)
}

fn parse_mine(args: &[String]) -> Result<MineOpts, String> {
    let mut header: Option<Vec<u8>> = None;
    let mut height: Option<u64> = None;
    let mut target: Option<[u8; 32]> = None;
    let mut threads = default_threads();
    let mut start = 0u64;
    let mut count = 1_000_000u64;
    let mut max_temp = lm_miner::thermal::DEFAULT_MAX_TEMP_C;
    let mut duty = 100i64;
    let mut gpu = GpuCfg::default();
    let mut i = 0;
    while i < args.len() {
        if try_parse_gpu_flag(args, &mut i, &mut gpu)? {
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--max-temp" => {
                i += 1;
                max_temp = args
                    .get(i)
                    .ok_or("--max-temp needs a value")?
                    .parse()
                    .map_err(|_| "--max-temp expects an integer")?;
            }
            "--duty" => {
                i += 1;
                duty = args
                    .get(i)
                    .ok_or("--duty needs a value")?
                    .parse()
                    .map_err(|_| "--duty expects an integer")?;
            }
            "--header" => {
                i += 1;
                header = Some(parse_hex(
                    args.get(i).ok_or("--header needs a value")?,
                    "header",
                )?);
            }
            "--height" => {
                i += 1;
                height = Some(parse_u64(
                    args.get(i).ok_or("--height needs a value")?,
                    "height",
                )?);
            }
            "--target" => {
                i += 1;
                target = Some(parse_target(args.get(i).ok_or("--target needs a value")?)?);
            }
            "--threads" => {
                i += 1;
                threads = parse_usize(args.get(i).ok_or("--threads needs a value")?, "threads")?;
            }
            "--start" => {
                i += 1;
                start = parse_u64(args.get(i).ok_or("--start needs a value")?, "start")?;
            }
            "--count" => {
                i += 1;
                count = parse_u64(args.get(i).ok_or("--count needs a value")?, "count")?;
            }
            other => return Err(format!("mine: unknown flag '{other}'")),
        }
        i += 1;
    }
    let header = header.ok_or("--header is required")?;
    if header.is_empty() {
        return Err("--header must not be empty".to_string());
    }
    Ok(MineOpts {
        header,
        height: height.ok_or("--height is required")?,
        target: target.ok_or("--target is required")?,
        threads,
        start,
        count,
        max_temp,
        duty,
        gpu,
    })
}

fn run_bench(o: &BenchOpts) {
    let gpu = match open_gpu(&o.gpu) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let report = match gpu {
        Some(g) => {
            println!("gpu: {} (batch {})", g.name(), g.batch());
            match g.bench(&o.header, o.height, o.hashes) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => match bench(&o.header, o.height, o.hashes, o.threads) {
            Some(r) => r,
            None => {
                eprintln!("bench: need --threads >= 1");
                std::process::exit(2);
            }
        },
    };
    if o.json {
        println!("{}", report.to_json());
        return;
    }
    println!(
        "luckyhash_v1 bench  arch={}  cpu={}",
        std::env::consts::ARCH,
        report.cpu
    );
    println!(
        "threads={}  hashes/thread={}  total={}  elapsed_ms={}",
        report.threads, report.hashes_per_thread, report.total_hashes, report.elapsed_ms
    );
    println!(
        "total: {:.1} H/s  ({:.3} ms/hash)",
        report.total_hps, report.ms_per_hash
    );
    for (t, h) in report.per_thread_hps.iter().enumerate() {
        println!("  thread {t}: {h:.1} H/s");
    }
}

fn run_mine(o: &MineOpts) -> ExitCode {
    let policy = match MiningPolicy::new(o.max_temp, o.duty) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let backend = match make_backend(&o.gpu, o.threads) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    match backend.search(&o.header, o.height, &o.target, o.start, o.count, &policy) {
        Ok(Some(s)) => {
            println!("found nonce={} out={}", s.nonce, hex_of(&s.out));
            ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!(
                "no solution in nonces [{}, {})",
                o.start,
                o.start.saturating_add(o.count)
            );
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

struct PoolOpts {
    pool: String,
    worker: String,
    agent: String,
    /// True when the user passed --agent (default agents get the device
    /// suffix appended so pools can label workers).
    agent_set: bool,
    threads: usize,
    start: u64,
    count: u64,
    max_shares: u64, // u64::MAX = unlimited
    job_timeout_secs: u64,
    max_temp: i64,
    duty: i64,
    gpu: GpuCfg,
    /// Exit on connection loss instead of reconnecting (scripted use).
    fail_fast: bool,
    /// Connect to the pool over TLS (stratum over TLS).
    tls: bool,
    /// Extra CA (PEM) to trust for TLS; default is the Mozilla root store.
    tls_ca: Option<String>,
    /// Skip TLS verification (tests only — never a public pool).
    tls_insecure: bool,
    /// Override the TLS server name (default: the host part of --pool).
    tls_server_name: Option<String>,
    /// Pool password sent in `mining.authorize` (see pool `--pool-password`).
    password: String,
    /// Speak Stratum V2 (Noise NX + Lucky jobs) instead of V1. The pool
    /// must serve it (`--v2-bind`); the password/token rides as the V2
    /// device id.
    v2: bool,
    /// Pool authority key to pin (§4.7 URL key or 64 hex chars). Unset =
    /// encrypted but unauthenticated (local testing only).
    v2_authority: Option<String>,
    /// Dev-fee percent of hash time (0-100, default 1.0; 0 opts out).
    dev_fee_pct: f64,
    /// Dev address the fee time mines to (required for the fee to apply).
    dev_address: Option<String>,
}

fn parse_pool(args: &[String]) -> Result<PoolOpts, String> {
    let mut pool: Option<String> = None;
    let mut worker: Option<String> = None;
    let mut agent = AGENT.to_string();
    let mut agent_set = false;
    let mut threads = default_threads();
    let mut start = 0u64;
    let mut count = 1_000_000u64;
    let mut max_shares = u64::MAX;
    let mut job_timeout_secs = 30u64;
    let mut max_temp = lm_miner::thermal::DEFAULT_MAX_TEMP_C;
    let mut duty = 100i64;
    let mut gpu = GpuCfg::default();
    let mut fail_fast = false;
    let mut tls = false;
    let mut tls_ca: Option<String> = None;
    let mut tls_insecure = false;
    let mut tls_server_name: Option<String> = None;
    let mut password = "x".to_string();
    let mut v2 = false;
    let mut v2_authority: Option<String> = None;
    let mut dev_fee_pct = 1.0f64;
    let mut dev_address: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if try_parse_gpu_flag(args, &mut i, &mut gpu)? {
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--fail-fast" => fail_fast = true,
            "--max-temp" => {
                i += 1;
                max_temp = args
                    .get(i)
                    .ok_or("--max-temp needs a value")?
                    .parse()
                    .map_err(|_| "--max-temp expects an integer")?;
            }
            "--duty" => {
                i += 1;
                duty = args
                    .get(i)
                    .ok_or("--duty needs a value")?
                    .parse()
                    .map_err(|_| "--duty expects an integer")?;
            }
            "--pool" => {
                i += 1;
                pool = Some(args.get(i).ok_or("--pool needs a value")?.to_string());
            }
            "--worker" => {
                i += 1;
                worker = Some(args.get(i).ok_or("--worker needs a value")?.to_string());
            }
            "--agent" => {
                i += 1;
                agent = args.get(i).ok_or("--agent needs a value")?.to_string();
                agent_set = true;
            }
            "--password" => {
                i += 1;
                password = args.get(i).ok_or("--password needs a value")?.to_string();
            }
            "--v2" => v2 = true,
            "--v2-authority" => {
                i += 1;
                v2_authority = Some(
                    args.get(i)
                        .ok_or("--v2-authority needs a value")?
                        .to_string(),
                );
            }
            "--dev-fee-pct" => {
                i += 1;
                dev_fee_pct = args
                    .get(i)
                    .ok_or("--dev-fee-pct needs a value")?
                    .parse()
                    .map_err(|_| "--dev-fee-pct expects a number 0-100")?;
                if !(0.0..=100.0).contains(&dev_fee_pct) {
                    return Err("--dev-fee-pct must be 0-100".to_string());
                }
            }
            "--dev-address" => {
                i += 1;
                dev_address = Some(
                    args.get(i)
                        .ok_or("--dev-address needs a value")?
                        .to_string(),
                );
            }
            "--tls" => tls = true,
            "--tls-insecure" => tls_insecure = true,
            "--tls-ca" => {
                i += 1;
                tls_ca = Some(args.get(i).ok_or("--tls-ca needs a value")?.to_string());
            }
            "--tls-server-name" => {
                i += 1;
                tls_server_name = Some(
                    args.get(i)
                        .ok_or("--tls-server-name needs a value")?
                        .to_string(),
                );
            }
            "--threads" => {
                i += 1;
                threads = parse_usize(args.get(i).ok_or("--threads needs a value")?, "threads")?;
            }
            "--start" => {
                i += 1;
                start = parse_u64(args.get(i).ok_or("--start needs a value")?, "start")?;
            }
            "--count" => {
                i += 1;
                count = parse_u64(args.get(i).ok_or("--count needs a value")?, "count")?;
            }
            "--max-shares" => {
                i += 1;
                max_shares = parse_u64(
                    args.get(i).ok_or("--max-shares needs a value")?,
                    "max-shares",
                )?;
            }
            "--job-timeout" => {
                i += 1;
                job_timeout_secs = parse_u64(
                    args.get(i).ok_or("--job-timeout needs a value")?,
                    "job-timeout",
                )?;
            }
            other => return Err(format!("pool: unknown flag '{other}'")),
        }
        i += 1;
    }
    let pool = pool.ok_or("--pool is required (HOST:PORT)")?;
    let worker = worker.ok_or("--worker is required")?;
    if worker.is_empty() {
        return Err("--worker must not be empty".to_string());
    }
    if threads == 0 {
        return Err("--threads must be >= 1".to_string());
    }
    if count == 0 {
        return Err("--count must be >= 1".to_string());
    }
    Ok(PoolOpts {
        pool,
        worker,
        agent,
        agent_set,
        threads,
        start,
        count,
        max_shares,
        job_timeout_secs,
        max_temp,
        duty,
        gpu,
        fail_fast,
        tls,
        tls_ca,
        tls_insecure,
        tls_server_name,
        password,
        v2,
        v2_authority,
        dev_fee_pct,
        dev_address,
    })
}

fn run_pool(o: &PoolOpts) -> ExitCode {
    let policy = match MiningPolicy::new(o.max_temp, o.duty) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = lm_miner::pool_loop::resolve_pool(&o.pool) {
        eprintln!("error: {e}");
        return ExitCode::from(2);
    }
    if o.dev_fee_pct <= 0.0 {
        eprintln!("note: dev fee disabled (--dev-fee-pct 0); consider supporting Lucky Lode development");
    } else if o.dev_address.as_deref().unwrap_or("").is_empty() {
        eprintln!("warning: --dev-fee-pct {} set but no --dev-address; mining 100% to worker", o.dev_fee_pct);
    } else {
        let dev_secs = (3600.0 * o.dev_fee_pct / 100.0) as u64;
        eprintln!(
            "dev fee: {:.2}% to {} (~{}s/hour, rotating sessions); thank you for supporting Lucky Lode",
            o.dev_fee_pct,
            o.dev_address.as_deref().unwrap(),
            dev_secs
        );
    }
    // Backend first: the default agent advertises the device to the pool
    // (dashboard labels), so connect only once it is known.
    let backend = match open_gpu(&o.gpu) {
        Ok(Some(g)) => {
            println!("gpu: {} (batch {})", g.name(), g.batch());
            Backend::Gpu(g)
        }
        Ok(None) => Backend::Cpu { threads: o.threads },
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let v2_authority = if !o.v2 {
        None
    } else {
        match &o.v2_authority {
            Some(s) => match parse_v2_authority(s) {
                Ok(k) => Some(k),
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(2);
                }
            },
            None => {
                eprintln!(
                    "warning: no --v2-authority: encrypted but unauthenticated (local testing only)"
                );
                None
            }
        }
    };
    let cfg = PoolConfig {
        pool: o.pool.clone(),
        worker: o.worker.clone(),
        agent: o.agent_set.then(|| o.agent.clone()),
        password: o.password.clone(),
        threads: o.threads,
        start: o.start,
        count: o.count,
        chunk: 65_536,
        max_shares: o.max_shares,
        job_timeout_secs: o.job_timeout_secs,
        max_temp: o.max_temp,
        duty: o.duty,
        fail_fast: o.fail_fast,
        tls: o.tls.then(|| TlsCfg {
            server_name: o.tls_server_name.clone(),
            ca_pem: o.tls_ca.clone(),
            insecure: o.tls_insecure,
        }),
        v2: o.v2,
        v2_authority,
        dev_fee_pct: o.dev_fee_pct,
        dev_address: o.dev_address.clone(),
        report_secs: 30,
    };
    // The CLI is a foreground service: no stop flag, console as the sink.
    let stop = AtomicBool::new(false);
    let pool = o.pool.clone();
    let gpu_backend = matches!(backend, Backend::Gpu(_));
    let mut sink = |ev: Event| match ev {
        Event::Connected { v2, worker, detail } => {
            let wire = if v2 { "pool(v2)" } else { "pool" };
            println!("{wire} {pool} as '{worker}' ({detail})");
        }
        Event::DevFee { worker } => {
            eprintln!("dev fee: mining as '{worker}' (rotating back shortly)");
        }
        Event::ShareAccepted { job, nonce } => {
            println!("share accepted: job={job} nonce={nonce}");
        }
        Event::ShareRejected { job, error } => {
            eprintln!("share rejected: job={job} ({error})");
        }
        Event::Rate { hps, hashes, window_secs } => {
            let gpu_note = if gpu_backend {
                match gpu::gpu_status() {
                    (t, Some(w)) => format!(" (gpu {t}C {w}W)"),
                    (t, None) if t >= 0 => format!(" (gpu {t}C)"),
                    _ => String::new(),
                }
            } else {
                String::new()
            };
            println!("rate: {hps:.0} H/s ({hashes} hashes/{window_secs}s window){gpu_note}");
        }
        Event::Reconnect { reason, wait_secs } => {
            if wait_secs > 0 {
                eprintln!("{reason}; retrying in {wait_secs}s");
            } else {
                eprintln!("{reason}; reconnecting");
            }
        }
        Event::Idle { timeout_secs, accepted, submitted } => {
            eprintln!(
                "idle: no job within {timeout_secs}s ({accepted}/{submitted} accepted); reconnecting"
            );
        }
        Event::JobExhausted { job } => {
            println!("job {job} exhausted, waiting for new template");
        }
        Event::JobAdopted { job, height } => {
            println!("job {job} height {height}");
        }
        Event::Note(n) => eprintln!("{n}"),
    };
    match lm_miner::pool_loop::run(&cfg, &backend, &policy, &stop, &mut sink) {
        Outcome::SessionComplete { accepted, submitted } => {
            println!("session complete: {accepted}/{submitted} shares accepted");
            ExitCode::SUCCESS
        }
        Outcome::Stopped { .. } => ExitCode::SUCCESS,
        Outcome::Failed { error } => {
            eprintln!("error: {error}");
            ExitCode::from(1)
        }
    }
}

/// Parse `--v2-authority`: a §4.7 base58check URL key first, else 64 hex
/// chars of x-only public key.
fn parse_v2_authority(s: &str) -> Result<[u8; 32], String> {
    if let Ok(k) = lm_stratum::v2_session::parse_authority_url_key(s) {
        return Ok(k);
    }
    let b = parse_hex(s, "v2-authority")?;
    if b.len() != 32 {
        return Err(format!(
            "--v2-authority expects a §4.7 URL key or 64 hex chars, got '{s}'"
        ));
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&b);
    Ok(k)
}

fn run_gpu_test(args: &[String]) -> ExitCode {
    let mut platform = 0usize;
    let mut device = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--platform" => {
                i += 1;
                platform = match args.get(i).and_then(|s| s.parse().ok()) {
                    Some(n) => n,
                    None => {
                        eprintln!("error: --platform needs a value");
                        return ExitCode::from(2);
                    }
                };
            }
            "--device" => {
                i += 1;
                device = match args.get(i).and_then(|s| s.parse().ok()) {
                    Some(n) => n,
                    None => {
                        eprintln!("error: --device needs a value");
                        return ExitCode::from(2);
                    }
                };
            }
            other => {
                eprintln!("error: gpu-test: unknown flag '{other}'");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    match opencl::GpuDevice::open(platform, device) {
        Ok(dev) => {
            let (wg, pm) = dev.kernel_limits();
            println!(
                "gpu: {} (platform {platform}, device {device}; max work-group {wg}, private mem/work-item {pm} B)",
                dev.name()
            );
            match opencl::gpu_kat_vectors(&dev) {
                Ok(()) => {
                    println!("gpu KAT: PASS (frozen vectors match lm-pow)");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("gpu KAT: FAIL: {e}");
                    ExitCode::from(1)
                }
            }
        }
        Err(e) => {
            eprintln!("error: cannot open GPU: {e}");
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        println!("lm-miner {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if argv.iter().any(|a| a == "-h" || a == "--help") || argv.is_empty() {
        println!("{USAGE}");
        return if argv.is_empty() {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
    }
    match argv[0].as_str() {
        "bench" => match parse_bench(&argv[1..]) {
            Ok(o) => {
                run_bench(&o);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}\n{USAGE}");
                ExitCode::from(2)
            }
        },
        "mine" => match parse_mine(&argv[1..]) {
            Ok(o) => run_mine(&o),
            Err(e) => {
                eprintln!("error: {e}\n{USAGE}");
                ExitCode::from(2)
            }
        },
        "pool" => match parse_pool(&argv[1..]) {
            Ok(o) => run_pool(&o),
            Err(e) => {
                eprintln!("error: {e}\n{USAGE}");
                ExitCode::from(2)
            }
        },
        "gpu-test" => run_gpu_test(&argv[1..]),
        other => {
            eprintln!("error: unknown command '{other}'\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
