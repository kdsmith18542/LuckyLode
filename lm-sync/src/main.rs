//! lm-sync — fast, stall-proof chain sync for Lucky Lode nodes.
//!
//! The node's P2P sync is correct but fragile on small networks: one banned
//! peer (a single `Misbehaving(100)` during a rule transition) stalls headers
//! AND blocks with no self-recovery. lm-sync bypasses P2P entirely:
//!
//! - `pull`: copy blocks `to_height+1 ..= from_height` from a source node to
//!   a target node over JSON-RPC (`getblock` hex → `submitblock`, in order).
//!   `submitblock` runs the full validation pipeline (header + ConnectBlock,
//!   including LuckyHash, LWMA, jackpot/pot/draw checks), so the target ends
//!   with identical, fully-validated state — just without waiting on P2P.
//! - `watch`: supervise a node; when its tip stalls while the source is
//!   ahead, pull the gap AND heal P2P (`clearbanned` + `addnode onetry`) so
//!   the normal path resumes too.
//! - `status`: one-line tip/headers/peers snapshot for monitors.
//!
//! std only (+ serde_json via lm-stratum's RpcClient). No new dependencies.

use lm_stratum::rpc::RpcClient;
use std::time::{Duration, Instant};

struct Endpoint {
    rpc: RpcClient,
    label: String,
}

impl Endpoint {
    fn height(&self) -> Result<u64, String> {
        let v = self.rpc.call("getblockcount", serde_json::json!([]))?;
        v.as_u64().ok_or_else(|| "bad getblockcount".to_string())
    }
    fn hash_at(&self, h: u64) -> Result<String, String> {
        let v = self.rpc.call("getblockhash", serde_json::json!([h]))?;
        v.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "bad getblockhash".to_string())
    }
    fn block_hex(&self, hash: &str) -> Result<String, String> {
        // verbosity 0 → raw hex.
        let v = self.rpc.call("getblock", serde_json::json!([hash, 0]))?;
        v.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "bad getblock hex".to_string())
    }
    fn submit(&self, hex: &str) -> Result<(), String> {
        match self.rpc.call("submitblock", serde_json::json!([hex])) {
            Ok(_) => Ok(()),
            // Idempotent re-runs: already-known blocks are not failures.
            Err(e) if is_duplicate(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }
    fn best(&self) -> Result<String, String> {
        let v = self.rpc.call("getbestblockhash", serde_json::json!([]))?;
        v.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "bad getbestblockhash".to_string())
    }
    fn headers(&self) -> Result<u64, String> {
        let v = self.rpc.call("getblockchaininfo", serde_json::json!([]))?;
        v.get("headers")
            .and_then(|h| h.as_u64())
            .ok_or_else(|| "bad headers".to_string())
    }
}

fn is_duplicate(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("duplicate") || l.contains("already have") || l.contains("block-already")
}

/// Copy every block in (to_height, from_height] from `src` to `dst`.
/// Returns (blocks_copied, elapsed).
fn pull_range(
    src: &Endpoint,
    dst: &Endpoint,
    from_h: u64,
    to_h: u64,
) -> Result<(u64, Duration), String> {
    if from_h <= to_h {
        return Ok((0, Duration::ZERO));
    }
    let t0 = Instant::now();
    let mut n = 0u64;
    for h in (to_h + 1)..=from_h {
        let hash = src.hash_at(h)?;
        let hex = src.block_hex(&hash)?;
        dst.submit(&hex)
            .map_err(|e| format!("height {h} ({hash}): {e}"))?;
        n += 1;
        if n.is_multiple_of(25) || h == from_h {
            let el = t0.elapsed().as_secs_f64().max(0.001);
            eprintln!("  …{h}/{from_h} ({:.0} blocks/s)", n as f64 / el);
        }
    }
    Ok((n, t0.elapsed()))
}

const HELP: &str = "lm-sync — Lucky Lode fast sync\n\
         \n\
         pull:   lm-sync pull --from RPC --from-user U --from-pass P \\\n\
         \u{2009}                      --to RPC   --to-user U   --to-pass P\n\
         watch:  lm-sync watch <same endpoints> [--source-p2p HOST:PORT]\n\
         \u{2009}                      [--stall-secs 180] [--interval 15]\n\
         status: lm-sync status --node RPC --node-user U --node-pass P\n\
         \n\
         notes: plain HTTP only — reach remote nodes through an SSH tunnel\n\
         (ssh -L 11927:127.0.0.1:9271 user@host). Unknown flags and\n\
         non-numeric --stall-secs/--interval are errors, not silent defaults.";

fn usage() -> ! {
    eprintln!("{HELP}");
    std::process::exit(2);
}

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

/// Reject unknown `--flags` (typos used to vanish silently). Values ride
/// the next argv slot after a known flag, so `--`-leading values still
/// work there; a bare `--foo` that names no known flag is an error.
fn check_flags(args: &[String], known: &[&str]) {
    let mut i = 2; // past binary + subcommand
    while i < args.len() {
        let a = &args[i];
        if known.contains(&a.as_str()) {
            i += 2; // flag + its value
            continue;
        }
        if a.starts_with("--") {
            eprintln!("error: unknown flag '{a}'\n{HELP}");
            std::process::exit(2);
        }
        i += 1;
    }
}

fn parse_u64_flag(args: &[String], name: &str, default: u64) -> u64 {
    let raw = arg(args, name, "");
    if raw.is_empty() {
        return default;
    }
    raw.parse().unwrap_or_else(|_| {
        eprintln!("error: {name} expects an integer, got '{raw}'");
        std::process::exit(2);
    })
}

fn endpoints(args: &[String]) -> (Endpoint, Endpoint) {
    let from = Endpoint {
        rpc: RpcClient::new(
            &arg(args, "--from", ""),
            &arg(args, "--from-user", ""),
            &arg(args, "--from-pass", ""),
        ),
        label: "source".into(),
    };
    let to = Endpoint {
        rpc: RpcClient::new(
            &arg(args, "--to", ""),
            &arg(args, "--to-user", ""),
            &arg(args, "--to-pass", ""),
        ),
        label: "target".into(),
    };
    if arg(args, "--from", "").is_empty() || arg(args, "--to", "").is_empty() {
        usage();
    }
    (from, to)
}

fn cmd_pull(args: &[String]) -> Result<(), String> {
    check_flags(
        args,
        &[
            "--from",
            "--from-user",
            "--from-pass",
            "--to",
            "--to-user",
            "--to-pass",
        ],
    );
    let (src, dst) = endpoints(args);
    let from_h = src.height()?;
    let to_h = dst.height()?;
    println!(
        "pull: {} @ {} → {} @ {}",
        src.label, from_h, dst.label, to_h
    );
    if from_h <= to_h {
        // Same-or-ahead height is not "in sync" by itself: compare tips.
        // A reorg or a wrong source leaves equal heights on different
        // chains, and blindly copying would fork the target.
        let (sb, db) = (src.best()?, dst.best()?);
        if sb == db {
            println!("already in sync at height {from_h} ({sb})");
            return Ok(());
        }
        return Err(format!(
            "DIVERGED: equal heights, different tips — src {sb} dst {db} (reorg or wrong source; refusing to overwrite)"
        ));
    }
    let (n, el) = pull_range(&src, &dst, from_h, to_h)?;
    let el_s = el.as_secs_f64().max(0.001);
    // Verify identical tips — proves header sync + state, not just count.
    let (sb, db) = (src.best()?, dst.best()?);
    println!("copied {n} blocks in {el_s:.1}s ({:.0}/s)", n as f64 / el_s);
    if sb == db {
        println!("tips match: {sb}");
        Ok(())
    } else {
        Err(format!("TIP MISMATCH: src {sb} dst {db}"))
    }
}

fn cmd_status(args: &[String]) -> Result<(), String> {
    check_flags(args, &["--node", "--node-user", "--node-pass"]);
    let ep = Endpoint {
        rpc: RpcClient::new(
            &arg(args, "--node", ""),
            &arg(args, "--node-user", ""),
            &arg(args, "--node-pass", ""),
        ),
        label: "node".into(),
    };
    if arg(args, "--node", "").is_empty() {
        usage();
    }
    let peers: usize = ep
        .rpc
        .call("getpeerinfo", serde_json::json!([]))?
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    let best: String = ep.best()?.chars().take(64).collect();
    println!(
        "height={} headers={} peers={} best={}",
        ep.height()?,
        ep.headers()?,
        peers,
        best,
    );
    Ok(())
}

fn cmd_watch(args: &[String]) -> Result<(), String> {
    check_flags(
        args,
        &[
            "--from",
            "--from-user",
            "--from-pass",
            "--to",
            "--to-user",
            "--to-pass",
            "--source-p2p",
            "--stall-secs",
            "--interval",
        ],
    );
    let (src, dst) = endpoints(args);
    let stall: u64 = parse_u64_flag(args, "--stall-secs", 180);
    let interval: u64 = parse_u64_flag(args, "--interval", 15);
    if interval == 0 {
        eprintln!("error: --interval must be >= 1 (0 busy-loops the RPC)");
        std::process::exit(2);
    }
    if stall == 0 {
        eprintln!("error: --stall-secs must be >= 1");
        std::process::exit(2);
    }
    let p2p = arg(args, "--source-p2p", "");
    println!(
        "watch: target={} source={} stall={stall}s interval={interval}s",
        arg(args, "--to", "(rpc)"),
        arg(args, "--from", "(rpc)"),
    );
    let mut last_tip = dst.height()?;
    let mut last_change = Instant::now();
    let mut err_since: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        let (sh, th) = match (src.height(), dst.height()) {
            (Ok(a), Ok(b)) => {
                err_since = None;
                (a, b)
            }
            // Visible, not silent: wrong credentials or a down node used
            // to look identical to a healthy idle watch. Rate-limited so
            // a dead node pages at most once a minute.
            _ => {
                if err_since
                    .map(|t| t.elapsed() >= Duration::from_secs(60))
                    .unwrap_or(true)
                {
                    eprintln!("watch: source/target RPC failing (wrong credentials or node down?) — retrying");
                    err_since = Some(Instant::now());
                }
                continue;
            }
        };
        if th != last_tip {
            last_tip = th;
            last_change = Instant::now();
        }
        if sh > th && last_change.elapsed() >= Duration::from_secs(stall) {
            eprintln!(
                "stall: target {th} vs source {sh} for {}s — pulling gap",
                last_change.elapsed().as_secs()
            );
            // Heal the P2P path too: unban everyone, one-shot redial source.
            let _ = dst.rpc.call("clearbanned", serde_json::json!([]));
            if !p2p.is_empty() {
                let _ = dst.rpc.call("addnode", serde_json::json!([p2p, "onetry"]));
            }
            match pull_range(&src, &dst, sh, th) {
                Ok((n, el)) => eprintln!("healed: {n} blocks in {:.1}s", el.as_secs_f64()),
                Err(e) => eprintln!("pull failed (will retry): {e}"),
            }
            last_tip = dst.height().unwrap_or(th);
            last_change = Instant::now();
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("lm-sync {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.len() < 2 {
        usage();
    }
    let rc = match args[1].as_str() {
        "pull" => cmd_pull(&args),
        "watch" => cmd_watch(&args),
        "status" => cmd_status(&args),
        _ => usage(),
    };
    if let Err(e) = rc {
        eprintln!("lm-sync: error: {e}");
        std::process::exit(1);
    }
}
