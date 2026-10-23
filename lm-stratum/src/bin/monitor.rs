//! lm-monitor — minimum-viable node health monitor (runbook §4).
//!
//! Polls a node's RPC and alerts on stderr:
//! - tip stall (no advance for `--stall-secs`, past a startup grace)
//! - reserve drops that the block's coinbase cannot account for
//!   (a legitimate jackpot/pot claim shows up in the coinbase)
//! - node unreachable
//! - disk free space below `--disk-min-free-pct` (best effort, `df`)
//!
//! Optional `--http HOST:PORT` serves `/status` (JSON) for external
//! monitoring; `--state-file` persists the baseline across restarts so a
//! monitor restart does not page.

use lm_stratum::monitor::{parse_df, parse_meminfo_available_pct, Event, Snapshot, Tracker};
use lm_stratum::rpc::RpcClient;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USAGE: &str = "usage:
  lm-monitor --node-rpc HOST:PORT [--rpc-user U] [--rpc-pass P]
             [--interval SECS] [--stall-secs N] [--once]
             [--disk-path PATH] [--disk-min-free-pct N] [--mem-min-free-pct N]
             [--state-file PATH] [--http HOST:PORT]

defaults: rpc-user=u, rpc-pass=p, interval=60, stall-secs=600,
          disk-min-free-pct=10, mem-min-free-pct=10 (0 disables);
          --http serves /status as JSON.
Reserve drops are verified against the tip block's coinbase; a drop the
coinbase cannot cover is reported as INCONSISTENT (critical).";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// UTC timestamp from unix seconds (civil-from-days, no chrono).
fn fmt_time(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02}Z")
}

fn log(line: &str) {
    println!("{} {line}", fmt_time(now()));
}

#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct Persisted {
    height: u64,
    reserve: u64,
    last_advance: u64,
}

fn load_state(path: &Path) -> Option<Persisted> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_state(path: &Path, p: &Persisted) {
    let tmp = path.with_extension("tmp");
    if let Ok(text) = serde_json::to_string(p) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Sum the coinbase outputs in base units. `getblock` exposes values as
/// coin floats (no `valueZat`), so this is approximate — acceptable for a
/// sanity check, never for consensus.
fn coinbase_zat(rpc: &RpcClient, block_hash: &str) -> Option<u64> {
    let b = rpc
        .call("getblock", serde_json::json!([block_hash, 2]))
        .ok()?;
    let cb = b.get("tx")?.as_array()?.first()?;
    let mut total = 0f64;
    for v in cb.get("vout")?.as_array()? {
        total += v.get("value")?.as_f64()?;
    }
    Some((total * 1e8).round() as u64)
}

fn poll(rpc: &RpcClient) -> Result<(Snapshot, String), String> {
    let height = rpc
        .call("getblockcount", serde_json::json!([]))?
        .as_u64()
        .ok_or("bad getblockcount")?;
    let info = rpc.call("getjackpotinfo", serde_json::json!([]))?;
    let reserve = info
        .get("reserve")
        .and_then(|v| v.as_u64())
        .ok_or("bad getjackpotinfo.reserve")?;
    let hash = info
        .get("blockhash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok((Snapshot { height, reserve }, hash))
}

fn disk_free_pct(path: &str) -> Option<u64> {
    let out = std::process::Command::new("df")
        .args(["-P", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_df(&String::from_utf8_lossy(&out.stdout))
}

fn http_serve(addr: &str, status: Arc<Mutex<serde_json::Value>>) {
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("http: cannot bind {addr}: {e}");
            return;
        }
    };
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let body = status.lock().map(|s| s.to_string()).unwrap_or_default();
        let _ = std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("monitor {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let mut node_rpc: Option<String> = None;
    let mut rpc_user = "u".to_string();
    let mut rpc_pass = "p".to_string();
    let mut interval = 60u64;
    let mut stall_secs = 600u64;
    let mut once = false;
    let mut disk_path: Option<String> = None;
    let mut disk_min_free_pct = 10u64;
    let mut mem_min_free_pct = 10u64;
    let mut state_file: Option<PathBuf> = None;
    let mut http: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let next = |flag: &str| -> Result<String, String> {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("--{flag} needs a value"))
        };
        match args[i].as_str() {
            "--node-rpc" => {
                node_rpc = Some(next("node-rpc").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }))
            }
            "--rpc-user" => {
                rpc_user = next("rpc-user").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--rpc-pass" => {
                rpc_pass = next("rpc-pass").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--interval" => {
                interval = next("interval")
                    .and_then(|v| v.parse().map_err(|_| "--interval expects an integer".into()))
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    })
            }
            "--stall-secs" => {
                stall_secs = next("stall-secs")
                    .and_then(|v| {
                        v.parse()
                            .map_err(|_| "--stall-secs expects an integer".into())
                    })
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    })
            }
            "--disk-path" => {
                disk_path = Some(next("disk-path").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }))
            }
            "--disk-min-free-pct" => {
                disk_min_free_pct = next("disk-min-free-pct")
                    .and_then(|v| {
                        v.parse()
                            .map_err(|_| "--disk-min-free-pct expects an integer".into())
                    })
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    })
            }
            "--mem-min-free-pct" => {
                mem_min_free_pct = next("mem-min-free-pct")
                    .and_then(|v| {
                        v.parse()
                            .map_err(|_| "--mem-min-free-pct expects an integer".into())
                    })
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    })
            }
            "--state-file" => {
                state_file = Some(PathBuf::from(next("state-file").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })))
            }
            "--http" => {
                http = Some(next("http").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }))
            }
            "--once" => {
                once = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            other => {
                eprintln!("error: unknown flag '{other}'\n{USAGE}");
                return ExitCode::from(2);
            }
        }
        i += 2;
    }
    let Some(node_rpc) = node_rpc else {
        eprintln!("error: --node-rpc is required\n{USAGE}");
        return ExitCode::from(2);
    };
    let rpc = RpcClient::new(&node_rpc, &rpc_user, &rpc_pass);

    let mut tracker = match state_file.as_deref().and_then(load_state) {
        Some(p) => Tracker::resume(
            now(),
            stall_secs,
            Some(Snapshot {
                height: p.height,
                reserve: p.reserve,
            }),
            p.last_advance,
        ),
        None => Tracker::new(now(), stall_secs),
    };
    let status = Arc::new(Mutex::new(serde_json::json!({
        "height": null, "reserve": null, "last_advance": null, "events": []
    })));
    if let Some(addr) = http {
        let s = Arc::clone(&status);
        let listen = addr.clone();
        std::thread::spawn(move || http_serve(&listen, s));
        log(&format!("monitor: status on http://{addr}/status"));
    }

    loop {
        match poll(&rpc) {
            Ok((snap, hash)) => {
                for ev in tracker.observe(now(), snap.height, snap.reserve) {
                    match &ev {
                        Event::ReserveFell { from, to, height } => {
                            let drop = from - to;
                            match coinbase_zat(&rpc, &hash) {
                                Some(cb) if cb + cb / 100 >= drop => log(&format!(
                                    "{} (claim consistent: coinbase {cb} >= drop {drop})",
                                    ev.message()
                                )),
                                Some(cb) => log(&format!(
                                    "CRITICAL reserve drop at height {height}: coinbase {cb} < drop {drop} — possible bug"
                                )),
                                None => log(&format!(
                                    "{} (WARNING: could not fetch coinbase to verify)",
                                    ev.message()
                                )),
                            }
                        }
                        _ => log(&ev.message()),
                    }
                }
                if let Some(path) = &disk_path {
                    if let Some(free) = disk_free_pct(path) {
                        if free < disk_min_free_pct {
                            log(&format!(
                                "CRITICAL disk {path}: {free}% free (< {disk_min_free_pct}%)"
                            ));
                        }
                    }
                }
                if mem_min_free_pct > 0 {
                    if let Ok(mi) = std::fs::read_to_string("/proc/meminfo") {
                        if let Some(free) = parse_meminfo_available_pct(&mi) {
                            if free < mem_min_free_pct {
                                log(&format!(
                                    "CRITICAL memory: {free}% available (< {mem_min_free_pct}%)"
                                ));
                            }
                        }
                    }
                }
                if let (Some(path), Some(last)) = (state_file.as_deref(), tracker.last()) {
                    save_state(
                        path,
                        &Persisted {
                            height: last.height,
                            reserve: last.reserve,
                            last_advance: tracker.last_advance(),
                        },
                    );
                }
                if let Ok(mut s) = status.lock() {
                    *s = serde_json::json!({
                        "height": snap.height,
                        "reserve": snap.reserve,
                        "last_advance": tracker.last_advance(),
                    });
                }
            }
            Err(e) => log(&format!("CRITICAL node unreachable: {e}")),
        }
        if once {
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
}
