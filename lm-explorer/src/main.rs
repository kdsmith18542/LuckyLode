//! Lucky Lode reference block explorer: tip-following SQLite index +
//! read-only HTTP UI. Point it at a node RPC; it walks the chain from
//! genesis (no `-txindex` needed) and serves blocks, transactions and
//! address balances to miners and the public.
//!
//! usage:
//!   explorer --node-rpc HOST:PORT --rpc-user U --rpc-pass P
//!            --bind HOST:PORT --db PATH [--resync] [--sync-interval SECS]

mod http;
mod index;

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use index::Indexer;
use lm_stratum::rpc::RpcClient;

const USAGE: &str = "usage:
  explorer --node-rpc HOST:PORT --rpc-user U --rpc-pass P --bind HOST:PORT --db PATH [--pool-api URL] [--resync] [--sync-interval SECS]
           [--price-url URL] [--price-source NAME]

  indexes the node's chain into SQLite (./explorer.db style path) and serves
  a read-only UI + JSON API: /block/<height|hash> /tx/<txid> /address/<addr>
  /game /league /mempool /richlist /charts /market /search?q= /api/tip /api/stats
  /api/game /api/league /api/mempool /api/richlist /api/charts /api/market.
  Reorg-safe (rolls back to the fork point). Game events (jackpot/pot/league per
  block, Luckynode Draws) and the seasonal league boards come from the pool's
  dashboard API (best-effort).

  --price-url is OPTIONAL: a JSON feed returning {{\"price\": <usd per coin>}}
  (http:// or https:// — https verifies against the Mozilla roots).
  Used by the market page for price and market cap. With no listing there
  is no price source, so without it the market page shows supply and
  liquidity only and says so plainly.
defaults: bind=127.0.0.1:8888; sync-interval=5s; pool-api=http://127.0.0.1:8081.";

fn arg_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("--{flag} needs a value"))
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") || args.is_empty() {
        println!("{USAGE}");
        return std::process::ExitCode::SUCCESS;
    }
    let mut node = String::new();
    let mut user = String::new();
    let mut pass = String::new();
    let mut bind = "127.0.0.1:8888".to_string();
    let mut db = String::new();
    let mut resync = false;
    let mut interval = 5u64;
    let mut pool_api = "http://127.0.0.1:8081".to_string();
    let mut price_url: Option<String> = None;
    let mut price_source: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--node-rpc" => node = arg_value(&args, &mut i, "node-rpc").unwrap_or_else(|e| fatal(e)),
            "--rpc-user" => user = arg_value(&args, &mut i, "rpc-user").unwrap_or_else(|e| fatal(e)),
            "--rpc-pass" => pass = arg_value(&args, &mut i, "rpc-pass").unwrap_or_else(|e| fatal(e)),
            "--bind" => bind = arg_value(&args, &mut i, "bind").unwrap_or_else(|e| fatal(e)),
            "--db" => db = arg_value(&args, &mut i, "db").unwrap_or_else(|e| fatal(e)),
            "--pool-api" => pool_api = arg_value(&args, &mut i, "pool-api").unwrap_or_else(|e| fatal(e)),
            "--price-url" => price_url = Some(arg_value(&args, &mut i, "price-url").unwrap_or_else(|e| fatal(e))),
            "--price-source" => price_source = Some(arg_value(&args, &mut i, "price-source").unwrap_or_else(|e| fatal(e))),
            "--sync-interval" => {
                interval = arg_value(&args, &mut i, "sync-interval")
                    .unwrap_or_else(|e| fatal(e))
                    .parse()
                    .unwrap_or_else(|_| fatal("--sync-interval expects seconds".to_string()))
            }
            "--resync" => resync = true,
            other => {
                eprintln!("error: unknown flag '{other}'\n{USAGE}");
                return std::process::ExitCode::from(2);
            }
        }
        i += 1;
    }
    if node.is_empty() || db.is_empty() {
        eprintln!("error: --node-rpc and --db are required\n{USAGE}");
        return std::process::ExitCode::from(2);
    }
    if resync {
        let _ = std::fs::remove_file(&db);
    }
    let rpc = RpcClient::new(&node, &user, &pass);
    let mut idx = match Indexer::open(&db, rpc) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("error: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    idx.set_price_feed(price_url, price_source);
    let idx = Arc::new(Mutex::new(idx));
    {
        let idx = Arc::clone(&idx);
        let pool_api = pool_api.clone();
        std::thread::spawn(move || {
            let mut last_pool_err: Option<String> = None;
            loop {
                match idx.lock().map(|mut i| i.sync()) {
                    Ok(Ok(r)) => {
                        if r.indexed > 0 || r.rolled_back > 0 {
                            println!(
                                "sync: tip {} (+{} blocks, -{} reorg)",
                                r.tip, r.indexed, r.rolled_back
                            );
                        }
                    }
                    Ok(Err(e)) => eprintln!("sync error: {e} (retrying)"),
                    Err(_) => eprintln!("sync error: lock poisoned"),
                }
                // Game events from the pool's dashboard API (best-effort;
                // the chain index works without it). Log on state change
                // only, so a pool outage does not spam every tick.
                let perr = idx
                    .lock()
                    .map_err(|_| "lock poisoned".to_string())
                    .and_then(|mut i| i.sync_pool(&pool_api).map(|_| ()))
                    .err();
                if perr != last_pool_err {
                    match &perr {
                        Some(e) => eprintln!("pool sync: {e} (game data may be stale)"),
                        None => println!("pool sync: ok"),
                    }
                    last_pool_err = perr;
                }
                std::thread::sleep(Duration::from_secs(interval.max(1)));
            }
        });
    }
    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot bind {bind}: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    println!("explorer on http://{bind}/ (db {db}); Ctrl-C to stop");
    http::serve(listener, idx);
    std::process::ExitCode::SUCCESS
}

fn fatal(msg: String) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}
