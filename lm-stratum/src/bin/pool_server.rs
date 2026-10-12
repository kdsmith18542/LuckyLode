//! `pool-server` — run the reference pool on a mock chain or a real node.
//!
//! ```text
//! pool-server [--bind HOST:PORT] [--reward R] [--fees F]
//!             [--pool-target (HEX64|max)] [--net-target (HEX64|max)]
//! pool-server --node-rpc HOST:PORT [--rpc-user U] [--rpc-pass P] [--bind HOST:PORT]
//!             [--claim-jackpots]
//! ```
//!
//! Mock mode (default) serves an instant-block mock chain (`max`/`max`,
//! reward 1M) on 127.0.0.1:3333. Node mode talks to a running forked node
//! via `getblocktemplate`/`submitblock` — jobs come from the real chain
//! and accepted shares advance it. Ctrl-C stops (connections drain on
//! their next tick).

use lm_stratum::node_provider::NodeProvider;
use lm_stratum::provider::MockProvider;
use lm_stratum::server::spawn_server;
use std::net::SocketAddr;

const USAGE: &str = "usage:
  pool-server [--bind HOST:PORT] [--reward R] [--fees F] [--pool-target (HEX64|max)] [--net-target (HEX64|max)]
  pool-server --node-rpc HOST:PORT [--rpc-user U] [--rpc-pass P] [--bind HOST:PORT]

defaults: bind=127.0.0.1:3333; mock mode: reward=1000000, fees=0, both targets=max (instant blocks);
node mode: jobs from the node's getblocktemplate.";

fn parse_u64(s: &str, flag: &str) -> Result<u64, String> {
    s.parse::<u64>().map_err(|_| format!("--{flag} expects a non-negative integer, got '{s}'"))
}

fn parse_target(s: &str, flag: &str) -> Result<[u8; 32], String> {
    if s == "max" {
        return Ok([0xffu8; 32]);
    }
    if s.len() != 64 {
        return Err(format!("--{flag} expects 64 hex chars or 'max', got '{s}'"));
    }
    let mut t = [0u8; 32];
    for i in 0..32 {
        t[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
            .map_err(|_| format!("--{flag} expects hex, got '{s}'"))?;
    }
    Ok(t)
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    let mut bind = "127.0.0.1:3333".to_string();
    let mut reward = 1_000_000u64;
    let mut fees = 0u64;
    let mut pool_target = [0xffu8; 32];
    let mut net_target = [0xffu8; 32];
    let mut node_rpc: Option<String> = None;
    let mut rpc_user = "u".to_string();
    let mut rpc_pass = "p".to_string();
    let mut claim_jackpots = false;
    let mut i = 0;
    while i < args.len() {
        let next = |flag: &str| -> Result<String, String> {
            args.get(i + 1).cloned().ok_or_else(|| format!("--{flag} needs a value"))
        };
        match args[i].as_str() {
            "--bind" => bind = next("bind").unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(2);
            }),
            "--reward" => {
                reward = parse_u64(&next("reward").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "reward")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--fees" => {
                fees = parse_u64(&next("fees").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "fees")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--pool-target" => {
                pool_target = parse_target(&next("pool-target").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "pool-target")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
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
            "--claim-jackpots" => {
                claim_jackpots = true;
                i += 1; // valueless flag: do not skip the next argument
                continue;
            }
            "--rpc-pass" => {
                rpc_pass = next("rpc-pass").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            "--net-target" => {
                net_target = parse_target(&next("net-target").unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                }), "net-target")
                .unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                })
            }
            other => {
                eprintln!("error: unknown flag '{other}'\n{USAGE}");
                return std::process::ExitCode::from(2);
            }
        }
        i += 2;
    }
    let addr: SocketAddr = match bind.parse() {
        Ok(a) => a,
        Err(_) => {
            eprintln!("error: --bind expects HOST:PORT, got '{bind}'");
            return std::process::ExitCode::from(2);
        }
    };
    if let Some(node) = node_rpc {
        let provider = match NodeProvider::connect_with_claims(&node, &rpc_user, &rpc_pass, claim_jackpots) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: cannot reach node RPC {node}: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        let server = match spawn_server(provider, &addr.to_string()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot bind {addr}: {e}");
                return std::process::ExitCode::from(1);
            }
        };
        println!(
            "pool on {} (node provider {}{}); Ctrl-C to stop",
            server.addr(),
            node,
            if claim_jackpots { ", claiming jackpots" } else { "" }
        );
    } else {
        let server =
            match spawn_server(MockProvider::new(pool_target, net_target, reward, fees), &addr.to_string())
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: cannot bind {addr}: {e}");
                    return std::process::ExitCode::from(1);
                }
            };
        println!("pool on {} (reward={reward} fees={fees}); Ctrl-C to stop", server.addr());
    }
    // Park the main thread; the accept + connection threads do the work.
    // `stop` is exercised by the test suite; here we simply never call it.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
