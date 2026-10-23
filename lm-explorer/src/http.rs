//! Read-only HTTP UI + JSON API over the index. std only (mirrors the
//! pool's dashboard server): one thread per connection, `Connection:
//! close`. Routes:
//!   /                      home (tip, recent blocks + txs, search)
//!   /block/<height|hash>   block page        /api/block/<height|hash>
//!   /tx/<txid>             tx page           /api/tx/<txid>
//!   /address/<addr>        address page      /api/address/<addr>
//!   /rewards               jackpot/pot hits (chain/pool-reported)
//!   /divisions             season division boards (unlisted)
//!   /mempool               pending txs (node view)
//!   /richlist              top funded addresses (unlisted)
//!   /search?q=             smart redirect    /api/tip  /api/stats  /api/divisions  /api/mempool  /api/richlist

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use crate::index::Indexer;

const CSS: &str = "body{background:#0d1117;color:#e6edf3;font:14px ui-monospace,Menlo,monospace;margin:0 auto;padding:24px;max-width:960px}h1{font-size:20px;color:#ffd166}a{color:#7ee787}table{border-collapse:collapse;width:100%;margin:12px 0}td,th{border:1px solid #30363d;padding:6px 10px;text-align:left;font-size:13px}.dim{color:#8b949e}.pos{color:#7ee787}.card{background:#161b22;border:1px solid #30363d;border-radius:10px;padding:12px;margin:12px 0}input{width:70%;padding:8px;background:#0d1117;color:#e6edf3;border:1px solid #30363d;border-radius:6px}button{padding:8px 14px;background:#161b22;color:#ffd166;border:1px solid #ffd166;border-radius:6px}svg.chart{width:100%;height:auto;display:block}";

/// Template wrapper for HTML pages.
fn html_page(_title: &str, body: String) -> String {
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Lucky Lode explorer</title><style>{CSS}</style></head><body><h1><a href=/ style=\"color:#ffd166;text-decoration:none\">Lucky Lode explorer</a></h1><div class=dim style=\"margin:-4px 0 12px;font-size:13px\"><a href=/>blocks</a> · <a href=/charts>charts</a> · <a href=/supply>supply</a> · <a href=/rewards>rewards</a> · <a href=/mempool>mempool</a></div>{body}</body></html>"
    )
}

fn blocks_page(blocks: &[serde_json::Value], page_num: i64, tip: i64, per: i64, coinbase: &std::collections::HashMap<i64, i64>) -> String {
    let mut rows = String::new();
    for b in blocks {
        let h = b["height"].as_i64().unwrap_or(0);
        let t = b["time"].as_i64().unwrap_or(0);
        let is_pos = b["pos"].as_bool().unwrap_or(false);
        let reward = coinbase.get(&h).map(|v| coins(*v)).unwrap_or_else(|| "<span class=dim>–</span>".to_string());
        rows += &format!(
            "<tr><td>{}</td><td>{}</td><td><a href=/block/{}>{}</a></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            h,
            if is_pos { "<b class=pos>PoS · minted</b>" } else { "<span class=dim>PoW</span>" },
            b["hash"].as_str().unwrap_or(""),
            &b["hash"].as_str().unwrap_or("")[..16],
            b["ntx"],
            fmt_time(t),
            reward,
        );
    }
    let prev = if page_num > 0 { format!("<a href=/blocks?page={}>&laquo; prev</a> ", page_num - 1) } else { "".to_string() };
    let next = {
        let has_more = page_num * per < tip; // stop at the oldest indexed block
        if has_more { format!(" <a href=/blocks?page={}>next &raquo;</a>", page_num + 1) } else { "".to_string() }
    };
    html_page(
        &format!("blocks page {}", page_num + 1),
        format!(
            "<div class=dim style=\"margin:12px 0\">{prev}{next}</div><table><thead><tr><th>height</th><th>type</th><th>block</th><th>txs</th><th>time</th><th>coinbase</th></tr></thead><tbody>{rows}</tbody></table><div class=dim style=\"margin:12px 0\">{prev}{next}</div>",
        ),
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

/// Fee rate in sat/vB from a captured fact. Null when the node could not
/// serve the raw tx (size unknown) — shown as a dash, never a guess.
fn feerate_cell(v: &serde_json::Value) -> String {
    match v.get("feerate").and_then(|x| x.as_f64()) {
        Some(r) if r > 0.0 => format!("{r:.2}"),
        Some(_) => "0.00".to_string(),
        None => "<span class=dim>–</span>".to_string(),
    }
}

/// Size in bytes, or a dash when unknown.
fn size_cell(v: &serde_json::Value) -> String {
    match v.get("size").and_then(|x| x.as_i64()) {
        Some(s) if s > 0 => format!("{s} B"),
        _ => "<span class=dim>–</span>".to_string(),
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

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// An address object counts as seen on this chain when it received
/// anything or has history rows (the fabricated unknown-address object
/// has neither).
fn address_seen(a: &serde_json::Value) -> bool {
    a["received"].as_i64().unwrap_or(0) > 0
        || !a["txs"].as_array().map(|t| t.is_empty()).unwrap_or(true)
}
/// checksum): the explorer shows any indexed address regardless, but an
/// unknown string must at least look like an address to earn a page
/// instead of a 404.
fn address_shaped(s: &str) -> bool {
    (33..=35).contains(&s.len())
        && s.starts_with(['R', 'L', 'm', 'n', '2'])
        && s.bytes().all(|b| {
            matches!(b, b'1'..=b'9' | b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'Z' | b'a'..=b'k' | b'm'..=b'z')
        })
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

/// Finder/winner/worker names are sometimes real addresses and sometimes
/// labels ("external", worker names). Link the former, dim the latter.
fn addr_or_text(s: &str) -> String {
    let shaped = (26..=35).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric());
    if shaped {
        format!("<a href=/address/{s}>{s}</a>")
    } else {
        format!("<span class=dim>{}</span>", esc(s))
    }
}

fn search_box() -> String {
    "<form action=/search><input name=q placeholder=\"height, block hash, txid or address\"><button>Go</button></form>".to_string()
}

/// ?page=N query for paginated views, clamped to >= 0.
fn query_page(query: &str) -> i64 {
    query
        .split('&')
        .find_map(|p| p.strip_prefix("page="))
        .and_then(|v| v.parse::<i64>().ok())
        .map(|p| p.max(0))
        .unwrap_or(0)
}

fn home(idx: &mut Indexer) -> String {
    let stats = idx.stats().unwrap_or(serde_json::json!({"height": -1}));
    let h = stats.get("height").and_then(|v| v.as_i64()).unwrap_or(-1);
    let mut rows = String::new();
    // Per-block game money (home columns; pool-reported, pool-mined only).
    // bonus = jackpot win, pot = splash paid to the finder (0 when no
    // event — NOT the reserve balance), prize = inflow credited.
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
                    g.get("prize")?.as_i64()?,
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
        "<div class=card>stake supply <b>{}</b> · PoS share last 20: <b>{}%</b> (target ~50%)</div>",
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
            "<tr><td><a href=/tx/{txid}>{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            &txid[..txid.len().min(16)],
            link_block(t["height"].as_i64().unwrap_or(0)),
            fmt_time(t["time"].as_i64().unwrap_or(0)),
            coins(t["value"].as_i64().unwrap_or(0)),
            fee,
            size_cell(&t),
            feerate_cell(&t),
        );
    }
    if txrows.is_empty() {
        txrows = "<tr><td colspan=7 class=dim>no transactions indexed yet</td></tr>".to_string();
    }
    html_page(
        "home",
        format!(
            "{}<div class=card>height <b>{}</b> · hash <span class=dim>{}</span> · supply <b>{}</b> · difficulty {:.2} · minted (PoS) in last 20: <b>{}</b></div>{}{}{}<h2>Recent blocks</h2><table><tr><th>height</th><th>type</th><th>hash</th><th>txs</th><th>block reward</th><th>jackpot bonus</th><th>pot splash</th><th>prize inflow</th></tr>{rows}</table><h2>Recent transactions</h2><table><tr><th>txid</th><th>block</th><th>time</th><th>value</th><th>fee</th><th>size</th><th>fee rate (sat/vB)</th></tr>{txrows}</table>",
            search_box(),
            if h >= 0 { link_block(h) } else { "–".to_string() },
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

/// Game layer: live jackpot/pot state, jackpot hits, pot splashes
/// (pool-reported per block), and pool-observed consensus Luckynode Draw
/// claims (epoch-boundary blocks whose coinbase paid the draw winner
/// straight from the prize fund).
fn game_page(idx: &Indexer) -> String {
    let state = idx.game_state().unwrap_or(serde_json::json!({}));
    let reserve = state.get("reserve").and_then(|v| v.as_i64()).unwrap_or(0);
    let pot = state.get("pot").and_then(|v| v.as_i64()).unwrap_or(0);
    let drought = state.get("drought").and_then(|v| v.as_i64()).unwrap_or(0);
    let jd = state.get("jackpotdivisor").and_then(|v| v.as_i64()).unwrap_or(100);
    let pd = state.get("potdivisor").and_then(|v| v.as_i64()).unwrap_or(1000);
    let dl = state.get("potdrought").and_then(|v| v.as_i64()).unwrap_or(4320);
    let faucet = state.get("faucet").and_then(|v| v.as_i64()).unwrap_or(0);
    let dev = state.get("dev").and_then(|v| v.as_i64()).unwrap_or(0);
    let blocks = idx.game_blocks_recent(100).unwrap_or_default();

    let mut hits = String::new();
    let mut splashes = String::new();
    for b in &blocks {
        let h = b["height"].as_i64().unwrap_or(0);
        let finder = addr_or_text(b["finder"].as_str().unwrap_or(""));
        if b["bonus"].as_i64().unwrap_or(0) > 0 {
            hits += &format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                link_block(h),
                finder,
                coins(b["bonus"].as_i64().unwrap_or(0)),
                coins(b["prize"].as_i64().unwrap_or(0)),
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
    let mut claims = String::new();
    for c in idx.game_claims_recent(50).unwrap_or_default() {
        claims += &format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            link_block(c["height"].as_i64().unwrap_or(0)),
            addr_or_text(c["finder"].as_str().unwrap_or("")),
            coins(c["claim"].as_i64().unwrap_or(0)),
            addr_or_text(c["winner"].as_str().unwrap_or("")),
        );
    }
    if claims.is_empty() {
        claims = "<tr><td colspan=4 class=dim>no prize draw claims yet</td></tr>".to_string();
    }

    html_page(
        "rewards",
        format!(
            "{}<div class=card>jackpot reserve <b>{}</b> · pot <b>{}</b> · pot drought <b>{}/{}</b> · faucet reserve <b>{}</b> · dev reserve <b>{}</b> · odds (PoW finds, pre-committed per height): jackpot 1-in-{} blocks,             pot 1-in-{} &mdash; forced splash after {} dry blocks max</div><h2>Jackpot hits</h2><table><tr><th>height</th><th>finder</th><th>bonus</th><th>prize inflow</th></tr>{hits}</table><h2>Pot splashes</h2><table><tr><th>height</th><th>finder</th><th>splash</th></tr>{splashes}</table><h2>Prize draw claims</h2><div class=dim>epoch-boundary blocks whose coinbase paid the Luckynode Draw winner straight from the prize fund — pool-observed claims only (pool-mined boundaries); the wallet Luckynodes tab shows the current epoch.</div><table><tr><th>height</th><th>finder</th><th>claim</th><th>winner</th></tr>{claims}</table>",
            search_box(),
            coins(reserve),
            coins(pot),
            drought,
            dl,
            coins(faucet),
            coins(dev),
            jd,
            pd,
            dl,
        ),
    )
}

/// Compact number for chart axes: 1.2k, 3.4M, 5.6B.
fn compact(v: f64) -> String {
    let a = v.abs();
    if a >= 1e9 {
        format!("{:.1}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else if a >= 1.0 {
        format!("{v:.1}")
    } else if a > 0.0 {
        format!("{v:.3}")
    } else {
        "0".to_string()
    }
}

/// One chart: a title, a value series (x = day index, y = value) and a
/// unit label. Rendered as a self-contained inline SVG (no JS, no
/// external assets — the explorer serves plain HTML).
struct Chart {
    title: String,
    unit: String,
    points: Vec<(f64, f64)>,
    /// Optional second series drawn as a second line (e.g. supply).
    overlay: Vec<(f64, f64)>,
    bars: bool,
}

impl Chart {
    fn new(title: &str, unit: &str, points: Vec<(f64, f64)>) -> Self {
        Self {
            title: title.to_string(),
            unit: unit.to_string(),
            points,
            overlay: Vec::new(),
            bars: false,
        }
    }

    fn with_overlay(mut self, o: Vec<(f64, f64)>) -> Self {
        self.overlay = o;
        self
    }

    fn bars(mut self) -> Self {
        self.bars = true;
        self
    }

    /// Render the chart as inline SVG. Width is fluid (viewBox), the
    /// page CSS scales it to the column.
    fn render(&self) -> String {
        let w = 640.0_f64;
        let h = 200.0_f64;
        let pad_l = 56.0;
        let pad_r = 12.0;
        let pad_t = 12.0;
        let pad_b = 26.0;
        let plot_w = w - pad_l - pad_r;
        let plot_h = h - pad_t - pad_b;

        let mut all: Vec<f64> = self.points.iter().map(|p| p.1).collect();
        all.extend(self.overlay.iter().map(|p| p.1));
        if all.is_empty() {
            return format!(
                "<div class=card><b>{}</b><p class=dim>no data yet</p></div>",
                esc(&self.title)
            );
        }
        let max_v = all.iter().cloned().fold(0.0_f64, f64::max);
        let min_v = all.iter().cloned().fold(f64::INFINITY, f64::min);
        let min_v = if min_v.is_finite() { min_v } else { 0.0 };
        // Include zero when the series does not go negative.
        let (lo, hi) = if min_v < 0.0 {
            (min_v, max_v)
        } else {
            (0.0, max_v)
        };
        let span = if (hi - lo).abs() < f64::EPSILON { 1.0 } else { hi - lo };
        let n = self.points.len().max(1) as f64;
        let x_of = |i: f64| pad_l + if n <= 1.0 { plot_w / 2.0 } else { i / (n - 1.0) * plot_w };
        let y_of = |v: f64| pad_t + plot_h - (v - lo) / span * plot_h;

        let mut svg = String::new();
        // No preserveAspectRatio override: the default (xMidYMid meet)
        // keeps axis labels undistorted at any container width. The CSS
        // gives the svg height:auto so the viewBox aspect is honored.
        svg.push_str(&format!(
            "<svg viewBox='0 0 {} {}' class=chart role=img aria-label='{}'>",
            w, h, esc(&self.title)
        ));
        // Horizontal gridlines + y labels.
        for i in 0..=4 {
            let v = lo + span * i as f64 / 4.0;
            let y = y_of(v);
            svg.push_str(&format!(
                "<line x1='{pad_l}' y1='{y:.1}' x2='{}' y2='{y:.1}' stroke='#30363d' stroke-width='1'/>",
                w - pad_r
            ));
            svg.push_str(&format!(
                "<text x='{}' y='{:.1}' fill='#8b949e' font-size='10' text-anchor='end'>{}</text>",
                pad_l - 6.0,
                y + 3.0,
                esc(&compact(v))
            ));
        }
        if self.bars {
            let bw = (plot_w / n).max(1.0);
            for (i, (_, v)) in self.points.iter().enumerate() {
                let x = x_of(i as f64) - bw / 2.0;
                let y = y_of(*v);
                svg.push_str(&format!(
                    "<rect x='{x:.1}' y='{y:.1}' width='{bw:.1}' height='{:.1}' fill='#388bfd' opacity='0.85'/>",
                    (y_of(lo) - y).max(0.0)
                ));
            }
        } else {
            let path: String = self
                .points
                .iter()
                .enumerate()
                .map(|(i, (_, v))| format!("{:.1},{:.1}", x_of(i as f64), y_of(*v)))
                .collect::<Vec<_>>()
                .join(" ");
            svg.push_str(&format!(
                "<polyline points='{path}' fill='none' stroke='#58a6ff' stroke-width='2'/>"
            ));
            // Area fill under the line.
            if !self.points.is_empty() {
                let base = y_of(lo);
                let area: String = self
                    .points
                    .iter()
                    .enumerate()
                    .map(|(i, (_, v))| format!("{:.1},{:.1}", x_of(i as f64), y_of(*v)))
                    .collect::<Vec<_>>()
                    .join(" ");
                svg.push_str(&format!(
                    "<polygon points='{:.1},{base:.1} {area} {:.1},{base:.1}' fill='#58a6ff' opacity='0.12'/>",
                    x_of(0.0),
                    x_of(self.points.len() as f64 - 1.0)
                ));
            }
        }
        if !self.overlay.is_empty() {
            let path: String = self
                .overlay
                .iter()
                .enumerate()
                .map(|(i, (_, v))| format!("{:.1},{:.1}", x_of(i as f64), y_of(*v)))
                .collect::<Vec<_>>()
                .join(" ");
            svg.push_str(&format!(
                "<polyline points='{path}' fill='none' stroke='#f0883e' stroke-width='2' stroke-dasharray='4 3'/>"
            ));
        }
        // X labels: first and last day.
        let fmt_day = |ts: f64| -> String {
            chrono::DateTime::from_timestamp(ts as i64, 0)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default()
        };
        if let Some((first, _)) = self.points.first() {
            svg.push_str(&format!(
                "<text x='{pad_l}' y='{}' fill='#8b949e' font-size='10'>{}</text>",
                h - 8.0,
                esc(&fmt_day(*first))
            ));
        }
        if let Some((last, _)) = self.points.last() {
            svg.push_str(&format!(
                "<text x='{}' y='{}' fill='#8b949e' font-size='10' text-anchor='end'>{}</text>",
                w - pad_r,
                h - 8.0,
                esc(&fmt_day(*last))
            ));
        }
        svg.push_str("</svg>");
        format!(
            "<div class=card><b>{}</b><p class=dim style='margin:2px 0 6px'>{}</p>{}</div>",
            esc(&self.title),
            esc(&self.unit),
            svg
        )
    }
}

/// Charts page: daily history — hashrate, difficulty, transactions, fees,
/// supply and active addresses. All series come from the local index.
fn charts_page(idx: &Indexer, days: i64) -> String {
    let series = idx.daily_series(days).unwrap_or(serde_json::json!({"days": []}));
    let d = series["days"].as_array().cloned().unwrap_or_default();
    let day_of = |i: usize| -> f64 { d[i]["day"].as_f64().unwrap_or(0.0) };
    let num = |i: usize, k: &str| -> f64 { d[i][k].as_f64().unwrap_or(0.0) };

    let hashrate: Vec<(f64, f64)> = (0..d.len())
        .filter_map(|i| d[i]["hashrate"].as_f64().map(|h| (day_of(i), h)))
        .collect();
    let difficulty: Vec<(f64, f64)> = (0..d.len()).map(|i| (day_of(i), num(i, "difficulty"))).collect();
    let txs: Vec<(f64, f64)> = (0..d.len()).map(|i| (day_of(i), num(i, "txs"))).collect();
    let fees: Vec<(f64, f64)> = (0..d.len()).map(|i| (day_of(i), num(i, "fees"))).collect();
    let supply: Vec<(f64, f64)> = (0..d.len()).map(|i| (day_of(i), num(i, "supply"))).collect();
    let mined: Vec<(f64, f64)> = (0..d.len()).map(|i| (day_of(i), num(i, "minted"))).collect();
    let addrs: Vec<(f64, f64)> = (0..d.len()).map(|i| (day_of(i), num(i, "addresses"))).collect();

    let mut body = String::new();
    body.push_str(&format!(
        "<div class=card>daily history · last <b>{}</b> days with indexed blocks · hashrate is work ÷ time over each day (the node's own estimate uses a 120-block window)</div>",
        days
    ));
    // Range switcher.
    body.push_str("<p>");
    for r in [7i64, 30, 90, 365] {
        let sel = if r == days { " style='color:#ffd166'" } else { "" };
        body.push_str(&format!("<a href='/charts?days={r}'{sel}> {r}d</a>"));
    }
    body.push_str("</p>");
    if d.is_empty() {
        body.push_str("<div class=card>no blocks indexed yet — charts appear once the index has history</div>");
    } else {
        body.push_str(&Chart::new("Hashrate", "solutions per second — work ÷ time over the day (the node's getnetworkhashps uses a 120-block window)", hashrate).render());
        body.push_str(&Chart::new("Difficulty", "average difficulty of the day's blocks", difficulty).render());
        body.push_str(&Chart::new("Transactions", "non-coinbase transactions per day", txs).bars().render());
        body.push_str(&Chart::new("Fees collected", "sats paid in transaction fees per day", fees).bars().render());
        body.push_str(&Chart::new("Cumulative supply", "coins minted so far (solid line: total; dashed: added that day)", supply).with_overlay(mined).render());
        body.push_str(&Chart::new("Active addresses", "distinct addresses touched per day", addrs).bars().render());
    }
    // A small table with the same numbers (readable, copyable).
    if !d.is_empty() {
        let mut rows = String::new();
        for day in d.iter().rev().take(30) {
            let day_ts = day["day"].as_i64().unwrap_or(0);
            let date = chrono::DateTime::from_timestamp(day_ts, 0)
                .map(|x| x.format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            rows.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                date,
                day["blocks"].as_i64().unwrap_or(0),
                day["txs"].as_i64().unwrap_or(0),
                day["addresses"].as_i64().unwrap_or(0),
                coins(day["fees"].as_i64().unwrap_or(0)),
                coins(day["minted"].as_i64().unwrap_or(0)),
                hs(day["hashrate"].as_f64().unwrap_or(0.0)),
            ));
        }
        body.push_str(&format!(
            "<h2>Daily detail</h2><table><tr><th>date</th><th>blocks</th><th>txs</th><th>addresses</th><th>fees</th><th>minted</th><th>hashrate</th></tr>{rows}</table>"
        ));
    }
    html_page("charts", format!("{}{}", search_box(), body))
}

/// Market page: on-chain market facts. Price and market cap appear only
/// when a price feed is configured — with no exchange listing there is no
/// price source, and the page says so instead of inventing one.
fn market_page(idx: &mut Indexer) -> String {
    let m = idx.market().unwrap_or(serde_json::json!({}));
    let price = m["price"].as_f64();
    let cap = m["market_cap"].as_f64();
    let mut body = String::new();

    let price_row = match (price, cap) {
        (Some(p), Some(c)) => format!(
            "<div class=card>price <b>${p:.8}</b> · market cap <b>${c:.0}</b> <span class=dim>({})</span></div>",
            esc(m["price_source"].as_str().unwrap_or("configured feed"))
        ),
        _ => "<div class=card>price <span class=dim>unavailable — no exchange listing for {ticker}, so there is no price source to read</span></div>".replace("{ticker}", TICK),
    };
    body.push_str(&price_row);
    body.push_str(&format!(
        "<div class=card>circulating supply <b>{}</b> · holders <b>{}</b> · unspent outputs <b>{}</b></div>",
        coins(m["supply"].as_i64().unwrap_or(0)),
        m["holders"].as_i64().unwrap_or(0),
        m["utxos"].as_i64().unwrap_or(0),
    ));
    body.push_str(&format!(
        "<div class=card>last 24h: <b>{}</b> blocks · <b>{}</b> transactions · <b>{}</b> active addresses · moved <b>{}</b> <span class=dim>(output value, includes change)</span> · fees <b>{}</b></div>",
        m["blocks_24h"].as_i64().unwrap_or(0),
        m["txs_24h"].as_i64().unwrap_or(0),
        m["addresses_24h"].as_i64().unwrap_or(0),
        coins(m["moved_24h"].as_i64().unwrap_or(0)),
        coins(m["fees_24h"].as_i64().unwrap_or(0)),
    ));
    // Supply curve.
    let curve = m["supply_curve"].as_array().cloned().unwrap_or_default();
    if curve.len() > 1 {
        // Sample to keep the SVG small.
        let step = (curve.len() / 200).max(1);
        let pts: Vec<(f64, f64)> = curve
            .iter()
            .step_by(step)
            .map(|p| (p["time"].as_f64().unwrap_or(0.0), p["supply"].as_f64().unwrap_or(0.0)))
            .collect();
        body.push_str(&Chart::new("Supply over time", "cumulative coins minted, per block height", pts).render());
    }
    // Top holders (reuse the rich list).
    let top = idx.richlist(20).unwrap_or_default();
    let mut rows = String::new();
    for (i, e) in top.iter().enumerate() {
        let addr = esc(e["address"].as_str().unwrap_or(""));
        rows.push_str(&format!(
            "<tr><td>{}</td><td><a href=/address/{addr}>{addr}</a></td><td>{}</td></tr>",
            i + 1,
            coins(e["balance"].as_i64().unwrap_or(0)),
        ));
    }
    body.push_str(&format!(
        "<h2>Largest balances</h2><p class=dim>unspent outputs only — exchange and pool wallets dominate by construction</p><table><tr><th>#</th><th>address</th><th>balance</th></tr>{rows}</table>"
    ));
    html_page("supply", format!("{}{}", search_box(), body))
}

/// Seasonal division boards (doc 03 §3.3: phones race phones), as last
/// synced from the pool's dashboard API. Snapshot data, not consensus —
/// the pool is the board authority; the explorer mirrors it.
fn divisions_page(idx: &Indexer) -> String {
    let boards = idx.division_boards().unwrap_or_default();
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
                        addr_or_text(e["name"].as_str().unwrap_or("")),
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
        sections = "<p class=dim>no division data yet — the explorer mirrors the pool boards on its next pool sync.</p>".to_string();
    }
    html_page(
        "divisions",
        format!(
            "{}<div class=card>Seasonal boards by device family (30-day effective work). Phones race phones; farms are paid, not feted. Archive snapshot mirrored from the official pool — the pool dashboard shows the live season; this page shows the last synced copy.</div>{sections}",
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
        "<tr><td colspan=5 class=dim>mempool empty — everything indexed is confirmed</td></tr>".to_string()
    } else {
        entries
            .iter()
            .map(|e| {
                let fee = match e.get("fee") {
                    Some(v) if v.is_i64() || v.is_u64() => coins(v.as_i64().unwrap_or(0)),
                    _ => "<span class=dim>–</span>".to_string(),
                };
                // Fee rate: fee ÷ vsize, when the fee is known.
                let rate = match (e.get("fee").and_then(|f| f.as_i64()), e.get("vsize").and_then(|s| s.as_u64())) {
                    (Some(f), Some(s)) if s > 0 => format!("{:.2}", f as f64 / s as f64),
                    _ => "<span class=dim>–</span>".to_string(),
                };
                format!(
                    "<tr><td><a href=/tx/{}>{}</a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    e["txid"].as_str().unwrap_or(""),
                    &e["txid"].as_str().unwrap_or("")[..e["txid"].as_str().unwrap_or("").len().min(16)],
                    fee,
                    e["vsize"].as_u64().unwrap_or(0),
                    rate,
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
    html_page(
        "mempool",
        format!(
            "{}<div class=card>pending <b>{}</b> · size <b>{} B</b> · mempool usage <b>{} B</b> · waiting fees <b>{}</b></div>{}<h2>Pending transactions</h2><table><tr><th>txid</th><th>fee</th><th>vsize</th><th>fee rate (sat/vB)</th><th>first seen</th></tr>{rows}</table>",
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
    html_page(
        "rich list",
        format!(
            "{}<div class=card>top 100 funded addresses by unspent balance · as of height <b>{}</b></div><table><tr><th>#</th><th>address</th><th>balance</th></tr>{}</table>",
            search_box(),
            tip,
            body,
        ),
    )
}

fn block_page(idx: &Indexer, b: &serde_json::Value) -> String {
    let h = b["height"].as_i64().unwrap_or(0);
    let txs = idx.block_txs(h).unwrap_or_default();
    let mut rows = String::new();
    for t in &txs {
        // Per-tx facts (size/fee rate) captured at index time.
        let fact = idx.tx_facts(t);
        let (size, rate) = match fact {
            Some((sz, _fee, r)) => (format!("{sz} B"), format!("{r:.2}")),
            None => ("<span class=dim>–</span>".to_string(), "<span class=dim>–</span>".to_string()),
        };
        rows += &format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            link_tx(t),
            size,
            rate
        );
    }
    let game_rows = match idx.game_block(h).unwrap_or(None) {
        Some(g) => {
            let claim = g["claim"].as_i64().unwrap_or(0);
            let claim_rows = if claim > 0 {
                format!(
                    "<tr><th>prize draw claim</th><td>{} → {}</td></tr>",
                    coins(claim),
                    addr_or_text(g["winner"].as_str().unwrap_or("")),
                )
            } else {
                String::new()
            };
            format!(
                "<tr><th>jackpot bonus</th><td>{}</td></tr><tr><th>pot splash</th><td>{}</td></tr><tr><th>prize inflow</th><td>{}</td></tr><tr><th>finder</th><td>{}</td></tr>{claim_rows}",
                coins(g["bonus"].as_i64().unwrap_or(0)),
                coins(g["pot"].as_i64().unwrap_or(0)),
                coins(g["prize"].as_i64().unwrap_or(0)),
                addr_or_text(g["finder"].as_str().unwrap_or("")),
            )
        }
        None => String::new(),
    };
    let tip = idx.height().unwrap_or(h);
    let nav = format!(
        "<div class=dim style=\"margin:12px 0\"><a href=/blocks>&laquo; all blocks</a>{}{}</div>",
        if h > 0 { format!(" · <a href=/block/{}>&laquo; prev</a>", h - 1) } else { String::new() },
        if h < tip { format!(" · <a href=/block/{}>next &raquo;</a>", h + 1) } else { String::new() },
    );
    html_page(
        &format!("block {h}"),
        format!(
            "{}{}<table><tr><th>height</th><td>{}</td></tr><tr><th>hash</th><td>{}</td></tr><tr><th>time</th><td>{}</td></tr><tr><th>type</th><td>{}</td></tr><tr><th>confirmations</th><td>{}</td></tr><tr><th>txs</th><td>{}</td></tr><tr><th>size</th><td>{} bytes</td></tr><tr><th>merkle root</th><td>{}</td></tr><tr><th>difficulty</th><td>{:.2}</td></tr><tr><th>fees collected</th><td>{}</td></tr><tr><th>jackpot reserve</th><td>{}</td></tr><tr><th>pot</th><td>{}</td></tr><tr><th>drought</th><td>{} blocks</td></tr>{game_rows}</table><h2>Transactions</h2><table><tr><th>txid</th><th>size</th><th>fee rate (sat/vB)</th></tr>{rows}</table>{}",
            search_box(),
            nav,
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
            nav,
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
        let state = match v.get("spent_by").and_then(|s| s.as_str()) {
            Some(spender) => format!("<a href=/tx/{spender}>spent →</a>"),
            None => "unspent".to_string(),
        };
        outs += &format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            n,
            link_addr(&v["address"].as_str().map(|s| s.to_string())),
            coins(v["value"].as_i64().unwrap_or(0)),
            state,
        );
    }
    let in_total: i64 = t["vin"]
        .as_array()
        .map(|a| a.iter().map(|v| v["value"].as_i64().unwrap_or(0)).sum())
        .unwrap_or(0);
    let out_total: i64 = t["vout"]
        .as_array()
        .map(|a| a.iter().map(|v| v["value"].as_i64().unwrap_or(0)).sum())
        .unwrap_or(0);
    html_page(
        "tx",
        format!(
            "{}<table><tr><th>txid</th><td>{}</td></tr><tr><th>block</th><td>{} ({} confs)</td></tr><tr><th>fee</th><td>{}</td></tr><tr><th>size</th><td>{}</td></tr><tr><th>fee rate</th><td>{} sat/vB</td></tr><tr><th>total in / out</th><td>{} / {}</td></tr></table><h2>Inputs</h2><table><tr><th>#</th><th>from</th><th>value</th><th>tx</th></tr>{ins}</table><h2>Outputs</h2><table><tr><th>#</th><th>to</th><th>value</th><th>state</th></tr>{outs}</table>",
            search_box(),
            t["txid"].as_str().unwrap_or(""),
            link_block(t["height"].as_i64().unwrap_or(0)),
            t["confirmations"],
            coins(t["fee"].as_i64().unwrap_or(0)),
            size_cell(t),
            feerate_cell(t),
            coins(in_total),
            coins(out_total),
        ),
    )
}

fn address_page(a: &serde_json::Value, page: i64) -> String {
    let addr = a["address"].as_str().unwrap_or("");
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
    if rows.is_empty() {
        rows = "<tr><td colspan=4 class=dim>no history on this chain</td></tr>".to_string();
    }
    // Paging: 100 newest-first rows per page; deep pages carry ?page=N.
    let pages = a["pages"].as_i64().unwrap_or(1).max(1);
    let prev = if page > 0 {
        format!("<a href=\"/address/{addr}?page={}\">&laquo; newer</a> ", page - 1)
    } else {
        String::new()
    };
    let next = if page + 1 < pages {
        format!(" <a href=\"/address/{addr}?page={}\">older &raquo;</a>", page + 1)
    } else {
        String::new()
    };
    let pager = if pages > 1 {
        format!(
            "<div class=dim style=\"margin:12px 0\">{prev}page {} of {pages}{next}</div>",
            page + 1
        )
    } else {
        String::new()
    };
    let unconfirmed = match a.get("unconfirmed") {
        Some(v) if !v.is_null() => coins(v.as_i64().unwrap_or(0)),
        _ => "<span class=dim>–</span>".to_string(),
    };
    let first_seen = match a.get("first_seen") {
        Some(v) if !v.is_null() => link_block(v.as_i64().unwrap_or(0)),
        _ => "<span class=dim>–</span>".to_string(),
    };
    html_page(
        "address",
        format!(
            "{}{}<table><tr><th>address</th><td>{}</td></tr><tr><th>balance</th><td>{}</td></tr><tr><th>total received</th><td>{}</td></tr><tr><th>transactions</th><td>{}</td></tr><tr><th>first seen</th><td>{}</td></tr><tr><th>unconfirmed</th><td>{}</td></tr></table>{}<h2>Transactions</h2><table><tr><th>txid</th><th>block</th><th>direction</th><th>net</th></tr>{rows}</table>{pager}",
            search_box(),
            "",
            esc(addr),
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
    html_resp_status(
        &html_page("not found", format!("<p>Unknown {what}. {}</p>", search_box())),
        "404 Not Found",
    )
}

fn json_resp(v: &serde_json::Value) -> String {
    json_resp_status(v, "200 OK")
}

fn json_resp_status(v: &serde_json::Value, status: &str) -> String {
    let body = serde_json::to_string(v).unwrap_or_default();
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// API misses are JSON errors (same status semantics as the pages,
/// machine-readable).
fn json_404() -> String {
    json_resp_status(&serde_json::json!({"error": "not found"}), "404 Not Found")
}

/// Search answers real redirects (not meta-refresh): every target is
/// built from validated input only (block digits, hex ids, or
/// shape-checked addresses), so no user bytes can break the header.
fn redirect(to: &str) -> String {
    format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
}

fn html_resp(body: &str) -> String {
    html_resp_status(body, "200 OK")
}

fn html_resp_status(body: &str, status: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn err404() -> String {
    let body = "not found";
    format!("HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
}

fn err500() -> String {
    let body = "index unavailable";
    format!("HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
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
            None => json_404(),
        };
    }
    if let Some(key) = route.strip_prefix("/api/tx/") {
        return match idx.tx(key).unwrap_or(None) {
            Some(t) => json_resp(&t),
            None => json_404(),
        };
    }
    if let Some(key) = route.strip_prefix("/api/address/") {
        let page = query_page(query);
        let a = idx
            .address_paged(key, page)
            .unwrap_or(serde_json::json!({"address": key, "balance": 0, "received": 0, "txs": []}));
        if address_seen(&a) || address_shaped(key) {
            return json_resp(&a);
        }
        return json_404();
    }
    if route == "/api/tip" || route == "/api/stats" {
        return json_resp(&idx.stats().unwrap_or(serde_json::json!({"height": -1})));
    }
    if route == "/api/game" {
        let out = serde_json::json!({
            "state": idx.game_state().unwrap_or(serde_json::json!({})),
            "blocks": idx.game_blocks_recent(100).unwrap_or_default(),
        });
        return json_resp(&out);
    }
    if route == "/api/divisions" {
        return json_resp(&serde_json::json!({
            "boards": idx.division_boards().unwrap_or_default(),
        }));
    }
    if route == "/api/charts" {
        let days = query
            .split('&')
            .find_map(|p| p.strip_prefix("days="))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(30)
            .clamp(1, 3650);
        return json_resp(&idx.daily_series(days).unwrap_or(serde_json::json!({"days": []})));
    }
    if route == "/api/market" {
        return json_resp(&idx.market().unwrap_or(serde_json::json!({})));
    }
    // HTML routes
    if route == "/" {
        return html_resp(&home(idx));
    }
    if route == "/rewards" || route == "/game" {
        return html_resp(&game_page(idx));
    }
    if route == "/divisions" {
        return html_resp(&divisions_page(idx));
    }
    if route == "/mempool" {
        return html_resp(&mempool_page(idx));
    }
    if route == "/richlist" {
        return html_resp(&richlist_page(idx));
    }
    if route == "/charts" {
        let days = query
            .split('&')
            .find_map(|p| p.strip_prefix("days="))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(30)
            .clamp(1, 3650);
        return html_resp(&charts_page(idx, days));
    }
    if route == "/supply" || route == "/market" {
        return html_resp(&market_page(idx));
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
            None => not_found("block"),
        };
    }
    if let Some(key) = route.strip_prefix("/tx/") {
        return match idx.tx(key).unwrap_or(None) {
            Some(t) if json => json_resp(&t),
            Some(t) => html_resp(&tx_page(idx, &t)),
            None => not_found("transaction"),
        };
    }
    if let Some(key) = route.strip_prefix("/address/") {
        // History pages: ?page=N, 100 newest-first rows each. Page 0 is
        // the default view; deeper pages need the explicit query.
        let page = query_page(query);
        let a = idx.address_paged(key, page).unwrap_or(serde_json::json!({"address": key, "balance": 0, "received": 0, "txs": []}));
        if json {
            return json_resp(&a);
        }
        // Indexed addresses always render; an unknown string must at
        // least look like an address or it is a typo (404, not a
        // zero-balance page that reads as valid-but-empty).
        if !address_seen(&a) && !address_shaped(key) {
            return not_found("address");
        }
        return html_resp(&address_page(&a, page));
    }
    if route == "/blocks" || route == "/api/blocks" {
        let page = query_page(query);
        let tip = idx.height().unwrap_or(-1);
        let per = 50;
        let top = tip - page * per;
        let list = idx.block_list(top, per).unwrap_or_default();
        // Coinbase totals for the reward column (indexed at index time)
        let mut cb = std::collections::HashMap::new();
        for b in &list {
            let h = b["height"].as_i64().unwrap_or(0);
            if let Ok(v) = idx.coinbase_total(h) {
                cb.insert(h, v);
            }
        }
        if json || route == "/api/blocks" {
            return json_resp(&serde_json::json!({"tip": tip, "page": page, "per_page": per, "blocks": list}));
        }
        return html_resp(&blocks_page(&list, page, tip, per, &cb));
    }
    if route == "/search" {
        let q = query
            .split('&')
            .find_map(|p| p.strip_prefix("q="))
            .unwrap_or("")
            .trim();
        if q.is_empty() {
            return html_resp(&html_page("search", search_box()));
        }
        if q.parse::<u64>().is_ok() {
            return redirect(&format!("/block/{q}"));
        }
        if q.len() == 64 && q.chars().all(|c| c.is_ascii_hexdigit()) {
            if resolve_block(idx, q).is_some() {
                return redirect(&format!("/block/{q}"));
            }
            if idx.tx(q).unwrap_or(None).is_some() {
                return redirect(&format!("/tx/{q}"));
            }
            return not_found("block or transaction");
        }
        if address_shaped(q) {
            return redirect(&format!("/address/{q}"));
        }
        return not_found("address");
    }
    if route.starts_with("/api/") {
        return json_404();
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
    // Bounded request handling: a read deadline and a line cap, so a
    // connected-and-silent peer parks a thread for seconds, not forever.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
    let mut reader = BufReader::new(&stream);
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() || request.len() > 16 * 1024 {
        return;
    }
    let mut parts = request.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    // Drain headers.
    let mut header_bytes = request.len();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                header_bytes += line.len();
                if header_bytes > 32 * 1024 {
                    break;
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
        }
    }
    let resp = match idx.lock() {
        Ok(mut i) => handle(&mut i, method, path),
        Err(_) => err500(),
    };
    let _ = (&stream).write_all(resp.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use lm_stratum::rpc::RpcClient;

    fn test_indexer(name: &str) -> (Indexer, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&dir);
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        (idx, dir)
    }

    #[test]
    fn esc_covers_quoted_attributes() {
        assert_eq!(esc("\"onmouseover=x"), "&quot;onmouseover=x");
        assert_eq!(esc("a'b"), "a&#39;b");
        assert_eq!(esc("<img>"), "&lt;img&gt;");
    }

    #[test]
    fn missing_blocks_txs_and_garbage_addresses_are_404() {
        let (mut idx, dir) = test_indexer("explorer-http-test.db");
        let r = handle(&mut idx, "GET", "/block/999999");
        assert!(r.starts_with("HTTP/1.1 404"), "missing block: {r:.40}");
        let r = handle(&mut idx, "GET", &format!("/tx/{}", "0".repeat(64)));
        assert!(r.starts_with("HTTP/1.1 404"), "missing tx: {r:.40}");
        let r = handle(&mut idx, "GET", "/address/not-an-address!!!");
        assert!(r.starts_with("HTTP/1.1 404"), "garbage: {r:.40}");
        let r = handle(&mut idx, "GET", "/nope");
        assert!(r.starts_with("HTTP/1.1 404"));
        // Unknown API routes are JSON 404s, not plain text.
        let r = handle(&mut idx, "GET", "/api/bogus");
        assert!(r.starts_with("HTTP/1.1 404"));
        assert!(r.contains("application/json") && r.contains("\"error\""));
        let r = handle(&mut idx, "GET", "/api/block/999999");
        assert!(r.starts_with("HTTP/1.1 404") && r.contains("application/json"));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn search_redirects_valid_input_and_rejects_garbage() {
        let (mut idx, dir) = test_indexer("explorer-http-search-test.db");
        let r = handle(&mut idx, "GET", "/search?q=12");
        assert!(r.starts_with("HTTP/1.1 302") && r.contains("Location: /block/12"), "height: {r:.60}");
        let r = handle(&mut idx, "GET", "/search?q=hello");
        assert!(r.starts_with("HTTP/1.1 404"), "garbage: {r:.40}");
        // Valid-shaped but unknown address: redirect to its (empty, with
        // typo hint) page.
        let shaped = format!("m{}", "1".repeat(33));
        assert_eq!(shaped.len(), 34);
        let r = handle(&mut idx, "GET", &format!("/search?q={shaped}"));
        assert!(r.starts_with("HTTP/1.1 302") && r.contains(&format!("Location: /address/{shaped}")));
        // Unknown 64-hex is neither block nor tx: 404, not an address page.
        let r = handle(&mut idx, "GET", &format!("/search?q={}", "a".repeat(64)));
        assert!(r.starts_with("HTTP/1.1 404"), "hex: {r:.40}");
        let _ = std::fs::remove_file(&dir);
    }
}

#[cfg(test)]
mod nav_tests {
    use super::*;
    use crate::index::Indexer;
    use lm_stratum::rpc::RpcClient;

    fn idx_with_blocks(name: &str, n: i64) -> (Indexer, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&dir);
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        for h in 0..n {
            idx.fixture_exec(&format!(
                "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought, pos)
                 VALUES ({h}, '{:064x}', 0, 1, 1.0, 0, 0, 0, {})",
                h,
                if h % 5 == 0 { 1 } else { 0 }
            ))
            .expect("fixture");
        }
        (idx, dir)
    }

    #[test]
    fn blocks_index_pages_and_links_blocks() {
        let (mut idx, dir) = idx_with_blocks("explorer-nav-blocks.db", 120);
        let r = handle(&mut idx, "GET", "/blocks");
        assert!(r.starts_with("HTTP/1.1 200"));
        assert!(r.contains("<td>119</td>"), "newest first");
        assert!(r.contains("next &raquo;") && r.contains("page=1"));
        let r2 = handle(&mut idx, "GET", "/blocks?page=1");
        assert!(r2.contains("<td>69</td>") && r2.contains("&laquo; prev"));
        let api = handle(&mut idx, "GET", "/api/blocks?page=0");
        assert!(api.starts_with("HTTP/1.1 200") && api.contains("\"per_page\":50"));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn block_and_tx_pages_carry_search_and_nav() {
        let (mut idx, dir) = idx_with_blocks("explorer-nav-block.db", 3);
        let r = handle(&mut idx, "GET", "/block/1");
        assert!(r.starts_with("HTTP/1.1 200"));
        assert!(r.contains("action=/search"), "search on block page: {r:.300}");
        assert!(r.contains("href=/block/0") && r.contains("href=/block/2"), "prev/next");
        assert!(r.contains("href=/blocks"), "back to index");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn address_history_pages_do_not_repeat_rows() {
        let (mut idx, dir) = idx_with_blocks("explorer-nav-addr.db", 1);
        let addr = format!("m{}", "1".repeat(33));
        // 150 funding txs to one address.
        idx.fixture_exec(&format!(
                "INSERT INTO txs(txid, height, coinbase) VALUES {}
                 ; INSERT INTO outs(txid, n, address, value, coinbase, spent_by, spent_n) VALUES {};",
                (0..150).map(|i| format!("('tx{i}', 0, 0)")).collect::<Vec<_>>().join(","),
                (0..150).map(|i| format!("('tx{i}', 0, '{addr}', 100, 0, NULL, NULL)")).collect::<Vec<_>>().join(","),
            ))
            .expect("fixture");
        let p0 = handle(&mut idx, "GET", &format!("/address/{addr}"));
        assert!(p0.starts_with("HTTP/1.1 200"));
        assert!(p0.contains("page 1 of 2"), "pager visible");
        assert!(p0.contains("href=\"/address/") && p0.contains("page=1"));
        let p1 = handle(&mut idx, "GET", &format!("/address/{addr}?page=1"));
        assert!(p1.contains("page 2 of 2") && p1.contains("page=0"), "prev link");
        let api0 = handle(&mut idx, "GET", &format!("/api/address/{addr}?page=1"));
        assert!(api0.contains("\"page\":1"));
        let _ = std::fs::remove_file(&dir);
    }
}
