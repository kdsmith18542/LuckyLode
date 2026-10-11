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

use lm_miner::{bench, cpu_label, search};
use lm_pow::difficulty::compact_to_target;
use lm_stratum::client::PoolClient;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage:
  lm-miner bench [--threads N] [--hashes N] [--height H] [--header HEX] [--json]
  lm-miner mine --header HEX --height H --target (HEX64 | nbits:XXXXXXXX | max) [--threads N] [--start N] [--count N]
  lm-miner pool --pool HOST:PORT --worker NAME [--agent STR] [--threads N] [--start N] [--count N] [--max-shares N] [--job-timeout SECS]

bench defaults: threads=auto, hashes=3/thread, height=1, 80-byte 0x00..0x4F header.
mine defaults: threads=auto, start=0, count=1000000.
pool defaults: agent=lm-miner/0.1, threads=auto, start=0, count=1000000/job, max-shares=unlimited, job-timeout=30s.
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

struct BenchOpts {
    threads: usize,
    hashes: u64,
    height: u64,
    header: Vec<u8>,
    json: bool,
}

struct MineOpts {
    header: Vec<u8>,
    height: u64,
    target: [u8; 32],
    threads: usize,
    start: u64,
    count: u64,
}

fn parse_bench(args: &[String]) -> Result<BenchOpts, String> {
    let mut o = BenchOpts {
        threads: default_threads(),
        hashes: 3,
        height: 1,
        header: (0u8..80).collect(),
        json: false,
    };
    let mut i = 0;
    while i < args.len() {
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
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
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
    })
}

fn run_bench(o: &BenchOpts) {
    match bench(&o.header, o.height, o.hashes, o.threads) {
        None => {
            eprintln!("bench: need --threads >= 1");
            std::process::exit(2);
        }
        Some(r) => {
            if o.json {
                println!("{}", r.to_json());
                return;
            }
            println!("emberhash_v1 bench  arch={}  cpu={}", std::env::consts::ARCH, cpu_label());
            println!(
                "threads={}  hashes/thread={}  total={}  elapsed_ms={}",
                r.threads, r.hashes_per_thread, r.total_hashes, r.elapsed_ms
            );
            println!("total: {:.1} H/s  ({:.3} ms/hash)", r.total_hps, r.ms_per_hash);
            for (t, h) in r.per_thread_hps.iter().enumerate() {
                println!("  thread {t}: {h:.1} H/s");
            }
        }
    }
}

fn run_mine(o: &MineOpts) -> ExitCode {
    match search(&o.header, o.height, &o.target, o.start, o.count, o.threads) {
        Some(s) => {
            println!("found nonce={} out={}", s.nonce, hex_of(&s.out));
            ExitCode::SUCCESS
        }
        None => {
            eprintln!(
                "no solution in nonces [{}, {})",
                o.start,
                o.start.saturating_add(o.count)
            );
            ExitCode::from(1)
        }
    }
}

struct PoolOpts {
    pool: String,
    worker: String,
    agent: String,
    threads: usize,
    start: u64,
    count: u64,
    max_shares: u64, // u64::MAX = unlimited
    job_timeout_secs: u64,
}

fn parse_pool(args: &[String]) -> Result<PoolOpts, String> {
    let mut pool: Option<String> = None;
    let mut worker: Option<String> = None;
    let mut agent = "lm-miner/0.1".to_string();
    let mut threads = default_threads();
    let mut start = 0u64;
    let mut count = 1_000_000u64;
    let mut max_shares = u64::MAX;
    let mut job_timeout_secs = 30u64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
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
    Ok(PoolOpts { pool, worker, agent, threads, start, count, max_shares, job_timeout_secs })
}

fn run_pool(o: &PoolOpts) -> ExitCode {
    let addr: SocketAddr = match o.pool.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("error: --pool expects HOST:PORT, got '{}'", o.pool);
            return ExitCode::from(2);
        }
    };
    let mut client = match PoolClient::connect(addr) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot reach pool {}: {e}", o.pool);
            return ExitCode::from(1);
        }
    };
    match client.subscribe(&o.agent) {
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
    println!("pool {} as '{}' ({} threads)", o.pool, o.worker, o.threads);
    let mut submitted = 0u64;
    let mut accepted = 0u64;
    let mut last_job = String::new();
    loop {
        if submitted >= o.max_shares {
            println!("session complete: {accepted}/{submitted} shares accepted");
            return ExitCode::SUCCESS;
        }
        let job = match client.read_job(Duration::from_secs(o.job_timeout_secs)) {
            Ok(Some(j)) => j,
            Ok(None) => {
                eprintln!("idle: no job within {}s ({accepted}/{submitted} accepted)", o.job_timeout_secs);
                return ExitCode::from(1);
            }
            Err(e) => {
                eprintln!("error: job read failed: {e}");
                return ExitCode::from(1);
            }
        };
        if job.id == last_job {
            continue; // already ground this template
        }
        last_job = job.id.clone();
        match search(&job.header, job.height, &job.pool_target, o.start, o.count, o.threads) {
            Some(sol) => match client.submit(&o.worker, &job.id, sol.nonce) {
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
            },
            None => {
                println!("job {} exhausted, waiting for new template", job.id);
            }
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
        other => {
            eprintln!("error: unknown command '{other}'\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
