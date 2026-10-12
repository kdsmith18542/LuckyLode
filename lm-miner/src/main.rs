//! `lm-miner` — reference EmberHash v1 miner binary.
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
use lm_miner::thermal::MiningPolicy;
use lm_miner::{bench, gpu, opencl, search_with_policy, Solution};
use lm_pow::difficulty::compact_to_target;
use lm_stratum::client::PoolClient;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage:
  lm-miner bench [--threads N] [--hashes N] [--height H] [--header HEX] [--json] [--device auto|cpu|opencl] [--gpu-platform N] [--gpu-device N] [--gpu-batch N]
  lm-miner mine --header HEX --height H --target (HEX64 | nbits:XXXXXXXX | max) [--threads N] [--start N] [--count N] [--max-temp C] [--duty N] [--device auto|cpu|opencl] [--gpu-platform N] [--gpu-device N] [--gpu-batch N]
  lm-miner pool --pool HOST:PORT --worker NAME [--agent STR] [--threads N] [--start N] [--count N] [--max-shares N] [--job-timeout SECS] [--max-temp C] [--duty N] [--device auto|cpu|opencl] [--gpu-platform N] [--gpu-device N] [--gpu-batch N]
  lm-miner gpu-test [--platform N] [--device N]

bench defaults: threads=auto, hashes=3/thread, height=1, 80-byte 0x00..0x4F header.
mine defaults: threads=auto, start=0, count=1000000.
pool defaults: agent=lm-miner/0.1, threads=auto, start=0, count=1000000/job, max-shares=unlimited, job-timeout=30s.
gpu-test: runs the frozen KAT vectors through the GPU (needs OpenCL hardware).
device: cpu (default), auto (OpenCL when present, else CPU), opencl (hard error if absent).
gpu knobs: --gpu-platform 0, --gpu-device 0, --gpu-batch 512 (Argon2 instances per launch, 1 MiB VRAM each).
safety defaults: --max-temp 75 (pause above; 0 disables), --duty 100 (hash percent).
NOTE: --max-temp guards the CPU package sensor only; GPU-side thermal/power control is doc 15 step 3.
exit: 0 found/done/session-complete, 1 exhausted/idle, 2 usage.";

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
            .ok_or_else(|| format!("--target nbits value '{s}' is not a valid difficulty encoding"));
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
}

impl Default for GpuCfg {
    fn default() -> Self {
        Self { sel: DeviceSel::Cpu, platform: 0, device: 0, batch: gpu::DEFAULT_BATCH }
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
            cfg.platform =
                parse_usize(args.get(*i).ok_or("--gpu-platform needs a value")?, "gpu-platform")?;
        }
        "--gpu-device" => {
            *i += 1;
            cfg.device =
                parse_usize(args.get(*i).ok_or("--gpu-device needs a value")?, "gpu-device")?;
        }
        "--gpu-batch" => {
            *i += 1;
            cfg.batch =
                parse_usize(args.get(*i).ok_or("--gpu-batch needs a value")?, "gpu-batch")?;
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
    match cfg.sel {
        DeviceSel::Cpu => Ok(None),
        DeviceSel::Auto => match GpuSearcher::open(cfg.platform, cfg.device, cfg.batch) {
            Ok(g) => Ok(Some(g)),
            Err(e) => {
                eprintln!("gpu: unavailable ({e}); using CPU");
                Ok(None)
            }
        },
        DeviceSel::OpenCl => GpuSearcher::open(cfg.platform, cfg.device, cfg.batch)
            .map(Some)
            .map_err(|e| format!("--device opencl: {e}")),
    }
}

/// One of the two grind engines: CPU threads or one GPU.
enum Backend {
    Cpu { threads: usize },
    Gpu(GpuSearcher),
}

impl Backend {
    fn search(
        &self,
        header: &[u8],
        height: u64,
        target: &[u8; 32],
        start: u64,
        count: u64,
        policy: &MiningPolicy,
    ) -> Result<Option<Solution>, String> {
        match self {
            Backend::Cpu { threads } => {
                Ok(search_with_policy(header, height, target, start, count, *threads, policy))
            }
            Backend::Gpu(g) => g.search(header, height, target, start, count, policy),
        }
    }
}

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
                max_temp = args.get(i).ok_or("--max-temp needs a value")?.parse().map_err(|_| "--max-temp expects an integer")?;
            }
            "--duty" => {
                i += 1;
                duty = args.get(i).ok_or("--duty needs a value")?.parse().map_err(|_| "--duty expects an integer")?;
            }
            "--header" => {
                i += 1;
                header = Some(parse_hex(args.get(i).ok_or("--header needs a value")?, "header")?);
            }
            "--height" => {
                i += 1;
                height = Some(parse_u64(args.get(i).ok_or("--height needs a value")?, "height")?);
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
    println!("emberhash_v1 bench  arch={}  cpu={}", std::env::consts::ARCH, report.cpu);
    println!(
        "threads={}  hashes/thread={}  total={}  elapsed_ms={}",
        report.threads, report.hashes_per_thread, report.total_hashes, report.elapsed_ms
    );
    println!("total: {:.1} H/s  ({:.3} ms/hash)", report.total_hps, report.ms_per_hash);
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
}

fn parse_pool(args: &[String]) -> Result<PoolOpts, String> {
    let mut pool: Option<String> = None;
    let mut worker: Option<String> = None;
    let mut agent = "lm-miner/0.2".to_string();
    let mut agent_set = false;
    let mut threads = default_threads();
    let mut start = 0u64;
    let mut count = 1_000_000u64;
    let mut max_shares = u64::MAX;
    let mut job_timeout_secs = 30u64;
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
                max_temp = args.get(i).ok_or("--max-temp needs a value")?.parse().map_err(|_| "--max-temp expects an integer")?;
            }
            "--duty" => {
                i += 1;
                duty = args.get(i).ok_or("--duty needs a value")?.parse().map_err(|_| "--duty expects an integer")?;
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
                max_shares =
                    parse_u64(args.get(i).ok_or("--max-shares needs a value")?, "max-shares")?;
            }
            "--job-timeout" => {
                i += 1;
                job_timeout_secs =
                    parse_u64(args.get(i).ok_or("--job-timeout needs a value")?, "job-timeout")?;
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
    Ok(PoolOpts { pool, worker, agent, agent_set, threads, start, count, max_shares, job_timeout_secs, max_temp, duty, gpu })
}

fn run_pool(o: &PoolOpts) -> ExitCode {
    let policy = match MiningPolicy::new(o.max_temp, o.duty) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let addr: SocketAddr = match o.pool.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("error: --pool expects HOST:PORT, got '{}'", o.pool);
            return ExitCode::from(2);
        }
    };
    // Backend first: the default agent advertises the device to the pool
    // (dashboard labels), so connect only once it is known.
    let backend = match open_gpu(&o.gpu) {
        Ok(Some(g)) => Backend::Gpu(g),
        Ok(None) => Backend::Cpu { threads: o.threads },
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let agent = if o.agent_set {
        o.agent.clone()
    } else {
        let device = match &backend {
            Backend::Gpu(_) => "opencl",
            Backend::Cpu { .. } => "cpu",
        };
        format!("{} {}", o.agent, device)
    };
    let mut client = match PoolClient::connect(addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot reach pool {}: {e}", o.pool);
            return ExitCode::from(1);
        }
    };
    match client.subscribe(&agent) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("error: pool refused subscribe");
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("error: subscribe failed: {e}");
            return ExitCode::from(1);
        }
    }
    match client.authorize(&o.worker) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("error: pool refused authorize for '{}'", o.worker);
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("error: authorize failed: {e}");
            return ExitCode::from(1);
        }
    }
    match &backend {
        Backend::Cpu { threads } => {
            println!("pool {} as '{}' ({} threads)", o.pool, o.worker, threads)
        }
        Backend::Gpu(g) => println!(
            "pool {} as '{}' (gpu: {}, batch {})",
            o.pool,
            o.worker,
            g.name(),
            g.batch()
        ),
    }
    let mut submitted = 0u64;
    let mut accepted = 0u64;
    // Grind each job in CHUNKS and adopt newer jobs as they arrive: one
    // search per rotation left the GPU idle ~80% of the time (the pool's
    // template rotates every ~20 s, the card finishes a search in ~4 s).
    const CHUNK: u64 = 65_536;
    let chunk = CHUNK.min(o.count);
    let mut current: Option<lm_stratum::v1::Job> = None;
    let mut pos = o.start;
    let mut hashed: u64 = 0;
    let mut report_at = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if submitted >= o.max_shares {
            println!("session complete: {accepted}/{submitted} shares accepted");
            return ExitCode::SUCCESS;
        }
        if std::time::Instant::now() >= report_at {
            let secs = 30.0;
            println!(
                "rate: {:.0} H/s ({} hashes/{}s window)",
                hashed as f64 / secs,
                hashed,
                secs
            );
            hashed = 0;
            report_at = std::time::Instant::now() + Duration::from_secs(30);
        }
        // Non-blocking: pick up any newer job sitting in the buffer.
        // A vardiff retarget keeps the SAME header with a fresh job id —
        // keep the nonce position then, or the range gets re-ground and
        // the pool dedupe rejects the replayed shares (live-caught: ~10%
        // of shares were duplicate re-finds after rotations).
        loop {
            match client.poll_job() {
                Ok(Some(j)) => {
                    if current.as_ref().map(|c| c.id != j.id).unwrap_or(true) {
                        let same_header =
                            current.as_ref().map(|c| c.header == j.header).unwrap_or(false);
                        current = Some(j);
                        if !same_header {
                            pos = o.start;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("error: job read failed: {e}");
                    return ExitCode::from(1);
                }
            }
        }
        if current.is_none() {
            match client.read_job(Duration::from_secs(o.job_timeout_secs)) {
                Ok(Some(j)) => {
                    current = Some(j);
                    pos = o.start;
                }
                Ok(None) => {
                    eprintln!("idle: no job within {}s ({accepted}/{submitted} accepted)", o.job_timeout_secs);
                    return ExitCode::from(1);
                }
                Err(e) => {
                    eprintln!("error: job read failed: {e}");
                    return ExitCode::from(1);
                }
            }
        }
        let job = current.clone().expect("job set above");
        let job_end = o.start.saturating_add(o.count);
        if pos >= job_end {
            println!("job {} exhausted, waiting for new template", job.id);
            current = None;
            pos = o.start;
            continue;
        }
        let take = chunk.min(job_end - pos);
        match backend.search(&job.header, job.height, &job.pool_target, pos, take, &policy) {
            Ok(Some(sol)) => {
                hashed += sol.nonce.saturating_sub(pos).saturating_add(1);
                match client.submit(&o.worker, &job.id, sol.nonce) {
                    Ok((true, _)) => {
                        submitted += 1;
                        accepted += 1;
                        println!("share accepted: job={} nonce={}", job.id, sol.nonce);
                    }
                    Ok((false, err)) => {
                        submitted += 1;
                        eprintln!("share rejected: job={} {:?}", job.id, err);
                    }
                    Err(e) => {
                        eprintln!("error: submit failed: {e}");
                        return ExitCode::from(1);
                    }
                }
                // Post-submit settle: an accepted block rotates the job
                // within the pool's refresh tick, and grinding the stale
                // job in the meantime submits shares the chain rejects.
                // Poll briefly for the rotation before carrying on.
                let mut rotated = false;
                let settle = std::time::Instant::now() + Duration::from_millis(300);
                while std::time::Instant::now() < settle {
                    match client.poll_job() {
                        Ok(Some(j)) => {
                            if current.as_ref().map(|c| c.id != j.id).unwrap_or(true) {
                                let same_header =
                                    current.as_ref().map(|c| c.header == j.header).unwrap_or(false);
                                current = Some(j);
                                if !same_header {
                                    rotated = true;
                                }
                            }
                            break;
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                        Err(e) => {
                            eprintln!("error: job read failed: {e}");
                            return ExitCode::from(1);
                        }
                    }
                }
                pos = if rotated { o.start } else { sol.nonce.saturating_add(1) };
            }
            Ok(None) => {
                hashed += take;
                pos += take;
            }
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        }
    }
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
    if argv.iter().any(|a| a == "-h" || a == "--help") || argv.is_empty() {
        println!("{USAGE}");
        return if argv.is_empty() { ExitCode::from(2) } else { ExitCode::SUCCESS };
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
