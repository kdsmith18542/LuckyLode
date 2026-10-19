//! Read-only HTTP UI + JSON API over the index. std only (mirrors the
//! pool's dashboard server): one thread per connection, `Connection:
//! close`. Routes:
//!   /                      home (tip, recent blocks + txs, search)
//!   /block/<height|hash>   block page        /api/block/<height|hash>
//!   /tx/<txid>             tx page           /api/tx/<txid>
//!   /address/<addr>        address page      /api/address/<addr>
//!   /game                  jackpot/pot/draws (pool-reported)
//!   /league                season league boards (pool-reported)
//!   /mempool               pending txs (node view)
//!   /richlist              top funded addresses (unspent only)
//!   /search?q=             smart redirect    /api/tip  /api/stats  /api/league  /api/mempool  /api/richlist

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::index::Indexer;

const CSS: &str = "body{background:#0d1117;color:#e6edf3;font:14px ui-monospace,Menlo,monospace;margin:0 auto;padding:24px;max-width:960px}h1{font-size:20px;color:#ffd166}a{color:#7ee787}table{border-collapse:collapse;width:100%;margin:12px 0}td,th{border:1px solid #30363d;padding:6px 10px;text-align:left;font-size:13px}.dim{color:#8b949e}.pos{color:#7ee787}.card{background:#161b22;border:1px solid #30363d;border-radius:10px;padding:12px;margin:12px 0}input{width:70%;padding:8px;background:#0d1117;color:#e6edf3;border:1px solid #30363d;border-radius:6px}button{padding:8px 14px;background:#161b22;color:#ffd166;border:1px solid #ffd166;border-radius:6px}";

fn page(title: &str, body: String) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{title} — Lucky Lode explorer</title><style>{CSS}</style></head><body><h1><a href=/ style=\"color:#ffd166;text-decoration:none\">Lucky Lode explorer</a></h1><div class=dim style=\"margin:-4px 0 12px;font-size:13px\"><a href=/>blocks</a> · <a href=/game>game</a> · <a href=/league>leagues</a> · <a href=/mempool>mempool</a> · <a href=/richlist>rich</a></div>{body}</body></html>"
    )
}

/// Chain ticker (testnet now; P3 mainnet switch flips this one const).
const TICK: &str = "tLKYL";

/// Epoch seconds → UTC date. No raw timestamps in the UI. Zero (unknown)
/// renders as a dash, never 1970.
fn fmt_time(ts: i64) -> String {
    if ts <= 0 {
        return "-".to_string();
    }
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn coins(v: i64) -> String {
    // Adaptive precision: up to 8 decimals,
    // trailing zeros trimmed — `0.33`, never `0.33000000`, and never `0`
    // for a non-zero balance. Same rule as the wallet/pool formatters.
    let s = format!("{}.{:08}", v / 100_000_000, (v % 100_000_000).abs());
    let t = s.trim_end_matches('0').trim_end_matches('.');
    let t = if t.is_empty() || t == "-" { "0" } else { t };
    format!("{t} {TICK}")
}

fn hs(v: f64) -> String {
    if v >= 1e6 {
        format!("{:.2} MH/s", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1} kH/s", v / 1e3)
    } else {
        format!("{v:.0} H/s")
    }
}

/// Emission card: current block reward from the locked schedule + the
/// next change (height, blocks to go, rough ETA at ~60 s blocks).
fn emission_card(tip: i64) -> String {
    use crate::index::Indexer;
    let h = tip.max(0) as u64;
    let cur = Indexer::subsidy_at(h);
    let next = Indexer::next_subsidy_change(h)
        .map(|(nh, ns)| {
            let left = nh.saturating_sub(h);
            format!(
                "next change <b>{}</b> at height {} ({} blocks, ~{}d)",
                coins(ns as i64),
                nh,
                left,
                left / 1440
            )
        })
        .unwrap_or_else(|| "fixed tail — no further changes".to_string());
    format!(
        "<div class=card>block reward <b>{}</b> · {}</div>",
        coins(cur as i64),
        next
    )
}

fn tx_or_note(t: &str) -> String {
    if t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        link_tx(t)
    } else {
        format!("<span class=dim>{}</span>", esc(t))
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn link_block(h: i64) -> String {
    format!("<a href=/block/{h}>{h}</a>")
}

fn link_tx(t: &str) -> String {
    format!("<a href=/tx/{t}>{}</a>", &t[..t.len().min(16)])
}

fn link_addr(a: &Option<String>) -> String {
    match a {
        Some(x) => format!("<a href=/address/{x}>{x}</a>"),
        None => "<span class=dim>nonstandard</span>".to_string(),
    }
}

fn search_box() -> String {
    "<form action=/search><input name=q placeholder=\"height, block hash, txid or address\"><button>Go</button></form>".to_string()
}

fn home(idx: &mut Indexer) -> String {
    let stats = idx.stats().unwrap_or(serde_json::json!({"height": -1}));
    let h = stats.get("height").and_then(|v| v.as_i64()).unwrap_or(-1);
    let mut rows = String::new();
    // Per-block game money (home columns; pool-reported, pool-mined only).
    // bonus = jackpot win, pot = splash paid to the finder (0 when no
    // event — NOT the reserve balance), league = overflow credited.
    let game_by_height: std::collections::HashMap<i64, (i64, i64, i64)> = idx
        .game_blocks_recent(100)
        .unwrap_or_default()
        .iter()
        .filter_map(|g| {
            let height = g.get("height")?.as_i64()?;
            Some((
                height,
                (
                    g.get("bonus")?.as_i64()?,
                    g.get("pot")?.as_i64()?,
                    g.get("league")?.as_i64()?,
                ),
            ))
        })
        .collect();
    let mut pos_count = 0;
    for hh in (0.max(h - 19)..=h).rev() {
        if let Ok(Some(b)) = idx.block_by_height(hh) {
            let is_pos = b["pos"].as_bool().unwrap_or(false);
            if is_pos {
                pos_count += 1;
            }
            rows += &format!(
                "<tr><td>{}</td><td>{}</td><td><a href=/block/{}>{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                hh,
                if is_pos { "<b class=pos>PoS · minted</b>" } else { "<span class=dim>PoW</span>" },
                b["hash"].as_str().unwrap_or(""),
                &b["hash"].as_str().unwrap_or("")[..16],
                b["ntx"],
                idx.coinbase_total(hh).map(coins).unwrap_or_else(|_| "<span class=dim>–</span>".to_string()),
                game_by_height.get(&hh).filter(|v| v.0 > 0).map(|v| coins(v.0)).unwrap_or_else(|| "<span class=dim>–</span>".to_string()),
                game_by_height.get(&hh).filter(|v| v.1 > 0).map(|v| coins(v.1)).unwrap_or_else(|| "<span class=dim>–</span>".to_string()),
                game_by_height.get(&hh).filter(|v| v.2 > 0).map(|v| coins(v.2)).unwrap_or_else(|| "<span class=dim>–</span>".to_string()),
            );
        }
    }
    let stake = idx.staking_state().unwrap_or(serde_json::json!({}));
    let stake_card = format!(
        "<div class=card>node staking <b>{}</b> · stake supply <b>{}</b> · PoS share last 20: <b>{}%</b> (target ~50%)</div>",
        if stake.get("staking_on").and_then(|v| v.as_bool()).unwrap_or(false) { "on" } else { "off" },
        coins(stake.get("staking_supply").and_then(|v| v.as_i64()).unwrap_or(0)),
        if h >= 0 { pos_count * 100 / 20 } else { 0 },
    );
    // Network card: live node view (peers, hashrate estimate, mempool).
    let net = idx.network().unwrap_or(serde_json::json!({}));
    let mp = idx.mempool().unwrap_or(serde_json::json!({}));
    let net_card = format!(
        "<div class=card>network hashrate <b>{}</b> · peers <b>{}</b> · mempool <b>{} tx / {} B</b> · unconfirmed fees waiting <b>{}</b></div>",
        hs(net.get("hashrate").and_then(|v| v.as_f64()).unwrap_or(0.0)),
        net.get("peers").and_then(|v| v.as_u64()).unwrap_or(0),
        mp.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
        mp.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
        coins(mp_entries_fee_total(&mp)),
    );
    // Recent transactions (index-only, no per-tx RPC).
    let mut txrows = String::new();
    for t in idx.recent_txs(10).unwrap_or_default() {
        let txid = t["txid"].as_str().unwrap_or("");
        let fee = if t["coinbase"].as_bool().unwrap_or(false) {
            "<span class=dim>–</span>".to_string()
        } else {
            coins(t["fee"].as_i64().unwrap_or(0))
        };
        txrows += &format!(
            "<tr><td><a href=/tx/{txid}>{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            &txid[..txid.len().min(16)],
            link_block(t["height"].as_i64().unwrap_or(0)),
            fmt_time(t["time"].as_i64().unwrap_or(0)),
            coins(t["value"].as_i64().unwrap_or(0)),
            fee,
        );
    }
    if txrows.is_empty() {
        txrows = "<tr><td colspan=5 class=dim>no transactions indexed yet</td></tr>".to_string();
    }
    page(
        "home",
        format!(
            "{}<div class=card>height <b>{}</b> · hash <span class=dim>{}</span> · supply <b>{}</b> · difficulty {:.2} · minted (PoS) in last 20: <b>{}</b></div>{}{}{}<h2>Recent blocks</h2><table><tr><th>height</th><th>type</th><th>hash</th><th>txs</th><th>block reward</th><th>jackpot bonus</th><th>pot splash</th><th>league inflow</th></tr>{rows}</table><h2>Recent transactions</h2><table><tr><th>txid</th><th>block</th><th>time</th><th>value</th><th>fee</th></tr>{txrows}</table>",
            search_box(),
            h,
            esc(stats.get("hash").and_then(|v| v.as_str()).unwrap_or("")),
            coins(stats.get("supply").and_then(|v| v.as_i64()).unwrap_or(0)),
            stats.get("difficulty").and_then(|v| v.as_f64()).unwrap_or(0.0),
            pos_count,
            stake_card,
            net_card,
            emission_card(h),
        ),
    )
}

/// Total fees sitting in a mempool snapshot (for the home network card).
fn mp_entries_fee_total(mp: &serde_json::Value) -> i64 {
    mp.get("entries")
        .and_then(|e| e.as_array())
        .map(|arr| arr.iter().map(|e| e.get("fee").and_then(|f| f.as_i64()).unwrap_or(0)).sum())
        .unwrap_or(0)
}

/// Game layer: live jackpot/pot state, Luckynode Draws, jackpot hits and
/// pot splashes (pool-reported per block).
fn game_page(idx: &Indexer) -> String {
    let state = idx.game_state().unwrap_or(serde_json::json!({}));
    let reserve = state.get("reserve").and_then(|v| v.as_i64()).unwrap_or(0);
    let pot = state.get("pot").and_then(|v| v.as_i64()).unwrap_or(0);
    let drought = state.get("drought").and_then(|v| v.as_i64()).unwrap_or(0);
    let jd = state.get("jackpotdivisor").and_then(|v| v.as_i64()).unwrap_or(100);
    let pd = state.get("potdivisor").and_then(|v| v.as_i64()).unwrap_or(1000);
    let dl = state.get("potdrought").and_then(|v| v.as_i64()).unwrap_or(4320);
    let blocks = idx.game_blocks_recent(100).unwrap_or_default();

    let mut hits = String::new();
    let mut splashes = String::new();
    for b in &blocks {
        let h = b["height"].as_i64().unwrap_or(0);
        let finder = esc(b["finder"].as_str().unwrap_or(""));
        if b["bonus"].as_i64().unwrap_or(0) > 0 {
            hits += &format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                link_block(h),
                finder,
                coins(b["bonus"].as_i64().unwrap_or(0)),
                coins(b["league"].as_i64().unwrap_or(0)),
            );
        }
        if b["pot"].as_i64().unwrap_or(0) > 0 {
            splashes += &format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                link_block(h),
                finder,
                coins(b["pot"].as_i64().unwrap_or(0)),
            );
        }
    }
    if hits.is_empty() {
        hits = "<tr><td colspan=4 class=dim>no jackpot hits yet</td></tr>".to_string();
    }
    if splashes.is_empty() {
        splashes = "<tr><td colspan=3 class=dim>no pot splashes yet</td></tr>".to_string();
    }

    let mut draws = String::new();
    for d in idx.draws_recent(20).unwrap_or_default() {
        let seed = d["seed"].as_str().unwrap_or("");
        let seed_cell = if seed.len() == 64 {
            format!("<a href=/block/{seed}>{}</a>", &seed[..12])
        } else {
            esc(seed)
        };
        let winner = d["winner"].as_str().unwrap_or("");
        let wshort = format!("{}{}", esc(&winner[..winner.len().min(12)]), if winner.len() > 12 { "…" } else { "" });
        draws += &format!(
            "<tr><td>{}</td><td>{}</td><td><span class=dim>{}</span></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            d["epoch"].as_i64().unwrap_or(0),
            link_block(d["height"].as_i64().unwrap_or(0)),
            wshort,
            coins(d["budget"].as_i64().unwrap_or(0)),
            d["cosigns"].as_i64().unwrap_or(0),
            tx_or_note(d["txid"].as_str().unwrap_or("")),
            seed_cell,
        );
    }
    if draws.is_empty() {
        draws = "<tr><td colspan=7 class=dim>no draws yet</td></tr>".to_string();
    }
    page(
        "game",
        format!(
            "{}<div class=card>jackpot reserve <b>{}</b> · pot <b>{}</b> · pot drought <b>{}/{}</b> · odds (PoW finds, pre-committed per height): jackpot 1-in-{} blocks,             pot 1-in-{} &mdash; forced splash after {} dry blocks max</div><h2>Luckynode Draws</h2><table><tr><th>epoch</th><th>height</th><th>winner</th><th>budget</th><th>cosigns</th><th>txid</th><th>seed (epoch-start block)</th></tr>{draws}</table><h2>Jackpot hits</h2><table><tr><th>height</th><th>finder</th><th>bonus</th><th>league inflow</th></tr>{hits}</table><h2>Pot splashes</h2><table><tr><th>height</th><th>finder</th><th>splash</th></tr>{splashes}</table>",
            search_box(),
            coins(reserve),
            coins(pot),
            drought,
            dl,
            jd,
            pd,
            dl,
        ),
    )
}

/// Seasonal league boards (doc 03 §3.3: phones race phones), as last
/// synced from the pool's dashboard API. Snapshot data, not consensus —
/// the pool is the board authority; the explorer mirrors it.
fn league_page(idx: &Indexer) -> String {
    let boards = idx.league_boards().unwrap_or_default();
    let mut sections = String::new();
    for b in &boards {
        let class = esc(b["class"].as_str().unwrap_or(""));
        let entries = b["entries"].as_array().cloned().unwrap_or_default();
        let rows = if entries.is_empty() {
            "<tr><td colspan=5 class=dim>no ranked workers this season</td></tr>".to_string()
        } else {
            entries
                .iter()
                .map(|e| {
                    format!(
                        "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}d</td><td>{}</td></tr>",
                        e["rank"].as_i64().unwrap_or(0),
                        esc(e["name"].as_str().unwrap_or("")),
                        e["eff_work"].as_i64().unwrap_or(0),
                        e["days"].as_i64().unwrap_or(0),
                        if e["streak"].as_i64().unwrap_or(0) > 0 {
                            format!("{}d", e["streak"].as_i64().unwrap_or(0))
                        } else {
                            "<span class=dim>–</span>".to_string()
                        },
                    )
                })
                .collect::<Vec<_>>()
                .join("")
        };
        sections += &format!(
            "<h2>{class} <span class=dim>· {size} ranked</span></h2><table><tr><th>rank</th><th>worker</th><th>eff. work</th><th>days</th><th>streak</th></tr>{rows}</table>",
            size = entries.len(),
        );
    }
    if sections.is_empty() {
        sections = "<p class=dim>no league data yet — the explorer mirrors the pool boards on its next pool sync.</p>".to_string();
    }
    page(
        "leagues",
        format!(
            "{}<div class=card>Seasonal boards by device family (30-day effective work). Phones race phones; farms are paid, not feted. Snapshot mirrored from the official pool.</div>{sections}",
            search_box(),
        ),
    )
}

/// Pending transactions: the node's own mempool view (every node differs
/// slightly). Fees shown are what miners would collect on top of subsidy.
fn mempool_page(idx: &mut Indexer) -> String {
    let mp = idx.mempool().unwrap_or(serde_json::json!({}));
    let entries = mp["entries"].as_array().cloned().unwrap_or_default();
    let rows = if entries.is_empty() {
        "<tr><td colspan=4 class=dim>mempool empty — everything indexed is confirmed</td></tr>".to_string()
    } else {
        entries
            .iter()
            .map(|e| {
                let fee = match e.get("fee") {
                    Some(v) if v.is_i64() || v.is_u64() => coins(v.as_i64().unwrap_or(0)),
                    _ => "<span class=dim>–</span>".to_string(),
                };
                format!(
                    "<tr><td><a href=/tx/{}>{}</a></td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    e["txid"].as_str().unwrap_or(""),
                    &e["txid"].as_str().unwrap_or("")[..e["txid"].as_str().unwrap_or("").len().min(16)],
                    fee,
                    e["vsize"].as_u64().unwrap_or(0),
                    fmt_time(e["time"].as_i64().unwrap_or(0)),
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let trunc = if mp["truncated"].as_bool().unwrap_or(false) {
        format!(
            "<p class=dim>showing {} of {} pending — full list via /api/mempool</p>",
            mp["shown"].as_u64().unwrap_or(0),
            mp["size"].as_u64().unwrap_or(0)
        )
    } else {
        String::new()
    };
    page(
        "mempool",
        format!(
            "{}<div class=card>pending <b>{}</b> · size <b>{} B</b> · mempool usage <b>{} B</b> · waiting fees <b>{}</b></div>{}<h2>Pending transactions</h2><table><tr><th>txid</th><th>fee</th><th>vsize</th><th>first seen</th></tr>{rows}</table>",
            search_box(),
            mp["size"].as_u64().unwrap_or(0),
            mp["bytes"].as_u64().unwrap_or(0),
            mp["usage"].as_u64().unwrap_or(0),
            coins(mp_entries_fee_total(&mp)),
            trunc,
        ),
    )
}

/// Top funded addresses (unspent outputs only) — the chain's rough
/// wealth map. Exchange/pool wallets dominate by construction.
fn richlist_page(idx: &Indexer) -> String {
    let tip = idx.height().unwrap_or(-1);
    let rows: Vec<serde_json::Value> = idx.richlist(100).unwrap_or_default();
    let body = if rows.is_empty() {
        "<tr><td colspan=3 class=dim>nothing indexed yet</td></tr>".to_string()
    } else {
        rows.iter()
            .enumerate()
            .map(|(i, e)| {
                format!(
                    "<tr><td>{}</td><td><a href=/address/{}>{}</a></td><td>{}</td></tr>",
                    i + 1,
                    e["address"].as_str().unwrap_or(""),
                    e["address"].as_str().unwrap_or(""),
                    coins(e["balance"].as_i64().unwrap_or(0)),
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    page(
        "rich list",
        format!(
            "{}<div class=card>top 100 funded addresses by unspent balance · as of height <b>{}</b></div><table><tr><th>#</th><th>address</th><th>balance</th></tr>{}</table>",
            search_box(),
            tip,
            body,
        ),
    )
}

fn block_page(idx: &Indexer, b: &serde_json::Value) -> String {    let h = b["height"].as_i64().unwrap_or(0);
    let txs = idx.block_txs(h).unwrap_or_default();
    let mut rows = String::new();
    for t in &txs {
        rows += &format!("<tr><td>{}</td></tr>", link_tx(t));
    }
    let game_rows = match idx.game_block(h).unwrap_or(None) {
        Some(g) => format!(
            "<tr><th>jackpot bonus</th><td>{}</td></tr><tr><th>pot splash</th><td>{}</td></tr><tr><th>league inflow</th><td>{}</td></tr><tr><th>finder</th><td>{}</td></tr>",
            coins(g["bonus"].as_i64().unwrap_or(0)),
            coins(g["pot"].as_i64().unwrap_or(0)),
            coins(g["league"].as_i64().unwrap_or(0)),
            esc(g["finder"].as_str().unwrap_or("")),
        ),
        None => String::new(),
    };
    page(
        &format!("block {h}"),
        format!(
            "<table><tr><th>height</th><td>{}</td></tr><tr><th>hash</th><td>{}</td></tr><tr><th>time</th><td>{}</td></tr><tr><th>type</th><td>{}</td></tr><tr><th>confirmations</th><td>{}</td></tr><tr><th>txs</th><td>{}</td></tr><tr><th>size</th><td>{} bytes</td></tr><tr><th>merkle root</th><td>{}</td></tr><tr><th>difficulty</th><td>{:.2}</td></tr><tr><th>fees collected</th><td>{}</td></tr><tr><th>jackpot reserve</th><td>{}</td></tr><tr><th>pot</th><td>{}</td></tr><tr><th>drought</th><td>{} blocks</td></tr>{game_rows}</table><h2>Transactions</h2><table>{rows}</table>",
            link_block(h),
            b["hash"].as_str().unwrap_or(""),
            fmt_time(b["time"].as_i64().unwrap_or(0)),
            if b["pos"].as_bool().unwrap_or(false) { "<b class=pos>PoS · minted</b>" } else { "<span class=dim>PoW</span>" },
            b["confirmations"],
            b["ntx"],
            b["size"].as_i64().unwrap_or(0),
            b["merkleroot"].as_str().unwrap_or("–"),
            b["difficulty"].as_f64().unwrap_or(0.0),
            coins(b["fees"].as_i64().unwrap_or(0)),
            coins(b["reserve"].as_i64().unwrap_or(0)),
            coins(b["pot"].as_i64().unwrap_or(0)),
            b["drought"],
        ),
    )
}

fn tx_page(_idx: &Indexer, t: &serde_json::Value) -> String {
    let mut ins = String::new();
    if t["coinbase"].as_bool().unwrap_or(false) {
        ins = "<tr><td colspan=3>coinbase (newly minted)</td></tr>".to_string();
    } else {
        for (n, v) in t["vin"].as_array().cloned().unwrap_or_default().iter().enumerate() {
            ins += &format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                n,
                link_addr(&v["address"].as_str().map(|s| s.to_string())),
                coins(v["value"].as_i64().unwrap_or(0)),
                link_tx(v["txid"].as_str().unwrap_or("")),
            );
        }
    }
    let mut outs = String::new();
    for (n, v) in t["vout"].as_array().cloned().unwrap_or_default().iter().enumerate() {
        outs += &format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            n,
            link_addr(&v["address"].as_str().map(|s| s.to_string())),
            coins(v["value"].as_i64().unwrap_or(0)),
            if v["spent"].as_bool().unwrap_or(false) { "spent" } else { "unspent" },
        );
    }
    page(
        "tx",
        format!(
            "<table><tr><th>txid</th><td>{}</td></tr><tr><th>block</th><td>{} ({} confs)</td></tr><tr><th>fee</th><td>{}</td></tr></table><h2>Inputs</h2><table><tr><th>#</th><th>from</th><th>value</th><th>tx</th></tr>{ins}</table><h2>Outputs</h2><table><tr><th>#</th><th>to</th><th>value</th><th>state</th></tr>{outs}</table>",
            t["txid"].as_str().unwrap_or(""),
            link_block(t["height"].as_i64().unwrap_or(0)),
            t["confirmations"],
            coins(t["fee"].as_i64().unwrap_or(0)),
        ),
    )
}

fn address_page(a: &serde_json::Value) -> String {
    let mut rows = String::new();
    for t in a["txs"].as_array().cloned().unwrap_or_default() {
        let flow = t["flow"].as_i64().unwrap_or(0);
        let dir = if flow > 0 {
            "<span class=pos>in</span>"
        } else if flow < 0 {
            "<span>out</span>"
        } else {
            "<span class=dim>self</span>"
        };
        rows += &format!(
            "<tr><td>{}</td><td>{}</td><td>{dir}</td><td>{}</td></tr>",
            link_tx(t["txid"].as_str().unwrap_or("")),
            link_block(t["height"].as_i64().unwrap_or(0)),
            coins(flow),
        );
    }
    if a["txs_truncated"].as_bool().unwrap_or(false) {
        rows += "<tr><td colspan=4 class=dim>…first 100, newest first — refine with a txid search for older</td></tr>";
    }
    let unconfirmed = match a.get("unconfirmed") {
        Some(v) if !v.is_null() => coins(v.as_i64().unwrap_or(0)),
        _ => "<span class=dim>–</span>".to_string(),
    };
    let first_seen = match a.get("first_seen") {
        Some(v) if !v.is_null() => link_block(v.as_i64().unwrap_or(0)),
        _ => "<span class=dim>–</span>".to_string(),
    };
    page(
        "address",
        format!(
            "<table><tr><th>address</th><td>{}</td></tr><tr><th>balance</th><td>{}</td></tr><tr><th>total received</th><td>{}</td></tr><tr><th>transactions</th><td>{}</td></tr><tr><th>first seen</th><td>{}</td></tr><tr><th>unconfirmed</th><td>{}</td></tr></table>{}<h2>Transactions</h2><table><tr><th>txid</th><th>block</th><th>direction</th><th>net</th></tr>{rows}</table>",
            a["address"].as_str().unwrap_or(""),
            coins(a["balance"].as_i64().unwrap_or(0)),
            coins(a["received"].as_i64().unwrap_or(0)),
            a["txcount"].as_i64().map(|n| n.to_string()).unwrap_or_else(|| "–".to_string()),
            first_seen,
            unconfirmed,
            if a["txs"].as_array().map(|t| t.is_empty()).unwrap_or(true) && a["received"].as_i64().unwrap_or(0) == 0 {
                "<div class=card>never seen on this chain — check the address for typos before sending</div>"
            } else {
                ""
            },
        ),
    )
}

fn not_found(what: &str) -> String {
    page("not found", format!("<p>Unknown {what}. {}</p>", search_box()))
}

fn json_resp(v: &serde_json::Value) -> String {
    let body = serde_json::to_string(v).unwrap_or_default();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn html_resp(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn err404() -> String {
    let body = "not found";
    format!("HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
}

fn resolve_block(idx: &Indexer, key: &str) -> Option<serde_json::Value> {
    if let Ok(h) = key.parse::<i64>() {
        if h >= 0 {
            return idx.block_by_height(h).unwrap_or(None);
        }
        return None;
    }
    if key.len() == 64 {
        return idx.block_by_hash(key).unwrap_or(None);
    }
    None
}

fn handle(idx: &mut Indexer, method: &str, path: &str) -> String {
    if method != "GET" {
        return err404();
    }
    let (route, query) = match path.split_once('?') {
        Some((r, q)) => (r, q),
        None => (path, ""),
    };
    let json = query.contains("format=json");
    // API routes
    if let Some(key) = route.strip_prefix("/api/block/") {
        return match resolve_block(idx, key) {
            Some(b) => json_resp(&b),
            None => err404(),
        };
    }
    if let Some(key) = route.strip_prefix("/api/tx/") {
        return match idx.tx(key).unwrap_or(None) {
            Some(t) => json_resp(&t),
            None => err404(),
        };
    }
    if let Some(key) = route.strip_prefix("/api/address/") {
        return json_resp(&idx.address(key).unwrap_or(serde_json::json!({"address": key, "balance": 0, "received": 0, "txs": []})));
    }
    if route == "/api/tip" || route == "/api/stats" {
        return json_resp(&idx.stats().unwrap_or(serde_json::json!({"height": -1})));
    }
    if route == "/api/game" {
        let out = serde_json::json!({
            "state": idx.game_state().unwrap_or(serde_json::json!({})),
            "draws": idx.draws_recent(50).unwrap_or_default(),
            "blocks": idx.game_blocks_recent(100).unwrap_or_default(),
        });
        return json_resp(&out);
    }
    if route == "/api/league" {
        return json_resp(&serde_json::json!({
            "boards": idx.league_boards().unwrap_or_default(),
        }));
    }
    // HTML routes
    if route == "/" {
        return html_resp(&home(idx));
    }
    if route == "/game" {
        return html_resp(&game_page(idx));
    }
    if route == "/league" {
        return html_resp(&league_page(idx));
    }
    if route == "/mempool" {
        return html_resp(&mempool_page(idx));
    }
    if route == "/richlist" {
        return html_resp(&richlist_page(idx));
    }
    if route == "/api/mempool" {
        return json_resp(&idx.mempool().unwrap_or(serde_json::json!({})));
    }
    if route == "/api/richlist" {
        return json_resp(&serde_json::json!({
            "height": idx.height().unwrap_or(-1),
            "entries": idx.richlist(100).unwrap_or_default(),
        }));
    }
    if let Some(key) = route.strip_prefix("/block/") {
        return match resolve_block(idx, key) {
            Some(b) if json => json_resp(&b),
            Some(b) => html_resp(&block_page(idx, &b)),
            None => html_resp(&not_found("block")),
        };
    }
    if let Some(key) = route.strip_prefix("/tx/") {
        return match idx.tx(key).unwrap_or(None) {
            Some(t) if json => json_resp(&t),
            Some(t) => html_resp(&tx_page(idx, &t)),
            None => html_resp(&not_found("transaction")),
        };
    }
    if let Some(key) = route.strip_prefix("/address/") {
        let a = idx.address(key).unwrap_or(serde_json::json!({"address": key, "balance": 0, "received": 0, "txs": []}));
        if json {
            return json_resp(&a);
        }
        return html_resp(&address_page(&a));
    }
    if route == "/search" {
        let q = query
            .split('&')
            .find_map(|p| p.strip_prefix("q="))
            .unwrap_or("")
            .trim();
        if q.is_empty() {
            return html_resp(&page("search", search_box()));
        }
        if q.parse::<u64>().is_ok() {
            return html_resp(&format!("<head><meta http-equiv=refresh content=\"0;url=/block/{q}\"></head>"));
        }
        if q.len() == 64 && q.chars().all(|c| c.is_ascii_hexdigit()) {
            if resolve_block(idx, q).is_some() {
                return html_resp(&format!("<head><meta http-equiv=refresh content=\"0;url=/block/{q}\"></head>"));
            }
            if idx.tx(q).unwrap_or(None).is_some() {
                return html_resp(&format!("<head><meta http-equiv=refresh content=\"0;url=/tx/{q}\"></head>"));
            }
        }
        return html_resp(&format!("<head><meta http-equiv=refresh content=\"0;url=/address/{q}\"></head>"));
    }
    err404()
}

pub fn serve(listener: TcpListener, idx: Arc<Mutex<Indexer>>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let idx = Arc::clone(&idx);
        std::thread::spawn(move || serve_one(stream, idx));
    }
}

fn serve_one(stream: TcpStream, idx: Arc<Mutex<Indexer>>) {
    let mut reader = BufReader::new(&stream);
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    let mut parts = request.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    // Drain headers.
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
        }
    }
    let resp = match idx.lock() {
        Ok(mut i) => handle(&mut i, method, path),
        Err(_) => err404(),
    };
    let _ = (&stream).write_all(resp.as_bytes());
}
