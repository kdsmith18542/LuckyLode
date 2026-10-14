//! `casino_server` — the Lucky Lode casino service (doc 14).
//!
//! Serves the slot client and JSON API at `/casino` (behind the site's nginx),
//! settles spins against the house ledger, and moves LKYL through the node
//! wallet (per-player deposit addresses, `sendtoaddress` withdrawals).
//!
//! Testnet posture: no auth/TLS here — nginx terminates TLS and gates access.
//! Real-value wagering is gated on licensing (doc 14 §14.5).

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lm_casino::bonus::BonusEngine;
use lm_casino::custody::Custody;
use lm_casino::engine::{Casino, LKYL};
use lm_casino::http::{self, App};
use lm_casino::oracle::{HttpJson, Mapping, Oracle};
use lm_casino::rpc::{self, MockNode, Node};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Persisted {
    casino: Casino,
    custody: Custody,
    #[serde(default)]
    bonus: BonusEngine,
    #[serde(default)]
    mappings: BTreeMap<u64, Mapping>,
}

struct Args {
    bind: String,
    node_rpc: Option<String>,
    rpc_user: String,
    rpc_pass: String,
    state_file: Option<String>,
    bankroll: u64,
    min_conf: u64,
    withdraw_min: u64,
    withdraw_max: u64,
    scan_interval: u64,
    save_interval: u64,
    network: String,
    mock: bool,
    admin_token: Option<String>,
    bonus_budget: u64,
    pool_api: Option<String>,
    luckynode_api: Option<String>,
}

fn usage() -> ExitCode {
    eprintln!(
        "casino_server — Lucky Lode casino (doc 14)\n\
         \n\
         usage: casino_server [--bind HOST:PORT] [--node-rpc HOST:PORT]\n\
         \u{2009}         [--rpc-user U --rpc-pass P] [--state-file PATH]\n\
         \u{2009}         [--house-bankroll LKYL] [--network NAME]\n\
         \u{2009}         [--min-conf N] [--withdraw-min LKYL] [--withdraw-max LKYL]\n\
         \u{2009}         [--scan-interval SECS] [--save-interval SECS] [--mock]\n\
         \u{2009}         [--admin-token TOKEN] [--bonus-budget LKYL]\n\
         \u{2009}         [--pool-api URL] [--luckynode-api URL]\n\
         \n\
         --mock     run without a node (deposits/withdrawals disabled safely)\n\
         --pool-api pool base URL for the participation oracle (e.g. http://127.0.0.1:8080)\n\
         defaults:  bind 127.0.0.1:8090, bankroll 100000 LKYL, min-conf 1,\n\
         \u{2009}         withdraw-min 1 LKYL, withdraw-max 10000 LKYL, network testnet,\n\
         \u{2009}         scan every 20 s, save every 10 s, bonus budget 1000 LKYL,\n\
         \u{2009}         admin disabled"
    );
    ExitCode::from(2)
}

fn next(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut a = Args {
        bind: "127.0.0.1:8090".to_string(),
        node_rpc: None,
        rpc_user: "u".to_string(),
        rpc_pass: "p".to_string(),
        state_file: None,
        bankroll: 100_000 * LKYL,
        min_conf: 1,
        withdraw_min: LKYL,
        withdraw_max: 10_000 * LKYL,
        scan_interval: 20,
        save_interval: 10,
        network: "testnet".to_string(),
        mock: false,
        admin_token: None,
        bonus_budget: 1_000 * LKYL,
        pool_api: None,
        luckynode_api: None,
    };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--bind" => a.bind = next(argv, &mut i, "--bind")?,
            "--node-rpc" => a.node_rpc = Some(next(argv, &mut i, "--node-rpc")?),
            "--rpc-user" => a.rpc_user = next(argv, &mut i, "--rpc-user")?,
            "--rpc-pass" => a.rpc_pass = next(argv, &mut i, "--rpc-pass")?,
            "--state-file" => a.state_file = Some(next(argv, &mut i, "--state-file")?),
            "--house-bankroll" => {
                a.bankroll = rpc::parse_amount_to_micros(&next(argv, &mut i, "--house-bankroll")?)
                    .map_err(|e| format!("--house-bankroll: {e}"))?
            }
            "--network" => a.network = next(argv, &mut i, "--network")?,
            "--min-conf" => {
                a.min_conf = next(argv, &mut i, "--min-conf")?
                    .parse()
                    .map_err(|_| "--min-conf expects an integer".to_string())?
            }
            "--withdraw-min" => {
                a.withdraw_min = rpc::parse_amount_to_micros(&next(argv, &mut i, "--withdraw-min")?)
                    .map_err(|e| format!("--withdraw-min: {e}"))?
            }
            "--withdraw-max" => {
                a.withdraw_max = rpc::parse_amount_to_micros(&next(argv, &mut i, "--withdraw-max")?)
                    .map_err(|e| format!("--withdraw-max: {e}"))?
            }
            "--scan-interval" => {
                a.scan_interval = next(argv, &mut i, "--scan-interval")?
                    .parse()
                    .map_err(|_| "--scan-interval expects an integer".to_string())?
            }
            "--save-interval" => {
                a.save_interval = next(argv, &mut i, "--save-interval")?
                    .parse()
                    .map_err(|_| "--save-interval expects an integer".to_string())?
            }
            "--mock" => a.mock = true,
            "--admin-token" => a.admin_token = Some(next(argv, &mut i, "--admin-token")?),
            "--bonus-budget" => {
                a.bonus_budget = rpc::parse_amount_to_micros(&next(argv, &mut i, "--bonus-budget")?)
                    .map_err(|e| format!("--bonus-budget: {e}"))?
            }
            "--pool-api" => a.pool_api = Some(next(argv, &mut i, "--pool-api")?),
            "--luckynode-api" => a.luckynode_api = Some(next(argv, &mut i, "--luckynode-api")?),
            other => return Err(format!("unknown flag: {other}")),
        }
        i += 1;
    }
    if !a.mock && a.node_rpc.is_none() {
        return Err("need --node-rpc (or --mock)".to_string());
    }
    Ok(a)
}

fn load_state(path: &Option<String>) -> Option<Persisted> {
    let path = path.as_ref()?;
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Persisted>(&text) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("state: cannot parse {path}: {e} — refusing to start");
            std::process::exit(1);
        }
    }
}

fn save_state(path: &str, app: &App) {
    let p = Persisted {
        casino: app.casino.lock().expect("casino lock").clone(),
        custody: app.custody.lock().expect("custody lock").clone(),
        bonus: app.bonus.lock().expect("bonus lock").clone(),
        mappings: app.oracle.lock().expect("oracle lock").mappings.clone(),
    };
    let Ok(text) = serde_json::to_string(&p) else {
        return;
    };
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return usage();
        }
    };

    let state = load_state(&args.state_file);
    let (casino, custody, mappings, loaded_bonus) = match state {
        Some(p) => {
            println!(
                "state: loaded (bankroll {} LKYL)",
                rpc::format_micros(p.casino.bankroll())
            );
            (p.casino, p.custody, p.mappings, p.bonus)
        }
        None => (
            Casino::new(args.bankroll),
            Custody::new(args.min_conf, args.withdraw_min, args.withdraw_max),
            BTreeMap::new(),
            BonusEngine::default(),
        ),
    };
    // A fresh run gets the configured budget; a loaded one keeps its own.
    let bonus = if loaded_bonus.budget() == 0 && args.bonus_budget > 0 {
        BonusEngine::new(args.bonus_budget)
    } else {
        loaded_bonus
    };

    let node: Node = if args.mock {
        let mock = MockNode::new();
        mock.set("getnewaddress", serde_json::json!("mock-deposit-address"));
        mock.set("getreceivedbyaddress", serde_json::json!(0.0));
        println!("node: MOCK (withdrawals disabled: no node behind this process)");
        Node::from_api(Box::new(mock))
    } else {
        Node::http(
            args.node_rpc.as_ref().expect("checked"),
            &args.rpc_user,
            &args.rpc_pass,
        )
    };

    let oracle = Oracle {
        pool_url: args.pool_api.clone(),
        luckynode_url: args.luckynode_api.clone(),
        http: Box::new(HttpJson),
        mappings,
    };
    if args.admin_token.is_some() {
        println!("admin: enabled (token set)");
    }
    if oracle.pool_url.is_some() || oracle.luckynode_url.is_some() {
        println!(
            "oracle: pool={} luckynode={}",
            oracle.pool_url.as_deref().unwrap_or("off"),
            oracle.luckynode_url.as_deref().unwrap_or("off")
        );
    }

    let app = Arc::new(App {
        casino: Mutex::new(casino),
        custody: Mutex::new(custody),
        node,
        network: args.network.clone(),
        bonus: Mutex::new(bonus),
        oracle: Mutex::new(oracle),
        admin_token: args.admin_token.clone(),
    });

    if let Some(path) = &args.state_file {
        let app2 = Arc::clone(&app);
        let path2 = path.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(args.save_interval));
            save_state(&path2, &app2);
        });
    }

    if args.scan_interval > 0 && !args.mock {
        let app2 = Arc::clone(&app);
        let secs = args.scan_interval;
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(secs));
            let mut c = app2.casino.lock().expect("casino lock");
            let mut cu = app2.custody.lock().expect("custody lock");
            match cu.scan(&app2.node, &mut c.ledger) {
                Ok(d) if !d.is_empty() => println!("scan: credited {} deposit(s)", d.len()),
                Ok(_) => {}
                Err(e) => eprintln!("scan: {e}"),
            }
        });
    }

    let (bankroll, max_bet) = {
        let c = app.casino.lock().expect("casino lock");
        (c.bankroll(), c.max_bet())
    };
    println!(
        "casino: network={} rtp={} bankroll={} LKYL max_bet={} LKYL",
        args.network,
        lm_casino::TARGET_RTP,
        rpc::format_micros(bankroll),
        rpc::format_micros(max_bet)
    );
    let listener = match http::bind(&args.bind) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot bind {}: {e}", args.bind);
            return ExitCode::FAILURE;
        }
    };
    println!("casino: serving http://{}/", args.bind);
    http::serve(listener, app);
}
