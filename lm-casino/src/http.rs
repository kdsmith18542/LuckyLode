//! Minimal HTTP service + slot client for the casino (std only — same
//! hand-rolled HTTP style as the reference pool).
//!
//! Routes (all JSON unless noted):
//! - `GET /`              → the slot client page (both titles + verifier + cashier)
//! - `GET /api/stats`     → house stats (bankroll, exposure cap, edge buckets)
//! - `POST /api/player`   → ensure + return player session/balance
//! - `POST /api/seed`     → set the player's client seed
//! - `POST /api/rotate`   → rotate + reveal the old server seed
//! - `POST /api/deposit`  → player deposit address (creates it)
//! - `POST /api/scan`     → credit confirmed deposits
//! - `POST /api/withdraw` → withdraw to an external address
//! - `POST /api/spin`     → spin and settle
//! - `POST /api/fun/claim`→ claim the free FUN play-money allowance
//! - `POST /api/fun/spin` → spin with FUN (never touches the house ledger)
//! - `POST /api/verify`   → recompute a spin from a revealed seed
//!
//! Testnet posture (doc 14 §14.5): no auth/TLS in this reference server; put
//! it behind the site's nginx (TLS + auth) before any real value.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::bonus::{BonusEngine, BonusKind};
use crate::custody::Custody;
use crate::engine::{Casino, Game};
use crate::oracle::{Mapping, Oracle};
use crate::rpc::{self, Node};
use crate::TARGET_RTP;

/// Shared application state.
pub struct App {
    /// Engine + ledger.
    pub casino: Mutex<Casino>,
    /// Custody (deposit addresses, withdrawal policy).
    pub custody: Mutex<Custody>,
    /// Node access for deposit/withdraw.
    pub node: Node,
    /// Network label for the UI (`testnet`/`regtest`/`mainnet`).
    pub network: String,
    /// Budgeted bonus engine (owner/manager tooling).
    pub bonus: Mutex<BonusEngine>,
    /// Participation oracle (PoW/PoS/Luckynode → tiers).
    pub oracle: Mutex<Oracle>,
    /// Admin token; `None` disables the admin routes.
    pub admin_token: Option<String>,
    /// Free FUN allowance per claim (play money, in the same fixed point).
    pub fun_grant: u64,
    /// Seconds between FUN claims.
    pub fun_cooldown: u64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn err_json(status: u16, msg: &str) -> (u16, &'static str, String) {
    (
        status,
        "application/json",
        json!({ "error": msg }).to_string(),
    )
}

fn ok_json(v: Value) -> (u16, &'static str, String) {
    (200, "application/json", v.to_string())
}

/// Wall-clock unix seconds (claim cooldowns; display only).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn body_json(body: &str) -> Result<Value, (u16, &'static str, String)> {
    serde_json::from_str(body).map_err(|e| err_json(400, &format!("bad json: {e}")))
}

fn as_u64(v: &Value, key: &str) -> Result<u64, (u16, &'static str, String)> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| err_json(400, &format!("missing/invalid '{key}'")))
}

fn as_str<'a>(v: &'a Value, key: &str) -> Result<&'a str, (u16, &'static str, String)> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| err_json(400, &format!("missing/invalid '{key}'")))
}

/// Route one request. Returns `(status, content-type, body)`.
#[must_use]
pub fn handle(app: &App, method: &str, path: &str, body: &str) -> (u16, &'static str, String) {
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            (200, "text/html; charset=utf-8", CLIENT_HTML.to_string())
        }
        ("GET", "/api/stats") => {
            let c = lock(&app.casino);
            let edge = c.ledger.edge_buckets();
            ok_json(json!({
                "network": app.network,
                "rtp": TARGET_RTP,
                "bankroll": c.bankroll(),
                "bankroll_lkyl": rpc::format_micros(c.bankroll()),
                "max_bet": c.max_bet(),
                "max_bet_lkyl": rpc::format_micros(c.max_bet()),
                "total_spins": c.total_spins,
                "edge": {
                    "dev": edge.dev,
                    "pot": edge.pot,
                    "airdrop": edge.airdrop,
                    "services": edge.services,
                },
                "net_ggr": c.ledger.net_ggr().to_string(),
                "fun": {
                    "grant": app.fun_grant,
                    "grant_fun": rpc::format_fun(app.fun_grant),
                    "cooldown": app.fun_cooldown,
                    "total_spins": c.total_fun_spins,
                    "wagered_fun": rpc::format_fun(c.total_fun_wagered),
                    "won_fun": rpc::format_fun(c.total_fun_won),
                },
            }))
        }
        ("POST", "/api/player") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let mut c = lock(&app.casino);
            c.ensure_player(player);
            let mut cu = lock(&app.custody);
            let address = cu.address_for(&app.node, player).unwrap_or_default();
            drop(cu);
            let p = c.player(player).expect("just ensured");
            ok_json(player_json(
                p,
                c.balance(player),
                &address,
                app.fun_grant,
                app.fun_cooldown,
            ))
        }
        ("POST", "/api/seed") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let seed = match as_str(&req, "client_seed") {
                Ok(s) => s.to_string(),
                Err(e) => return e,
            };
            let mut c = lock(&app.casino);
            match c.set_client_seed(player, &seed) {
                Ok(()) => ok_json(json!({ "client_seed": seed })),
                Err(e) => err_json(400, &e.to_string()),
            }
        }
        ("POST", "/api/rotate") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let mut c = lock(&app.casino);
            match c.rotate(player) {
                Ok(revealed) => {
                    let p = c.player(player).expect("rotated player");
                    ok_json(json!({
                        "revealed_server_seed": revealed,
                        "new_commitment": p.commitment,
                    }))
                }
                Err(e) => err_json(400, &e.to_string()),
            }
        }
        ("POST", "/api/deposit") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            lock(&app.casino).ensure_player(player);
            let mut cu = lock(&app.custody);
            match cu.address_for(&app.node, player) {
                Ok(addr) => ok_json(json!({ "address": addr })),
                Err(e) => err_json(502, &e),
            }
        }
        ("POST", "/api/scan") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let mut c = lock(&app.casino);
            c.ensure_player(player);
            let mut cu = lock(&app.custody);
            match cu.scan(&app.node, &mut c.ledger) {
                Ok(deposits) => ok_json(json!({
                    "credited": deposits.iter().filter(|d| d.player == player)
                        .map(|d| d.micros).sum::<u64>(),
                    "balance": c.balance(player),
                })),
                Err(e) => err_json(502, &e),
            }
        }
        ("POST", "/api/withdraw") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let address = match as_str(&req, "address") {
                Ok(a) => a.to_string(),
                Err(e) => return e,
            };
            let amount = match as_str(&req, "amount")
                .and_then(|s| rpc::parse_amount_to_micros(s).map_err(|e| err_json(400, &e)))
            {
                Ok(a) => a,
                Err(e) => return e,
            };
            let mut c = lock(&app.casino);
            let mut cu = lock(&app.custody);
            match cu.withdraw(&app.node, &mut c.ledger, player, &address, amount) {
                Ok(txid) => ok_json(json!({ "txid": txid, "balance": c.balance(player) })),
                Err(e) => err_json(400, &e),
            }
        }
        ("POST", "/api/spin") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let game = match as_str(&req, "game").ok().and_then(Game::parse) {
                Some(g) => g,
                None => return err_json(400, "unknown game"),
            };
            let bet = match as_str(&req, "bet")
                .and_then(|s| rpc::parse_amount_to_micros(s).map_err(|e| err_json(400, &e)))
            {
                Ok(b) => b,
                Err(e) => return e,
            };
            let mut c = lock(&app.casino);
            if !c.players.contains_key(&player) {
                return err_json(400, "unknown player");
            }
            match c.spin(player, game, bet) {
                Ok(rec) => ok_json(json!({
                    "spin": rec,
                    "balance": c.balance(player),
                    "balance_lkyl": rpc::format_micros(c.balance(player)),
                    "win_lkyl": rpc::format_micros(rec.win),
                })),
                Err(e) => err_json(400, &e.to_string()),
            }
        }
        ("POST", "/api/fun/claim") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let mut c = lock(&app.casino);
            if !c.players.contains_key(&player) {
                return err_json(400, "unknown player");
            }
            match c.claim_fun(player, unix_now(), app.fun_grant, app.fun_cooldown) {
                Ok(fc) => ok_json(json!({
                    "granted": fc.granted,
                    "granted_fun": rpc::format_fun(fc.granted),
                    "balance": fc.balance,
                    "balance_fun": rpc::format_fun(fc.balance),
                    "next_claim_at": fc.next_claim_at,
                })),
                Err(crate::engine::CasinoError::FunCooldown(next)) => (
                    429,
                    "application/json",
                    json!({
                        "error": format!("FUN claim on cooldown — next at {next}"),
                        "next_claim_at": next,
                    })
                    .to_string(),
                ),
                Err(e) => err_json(400, &e.to_string()),
            }
        }
        ("POST", "/api/fun/spin") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let game = match as_str(&req, "game").ok().and_then(Game::parse) {
                Some(g) => g,
                None => return err_json(400, "unknown game"),
            };
            let bet = match as_str(&req, "bet")
                .and_then(|s| rpc::parse_amount_to_micros(s).map_err(|e| err_json(400, &e)))
            {
                Ok(b) => b,
                Err(e) => return e,
            };
            let mut c = lock(&app.casino);
            if !c.players.contains_key(&player) {
                return err_json(400, "unknown player");
            }
            match c.spin_fun(player, game, bet) {
                Ok(rec) => ok_json(json!({
                    "spin": rec,
                    "balance": c.fun_balance(player),
                    "balance_fun": rpc::format_fun(c.fun_balance(player)),
                    "win_fun": rpc::format_fun(rec.win),
                })),
                Err(e) => err_json(400, &e.to_string()),
            }
        }
        ("POST", "/api/verify") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let game = match as_str(&req, "game").ok().and_then(Game::parse) {
                Some(g) => g,
                None => return err_json(400, "unknown game"),
            };
            let server_seed = match as_str(&req, "server_seed") {
                Ok(s) => s.to_string(),
                Err(e) => return e,
            };
            let client_seed = match as_str(&req, "client_seed") {
                Ok(s) => s.to_string(),
                Err(e) => return e,
            };
            let nonce = match as_u64(&req, "nonce") {
                Ok(n) => n,
                Err(e) => return e,
            };
            let bet = match as_str(&req, "bet")
                .and_then(|s| rpc::parse_amount_to_micros(s).map_err(|e| err_json(400, &e)))
            {
                Ok(b) => b,
                Err(e) => return e,
            };
            let c = lock(&app.casino);
            match c.replay(game, &server_seed, &client_seed, nonce) {
                Ok(symbols) => {
                    let win = c.outcome_win(game, &symbols, bet);
                    // Also confirm the seeds hash to the commitment the player saw.
                    let commitment = crate::fair::from_hex(&server_seed)
                        .map(|b| crate::fair::to_hex(&crate::fair::commitment(&b)))
                        .unwrap_or_default();
                    ok_json(json!({
                        "symbols": symbols,
                        "win": win,
                        "win_lkyl": rpc::format_micros(win),
                        "commitment": commitment,
                    }))
                }
                Err(e) => err_json(400, &e),
            }
        }
        ("POST", "/api/tier") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let o = lock(&app.oracle);
            let participation = o.participation(player);
            let tier = crate::tier::tier_for(&participation);
            ok_json(json!({
                "player": player,
                "score": crate::tier::participation_score(&participation),
                "participation": {
                    "pow_shares": participation.pow_shares,
                    "pos_stake_lkyl": rpc::format_micros(participation.pos_stake_micros),
                    "luckynode_service_days": participation.luckynode_service_days,
                },
                "tier": {
                    "level": tier.level,
                    "rakeback_permille": tier.rakeback_permille,
                    "bonus_free_spins": tier.bonus_free_spins,
                    "tournaments": tier.tournaments,
                },
            }))
        }
        ("POST", "/api/admin/award") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            if let Err(e) = admin_ok(app, &req) {
                return e;
            }
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let micros = match as_str(&req, "amount")
                .and_then(|s| rpc::parse_amount_to_micros(s).map_err(|e| err_json(400, &e)))
            {
                Ok(m) => m,
                Err(e) => return e,
            };
            let reason = req
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("manual bonus")
                .to_string();
            let mut c = lock(&app.casino);
            let mut b = lock(&app.bonus);
            if !c.players.contains_key(&player) {
                return err_json(400, "unknown player");
            }
            if b.remaining() < micros {
                return err_json(400, "bonus budget exhausted");
            }
            if c.bankroll() < micros {
                return err_json(400, "bankroll cannot cover the bonus");
            }
            if let Err(e) = c.ledger.grant(player, micros) {
                return err_json(400, &format!("{e:?}"));
            }
            let award = b
                .award(player, BonusKind::Manual, micros, reason)
                .expect("prechecked budget");
            ok_json(json!({
                "award": award,
                "remaining": b.remaining(),
                "balance": c.balance(player),
            }))
        }
        ("POST", "/api/admin/award-top") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            if let Err(e) = admin_ok(app, &req) {
                return e;
            }
            let kind = match as_str(&req, "kind") {
                Ok("miners") => BonusKind::TopMiner,
                Ok("spenders") => BonusKind::TopSpender,
                Ok(_) => return err_json(400, "kind must be miners|spenders"),
                Err(e) => return e,
            };
            let n = match as_u64(&req, "n") {
                Ok(n) => n as usize,
                Err(e) => return e,
            };
            let micros_each = match as_str(&req, "amount")
                .and_then(|s| rpc::parse_amount_to_micros(s).map_err(|e| err_json(400, &e)))
            {
                Ok(m) => m,
                Err(e) => return e,
            };
            let reason = req
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("top performer bonus")
                .to_string();
            let entries = if kind == BonusKind::TopMiner {
                let o = lock(&app.oracle);
                o.mappings
                    .keys()
                    .map(|p| (*p, o.participation(*p).pow_shares))
                    .collect::<Vec<_>>()
            } else {
                let c = lock(&app.casino);
                c.players.iter().map(|(p, s)| (*p, s.wagered)).collect()
            };
            let recipients = BonusEngine::top_n(&entries, n);
            let need = micros_each.saturating_mul(recipients.len() as u64);
            let mut c = lock(&app.casino);
            let mut b = lock(&app.bonus);
            if need == 0 {
                return err_json(400, "no eligible recipients");
            }
            if need > b.remaining() {
                return err_json(400, "bonus budget exhausted");
            }
            if need > c.bankroll() {
                return err_json(400, "bankroll cannot cover the bonuses");
            }
            let awards = b
                .award_top(kind, &entries, n, micros_each, reason)
                .expect("prechecked budget");
            for a in &awards {
                c.ledger
                    .grant(a.player, a.micros)
                    .expect("prechecked bankroll");
            }
            ok_json(json!({
                "awarded": awards,
                "remaining": b.remaining(),
            }))
        }
        ("POST", "/api/admin/log") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            if let Err(e) = admin_ok(app, &req) {
                return e;
            }
            let b = lock(&app.bonus);
            ok_json(json!({ "log": b.log(), "remaining": b.remaining() }))
        }
        ("POST", "/api/admin/map") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            if let Err(e) = admin_ok(app, &req) {
                return e;
            }
            let player = match as_u64(&req, "player") {
                Ok(p) => p,
                Err(e) => return e,
            };
            let worker = match as_str(&req, "worker") {
                Ok(w) => w.to_string(),
                Err(e) => return e,
            };
            let stake_micros = match req.get("stake").and_then(Value::as_str) {
                Some(s) => match rpc::parse_amount_to_micros(s) {
                    Ok(m) => m,
                    Err(e) => return err_json(400, &e),
                },
                None => 0,
            };
            let luckynode_id = req
                .get("luckynode")
                .and_then(Value::as_str)
                .map(str::to_string);
            lock(&app.oracle).set_mapping(
                player,
                Mapping {
                    worker,
                    stake_micros,
                    luckynode_id,
                },
            );
            ok_json(json!({ "player": player, "mapped": true }))
        }
        ("POST", "/api/admin/state") => {
            let req = match body_json(body) {
                Ok(v) => v,
                Err(e) => return e,
            };
            if let Err(e) = admin_ok(app, &req) {
                return e;
            }
            let c = lock(&app.casino);
            let b = lock(&app.bonus);
            let o = lock(&app.oracle);
            let mappings: Vec<Value> = o
                .mappings
                .iter()
                .map(|(p, m)| {
                    json!({
                        "player": p,
                        "worker": m.worker,
                        "stake_lkyl": rpc::format_micros(m.stake_micros),
                        "luckynode": m.luckynode_id,
                    })
                })
                .collect();
            ok_json(json!({
                "bankroll_lkyl": rpc::format_micros(c.bankroll()),
                "max_bet_lkyl": rpc::format_micros(c.max_bet()),
                "total_spins": c.total_spins,
                "bonus_remaining_lkyl": rpc::format_micros(b.remaining()),
                "bonus_awards": b.log().len(),
                "mappings": mappings,
                "players": c.players.len(),
            }))
        }
        _ => err_json(404, "not found"),
    }
}

fn admin_ok(app: &App, req: &Value) -> Result<(), (u16, &'static str, String)> {
    let Some(expected) = &app.admin_token else {
        return Err(err_json(403, "admin disabled (start with --admin-token)"));
    };
    match req.get("token").and_then(Value::as_str) {
        Some(t) if t == expected => Ok(()),
        _ => Err(err_json(403, "bad admin token")),
    }
}

fn player_json(
    p: &crate::engine::Player,
    balance: u64,
    address: &str,
    fun_grant: u64,
    fun_cooldown: u64,
) -> Value {
    let fun_next_claim_at = if p.fun_last_claim == 0 {
        0
    } else {
        p.fun_last_claim.saturating_add(fun_cooldown)
    };
    json!({
        "player": p.id,
        "balance": balance,
        "balance_lkyl": rpc::format_micros(balance),
        "commitment": p.commitment,
        "client_seed": p.client_seed,
        "nonce": p.nonce,
        "spins": p.spins,
        "wagered_lkyl": rpc::format_micros(p.wagered),
        "won_lkyl": rpc::format_micros(p.won),
        "deposit_address": address,
        "last": p.last,
        "fun": p.fun,
        "fun_balance": rpc::format_fun(p.fun),
        "fun_spins": p.fun_spins,
        "fun_wagered_fun": rpc::format_fun(p.fun_wagered),
        "fun_won_fun": rpc::format_fun(p.fun_won),
        "fun_claimed_fun": rpc::format_fun(p.fun_claimed),
        "fun_last_claim": p.fun_last_claim,
        "fun_next_claim_at": fun_next_claim_at,
        "fun_grant_fun": rpc::format_fun(fun_grant),
        "fun_cooldown": fun_cooldown,
        "fun_last": p.fun_last,
    })
}

/// Bind the service.
///
/// # Errors
/// `Err` if the address cannot be bound.
pub fn bind(addr: &str) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr)
}

/// Serve connections forever (one thread per connection).
pub fn serve(listener: TcpListener, app: Arc<App>) -> ! {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let app = Arc::clone(&app);
        std::thread::spawn(move || {
            let _ = serve_one(stream, &app);
        });
    }
    unreachable!("listener loop never ends");
}

fn serve_one(mut stream: TcpStream, app: &App) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some((method, path, body)) = read_request(&mut stream) else {
        return Ok(());
    };
    let (status, ctype, resp) = handle(app, &method, &path, &body);
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        reason(status),
        resp.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let want = content_length(&head);
            let mut body = buf[pos + 4..].to_vec();
            while body.len() < want {
                let n = stream.read(&mut tmp).ok()?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or("");
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or("GET").to_string();
            let path = parts.next().unwrap_or("/").to_string();
            return Some((method, path, String::from_utf8_lossy(&body).to_string()));
        }
        if buf.len() > 1 << 20 {
            return None;
        }
    }
    None
}

fn content_length(head: &str) -> usize {
    for line in head.lines() {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            return v.trim().parse().unwrap_or(0);
        }
    }
    0
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The client page: two titles, a provably-fair verifier and a cashier.
const CLIENT_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Lucky Lode — Casino</title>
<style>
:root{--bg:#0d0f14;--panel:#161a23;--ink:#e8e6df;--dim:#9aa0ad;--gold:#e9b949;--line:#262c38}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:14px/1.5 system-ui,sans-serif}
header{display:flex;gap:8px;align-items:center;padding:10px 14px;border-bottom:1px solid var(--line);flex-wrap:wrap}
h1{font-size:16px;margin:0;color:var(--gold);letter-spacing:.5px}
.tabs button{background:var(--panel);color:var(--dim);border:1px solid var(--line);padding:6px 12px;border-radius:6px;cursor:pointer}
.tabs button.on{color:var(--ink);border-color:var(--gold)}
main{max-width:860px;margin:0 auto;padding:16px}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:10px;padding:14px;margin-bottom:14px}
.row{display:flex;gap:10px;flex-wrap:wrap;align-items:center}
label{color:var(--dim);font-size:12px}
input,select{background:#0d0f14;color:var(--ink);border:1px solid var(--line);border-radius:6px;padding:6px 8px;min-width:120px}
button.act{background:var(--gold);color:#181203;border:0;border-radius:6px;padding:8px 14px;font-weight:600;cursor:pointer}
button.ghost{background:transparent;color:var(--dim);border:1px solid var(--line);border-radius:6px;padding:6px 10px;cursor:pointer}
.reels{display:flex;gap:8px;justify-content:center;margin:14px 0;min-height:96px;align-items:center}
.reel{display:flex;flex-direction:column;gap:6px}
.sym{background:#0d0f14;border:1px solid var(--line);border-radius:8px;padding:8px 10px;min-width:64px;text-align:center;font-size:12px}
.sym.hi{border-color:var(--gold);color:var(--gold)}
.sym.sc{border-color:#5ac8fa;color:#5ac8fa}
.win{color:var(--gold);font-weight:700;font-size:18px;text-align:center}
.dim{color:var(--dim)}code{background:#0d0f14;padding:2px 5px;border-radius:4px;font-size:12px;word-break:break-all}
.hide{display:none}
pre{background:#0d0f14;border:1px solid var(--line);border-radius:8px;padding:10px;overflow:auto;font-size:12px}
.err{color:#ff6b6b}.ok{color:#7bd88f}
</style></head><body>
<header>
  <h1>LUCKY LODE</h1><span class="dim" id="net"></span>
  <div class="tabs" style="margin-left:auto">
    <button data-tab="lode" class="on">Lucky Lode</button>
    <button data-tab="ways">Lucky Lode Ways</button>
    <button data-tab="fun">FUN (free)</button>
    <button data-tab="fair">Provably fair</button>
    <button data-tab="cash">Cashier</button>
    <button data-tab="house">House</button>
  </div>
</header>
<main>
  <div class="panel row">
    <label>Player id <input id="pid" type="number" value="1" style="min-width:80px"></label>
    <button class="ghost" id="load">Load</button>
    <span class="dim">Balance</span><b id="bal">—</b>
    <span class="dim">Nonce</span><b id="nonce">—</b>
    <span class="dim">Commitment</span><code id="commit">—</code>
    <span class="dim">FUN</span><b id="funbal">—</b>
  </div>

  <section id="tab-lode" class="panel">
    <div class="row">
      <label>Bet (LKYL) <input id="bet-lode" value="0.01"></label>
      <button class="act" id="spin-lode">SPIN</button>
      <span class="dim">5×3 · 10 lines · 91% RTP · max ×8000</span>
    </div>
    <div class="reels" id="reels-lode"></div>
    <div class="win" id="win-lode"></div>
  </section>

  <section id="tab-ways" class="panel hide">
    <div class="row">
      <label>Bet (LKYL) <input id="bet-ways" value="0.01"></label>
      <button class="act" id="spin-ways">SPIN</button>
      <span class="dim">6 reels · up to 117,649 ways · 91% RTP</span>
    </div>
    <div class="reels" id="reels-ways"></div>
    <div class="win" id="win-ways"></div>
  </section>

  <section id="tab-fun" class="panel hide">
    <div class="row">
      <b style="color:var(--gold)">FUN</b>
      <span class="dim">free play money — no value, never on-chain, not withdrawable</span>
      <span class="dim">Balance</span><b id="funbal2">—</b>
      <button class="act" id="claimfun">Claim free FUN</button>
    </div>
    <div class="row" style="margin-top:10px">
      <label>Game <select id="fgame"><option value="lucky-lode">Lucky Lode</option><option value="lucky-lode-ways">Lucky Lode Ways</option></select></label>
      <label>Bet (FUN) <input id="fbet" value="10" style="min-width:90px"></label>
      <button class="act" id="fspin">SPIN (FUN)</button>
      <span class="dim">same provably-fair RNG · same 91% RTP</span>
    </div>
    <div class="reels" id="freels"></div>
    <div class="win" id="fwin"></div>
    <div id="fnote" class="dim"></div>
  </section>

  <section id="tab-fair" class="panel hide">
    <p class="dim">Every spin is provably fair: outcomes come from
    <code>HMAC-SHA256(server_seed, client_seed ‖ nonce ‖ cursor)</code>. The hash
    of the server seed is published before play; rotating reveals the seed so
    you can recompute every spin.</p>
    <div class="row">
      <label>Client seed <input id="cseed" style="min-width:220px"></label>
      <button class="ghost" id="saveseed">Save</button>
      <button class="ghost" id="rotate">Reveal &amp; rotate seed</button>
    </div>
    <div id="reveal" class="dim"></div>
    <hr style="border-color:var(--line)">
    <div class="row">
      <label>Game <select id="vgame"><option value="lucky-lode">Lucky Lode</option><option value="lucky-lode-ways">Lucky Lode Ways</option></select></label>
      <label>Server seed <input id="vseed" style="min-width:220px"></label>
      <label>Client seed <input id="vcseed" style="min-width:160px"></label>
      <label>Nonce <input id="vnonce" type="number" value="0" style="min-width:70px"></label>
      <label>Bet <input id="vbet" value="0.01" style="min-width:80px"></label>
      <button class="ghost" id="verify">Verify</button>
    </div>
    <pre id="vout" class="hide"></pre>
  </section>

  <section id="tab-cash" class="panel hide">
    <div class="row"><label>Deposit address</label><code id="addr">—</code>
      <button class="ghost" id="mkaddr">Get address</button>
      <button class="ghost" id="scan">Check deposits</button></div>
    <div class="row" style="margin-top:10px">
      <label>Withdraw to <input id="waddr" style="min-width:260px"></label>
      <label>Amount (LKYL) <input id="wamt" value="0.1" style="min-width:90px"></label>
      <button class="ghost" id="withdraw">Withdraw</button>
    </div>
    <pre id="cashout" class="hide"></pre>
  </section>

  <section id="tab-house" class="panel hide">
    <div class="row"><b>House</b><span class="dim">edge flow · bonuses · mappings</span></div>
    <div id="house-stats" class="dim" style="margin:8px 0"></div>
    <div class="row"><label>Admin token <input id="atok" style="min-width:170px"></label>
      <button class="ghost" id="hstate">Refresh</button>
      <button class="ghost" id="hlog">Show bonus log</button></div>
    <div class="row" style="margin-top:8px">
      <label>Player <input id="ap" type="number" value="1" style="min-width:70px"></label>
      <label>Amount (LKYL) <input id="aamt" value="1" style="min-width:80px"></label>
      <label>Reason <input id="areason" value="exclusive bonus" style="min-width:150px"></label>
      <button class="ghost" id="award">Award</button>
    </div>
    <div class="row" style="margin-top:8px">
      <label>Top <select id="atop"><option value="miners">miners</option><option value="spenders">spenders</option></select></label>
      <label>N <input id="an" type="number" value="3" style="min-width:60px"></label>
      <label>Each (LKYL) <input id="aeach" value="0.5" style="min-width:80px"></label>
      <button class="ghost" id="awardtop">Award top</button>
    </div>
    <div class="row" style="margin-top:8px">
      <label>Map player <input id="mp" type="number" value="1" style="min-width:60px"></label>
      <label>Worker <input id="mw" value="j1" style="min-width:100px"></label>
      <label>Stake (LKYL) <input id="ms" value="0" style="min-width:80px"></label>
      <label>Luckynode id <input id="ml" style="min-width:110px"></label>
      <button class="ghost" id="map">Map</button>
    </div>
    <pre id="hout" class="hide"></pre>
  </section>

  <div id="status" class="dim"></div>
</main>
<script>
const SYMS=["spark","glint","flare","ember","cinder","bonfire","beacon","comet","aurora","mythic","lodestone 📍","almanac 📖"];
const $=id=>document.getElementById(id);
const pid=()=>parseInt($("pid").value||"1",10);
let me=null;
function status(t,ok){$("status").innerHTML=`<span class="${ok?'ok':'err'}">${t||""}</span>`;}
async function api(path,payload){
  const r=await fetch(path,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(payload||{})});
  const j=await r.json();
  if(!r.ok||j.error) throw new Error(j.error||r.statusText);
  return j;
}
function render(el,symbols){
  el.innerHTML="";
  symbols.forEach(reel=>{
    const d=document.createElement("div");d.className="reel";
    reel.forEach(s=>{const b=document.createElement("div");b.className="sym"+(s>=10?" hi":(s===11?" sc":""));b.textContent=SYMS[s]||s;d.appendChild(b);});
    el.appendChild(d);
  });
}
async function loadPlayer(){
  const p=await api("/api/player",{player:pid()});
  me=p;$("bal").textContent=p.balance_lkyl+" LKYL";$("nonce").textContent=p.nonce;
  $("commit").textContent=p.commitment;$("cseed").value=p.client_seed;
  setFun(p.fun_balance);
  if(p.deposit_address)$("addr").textContent=p.deposit_address;
  status("loaded player "+p.player,true);
}
function setFun(b){if(b===undefined||b===null)return;$("funbal").textContent=b+" FUN";$("funbal2").textContent=b+" FUN";}
async function claimFun(){
  try{const r=await api("/api/fun/claim",{player:pid()});
    setFun(r.balance_fun);
    $("fnote").textContent="claimed "+r.granted_fun+" FUN — next claim "+new Date(r.next_claim_at*1000).toLocaleTimeString();
    status("claimed "+r.granted_fun+" free FUN",true);}
  catch(e){status(e.message,false);}
}
async function funSpin(){
  try{
    const res=await api("/api/fun/spin",{player:pid(),game:$("fgame").value,bet:$("fbet").value});
    render($("freels"),res.spin.symbols);
    $("fwin").textContent=res.spin.win>0?("WIN "+res.win_fun+" FUN"):"no win";
    setFun(res.balance_fun);
    status("FUN spin #"+res.spin.nonce+" (free play — no real value)",true);
  }catch(e){status(e.message,false);}
}
async function spin(game,els){
  try{
    const bet=$(els.bet).value;
    const res=await api("/api/spin",{player:pid(),game,bet});
    render($(els.reels),res.spin.symbols);
    $(els.win).textContent=res.spin.win>0?("WIN "+res.win_lkyl+" LKYL"):"no win";
    $("bal").textContent=res.balance_lkyl+" LKYL";$("nonce").textContent=res.spin.nonce+1;
    status("spin #"+res.spin.nonce+" verified client seed "+res.spin.client_seed,true);
  }catch(e){status(e.message,false);}
}
$("load").onclick=()=>loadPlayer().catch(e=>status(e.message,false));
$("spin-lode").onclick=()=>spin("lucky-lode",{bet:"bet-lode",reels:"reels-lode",win:"win-lode"});
$("spin-ways").onclick=()=>spin("lucky-lode-ways",{bet:"bet-ways",reels:"reels-ways",win:"win-ways"});
$("claimfun").onclick=claimFun;
$("fspin").onclick=funSpin;
$("saveseed").onclick=async()=>{try{await api("/api/seed",{player:pid(),client_seed:$("cseed").value});status("client seed saved",true);}catch(e){status(e.message,false);}};
$("rotate").onclick=async()=>{try{const r=await api("/api/rotate",{player:pid()});$("reveal").innerHTML="revealed server seed: <code>"+r.revealed_server_seed+"</code> · new commitment <code>"+r.new_commitment+"</code>";$("vseed").value=r.revealed_server_seed;status("seed rotated — old seed revealed",true);await loadPlayer();}catch(e){status(e.message,false);}};
$("verify").onclick=async()=>{try{
  const r=await api("/api/verify",{game:$("vgame").value,server_seed:$("vseed").value,client_seed:$("vcseed").value,nonce:parseInt($("vnonce").value||"0",10),bet:$("vbet").value});
  $("vout").classList.remove("hide");
  $("vout").textContent="symbols: "+JSON.stringify(r.symbols)+"\nwin: "+r.win_lkyl+" LKYL\ncommitment: "+r.commitment;
}catch(e){status(e.message,false);}};
$("mkaddr").onclick=async()=>{try{const r=await api("/api/deposit",{player:pid()});$("addr").textContent=r.address;status("send LKYL to "+r.address,true);}catch(e){status(e.message,false);}};
$("scan").onclick=async()=>{try{const r=await api("/api/scan",{player:pid()});status("credited "+r.credited+" µLKYL",true);await loadPlayer();}catch(e){status(e.message,false);}};
$("withdraw").onclick=async()=>{try{const r=await api("/api/withdraw",{player:pid(),address:$("waddr").value,amount:$("wamt").value});$("cashout").classList.remove("hide");$("cashout").textContent="txid: "+r.txid;status("withdrawal sent",true);await loadPlayer();}catch(e){status(e.message,false);}};
document.querySelectorAll(".tabs button").forEach(b=>b.onclick=()=>{
  document.querySelectorAll(".tabs button").forEach(x=>x.classList.toggle("on",x===b));
  ["lode","ways","fun","fair","cash","house"].forEach(t=>$("tab-"+t).classList.toggle("hide",t!==b.dataset.tab));
});
async function adminPost(path,payload){payload.token=$("atok").value;
  const r=await fetch(path,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(payload)});
  const j=await r.json();if(!r.ok||j.error)throw new Error(j.error||r.statusText);return j;}
$("hstate").onclick=async()=>{try{const j=await adminPost("/api/admin/state",{});$("hout").classList.remove("hide");$("hout").textContent=JSON.stringify(j,null,2);status("admin state loaded",true);}catch(e){status(e.message,false);}};
$("hlog").onclick=async()=>{try{const j=await adminPost("/api/admin/log",{});$("hout").classList.remove("hide");$("hout").textContent=JSON.stringify(j,null,2);status("bonus log loaded",true);}catch(e){status(e.message,false);}};
$("award").onclick=async()=>{try{const j=await adminPost("/api/admin/award",{player:parseInt($("ap").value,10),amount:$("aamt").value,reason:$("areason").value});status("awarded "+j.award.micros+" µLKYL to player "+j.award.player,true);}catch(e){status(e.message,false);}};
$("awardtop").onclick=async()=>{try{const j=await adminPost("/api/admin/award-top",{kind:$("atop").value,n:parseInt($("an").value,10),amount:$("aeach").value});status("awarded "+j.awarded.length+" bonuses",true);}catch(e){status(e.message,false);}};
$("map").onclick=async()=>{try{await adminPost("/api/admin/map",{player:parseInt($("mp").value,10),worker:$("mw").value,stake:$("ms").value,luckynode:$("ml").value||undefined});status("mapped player "+$("mp").value,true);}catch(e){status(e.message,false);}};
fetch("/api/stats").then(r=>r.json()).then(s=>{$("net").textContent=s.network+" · RTP "+(s.rtp*100).toFixed(0)+"% · bankroll "+s.bankroll_lkyl+" LKYL";
  const e=s.edge;$("house-stats").innerHTML="edge → dev "+e.dev+" · pot "+e.pot+" · airdrop "+e.airdrop+" · services "+e.services+" µLKYL · spins "+s.total_spins+" · max bet "+s.max_bet_lkyl+" LKYL";}).catch(()=>{});
loadPlayer().catch(()=>{});
</script></body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::MockNode;
    use std::net::SocketAddr;

    fn app() -> Arc<App> {
        let mock = MockNode::new();
        mock.set("getnewaddress", json!("addr-test"));
        mock.set("getreceivedbyaddress", json!(0.0));
        mock.set("sendtoaddress", json!("txid-test"));
        Arc::new(App {
            casino: Mutex::new(Casino::new(1_000 * crate::engine::LKYL)),
            custody: Mutex::new(Custody::new(1, 1_000, 1_000_000_000)),
            node: Node::from_api(Box::new(mock)),
            network: "testnet".to_string(),
            bonus: Mutex::new(BonusEngine::new(10 * crate::engine::LKYL)),
            oracle: Mutex::new(Oracle::disabled()),
            admin_token: Some("secret".to_string()),
            fun_grant: 1_000 * crate::engine::LKYL,
            fun_cooldown: 3_600,
        })
    }

    fn post(app: &App, path: &str, body: Value) -> (u16, Value) {
        let (s, _, b) = handle(app, "POST", path, &body.to_string());
        (s, serde_json::from_str(&b).unwrap_or(Value::Null))
    }

    #[test]
    fn index_and_stats_render() {
        let a = app();
        let (s, ct, body) = handle(&a, "GET", "/", "");
        assert_eq!(s, 200);
        assert!(ct.starts_with("text/html"));
        assert!(body.contains("Lucky Lode"));
        let (s, _, body) = handle(&a, "GET", "/api/stats", "");
        assert_eq!(s, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!((v["rtp"].as_f64().unwrap() - TARGET_RTP).abs() < 1e-9);
        assert_eq!(v["network"], "testnet");
    }

    #[test]
    fn player_deposit_spin_flow_over_http() {
        let a = app();
        let (s, p) = post(&a, "/api/player", json!({"player": 1}));
        assert_eq!(s, 200);
        assert_eq!(p["deposit_address"], "addr-test");
        let (s, d) = post(&a, "/api/deposit", json!({"player": 1}));
        assert_eq!(s, 200);
        assert_eq!(d["address"], "addr-test");
        // Fund the player directly (no chain), then spin.
        lock(&a.casino)
            .ledger
            .deposit(1, 10 * crate::engine::LKYL)
            .unwrap();
        let (s, r) = post(
            &a,
            "/api/spin",
            json!({"player": 1, "game": "lucky-lode", "bet": "0.01"}),
        );
        assert_eq!(s, 200, "{r}");
        assert!(r["spin"]["symbols"].is_array());
        assert_eq!(r["spin"]["bet"].as_u64(), Some(10_000));
        // Unknown player and bad game are refused.
        assert_eq!(
            post(
                &a,
                "/api/spin",
                json!({"player": 42, "game": "lucky-lode", "bet": "0.01"})
            )
            .0,
            400
        );
        assert_eq!(
            post(
                &a,
                "/api/spin",
                json!({"player": 1, "game": "nope", "bet": "0.01"})
            )
            .0,
            400
        );
    }

    #[test]
    fn fun_claim_and_spin_over_http() {
        let a = app();
        let (s, p) = post(&a, "/api/player", json!({"player": 7}));
        assert_eq!(s, 200);
        assert_eq!(p["fun_balance"], "0.00");
        assert_eq!(p["fun_grant_fun"], "1000.00");
        // Claim: free FUN, then a 429 inside the cooldown.
        let (s, c) = post(&a, "/api/fun/claim", json!({"player": 7}));
        assert_eq!(s, 200);
        assert_eq!(c["granted_fun"], "1000.00");
        assert_eq!(c["balance_fun"], "1000.00");
        let (s, c) = post(&a, "/api/fun/claim", json!({"player": 7}));
        assert_eq!(s, 429);
        assert!(c["next_claim_at"].as_u64().unwrap() > 0);
        // Play with FUN: the real balance stays untouched.
        let (s, r) = post(
            &a,
            "/api/fun/spin",
            json!({"player": 7, "game": "lucky-lode", "bet": "10"}),
        );
        assert_eq!(s, 200, "{r}");
        assert!(r["spin"]["symbols"].is_array());
        assert_eq!(r["spin"]["bet"].as_u64(), Some(10 * crate::engine::LKYL));
        assert!(r["balance_fun"].is_string());
        let (s, p) = post(&a, "/api/player", json!({"player": 7}));
        assert_eq!(s, 200);
        assert_eq!(p["balance"].as_u64(), Some(0));
        assert_eq!(p["spins"].as_u64(), Some(0));
        assert_eq!(p["fun_spins"].as_u64(), Some(1));
        assert!(p["fun_last"].is_object());
        // Unknown player is refused; a line bet must divide by the lines.
        assert_eq!(post(&a, "/api/fun/claim", json!({"player": 99})).0, 400);
        assert_eq!(
            post(
                &a,
                "/api/fun/spin",
                json!({"player": 7, "game": "lucky-lode", "bet": "0.000001"})
            )
            .0,
            400
        );
        let (_, _, body) = handle(&a, "GET", "/api/stats", "");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["fun"]["grant_fun"], "1000.00");
        assert_eq!(v["fun"]["cooldown"].as_u64(), Some(3_600));
        assert_eq!(v["fun"]["total_spins"].as_u64(), Some(1));
    }

    #[test]
    fn verify_endpoint_reproduces_a_spin() {
        let a = app();
        let mut c = lock(&a.casino);
        c.ensure_player(1);
        c.ledger.deposit(1, 10 * crate::engine::LKYL).unwrap();
        let rec = c.spin(1, Game::LuckyLode, 10_000).unwrap();
        let seed = c.player(1).unwrap().server_seed.clone();
        drop(c);
        let (s, v) = post(
            &a,
            "/api/verify",
            json!({"game": "lucky-lode", "server_seed": seed, "client_seed": rec.client_seed, "nonce": rec.nonce, "bet": "0.01"}),
        );
        assert_eq!(s, 200);
        assert_eq!(v["win"].as_u64(), Some(rec.win));
    }

    #[test]
    fn withdraw_and_scan_endpoints() {
        let a = app();
        post(&a, "/api/player", json!({"player": 1}));
        lock(&a.casino)
            .ledger
            .deposit(1, 10 * crate::engine::LKYL)
            .unwrap();
        let (s, r) = post(
            &a,
            "/api/withdraw",
            json!({"player": 1, "address": "LKYLdest", "amount": "1"}),
        );
        assert_eq!(s, 200, "{r}");
        assert_eq!(r["txid"], "txid-test");
        let (s, sc) = post(&a, "/api/scan", json!({"player": 1}));
        assert_eq!(s, 200);
        assert_eq!(sc["credited"].as_u64(), Some(0));
    }

    #[test]
    fn rotation_endpoint_reveals_seed() {
        let a = app();
        post(&a, "/api/player", json!({"player": 1}));
        let (s, r) = post(&a, "/api/rotate", json!({"player": 1}));
        assert_eq!(s, 200);
        let revealed = r["revealed_server_seed"].as_str().unwrap();
        assert_eq!(revealed.len(), 64);
        assert_eq!(r["new_commitment"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn http_round_trip_over_a_socket() {
        let listener = bind("127.0.0.1:0").expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        let app = app();
        std::thread::spawn(move || serve(listener, app));
        let mut s = TcpStream::connect(addr).expect("connect");
        s.write_all(b"GET /api/stats HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 200 OK"), "{out}");
        assert!(out.contains("\"rtp\":0.91"), "{out}");
    }

    #[test]
    fn tier_endpoint_uses_oracle_mapping() {
        let a = app();
        let (s, t) = post(&a, "/api/tier", json!({"player": 1}));
        assert_eq!(s, 200);
        assert_eq!(t["tier"]["level"].as_u64(), Some(0));
        // Mapping with attested stake lifts the tier even with sources off.
        let (s, _) = post(
            &a,
            "/api/admin/map",
            json!({"token": "secret", "player": 1, "worker": "j1", "stake": "5000"}),
        );
        assert_eq!(s, 200);
        let (_, t) = post(&a, "/api/tier", json!({"player": 1}));
        assert!(t["tier"]["level"].as_u64().unwrap() >= 1, "{t}");
    }

    #[test]
    fn admin_routes_require_the_token() {
        let a = app();
        assert_eq!(
            post(&a, "/api/admin/award", json!({"player": 1, "amount": "1"})).0,
            403
        );
        assert_eq!(
            post(
                &a,
                "/api/admin/award",
                json!({"token": "wrong", "player": 1, "amount": "1"})
            )
            .0,
            403
        );
        assert_eq!(
            post(&a, "/api/admin/log", json!({"token": "secret"})).0,
            200
        );
    }

    #[test]
    fn admin_award_moves_bankroll_to_player_and_logs() {
        let a = app();
        post(&a, "/api/player", json!({"player": 1}));
        let bankroll_before = lock(&a.casino).bankroll();
        let (s, r) = post(
            &a,
            "/api/admin/award",
            json!({"token": "secret", "player": 1, "amount": "2", "reason": "top miner"}),
        );
        assert_eq!(s, 200, "{r}");
        assert_eq!(r["award"]["micros"].as_u64(), Some(2_000_000));
        assert_eq!(r["balance"].as_u64(), Some(2_000_000));
        {
            let c = lock(&a.casino);
            assert_eq!(c.bankroll(), bankroll_before - 2_000_000);
            assert!(c.ledger.is_conserving());
        }
        // Budget is 10 LKYL: 2 + 8 spends it; the next award is refused.
        assert_eq!(
            post(
                &a,
                "/api/admin/award",
                json!({"token": "secret", "player": 1, "amount": "8"})
            )
            .0,
            200
        );
        assert_eq!(
            post(
                &a,
                "/api/admin/award",
                json!({"token": "secret", "player": 1, "amount": "1"})
            )
            .0,
            400
        );
        let (s, l) = post(&a, "/api/admin/log", json!({"token": "secret"}));
        assert_eq!(s, 200);
        assert_eq!(l["log"].as_array().unwrap().len(), 2);
        assert_eq!(l["remaining"].as_u64(), Some(0));
    }

    #[test]
    fn admin_award_top_picks_the_biggest_spender() {
        let a = app();
        {
            let mut c = lock(&a.casino);
            for p in [1u64, 2] {
                c.ensure_player(p);
                c.ledger.deposit(p, 10 * crate::engine::LKYL).unwrap();
            }
            c.spin(1, Game::LuckyLode, 40_000).unwrap();
            c.spin(2, Game::LuckyLode, 90_000).unwrap();
        }
        let (s, r) = post(
            &a,
            "/api/admin/award-top",
            json!({"token": "secret", "kind": "spenders", "n": 1, "amount": "1"}),
        );
        assert_eq!(s, 200, "{r}");
        let awarded = r["awarded"].as_array().unwrap();
        assert_eq!(awarded.len(), 1);
        assert_eq!(awarded[0]["player"].as_u64(), Some(2));
        assert!(lock(&a.casino).ledger.is_conserving());
    }
}
