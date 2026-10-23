//! Tip-following chain indexer (SQLite): blocks, transactions, address
//! balances — everything the explorer UI/API serves, built from plain
//! node RPC (`getblock` verbose + `getjackpotinfo`). No `-txindex`
//! needed: the index is a full walk from genesis, then tip-following.
//!
//! Money rules: an output is *unspent* until a later indexed input
//! references it (`spent_by`/`spent_n`). Address balance = sum of
//! unspent outputs. Supply = sum of all UNSPENT outputs (this chain
//! burns fees, so the minted-coinbase total overstates it; the unspent
//! sum matches the node's `coinsupply` to the satoshi).

use lm_stratum::rpc::RpcClient;
use lm_stratum::tls;
use rusqlite::{params, Connection};

use std::io::{Read, Write};
use std::net::TcpStream;

pub struct Indexer {
    db: Connection,
    rpc: RpcClient,
    /// Optional price feed (JSON URL) for the market page. None = no
    /// price source; the market view then shows supply/liquidity only
    /// and says so plainly rather than inventing a price.
    price_url: Option<String>,
    price_source: Option<String>,
    /// Cached price feed result: (price_per_coin, fetched_at, source).
    price_cache: Option<(f64, i64, String)>,
    /// Node-derived views refreshed by the sync thread. Pages read these
    /// instead of calling the node while holding the index lock: one slow
    /// node used to queue every page view behind its RPC timeout.
    cache_net: serde_json::Value,
    cache_mempool: serde_json::Value,
    cache_game: serde_json::Value,
    cache_staking: serde_json::Value,
    cache_node_ok: bool,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub rolled_back: u64,
    pub indexed: u64,
    pub tip: u64,
}

/// One mirrored division-board row (pool snapshot → `division_boards`).
struct DivisionBoardRow {
    class: String,
    rank: i64,
    miner: String,
    name: String,
    eff_work: i64,
    days: i64,
    streak: i64,
    attested: i64,
}

/// Split a feed URL into (use_tls, host for SNI, connect address,
/// Host header value, request path). Pure parsing — no I/O, so it is
/// unit-testable.
fn split_feed_url(url: &str) -> Result<(bool, String, String, String, String), String> {
    let (tls_on, rest) = match url.strip_prefix("https://") {
        Some(r) => (true, r),
        None => (
            false,
            url.strip_prefix("http://")
                .ok_or_else(|| format!("need http:// or https:// url: {url}"))?,
        ),
    };
    let (host_port, path) = match rest.split_once('/') {
        Some((hp, p)) => (hp.to_string(), format!("/{p}")),
        None => (rest.to_string(), "/".to_string()),
    };
    // SNI + connect target: an explicit port wins, else the scheme
    // default. The server name for TLS never carries the port.
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.contains(']') && p.parse::<u16>().is_ok() => {
            (h.to_string(), p.to_string())
        }
        _ => (
            host_port.clone(),
            if tls_on { "443".to_string() } else { "80".to_string() },
        ),
    };
    if host.is_empty() {
        return Err(format!("bad url, empty host: {url}"));
    }
    Ok((tls_on, host.clone(), format!("{host}:{port}"), host_port, path))
}

/// Minimal HTTP(S) GET (std + the workspace TLS stack) → parsed JSON.
/// The explorer reads the pool's dashboard API and the optional price
/// feed with this; everything else is node RPC.
fn http_get_json(url: &str) -> Result<serde_json::Value, String> {
    let (tls_on, host, addr, host_port, path) = split_feed_url(url)?;
    let stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    let mut raw = Vec::new();
    if tls_on {
        // Mozilla roots, strict verification — a price feed is
        // operator-configured, but it still must not be MITMable.
        let cfg = tls::client_config(None, false).map_err(|e| format!("tls config: {e}"))?;
        let mut stream = tls::connect(stream, host.as_str(), Some(&cfg))
            .map_err(|e| format!("tls {addr}: {e}"))?;
        stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        stream
            .read_to_end(&mut raw)
            .map_err(|e| format!("read: {e}"))?;
    } else {
        (&stream)
            .write_all(req.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        (&stream)
            .read_to_end(&mut raw)
            .map_err(|e| format!("read: {e}"))?;
    }
    let text = String::from_utf8_lossy(&raw);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    serde_json::from_str(body).map_err(|e| format!("json: {e}"))
}

fn schema(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS blocks(
           height INTEGER PRIMARY KEY, hash TEXT UNIQUE NOT NULL,
           time INTEGER NOT NULL, ntx INTEGER NOT NULL,
           difficulty REAL NOT NULL, reserve INTEGER NOT NULL,
           pot INTEGER NOT NULL, drought INTEGER NOT NULL,
           pos INTEGER NOT NULL DEFAULT 0,
           size INTEGER NOT NULL DEFAULT 0,
           merkleroot TEXT NOT NULL DEFAULT '');
         CREATE TABLE IF NOT EXISTS txs(
           txid TEXT PRIMARY KEY, height INTEGER NOT NULL, coinbase INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS idx_txs_height ON txs(height);
         CREATE TABLE IF NOT EXISTS outs(
           txid TEXT NOT NULL, n INTEGER NOT NULL, address TEXT,
           value INTEGER NOT NULL, coinbase INTEGER NOT NULL,
           spent_by TEXT, spent_n INTEGER,
           PRIMARY KEY(txid, n));
         CREATE INDEX IF NOT EXISTS idx_outs_address ON outs(address);
         CREATE INDEX IF NOT EXISTS idx_outs_spent ON outs(spent_by);
          CREATE TABLE IF NOT EXISTS game_blocks(
            height INTEGER PRIMARY KEY, bonus INTEGER NOT NULL,
            pot INTEGER NOT NULL, prize INTEGER NOT NULL, finder TEXT NOT NULL,
            claim INTEGER NOT NULL DEFAULT 0, winner TEXT NOT NULL DEFAULT '');
          CREATE TABLE IF NOT EXISTS division_boards(
            class TEXT NOT NULL, rank INTEGER NOT NULL, miner TEXT NOT NULL,
            name TEXT NOT NULL, eff_work INTEGER NOT NULL, days INTEGER NOT NULL,
            streak INTEGER NOT NULL, attested INTEGER NOT NULL DEFAULT 0,
            synced_at INTEGER NOT NULL,
            PRIMARY KEY(class, rank));
          CREATE TABLE IF NOT EXISTS mempool_seen(
            txid TEXT PRIMARY KEY, first_seen INTEGER NOT NULL);
          -- Current consensus Luckynode Draw, mirrored from the pool's
          -- /api/stats `draw` object (single row; display only).
          CREATE TABLE IF NOT EXISTS draw_state(
            id INTEGER PRIMARY KEY CHECK (id = 1),
            epoch INTEGER NOT NULL DEFAULT 0, budget INTEGER NOT NULL DEFAULT 0,
            remaining INTEGER NOT NULL DEFAULT 0, winner TEXT NOT NULL DEFAULT '',
            synced_at INTEGER NOT NULL DEFAULT 0);
          -- Per-tx fee/weight facts, resolved from the node's raw hex (the
          -- fork's verbose tx format carries no vsize for confirmed txs).
          CREATE TABLE IF NOT EXISTS tx_facts(
            txid TEXT PRIMARY KEY, size INTEGER NOT NULL, fee INTEGER NOT NULL,
            feerate REAL NOT NULL DEFAULT 0, height INTEGER NOT NULL DEFAULT 0);",
    )
}

impl Indexer {
    /// Test-only fixture hook: sibling modules exercise the HTTP layer on
    /// an index they populate directly (the connection stays private).
    #[cfg(test)]
    pub(crate) fn fixture_exec(&self, sql: &str) -> rusqlite::Result<()> {
        self.db.execute_batch(sql)
    }

    pub fn open(path: &str, rpc: RpcClient) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|e| format!("open db {path}: {e}"))?;
        schema(&db).map_err(|e| format!("schema: {e}"))?;
        // Migration: game_blocks `league` column renamed to `prize`
        // (2026-10-21 Divisions/Prize Fund naming). Fresh DBs already have
        // `prize`; the rename is a no-op error on them, ignored.
        let _ = db.execute(
            "ALTER TABLE game_blocks RENAME COLUMN league TO prize",
            [],
        );
        // Migration: older DBs predate the `pos` column (minted-block
        // badges, 2026-10-18). CREATE TABLE IF NOT EXISTS will not add it.
        let _ = db.execute(
            "ALTER TABLE blocks ADD COLUMN pos INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Migration: block `size` + `merkleroot` (2026-10-19 block page).
        let _ = db.execute(
            "ALTER TABLE blocks ADD COLUMN size INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = db.execute(
            "ALTER TABLE blocks ADD COLUMN merkleroot TEXT NOT NULL DEFAULT ''",
            [],
        );
        // Migration: block `chainwork` (hex) — the cumulative work the
        // node's own hashrate estimator differentiates (2026-10-19 charts).
        let _ = db.execute(
            "ALTER TABLE blocks ADD COLUMN chainwork TEXT NOT NULL DEFAULT ''",
            [],
        );
        // Migration: block `fees` (sats) — total fees collected, so daily
        // aggregates don't need the outs join on every chart query.
        let _ = db.execute(
            "ALTER TABLE blocks ADD COLUMN fees INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Migration: block header details (version/bits/nonce/prevhash)
        // for the block page. Idempotent; older DBs backfill below.
        let _ = db.execute(
            "ALTER TABLE blocks ADD COLUMN version INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = db.execute(
            "ALTER TABLE blocks ADD COLUMN bits TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = db.execute(
            "ALTER TABLE blocks ADD COLUMN nonce TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = db.execute(
            "ALTER TABLE blocks ADD COLUMN prevhash TEXT NOT NULL DEFAULT ''",
            [],
        );
        // Migration: game_blocks prize-draw `claim` + `winner` (consensus
        // Luckynode Draw claims observed on pool-mined boundary blocks).
        let _ = db.execute(
            "ALTER TABLE game_blocks ADD COLUMN claim INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = db.execute(
            "ALTER TABLE game_blocks ADD COLUMN winner TEXT NOT NULL DEFAULT ''",
            [],
        );
        let mut this = Self {
            db,
            rpc,
            price_url: None,
            price_source: None,
            price_cache: None,
            cache_net: serde_json::json!({}),
            cache_mempool: serde_json::json!({}),
            cache_game: serde_json::json!({}),
            cache_staking: serde_json::json!({}),
            cache_node_ok: true,
        };
        // Backfill the exact PoS marker for indexed rows (cheap header
        // call each — the whole testnet chain is a few hundred blocks).
        // Rows already marked stay untouched.
        this.backfill_pos();
        // Backfill size/merkleroot the same way (one getblock each, skipped
        // silently when the node is unreachable — the columns just stay 0).
        this.backfill_block_meta();
        // Header details (version/bits/nonce/prev) for rows indexed before
        // the columns existed.
        this.backfill_block_header();
        // Backfill chainwork the same way (getblock carries it).
        this.backfill_chainwork();
        // Backfill per-block fee totals for rows indexed before the
        // column existed. Index-only (no node call): a block with no
        // non-coinbase tx has no fees by definition, so only blocks
        // that contain spends are recomputed.
        this.backfill_block_fees();
        // Backfill exact tx sizes/rates for recent spends (one raw-tx
        // call each, bounded so a long chain never stalls startup).
        this.backfill_tx_facts(2000);
        Ok(this)
    }

    /// Configure an optional price feed for the market page. The feed is
    /// a JSON URL returning `{"price": <usd per coin>, "source": "..."}`.
    /// With no feed configured the market view reports liquidity/supply
    /// only — it never invents a price.
    pub fn set_price_feed(&mut self, url: Option<String>, source: Option<String>) {
        self.price_url = url.filter(|u| !u.trim().is_empty());
        self.price_source = source;
        self.price_cache = None;
    }

    /// Fill `pos` for rows indexed before the column existed, newest
    /// first. Stops at the first fully-marked stretch (steady state is
    /// one header call per new block via index_height).
    fn backfill_pos(&mut self) {
        let heights: Vec<i64> = self
            .db
            .prepare("SELECT height FROM blocks WHERE pos = 0 ORDER BY height DESC LIMIT 2000")
            .and_then(|mut st| st.query_map([], |r| r.get(0)).and_then(|it| it.collect::<Result<Vec<_>, _>>()))
            .unwrap_or_default();
        for h in heights {
            let Ok(hash) = self.node_hash(h as u64) else { continue };
            let Ok(hdr) = self.rpc.call("getblockheader", serde_json::json!([hash])) else { continue };
            // A row that is genuinely PoW keeps pos = 0 — but only the
            // node knows, so mark every checked row with its verdict.
            if hdr.get("pos").and_then(|v| v.as_bool()).unwrap_or(false) {
                let _ = self.db.execute("UPDATE blocks SET pos = 1 WHERE height = ?1", [h]);
            } else {
                // Distinguish "checked, genuinely PoW" from "never
                // checked": PoW needs no mark, and every new block is
                // indexed with its verdict — so skipping checked-PoW rows
                // is safe as long as backfill runs to completion once.
                // (No-op by design; the SELECT above re-checks them until
                // a PoS row ends the ambiguity — bounded by chain size.)
                let _ = self.db.execute("UPDATE blocks SET pos = -1 WHERE height = ?1", [h]);
            }
        }
        // Normalize the checked-PoW sentinel back to 0 (pos means PoS).
        let _ = self.db.execute("UPDATE blocks SET pos = 0 WHERE pos = -1", []);
    }

    /// Fill `size`/`merkleroot` for rows indexed before those columns
    /// existed. size = 0 unambiguously means "not yet filled" (no block
    /// is 0 bytes); failures (node unreachable) just leave rows for the
    /// next open — steady state needs no RPC at all.
    fn backfill_block_meta(&mut self) {
        let rows: Vec<(i64, String)> = self
            .db
            .prepare("SELECT height, hash FROM blocks WHERE size = 0 ORDER BY height DESC LIMIT 5000")
            .and_then(|mut st| {
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
            })
            .unwrap_or_default();
        for (h, hash) in rows {
            let Ok(b) = self.rpc.call("getblock", serde_json::json!([hash, 1])) else { continue };
            let size = b.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
            let root = b.get("merkleroot").and_then(|v| v.as_str()).unwrap_or("");
            if size > 0 {
                let _ = self.db.execute(
                    "UPDATE blocks SET size = ?1, merkleroot = ?2 WHERE height = ?3",
                    rusqlite::params![size, root, h],
                );
            }
        }
    }

    /// Fill header details (version/bits/nonce/previous hash) for rows
    /// indexed before those columns existed. `getblockheader` is cheap;
    /// bounded per pass like the other backfills.
    fn backfill_block_header(&mut self) {
        let rows: Vec<(i64, String)> = self
            .db
            .prepare("SELECT height, hash FROM blocks WHERE prevhash = '' AND height > 0 ORDER BY height DESC LIMIT 5000")
            .and_then(|mut st| {
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
            })
            .unwrap_or_default();
        for (h, hash) in rows {
            let Ok(hdr) = self.rpc.call("getblockheader", serde_json::json!([hash])) else { continue };
            let version = hdr.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
            let bits = hdr.get("bits").and_then(|v| v.as_str()).unwrap_or("");
            let nonce = hdr
                .get("nonce")
                .and_then(|v| v.as_u64())
                .map(|n| format!("{n:016x}"))
                .unwrap_or_default();
            let prev = hdr
                .get("previousblockhash")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let _ = self.db.execute(
                "UPDATE blocks SET version = ?1, bits = ?2, nonce = ?3, prevhash = ?4 WHERE height = ?5",
                rusqlite::params![version, bits, nonce, prev, h],
            );
        }
    }

    /// Fill `chainwork` for rows indexed before the column existed. The
    /// node returns the cumulative work hex in every `getblock`; the
    /// hashrate chart differentiates it exactly the way the node's own
    /// `getnetworkhashps` does. Empty string = unknown (node unreachable).
    fn backfill_chainwork(&mut self) {
        let rows: Vec<(i64, String)> = self
            .db
            .prepare("SELECT height, hash FROM blocks WHERE chainwork = '' ORDER BY height DESC LIMIT 5000")
            .and_then(|mut st| {
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
            })
            .unwrap_or_default();
        for (h, hash) in rows {
            let Ok(b) = self.rpc.call("getblock", serde_json::json!([hash, 1])) else { continue };
            let cw = b.get("chainwork").and_then(|v| v.as_str()).unwrap_or("");
            if !cw.is_empty() {
                let _ = self.db.execute(
                    "UPDATE blocks SET chainwork = ?1 WHERE height = ?2",
                    rusqlite::params![cw, h],
                );
            }
        }
    }

    /// Recompute per-block fee totals for rows indexed before the
    /// `fees` column existed. Index-only — no node call. Only blocks
    /// holding non-coinbase transactions can have fees, so the rest
    /// are left alone.
    fn backfill_block_fees(&mut self) {
        let heights: Vec<i64> = self
            .db
            .prepare(
                "SELECT DISTINCT height FROM txs WHERE coinbase = 0
                 AND height IN (SELECT height FROM blocks WHERE fees = 0)
                 ORDER BY height DESC LIMIT 20000",
            )
            .and_then(|mut st| {
                st.query_map([], |r| r.get(0)).and_then(|it| it.collect::<Result<Vec<_>, _>>())
            })
            .unwrap_or_default();
        for h in heights {
            let fees: i64 = self
                .db
                .query_row(
                    "SELECT COALESCE((SELECT SUM(o2.value) FROM outs o2 WHERE o2.spent_by IN (SELECT txid FROM txs WHERE height = ?1)),0)
                          - COALESCE((SELECT SUM(o1.value) FROM outs o1 JOIN txs t ON t.txid = o1.txid WHERE t.height = ?1 AND t.coinbase = 0),0)",
                    [h],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let _ = self.db.execute(
                "UPDATE blocks SET fees = ?1 WHERE height = ?2",
                params![fees, h],
            );
        }
    }

    /// Backfill exact sizes + fee rates for recent spends that were
    /// indexed before the facts table existed. One raw-tx call per tx,
    /// newest first, bounded by `limit`.
    fn backfill_tx_facts(&mut self, limit: i64) {
        let txids: Vec<String> = self
            .db
            .prepare(
                "SELECT t.txid FROM txs t
                 WHERE t.coinbase = 0
                 AND t.txid NOT IN (SELECT txid FROM tx_facts)
                 ORDER BY t.height DESC LIMIT ?1",
            )
            .and_then(|mut st| {
                st.query_map([limit], |r| r.get(0)).and_then(|it| it.collect::<Result<Vec<_>, _>>())
            })
            .unwrap_or_default();
        for txid in txids {
            let size = self.tx_size(&txid);
            if size <= 0 {
                continue;
            }
            let fee: i64 = self
                .db
                .query_row(
                    "SELECT COALESCE((SELECT SUM(value) FROM outs WHERE spent_by = ?1),0)
                          - COALESCE((SELECT SUM(value) FROM outs WHERE txid = ?1),0)",
                    [&txid],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let height: i64 = self
                .db
                .query_row("SELECT height FROM txs WHERE txid = ?1", [&txid], |r| r.get(0))
                .unwrap_or(0);
            let feerate = fee as f64 / size as f64;
            let _ = self.db.execute(
                "INSERT INTO tx_facts(txid, size, fee, feerate, height) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(txid) DO UPDATE SET size = ?2, fee = ?3, feerate = ?4, height = ?5",
                params![txid, size, fee, feerate, height],
            );
        }
    }

    pub fn height(&self) -> Result<i64, String> {
        let h: Option<i64> = self
            .db
            .query_row("SELECT MAX(height) FROM blocks", [], |r| r.get(0))
            .map_err(|e| format!("db height: {e}"))?;
        Ok(h.unwrap_or(-1))
    }

    fn hash_at(&self, height: i64) -> Result<Option<String>, String> {
        let h: Option<String> = self
            .db
            .query_row("SELECT hash FROM blocks WHERE height = ?1", [height], |r| {
                r.get(0)
            })
            .map_err(|e| format!("db hash: {e}"))?;
        Ok(h)
    }

    fn node_hash(&self, height: u64) -> Result<String, String> {
        let v = self
            .rpc
            .call("getblockhash", serde_json::json!([height]))?;
        v.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "getblockhash: not a string".to_string())
    }

    /// Delete one indexed height (and un-mark anything it spent) — the
    /// reorg primitive. Caller walks back to the fork point with this.
    fn rollback_height(&mut self, height: i64) -> Result<(), String> {
        let tx = self
            .db
            .transaction()
            .map_err(|e| format!("rollback txn: {e}"))?;
        tx.execute(
            "UPDATE outs SET spent_by = NULL, spent_n = NULL
             WHERE spent_by IN (SELECT txid FROM txs WHERE height = ?1)",
            [height],
        )
        .map_err(|e| format!("rollback unspend: {e}"))?;
        tx.execute("DELETE FROM outs WHERE txid IN (SELECT txid FROM txs WHERE height = ?1)", [height])
            .map_err(|e| format!("rollback outs: {e}"))?;
        tx.execute("DELETE FROM txs WHERE height = ?1", [height])
            .map_err(|e| format!("rollback txs: {e}"))?;
        // Per-tx facts are keyed by txid (not height) so they would
        // otherwise linger after a reorg. They are invisible without a
        // matching txs row, but delete them anyway — no dead rows.
        tx.execute("DELETE FROM tx_facts WHERE height = ?1", [height])
            .map_err(|e| format!("rollback tx facts: {e}"))?;
        tx.execute("DELETE FROM blocks WHERE height = ?1", [height])
            .map_err(|e| format!("rollback blocks: {e}"))?;
        tx.commit().map_err(|e| format!("rollback commit: {e}"))?;
        Ok(())
    }

    /// Index block `height` from the node (full txs + game state).
    fn index_height(&mut self, height: u64) -> Result<(), String> {
        let hash = self.node_hash(height)?;
        let b = self
            .rpc
            .call("getblock", serde_json::json!([hash, 2]))?;
        let ji = self.rpc.call("getjackpotinfo", serde_json::json!([height]))?;
        let time = b.get("time").and_then(|v| v.as_i64()).unwrap_or(0);
        let difficulty = b.get("difficulty").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let txs = b
            .get("tx")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("getblock {height}: no tx list"))?;
        let reserve = ji.get("reserve").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
        let pot = ji.get("pot").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
        let drought = ji.get("drought").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
        // Exact PoS marker from the node (getblock "pos": the same
        // IsLuckyPOSBlock() the validator uses). Missing on old nodes → 0.
        let pos = b.get("pos").and_then(|v| v.as_bool()).unwrap_or(false) as i64;
        let size = b.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
        let merkleroot = b.get("merkleroot").and_then(|v| v.as_str()).unwrap_or("");
        // Cumulative work hex — the hashrate chart differentiates this
        // the same way the node's `getnetworkhashps` does.
        let chainwork = b.get("chainwork").and_then(|v| v.as_str()).unwrap_or("");
        let version = b.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
        let bits = b.get("bits").and_then(|v| v.as_str()).unwrap_or("");
        let nonce = b
            .get("nonce")
            .and_then(|v| v.as_u64())
            .map(|n| format!("{n:016x}"))
            .unwrap_or_default();
        let prevhash = b
            .get("previousblockhash")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let txn = self
            .db
            .transaction()
            .map_err(|e| format!("index txn: {e}"))?;
        txn.execute(
            "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot, chainwork, version, bits, nonce, prevhash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![height as i64, hash, time, txs.len() as i64, difficulty, reserve, pot, drought, pos, size, merkleroot, chainwork, version, bits, nonce, prevhash],
        )
        .map_err(|e| format!("index block {height}: {e}"))?;
        for t in txs {
            let txid = t
                .get("txid")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("block {height}: tx without txid"))?;
            let vin = t.get("vin").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let coinbase = vin
                .first()
                .map(|v| v.get("coinbase").is_some())
                .unwrap_or(false);
            txn.execute(
                "INSERT INTO txs(txid, height, coinbase) VALUES (?1, ?2, ?3)",
                params![txid, height as i64, coinbase as i32],
            )
            .map_err(|e| format!("index tx {txid}: {e}"))?;
            for (idx, inp) in vin.iter().enumerate() {
                if let (Some(prev), Some(n)) = (
                    inp.get("txid").and_then(|v| v.as_str()),
                    inp.get("vout").and_then(|v| v.as_u64()),
                ) {
                    let changed = txn
                        .execute(
                            "UPDATE outs SET spent_by = ?1, spent_n = ?2
                             WHERE txid = ?3 AND n = ?4 AND spent_by IS NULL",
                            params![txid, idx as i64, prev, n as i64],
                        )
                        .map_err(|e| format!("spend {prev}:{n}: {e}"))?;
                    if changed != 1 {
                        return Err(format!("spend {prev}:{n} by {txid}: not found or double-spent"));
                    }
                }
            }
            let vout = t.get("vout").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            for o in &vout {
                let n = o.get("n").and_then(|v| v.as_u64()).unwrap_or(0) as i64;
                let value = (o
                    .get("value")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
                    * 100_000_000.0)
                    .round() as i64;
                let addr = o
                    .get("scriptPubKey")
                    .and_then(|s| s.get("addresses"))
                    .and_then(|a| a.as_array())
                    .and_then(|a| a.first())
                    .and_then(|a| a.as_str())
                    .map(|s| s.to_string());
                txn.execute(
                    "INSERT INTO outs(txid, n, address, value, coinbase) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![txid, n, addr, value, coinbase as i32],
                )
                .map_err(|e| format!("index out {txid}:{n}: {e}"))?;
            }
        }
        txn.commit().map_err(|e| format!("index commit {height}: {e}"))?;
        // Per-tx facts (exact size + fee rate) and the block's fee total.
        // Best-effort: a node that cannot serve the raw tx just leaves
        // size at 0 (the UI then shows "–" rather than a wrong number).
        self.record_block_facts(height, txs);
        Ok(())
    }

    /// Resolve exact serialized size + fee rate for a block's
    /// transactions, and total the block's fees.
    ///
    /// The fork's verbose tx JSON carries no `size`/`vsize` for confirmed
    /// txs (its `getrawtransaction` verbose form omits them), so the
    /// exact byte size comes from the raw hex — `getrawtransaction <txid>`
    /// with verbosity 0 — which the node serves for confirmed txs
    /// (the fork keeps a tx index; verified against old confirmed
    /// heights). Fee rate is then fee ÷ size, in sat/vB.
    ///
    /// This chain has no segwit, so serialized size == virtual size.
    fn record_block_facts(&mut self, height: u64, txs: &[serde_json::Value]) {
        // Total fees for the block: spent inputs minus non-coinbase
        // outputs (index-only, exact — the same definition the block page
        // uses).
        let block_fees: i64 = self
            .db
            .query_row(
                "SELECT COALESCE((SELECT SUM(o2.value) FROM outs o2 WHERE o2.spent_by IN (SELECT txid FROM txs WHERE height = ?1)),0)
                      - COALESCE((SELECT SUM(o1.value) FROM outs o1 JOIN txs t ON t.txid = o1.txid WHERE t.height = ?1 AND t.coinbase = 0),0)",
                [height as i64],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let _ = self.db.execute(
            "UPDATE blocks SET fees = ?1 WHERE height = ?2",
            params![block_fees, height as i64],
        );

        for t in txs {
            let Some(txid) = t.get("txid").and_then(|v| v.as_str()) else { continue };
            let vin = t.get("vin").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            if vin.first().map(|v| v.get("coinbase").is_some()).unwrap_or(false) {
                // Coinbase: no fee by definition. Record its size only if
                // it is cheap to get; the fee rate is not meaningful.
                continue;
            }
            let fee: i64 = self
                .db
                .query_row(
                    "SELECT COALESCE((SELECT SUM(value) FROM outs WHERE spent_by = ?1),0)
                          - COALESCE((SELECT SUM(value) FROM outs WHERE txid = ?1),0)",
                    [txid],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let size = self.tx_size(txid);
            if size <= 0 {
                continue;
            }
            // sat/vB — fee is in sats, size in bytes.
            let feerate = fee as f64 / size as f64;
            let _ = self.db.execute(
                "INSERT INTO tx_facts(txid, size, fee, feerate, height) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(txid) DO UPDATE SET size = ?2, fee = ?3, feerate = ?4, height = ?5",
                params![txid, size, fee, feerate, height as i64],
            );
        }
    }

    /// Exact serialized size of a transaction in bytes, from the node's
    /// raw hex. 0 when unavailable (node unreachable, tx not indexed).
    fn tx_size(&mut self, txid: &str) -> i64 {
        if let Ok(v) = self.rpc.call("getrawtransaction", serde_json::json!([txid, 0])) {
            if let Some(hex) = v.as_str() {
                // Two hex chars per byte.
                return (hex.len() / 2) as i64;
            }
        }
        0
    }

    /// One sync pass: roll back any diverged tip, then index forward.
    /// Snapshot the node-derived views the UI shows. Runs on the sync
    /// thread (which already talks to the node); request threads then read
    /// the snapshots without touching RPC under the index lock. A node
    /// error keeps the previous snapshot.
    fn refresh_node_caches(&mut self) {
        if let Ok(v) = self.fetch_network() {
            self.cache_net = v;
        }
        if let Ok(v) = self.fetch_mempool() {
            self.cache_mempool = v;
        }
        if let Ok(v) = self.fetch_game_state() {
            self.cache_game = v;
        }
        if let Ok(v) = self.fetch_staking_state() {
            self.cache_staking = v;
        }
        self.cache_node_ok = self
            .rpc
            .call("getblockcount", serde_json::json!([]))
            .is_ok();
        // Price feed has its own TTL cache; refresh it here too so the
        // supply page never performs network I/O under the lock.
        self.refresh_price();
    }

    pub fn sync(&mut self) -> Result<SyncReport, String> {
        // Catch up any history the indexer predates (fee totals, exact
        // tx sizes) — bounded per pass so a long chain fills in over
        // several passes instead of stalling the sync.
        self.backfill_block_fees();
        self.backfill_tx_facts(200);
        let tip = match self.rpc.call("getblockcount", serde_json::json!([])) {
            Ok(v) => v
                .as_u64()
                .ok_or_else(|| "getblockcount: not a number".to_string())?,
            Err(e) => {
                // Mark the node down before returning: the UI banner reads
                // this flag and a sync that never reaches the cache refresh
                // would otherwise keep claiming the node is healthy.
                self.cache_node_ok = false;
                return Err(e);
            }
        };
        let mut have = self.height()?;
        let mut rolled_back = 0u64;
        while have >= 0 {
            let (node, ours) = (self.node_hash(have as u64)?, self.hash_at(have)?);
            if Some(node) == ours {
                break;
            }
            self.rollback_height(have)?;
            rolled_back += 1;
            have -= 1;
        }
        let mut indexed = 0u64;
        while (have + 1) as u64 <= tip {
            have += 1;
            self.index_height(have as u64)?;
            indexed += 1;
        }
        self.refresh_node_caches();
        Ok(SyncReport { rolled_back, indexed, tip })
    }

    /// Pool-authoritative per-block game record (bonus/pot/prize inflow,
    /// draw claim + winner, finder). Pool-mined rows overwrite wholesale;
    /// external-find rows (prize-only) never clobber a pool record's
    /// bonus/pot/claim/winner — see [`Indexer::apply_prize_credits`].
    #[allow(clippy::too_many_arguments)] // one param per game_blocks column
    fn upsert_game_block(
        &self,
        h: i64,
        bonus: i64,
        pot: i64,
        prize: i64,
        finder: &str,
        claim: i64,
        winner: &str,
    ) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO game_blocks(height, bonus, pot, prize, finder, claim, winner)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(height) DO UPDATE SET
                   bonus=?2, pot=?3, prize=?4, finder=?5, claim=?6, winner=?7",
                params![h, bonus, pot, prize, finder, claim, winner],
            )
            .map_err(|e| format!("game block {h}: {e}"))?;
        Ok(())
    }

    /// Pull game events (per-block jackpot/pot/prize) from the pool's dashboard API into the index. Best-effort: the
    /// chain index works without it; a pool outage just leaves these
    /// tables stale (the caller decides how loudly to report that).
    pub fn sync_pool(&mut self, pool_api: &str) -> Result<u64, String> {
        let v = http_get_json(&format!("{}/api/stats", pool_api.trim_end_matches('/')))?;
        let mut n = 0u64;
        if let Some(blocks) = v.get("last_blocks").and_then(|b| b.as_array()) {
            for b in blocks {
                let h = b.get("height").and_then(|x| x.as_i64()).unwrap_or(-1);
                if h < 0 {
                    continue;
                }
                let bonus = b.get("bonus").and_then(|x| x.as_i64()).unwrap_or(0);
                let pot = b.get("pot").and_then(|x| x.as_i64()).unwrap_or(0);
                let prize = b.get("prize_inflow").and_then(|x| x.as_i64()).unwrap_or(0);
                let finder = b.get("finder_name").and_then(|x| x.as_str()).unwrap_or("");
                let claim = b.get("prize_claim").and_then(|x| x.as_i64()).unwrap_or(0);
                let winner = b.get("prize_winner").and_then(|x| x.as_str()).unwrap_or("");
                self.upsert_game_block(h, bonus, pot, prize, finder, claim, winner)?;
                n += 1;
            }
        }
        // Per-height prize inflow the pool credited. This covers blocks
        // the pool did NOT mine (staker/peer finds): they never appear in
        // `last_blocks`, but their overflow still moved money. Upsert
        // prize only — never clobber bonus/pot/finder already recorded
        // from a pool-found block.
        if let Some(credits) = v.get("prize").and_then(|p| p.get("credits")).and_then(|c| c.as_array()) {
            let rows: Vec<(i64, i64)> = credits
                .iter()
                .filter_map(|c| {
                    Some((
                        c.get("height")?.as_i64()?,
                        c.get("amount")?.as_i64()?,
                    ))
                })
                .filter(|(h, a)| *h >= 0 && *a > 0)
                .collect();
            n += self.apply_prize_credits(&rows)?;
        }
        // Current-epoch draw advisory. The pool reads it from the node's
        // GBT every tick; mirror it so the rewards page can show the
        // live draw without hitting node RPC per request.
        if let Some(d) = v.get("draw") {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_secs() as i64)
                .unwrap_or(0);
            let epoch = d.get("epoch").and_then(|x| x.as_i64()).unwrap_or(0);
            if epoch > 0 {
                self.db
                    .execute(
                        "INSERT INTO draw_state(id, epoch, budget, remaining, winner, synced_at)
                         VALUES (1, ?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(id) DO UPDATE SET epoch=?1, budget=?2, remaining=?3, winner=?4, synced_at=?5",
                        params![
                            epoch,
                            d.get("budget").and_then(|x| x.as_i64()).unwrap_or(0),
                            d.get("remaining").and_then(|x| x.as_i64()).unwrap_or(0),
                            d.get("winner").and_then(|x| x.as_str()).unwrap_or(""),
                            now
                        ],
                    )
                    .map_err(|e| format!("draw state: {e}"))?;
            }
        }
        // Seasonal division boards (doc 03 §3.3: phones race phones). The
        // pool's `divisions.boards` is a point-in-time snapshot, so the table
        // is replaced wholesale on each well-formed non-empty fetch (a
        // missing/empty fetch leaves the last boards in place).
        if let Some(boards) = v.get("divisions").and_then(|d| d.get("boards")).and_then(|b| b.as_array()) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut rows: Vec<DivisionBoardRow> = Vec::new();
            for b in boards.iter().take(8) {
                let class = b.get("class").and_then(|c| c.as_str()).unwrap_or("");
                if class.is_empty() {
                    continue;
                }
                let entries = b.get("entries").and_then(|e| e.as_array()).cloned().unwrap_or_default();
                for e in entries.iter().take(500) {
                    let rank = e.get("rank").and_then(|r| r.as_i64()).unwrap_or(0);
                    if rank <= 0 {
                        continue;
                    }
                    let miner = e.get("miner").and_then(|m| m.as_str()).unwrap_or("").to_string();
                    let name = e.get("name").and_then(|x| x.as_str()).unwrap_or(&miner).to_string();
                    let eff = e.get("eff_work").and_then(|x| x.as_i64()).unwrap_or(0);
                    let days = e.get("days_active").and_then(|x| x.as_i64()).unwrap_or(0);
                    let streak = e.get("streak").and_then(|x| x.as_i64()).unwrap_or(0);
                    let attested = e.get("attested").and_then(|x| x.as_bool()).unwrap_or(false) as i64;
                    rows.push(DivisionBoardRow { class: class.to_string(), rank, miner, name, eff_work: eff, days, streak, attested });
                }
            }
            if !rows.is_empty() {
                let tx = self.db.transaction().map_err(|e| format!("division snapshot: {e}"))?;
                tx.execute("DELETE FROM division_boards", []).map_err(|e| format!("division snapshot: {e}"))?;
                {
                    let mut ins = tx
                        .prepare(
                            "INSERT INTO division_boards(class, rank, miner, name, eff_work, days, streak, attested, synced_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        )
                        .map_err(|e| format!("division snapshot: {e}"))?;
                    for r in &rows {
                        ins.execute(params![
                            r.class, r.rank, r.miner, r.name, r.eff_work, r.days, r.streak, r.attested, now
                        ])
                        .map_err(|e| format!("division snapshot: {e}"))?;
                    }
                }
                tx.commit().map_err(|e| format!("division snapshot: {e}"))?;
                n += rows.len() as u64;
            }
        }
        Ok(n)
    }

    // ---- query surface for the HTTP layer ----

    /// Block reward = sum of the block's coinbase outputs (subsidy +
    /// fees, before the 5% game-fund levy split the finder actually
    /// receives). Exact: straight from the indexed UTXO set.
    pub fn coinbase_total(&self, height: i64) -> Result<i64, String> {
        self.db
            .query_row(
                "SELECT COALESCE(SUM(o.value),0) FROM outs o JOIN txs t ON t.txid = o.txid WHERE t.height = ?1 AND o.coinbase = 1",
                [height],
                |r| r.get(0),
            )
            .map_err(|e| format!("coinbase total: {e}"))
    }

    pub fn block_by_height(&self, height: i64) -> Result<Option<serde_json::Value>, String> {
        type BlockRow = (String, i64, i64, f64, i64, i64, i64, i64, i64, String, i64, String, String, String);
        let row: Option<BlockRow> = self
            .db
            .query_row(
                "SELECT hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot, version, bits, nonce, prevhash FROM blocks WHERE height = ?1",
                [height],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?, r.get(13)?)),
            )
            .map(Some)
            .unwrap_or(None);
        let Some((hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot, version, bits, nonce, prevhash)) = row else {
            return Ok(None);
        };
        // Fees collected by this block: sum of spent inputs minus sum of
        // non-coinbase outputs (index-only, exact; coinbase has no inputs).
        let fees: i64 = self
            .db
            .query_row(
                "SELECT COALESCE((SELECT SUM(o2.value) FROM outs o2 WHERE o2.spent_by IN (SELECT txid FROM txs WHERE height = ?1)),0)
                      - COALESCE((SELECT SUM(o1.value) FROM outs o1 JOIN txs t ON t.txid = o1.txid WHERE t.height = ?1 AND t.coinbase = 0),0)",
                [height],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let tip = self.height().unwrap_or(height);
        Ok(Some(serde_json::json!({
            "height": height, "hash": hash, "time": time, "ntx": ntx,
            "confirmations": tip - height + 1,
            "difficulty": difficulty, "reserve": reserve, "pot": pot, "drought": drought,
            "pos": pos != 0, "size": size, "merkleroot": merkleroot, "fees": fees,
            "version": version, "bits": bits, "nonce": nonce, "prevhash": prevhash,
        })))
    }

    pub fn block_by_hash(&self, hash: &str) -> Result<Option<serde_json::Value>, String> {
        let height: Option<i64> = self
            .db
            .query_row("SELECT height FROM blocks WHERE hash = ?1", [hash], |r| r.get(0))
            .map(Some)
            .unwrap_or(None);
        match height {
            Some(h) => self.block_by_height(h),
            None => Ok(None),
        }
    }

    /// Newest-first block summaries at or below `top`, up to `limit`
    /// rows. Backs the /blocks index page (the home page only shows the
    /// last 20). Each row is the full [`Indexer::block_by_height`] object.
    pub fn block_list(&self, top: i64, limit: i64) -> Result<Vec<serde_json::Value>, String> {
        let tip = self.height().unwrap_or(top);
        let top = top.min(tip).max(0);
        let mut out = Vec::new();
        for h in ((top - limit + 1).max(0)..=top).rev() {
            if let Some(b) = self.block_by_height(h).unwrap_or(None) {
                out.push(b);
            }
        }
        Ok(out)
    }

    pub fn block_txs(&self, height: i64) -> Result<Vec<String>, String> {
        let mut st = self
            .db
            .prepare("SELECT txid FROM txs WHERE height = ?1 ORDER BY rowid")
            .map_err(|e| format!("tx list: {e}"))?;
        let rows: Vec<String> = st
            .query_map([height], |r| r.get(0))
            .map_err(|e| format!("tx list: {e}"))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|e| format!("tx list: {e}"))?;
        Ok(rows)
    }

    pub fn tx(&self, txid: &str) -> Result<Option<serde_json::Value>, String> {
        let row: Option<(i64, i32)> = self
            .db
            .query_row("SELECT height, coinbase FROM txs WHERE txid = ?1", [txid], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map(Some)
            .unwrap_or(None);
        let Some((height, coinbase)) = row else {
            return Ok(None);
        };
        let mut ins = Vec::new();
        // Inputs: walk our own spend edges backwards (outs spent BY this txid).
        let mut st = self
            .db
            .prepare("SELECT txid, n, address, value FROM outs WHERE spent_by = ?1 ORDER BY spent_n")
            .map_err(|e| format!("tx in: {e}"))?;
        let spent = st
            .query_map([txid], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?))
            })
            .map_err(|e| format!("tx in: {e}"))?;
        let mut in_total = 0i64;
        for s in spent {
            let (ptx, pn, addr, value): (String, i64, Option<String>, i64) =
                s.map_err(|e| format!("tx in: {e}"))?;
            in_total += value;
            ins.push(serde_json::json!({"txid": ptx, "vout": pn, "address": addr, "value": value}));
        }
        let mut st = self
            .db
            .prepare("SELECT n, address, value, spent_by IS NOT NULL, spent_by FROM outs WHERE txid = ?1 ORDER BY n")
            .map_err(|e| format!("tx out: {e}"))?;
        let outs = st
            .query_map([txid], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?, r.get::<_, bool>(3)?, r.get::<_, Option<String>>(4)?))
            })
            .map_err(|e| format!("tx out: {e}"))?;
        let mut out_list = Vec::new();
        let mut out_total = 0i64;
        for o in outs {
            let (n, addr, value, spent, spent_by): (i64, Option<String>, i64, bool, Option<String>) =
                o.map_err(|e| format!("tx out: {e}"))?;
            out_total += value;
            out_list.push(serde_json::json!({"n": n, "address": addr, "value": value, "spent": spent, "spent_by": spent_by}));
        }
        let tip = self.height().unwrap_or(height);
        // Exact size + fee rate when captured at index time.
        let facts = self.tx_facts(txid);
        Ok(Some(serde_json::json!({
            "txid": txid, "height": height, "coinbase": coinbase == 1,
            "confirmations": tip - height + 1,
            "vin": ins, "vout": out_list,
            "fee": if coinbase == 1 { 0 } else { in_total - out_total },
            "size": facts.map(|f| f.0),
            "feerate": facts.map(|f| f.2),
        })))
    }

    /// Full address view, newest-first (page 0). Kept for callers that
    /// want the first screen; [`Indexer::address_paged`] serves the rest.
    #[allow(dead_code)] // convenience wrapper retained for tests/tools
    pub fn address(&self, addr: &str) -> Result<serde_json::Value, String> {
        self.address_paged(addr, 0)
    }

    /// Address history page (`PAGE` rows, newest first). Pages are stable
    /// while the chain is: a reorg rewrites history and resets them,
    /// which the rollback-safe index already guarantees.
    pub const ADDRESS_PAGE_ROWS: i64 = 100;

    pub fn address_paged(&self, addr: &str, page: i64) -> Result<serde_json::Value, String> {
        let received: i64 = self
            .db
            .query_row(
                "SELECT COALESCE(SUM(value),0) FROM outs WHERE address = ?1",
                [addr],
                |r| r.get(0),
            )
            .map_err(|e| format!("addr received: {e}"))?;
        let balance: i64 = self
            .db
            .query_row(
                "SELECT COALESCE(SUM(value),0) FROM outs WHERE address = ?1 AND spent_by IS NULL",
                [addr],
                |r| r.get(0),
            )
            .map_err(|e| format!("addr balance: {e}"))?;
        // Total sent = value of this address's outputs that later got
        // spent (self-spends net out naturally: both sides count once).
        let sent: i64 = self
            .db
            .query_row(
                "SELECT COALESCE(SUM(value),0) FROM outs WHERE address = ?1 AND spent_by IS NOT NULL",
                [addr],
                |r| r.get(0),
            )
            .map_err(|e| format!("addr sent: {e}"))?;
        // Unspent outputs, newest first (standard explorer view).
        let mut utxos = Vec::new();
        {
            let mut st = self
                .db
                .prepare(
                    "SELECT o.txid, o.n, o.value, t.height FROM outs o
                     JOIN txs t ON t.txid = o.txid
                     WHERE o.address = ?1 AND o.spent_by IS NULL
                     ORDER BY t.height DESC LIMIT 100",
                )
                .map_err(|e| format!("addr utxos: {e}"))?;
            let rows = st
                .query_map([addr], |r| {
                    Ok(serde_json::json!({
                        "txid": r.get::<_, String>(0)?,
                        "n": r.get::<_, i64>(1)?,
                        "value": r.get::<_, i64>(2)?,
                        "height": r.get::<_, i64>(3)?,
                    }))
                })
                .map_err(|e| format!("addr utxos: {e}"))?;
            for r in rows {
                utxos.push(r.map_err(|e| format!("addr utxos: {e}"))?);
            }
        }
        let mut st = self
            .db
            .prepare(
                "SELECT o.txid, t.height FROM outs o JOIN txs t ON t.txid = o.txid
                 WHERE o.address = ?1
                 UNION SELECT o.spent_by AS txid, t.height FROM outs o JOIN txs t ON t.txid = o.spent_by
                 WHERE o.address = ?2 AND o.spent_by IS NOT NULL
                 ORDER BY height DESC LIMIT ?3 OFFSET ?4",
            )
            .map_err(|e| format!("addr history: {e}"))?;
        let page = page.max(0);
        let rows = st
            .query_map(
                rusqlite::params![addr, addr, Self::ADDRESS_PAGE_ROWS, page * Self::ADDRESS_PAGE_ROWS],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(|e| format!("addr history: {e}"))?;
        let mut history = Vec::new();
        for r in rows {
            let (txid, height): (String, i64) = r.map_err(|e| format!("addr history: {e}"))?;
            // Net flow for this address in this tx: +received -sent.
            let flow: i64 = self
                .db
                .query_row(
                    "SELECT COALESCE((SELECT SUM(value) FROM outs WHERE txid = ?1 AND address = ?2),0) - COALESCE((SELECT SUM(value) FROM outs WHERE spent_by = ?1 AND address = ?2),0)",
                    [&txid, &addr.to_string()],
                    |rr| rr.get(0),
                )
                .unwrap_or(0);
            history.push(serde_json::json!({"txid": txid, "height": height, "flow": flow}));
        }
        Ok(serde_json::json!({
            "address": addr, "balance": balance, "received": received, "sent": sent,
            "utxos": utxos, "txs": history, "txs_truncated": history.len() == Self::ADDRESS_PAGE_ROWS as usize,
            "page": page, "pages": (tx_count(addr, &self.db).unwrap_or(history.len() as i64) + Self::ADDRESS_PAGE_ROWS - 1) / Self::ADDRESS_PAGE_ROWS,
            "txcount": tx_count(addr, &self.db).unwrap_or(history.len() as i64),
            "first_seen": first_seen(addr, &self.db),
            "unconfirmed": address_unconfirmed_opt(&self.rpc, addr),
        }))
    }
}

/// Unconfirmed net for one address, or Value::Null when the node lacks
/// the address-mempool index (best-effort display field).
fn address_unconfirmed_opt(rpc: &RpcClient, addr: &str) -> serde_json::Value {
    match rpc.call("getaddressmempool", serde_json::json!([{"addresses": [addr]}])) {
        Ok(v) => v
            .as_array()
            .map(|arr| {
                serde_json::json!(arr
                    .iter()
                    .map(|e| e.get("satoshis").and_then(|s| s.as_i64()).unwrap_or(0))
                    .sum::<i64>())
            })
            .unwrap_or(serde_json::Value::Null),
        Err(_) => serde_json::Value::Null,
    }
}

/// Distinct transactions touching one address (both directions, no
/// double-count on self-spends).
fn tx_count(addr: &str, db: &Connection) -> Result<i64, String> {
    db.query_row(
        "SELECT COUNT(*) FROM (
           SELECT txid FROM outs WHERE address = ?1
           UNION SELECT spent_by FROM outs WHERE address = ?2 AND spent_by IS NOT NULL)",
        [addr, addr],
        |r| r.get(0),
    )
    .map_err(|e| format!("addr txcount: {e}"))
}

/// First height an address appears (either direction; null = never seen).
fn first_seen(addr: &str, db: &Connection) -> Option<i64> {
    db.query_row(
        "SELECT MIN(height) FROM (
           SELECT t.height AS height FROM outs o JOIN txs t ON t.txid = o.txid WHERE o.address = ?1
           UNION SELECT t.height FROM outs o JOIN txs t ON t.txid = o.spent_by WHERE o.address = ?2 AND o.spent_by IS NOT NULL)",
        [addr, addr],
        |r| r.get(0),
    )
    .unwrap_or(None)
}

// Methods resume here (helpers above are free functions): chain stats,
// game state, mempool/network views, the locked emission schedule.
impl Indexer {
    pub fn stats(&self) -> Result<serde_json::Value, String> {
        let tip = self.height()?;
        // Circulating supply = sum of UNSPENT outputs. This chain burns
        // transaction fees (the node's `GetNewCoins` subtracts them from
        // the emitted total, and `coinsupply` reports the unspent sum),
        // so the full coinbase sum overstates money in existence by the
        // fees paid. The unspent sum matches the node's `coinsupply`
        // exactly.
        let supply: i64 = self
            .db
            .query_row(
                "SELECT COALESCE(SUM(value),0) FROM outs WHERE spent_by IS NULL",
                [],
                |r| r.get(0),
            )
            .map_err(|e| format!("supply: {e}"))?;
        let tip_row: Option<(String, i64, f64)> = self
            .db
            .query_row("SELECT hash, time, difficulty FROM blocks WHERE height = ?1", [tip], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map(Some)
            .unwrap_or(None);
        Ok(serde_json::json!({
            "height": tip,
            "hash": tip_row.as_ref().map(|r| r.0.clone()).unwrap_or_default(),
            "time": tip_row.as_ref().map(|r| r.1).unwrap_or(0),
            "difficulty": tip_row.as_ref().map(|r| r.2).unwrap_or(0.0),
            "supply": supply,
        }))
    }

    /// Per-block game columns for one height (bonus/pot/divisions/finder).
    pub fn game_block(&self, height: i64) -> Result<Option<serde_json::Value>, String> {
        let row: Option<(i64, i64, i64, String, i64, String)> = self
            .db
            .query_row(
                "SELECT bonus, pot, prize, finder, claim, winner FROM game_blocks WHERE height = ?1",
                [height],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .map(Some)
            .unwrap_or(None);
        Ok(row.map(|(bonus, pot, prize, finder, claim, winner)| {
            serde_json::json!({
                "height": height, "bonus": bonus, "pot": pot,
                "prize": prize, "finder": finder,
                "claim": claim, "winner": winner,
            })
        }))
    }

    /// Apply per-height prize inflow from the pool (including blocks it
    /// did not mine). Insert-or-update prize only, so a pool-found
    /// block's bonus/pot/finder stay intact. Returns rows touched.
    fn apply_prize_credits(&mut self, credits: &[(i64, i64)]) -> Result<u64, String> {
        let mut n = 0u64;
        for (height, amount) in credits {
            self.db
                .execute(
                    "INSERT INTO game_blocks(height, bonus, pot, prize, finder)
                     VALUES (?1, 0, 0, ?2, 'external')
                     ON CONFLICT(height) DO UPDATE SET prize=excluded.prize",
                    params![height, amount],
                )
                .map_err(|e| format!("prize credit {height}: {e}"))?;
            n += 1;
        }
        Ok(n)
    }

    /// Recent game blocks (newest first): every row the pool reported.
    pub fn game_blocks_recent(&self, limit: i64) -> Result<Vec<serde_json::Value>, String> {
        let mut st = self
            .db
            .prepare(
                "SELECT height, bonus, pot, prize, finder, claim, winner FROM game_blocks
                 ORDER BY height DESC LIMIT ?1",
            )
            .map_err(|e| format!("game list: {e}"))?;
        let rows = st
            .query_map([limit], |r| {
                Ok(serde_json::json!({
                    "height": r.get::<_, i64>(0)?, "bonus": r.get::<_, i64>(1)?,
                    "pot": r.get::<_, i64>(2)?, "prize": r.get::<_, i64>(3)?,
                    "finder": r.get::<_, String>(4)?,
                    "claim": r.get::<_, i64>(5)?, "winner": r.get::<_, String>(6)?,
                }))
            })
            .map_err(|e| format!("game list: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("game list: {e}"))?;
        Ok(rows)
    }

    /// Pool-observed Luckynode Draw claims (newest first): epoch-boundary
    /// blocks whose coinbase carried the directed claim to the winner.
    /// Covers pool-mined boundaries only — external finds never pass
    /// through the pool's block records, so their claims stay node-RPC
    /// visible but are absent here (same scope as the pool dashboard).
    pub fn game_claims_recent(&self, limit: i64) -> Result<Vec<serde_json::Value>, String> {
        let mut st = self
            .db
            .prepare(
                "SELECT height, finder, claim, winner FROM game_blocks
                 WHERE claim > 0 ORDER BY height DESC LIMIT ?1",
            )
            .map_err(|e| format!("claim list: {e}"))?;
        let rows = st
            .query_map([limit], |r| {
                Ok(serde_json::json!({
                    "height": r.get::<_, i64>(0)?, "finder": r.get::<_, String>(1)?,
                    "claim": r.get::<_, i64>(2)?, "winner": r.get::<_, String>(3)?,
                }))
            })
            .map_err(|e| format!("claim list: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("claim list: {e}"))?;
        Ok(rows)
    }

    /// Recent transactions, newest first: txid, height, time, output
    /// value and fee (index-only; coinbase fee is 0 by definition).
    pub fn recent_txs(&self, limit: i64) -> Result<Vec<serde_json::Value>, String> {
        let mut st = self
            .db
            .prepare(
                "SELECT t.txid, t.height, t.coinbase, b.time FROM txs t
                 JOIN blocks b ON b.height = t.height
                 ORDER BY t.rowid DESC LIMIT ?1",
            )
            .map_err(|e| format!("recent txs: {e}"))?;
        let rows = st
            .query_map([limit], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| format!("recent txs: {e}"))?
            .collect::<Result<Vec<(String, i64, i64, i64)>, _>>()
            .map_err(|e| format!("recent txs: {e}"))?;
        let mut out = Vec::new();
        for (txid, height, coinbase, time) in rows {
            let outs: i64 = self
                .db
                .query_row("SELECT COALESCE(SUM(value),0) FROM outs WHERE txid = ?1", [&txid], |r| r.get(0))
                .unwrap_or(0);
            let fee: i64 = if coinbase == 1 {
                0
            } else {
                let ins: i64 = self
                    .db
                    .query_row("SELECT COALESCE(SUM(value),0) FROM outs WHERE spent_by = ?1", [&txid], |r| {
                        r.get(0)
                    })
                    .unwrap_or(0);
                ins.saturating_sub(outs)
            };
            out.push(serde_json::json!({
                "txid": txid, "height": height, "time": time,
                "coinbase": coinbase == 1, "value": outs, "fee": fee,
                "size": self.tx_facts(&txid).map(|f| f.0),
                "feerate": self.tx_facts(&txid).map(|f| f.2),
            }));
        }
        Ok(out)
    }

    /// Daily history series for the charts page: one bucket per UTC day
    /// with blocks, transactions, fees, subsidy paid, average difficulty
    /// and the day's average hashrate.
    ///
    /// Hashrate uses the node's own estimator: the work difference
    /// (`chainwork`) over the time difference, matching
    /// `getnetworkhashps`. The node's version is a windowed estimate over
    /// the last N blocks; here the same quantity is computed per day from
    /// the indexed chainwork, so the series is self-contained.
    pub fn daily_series(&self, days: i64) -> Result<serde_json::Value, String> {
        // Per-day block facts.
        let mut st = self
            .db
            .prepare(
                "SELECT (time / 86400) * 86400 AS day,
                        COUNT(*), SUM(ntx), SUM(fees), AVG(difficulty),
                        MIN(time), MAX(time), MIN(chainwork), MAX(chainwork)
                 FROM blocks
                 WHERE time > 0
                 GROUP BY day
                 ORDER BY day DESC
                 LIMIT ?1",
            )
            .map_err(|e| format!("daily series: {e}"))?;
        let rows = st
            .query_map([days.max(1)], |r| {
                Ok((
                    r.get::<_, i64>(0)?, // day
                    r.get::<_, i64>(1)?, // blocks
                    r.get::<_, i64>(2)?, // txs
                    r.get::<_, i64>(3)?, // fees
                    r.get::<_, f64>(4)?, // avg difficulty
                    r.get::<_, i64>(5)?, // first block time
                    r.get::<_, i64>(6)?, // last block time
                    r.get::<_, Option<String>>(7)?, // chainwork at first block
                    r.get::<_, Option<String>>(8)?, // chainwork at last block
                ))
            })
            .map_err(|e| format!("daily series: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("daily series: {e}"))?;
        drop(st);

        // Per-day supply minted (coinbase outputs) and unique addresses
        // touched — both from the outs index.
        let mut supply: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        {
            let mut s2 = self
                .db
                .prepare(
                    "SELECT (b.time / 86400) * 86400 AS day, COALESCE(SUM(o.value),0)
                     FROM outs o JOIN txs t ON t.txid = o.txid JOIN blocks b ON b.height = t.height
                     WHERE o.coinbase = 1
                     GROUP BY day",
                )
                .map_err(|e| format!("daily supply: {e}"))?;
            let r = s2
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|e| format!("daily supply: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("daily supply: {e}"))?;
            for (d, v) in r {
                supply.insert(d, v);
            }
        }
        let mut addrs: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        {
            let mut s3 = self
                .db
                .prepare(
                    "SELECT (b.time / 86400) * 86400 AS day, COUNT(DISTINCT o.address)
                     FROM outs o JOIN txs t ON t.txid = o.txid JOIN blocks b ON b.height = t.height
                     WHERE o.address IS NOT NULL
                     GROUP BY day",
                )
                .map_err(|e| format!("daily addrs: {e}"))?;
            let r = s3
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|e| format!("daily addrs: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("daily addrs: {e}"))?;
            for (d, v) in r {
                addrs.insert(d, v);
            }
        }

        let mut out = Vec::new();
        // rows are newest-first; walk oldest-first for a cumulative
        // supply curve.
        let mut cumulative: i64 = 0;
        let mut oldest_first: Vec<_> = rows.into_iter().collect();
        oldest_first.reverse();
        // Seed the cumulative total with everything before the window.
        // Supply = coins in existence: coinbases minus the fees this
        // chain burns (matches the node's `coinsupply`).
        if let Some(first_day) = oldest_first.first().map(|r| r.0) {
            let before: i64 = self
                .db
                .query_row(
                    "SELECT COALESCE((SELECT SUM(o.value) FROM outs o JOIN txs t ON t.txid = o.txid
                         JOIN blocks b ON b.height = t.height
                         WHERE o.coinbase = 1 AND b.time < ?1),0)
                       - COALESCE((SELECT SUM(fees) FROM blocks b WHERE b.time < ?1),0)",
                    [first_day],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            cumulative = before;
        }
        for (day, blocks, txs, fees, difficulty, t0, t1, cw0, cw1) in oldest_first {
            let minted = supply.get(&day).copied().unwrap_or(0);
            cumulative += minted - fees;
            // Average hashrate for the day: work delta ÷ time delta, the
            // same quantity `getnetworkhashps` reports.
            let hashrate = match (cw0, cw1) {
                (Some(a), Some(b)) if t1 > t0 => {
                    match (u128::from_str_radix(a.trim_start_matches("0x"), 16), u128::from_str_radix(b.trim_start_matches("0x"), 16)) {
                        (Ok(lo), Ok(hi)) if hi > lo => {
                            Some((hi - lo) as f64 / (t1 - t0) as f64)
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            out.push(serde_json::json!({
                "day": day, "blocks": blocks, "txs": txs, "fees": fees,
                "minted": minted, "supply": cumulative,
                "difficulty": difficulty, "hashrate": hashrate,
                "addresses": addrs.get(&day).copied().unwrap_or(0),
                "first_time": t0, "last_time": t1,
            }));
        }
        Ok(serde_json::json!({ "days": out }))
    }

    /// Chain-supply series (cumulative coins in existence) for the
    /// market view. Uses the same definition as the node's `coinsupply`:
    /// coins minted from coinbases, minus the transaction fees this chain
    /// burns.
    fn supply_curve(&self) -> Result<Vec<serde_json::Value>, String> {
        let mut st = self
            .db
            .prepare(
                "SELECT b.height, b.time,
                        COALESCE((SELECT SUM(o.value) FROM outs o
                             JOIN txs t ON t.txid = o.txid WHERE t.height = b.height AND o.coinbase = 1),0),
                        COALESCE(b.fees,0)
                 FROM blocks b ORDER BY b.height",
            )
            .map_err(|e| format!("supply curve: {e}"))?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| format!("supply curve: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("supply curve: {e}"))?;
        let mut cum: i64 = 0;
        let mut out = Vec::new();
        for (h, t, minted, fees) in rows {
            cum += minted - fees;
            out.push(serde_json::json!({"height": h, "time": t, "supply": cum}));
        }
        Ok(out)
    }

    /// Market page data: supply, liquidity, activity, and — only when a
    /// price feed is configured — price and market cap.
    ///
    /// With no exchange listing there is no price source, so the market
    /// view reports what the chain itself proves (supply, mined per day,
    /// active addresses, value moved) and says plainly that price is
    /// unavailable rather than inventing one.
    pub fn market(&mut self) -> Result<serde_json::Value, String> {
        let tip = self.height()?;
        // Circulating supply: coins in existence. This chain burns
        // transaction fees, so the unspent-output total is the correct
        // figure (it matches the node's `coinsupply` exactly); the raw
        // coinbase sum would be higher by the fees paid.
        let supply: i64 = self
            .db
            .query_row("SELECT COALESCE(SUM(value),0) FROM outs WHERE spent_by IS NULL", [], |r| r.get(0))
            .map_err(|e| format!("market supply: {e}"))?;
        // Value moved in the last 24h (non-coinbase outputs) and fees.
        let day_ago = self
            .db
            .query_row("SELECT COALESCE(MAX(time),0) - 86400 FROM blocks", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0);
        let (moved_24h, fees_24h, txs_24h, blocks_24h, addrs_24h): (i64, i64, i64, i64, i64) = self
            .db
            .query_row(
                "SELECT
                   COALESCE((SELECT SUM(o.value) FROM outs o JOIN txs t ON t.txid = o.txid
                             JOIN blocks b ON b.height = t.height
                             WHERE t.coinbase = 0 AND b.time >= ?1),0),
                   COALESCE((SELECT SUM(b.fees) FROM blocks b WHERE b.time >= ?1),0),
                   COALESCE((SELECT COUNT(*) FROM txs t JOIN blocks b ON b.height = t.height
                             WHERE t.coinbase = 0 AND b.time >= ?1),0),
                   COALESCE((SELECT COUNT(*) FROM blocks b WHERE b.time >= ?1),0),
                   COALESCE((SELECT COUNT(DISTINCT o.address) FROM outs o JOIN txs t ON t.txid = o.txid
                             JOIN blocks b ON b.height = t.height
                             WHERE b.time >= ?1),0)",
                [day_ago],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap_or((0, 0, 0, 0, 0));
        let holders: i64 = self
            .db
            .query_row(
                "SELECT COUNT(DISTINCT address) FROM outs WHERE address IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let utxos: i64 = self
            .db
            .query_row(
                "SELECT COUNT(*) FROM outs WHERE spent_by IS NULL AND address IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        // Price: only from a configured feed. None configured = no
        // price, and the UI says so.
        let price = self.cached_price();
        let market_cap = price.map(|p| p * supply as f64 / 100_000_000.0);
        let curve = self.supply_curve()?;
        Ok(serde_json::json!({
            "height": tip,
            "supply": supply,
            "holders": holders,
            "utxos": utxos,
            "moved_24h": moved_24h,
            "fees_24h": fees_24h,
            "txs_24h": txs_24h,
            "blocks_24h": blocks_24h,
            "addresses_24h": addrs_24h,
            "price": price,
            "price_source": self.price_source.clone(),
            "market_cap": market_cap,
            "supply_curve": curve,
        }))
    }

    /// Cached price for rendering (no I/O). The sync thread refreshes it.
    fn cached_price(&self) -> Option<f64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        match &self.price_cache {
            Some((p, at, _)) if now - at < 600 => Some(*p),
            _ => None,
        }
    }

    /// Refresh the price feed (sync thread only; cached ~10 min).
    fn refresh_price(&mut self) {
        let _ = self.price();
    }

    /// Fetch the configured price feed (cached ~10 min). Returns None
    /// when no feed is configured or the fetch failed — the market view
    /// then shows no price at all.
    fn price(&mut self) -> Option<f64> {
        let url = self.price_url.clone()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Some((p, at, _)) = &self.price_cache {
            if now - at < 600 {
                return Some(*p);
            }
        }
        let v = http_get_json(&url).ok()?;
        // Accept {"price": 0.123} or {"ticker": {"price": ...}} shapes.
        let p = v
            .get("price")
            .or_else(|| v.get("ticker").and_then(|t| t.get("price")))
            .or_else(|| v.get("last"))
            .and_then(|x| x.as_f64())
            .filter(|x| x.is_finite() && *x > 0.0)?;
        let source = v
            .get("source")
            .or_else(|| v.get("ticker").and_then(|t| t.get("source")))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .or_else(|| self.price_source.clone())
            .unwrap_or_else(|| url.clone());
        self.price_cache = Some((p, now, source));
        Some(p)
    }

    /// Per-tx facts captured at index time: exact size, fee, fee rate
    /// (sat/vB). None when the node could not serve the raw tx.
    pub fn tx_facts(&self, txid: &str) -> Option<(i64, i64, f64)> {
        self.db
            .query_row(
                "SELECT size, fee, feerate FROM tx_facts WHERE txid = ?1",
                [txid],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?)),
            )
            .ok()
    }

    /// Top funded addresses (unspent only). Small-chain GROUP BY — exact
    /// at index time; the caller labels the snapshot height.
    pub fn richlist(&self, limit: i64) -> Result<Vec<serde_json::Value>, String> {
        let mut st = self
            .db
            .prepare(
                "SELECT address, SUM(value) AS b FROM outs
                 WHERE spent_by IS NULL AND address IS NOT NULL
                 GROUP BY address ORDER BY b DESC LIMIT ?1",
            )
            .map_err(|e| format!("richlist: {e}"))?;
        let rows = st
            .query_map([limit], |r| {
                Ok(serde_json::json!({
                    "address": r.get::<_, String>(0)?,
                    "balance": r.get::<_, i64>(1)?,
                }))
            })
            .map_err(|e| format!("richlist: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("richlist: {e}"))?;
        Ok(rows)
    }

    /// Live mempool snapshot (node RPC, best-effort): count/bytes plus a
    /// bounded entry list (fee from the confirmed index, size from the
    /// raw hex, first-seen tracked locally since the fork's verbose tx
    /// carries no timestamps). No stored history — the mempool is the
    /// node's, not the chain's.
    fn fetch_mempool(&mut self) -> Result<serde_json::Value, String> {
        let info = self.rpc.call("getmempoolinfo", serde_json::json!([]))?;
        let ids: Vec<String> = self
            .rpc
            .call("getrawmempool", serde_json::json!([]))
            .map(|v| {
                v.as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let seen = self.track_mempool_seen(&ids);
        let mut entries = Vec::new();
        for txid in ids.iter().take(50) {
            // Verbose tx (works for mempool entries too); fees resolve
            // against the confirmed index — null when an input isn't
            // indexed yet (mempool chains).
            let v = match self
                .rpc
                .call("getrawtransaction", serde_json::json!([txid, 1]))
            {
                Ok(v) => v,
                Err(_) => continue,
            };
            let mut in_total: i64 = 0;
            let mut out_total: i64 = 0;
            let mut known = true;
            for inp in v
                .get("vin")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if inp.get("coinbase").is_some() {
                    continue;
                }
                let (pt, pn) = (
                    inp.get("txid").and_then(|x| x.as_str()).unwrap_or(""),
                    inp.get("vout").and_then(|x| x.as_u64()).unwrap_or(0) as i64,
                );
                match self.db.query_row(
                    "SELECT value FROM outs WHERE txid = ?1 AND n = ?2",
                    rusqlite::params![pt, pn],
                    |r| r.get::<_, i64>(0),
                ) {
                    Ok(val) => in_total += val,
                    Err(_) => {
                        known = false;
                        break;
                    }
                }
            }
            for o in v
                .get("vout")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default()
            {
                out_total += o.get("valueSat").and_then(|x| x.as_i64()).unwrap_or_else(|| {
                    (o.get("value").and_then(|x| x.as_f64()).unwrap_or(0.0) * 100_000_000.0).round() as i64
                });
            }
            // No segwit on this chain: vsize == size == raw byte length.
            let vsize = v
                .get("hex")
                .and_then(|h| h.as_str())
                .map(|h| h.len() as u64 / 2)
                .unwrap_or(0);
            entries.push(serde_json::json!({
                "txid": txid,
                "fee": if known { serde_json::json!(in_total - out_total) } else { serde_json::Value::Null },
                "vsize": vsize,
                "time": seen.get(txid).cloned().unwrap_or(0),
            }));
        }
        Ok(serde_json::json!({
            "size": info.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
            "bytes": info.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0),
            "usage": info.get("usage").and_then(|v| v.as_u64()).unwrap_or(0),
            "shown": entries.len(),
            "truncated": ids.len() > entries.len(),
            "entries": entries,
        }))
    }

    /// First-seen tracking for mempool txids (the fork's verbose tx has
    /// no timestamps): record newly observed ids, forget ones that left
    /// the mempool. Best-effort — any failure just yields an empty map
    /// (entries render "–" for age). No writes when empty (common case).
    fn track_mempool_seen(&mut self, ids: &[String]) -> std::collections::HashMap<String, i64> {
        let mut seen = std::collections::HashMap::new();
        if ids.is_empty() {
            return seen;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let tx = match self.db.transaction() {
            Ok(t) => t,
            Err(_) => return seen,
        };
        for txid in ids {
            let _ = tx.execute(
                "INSERT OR IGNORE INTO mempool_seen(txid, first_seen) VALUES (?1, ?2)",
                rusqlite::params![txid, now],
            );
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM mempool_seen WHERE txid NOT IN ({placeholders})");
        let refs: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let pruned = tx
            .prepare(&sql)
            .and_then(|mut st| st.execute(refs.as_slice()))
            .is_ok();
        if !pruned || tx.commit().is_err() {
            return seen;
        }
        if let Ok(mut st) = self.db.prepare("SELECT txid, first_seen FROM mempool_seen") {
            if let Ok(rows) =
                st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            {
                for r in rows.flatten() {
                    seen.insert(r.0, r.1);
                }
            }
        }
        seen
    }

    /// Live network snapshot (node RPC, best-effort): peer count plus the
    /// node's own hashrate estimate.
    /// Node reachability for UI banners, from the sync thread's last
    /// probe (no per-request RPC). Pages that would otherwise render
    /// silent zeros use this to say the node is down.
    pub fn node_reachable(&self) -> bool {
        self.cache_node_ok
    }

    fn fetch_network(&self) -> Result<serde_json::Value, String> {
        let peers = self
            .rpc
            .call("getconnectioncount", serde_json::json!([]))
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let hashrate = self
            .rpc
            .call("getnetworkhashps", serde_json::json!([]))
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        Ok(serde_json::json!({"peers": peers, "hashrate": hashrate}))
    }

    // ---- cached node views for request threads ----
    //
    // These return the sync thread's last snapshot instead of calling the
    // node: a slow/unreachable node used to queue every page view behind
    // its RPC timeout while the index lock was held.

    pub fn network(&self) -> Result<serde_json::Value, String> {
        Ok(self.cache_net.clone())
    }

    pub fn mempool(&mut self) -> Result<serde_json::Value, String> {
        Ok(self.cache_mempool.clone())
    }

    pub fn game_state(&self) -> Result<serde_json::Value, String> {
        Ok(self.cache_game.clone())
    }

    pub fn staking_state(&self) -> Result<serde_json::Value, String> {
        Ok(self.cache_staking.clone())
    }

    /// Locked emission schedule, base units (chainparams: testnet mirrors
    /// mainnet; verified live — getblocksubsidy h100 → 0.15873015 exact,
    /// h10081 → 16, h1062000 → 8, h4217760 → 1). Era 1 (0–10080) ramps
    /// 0→16 linearly; era 2 (10081–4217760) halves every 1,051,920
    /// (16/8/4/2, the node already applies the 4th halving at exactly
    /// 4217760); era 3 is the fixed 1-coin tail forever.
    #[must_use]
    pub fn subsidy_at(height: u64) -> u64 {
        // One shared mirror of the locked schedule (lm-rewards::emission).
        lm_rewards::emission::subsidy(height)
    }

    /// Next subsidy change after `height`: (change_height, new_subsidy).
    /// None once the fixed tail is reached.
    #[must_use]
    pub fn next_subsidy_change(height: u64) -> Option<(u64, u64)> {
        use lm_rewards::emission::{ERA2_END, ERA2_REWARD as PEAK, HALVING, RAMP_END};
        if height < RAMP_END {
            Some((RAMP_END + 1, PEAK))
        } else if height <= ERA2_END {
            let k = (height - RAMP_END) / HALVING;
            let next_h = RAMP_END + (k + 1) * HALVING;
            if next_h > ERA2_END {
                Some((ERA2_END + 1, 100_000_000))
            } else {
                Some((next_h, PEAK >> (k + 1).min(63)))
            }
        } else {
            None
        }
    }

    /// Live game state from the node (reserve/pot/drought/divisors).
    /// Raw transaction hex, fetched from the node on demand (the /tx/raw
    /// route). Not cached: raw hex is only requested when a user asks.
    pub fn raw_tx(&self, txid: &str) -> Result<String, String> {
        self.rpc
            .call("getrawtransaction", serde_json::json!([txid, 0]))?
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "node returned no hex".to_string())
    }

    /// Current consensus Luckynode Draw as last mirrored from the pool
    /// (epoch 0 = no pool sync yet / pre-draw node).
    pub fn draw_state(&self) -> serde_json::Value {
        self.db
            .query_row(
                "SELECT epoch, budget, remaining, winner, synced_at FROM draw_state WHERE id = 1",
                [],
                |r| {
                    Ok(serde_json::json!({
                        "epoch": r.get::<_, i64>(0)?,
                        "budget": r.get::<_, i64>(1)?,
                        "remaining": r.get::<_, i64>(2)?,
                        "winner": r.get::<_, String>(3)?,
                        "synced_at": r.get::<_, i64>(4)?,
                    }))
                },
            )
            .unwrap_or_else(|_| serde_json::json!({"epoch": 0}))
    }

    fn fetch_game_state(&self) -> Result<serde_json::Value, String> {
        let ji = self.rpc.call("getjackpotinfo", serde_json::json!([]))?;
        Ok(serde_json::json!({
            "height": ji.get("height").and_then(|v| v.as_i64()).unwrap_or(-1),
            "reserve": ji.get("reserve").and_then(|v| v.as_i64()).unwrap_or(0),
            "pot": ji.get("pot").and_then(|v| v.as_i64()).unwrap_or(0),
            "drought": ji.get("drought").and_then(|v| v.as_i64()).unwrap_or(0),
            "potdivisor": ji.get("potdivisor").and_then(|v| v.as_i64()).unwrap_or(1000),
            "jackpotdivisor": ji.get("jackpotdivisor").and_then(|v| v.as_i64()).unwrap_or(100),
            "potdrought": ji.get("potdrought").and_then(|v| v.as_i64()).unwrap_or(4320),
            "faucet": ji.get("faucet").and_then(|v| v.as_i64()).unwrap_or(0),
            "dev": ji.get("dev").and_then(|v| v.as_i64()).unwrap_or(0),
            "prize": ji.get("prize").and_then(|v| v.as_i64()).unwrap_or(0),
            "prizeepochlen": ji.get("prizeepochlen").and_then(|v| v.as_i64()).unwrap_or(0),
        }))
    }

    /// Live node staking state (not indexed — read through on each call).
    /// PUBLIC surface: on/off + chain supply only. Operator-wallet shape
    /// (eligible outputs/balance) is deliberately NOT published — it maps
    /// an idle hot wallet.
    fn fetch_staking_state(&self) -> Result<serde_json::Value, String> {
        let mining = self.rpc.call("getmininginfo", serde_json::json!([]))?;
        let coins = |v: Option<&serde_json::Value>| -> i64 {
            (v.and_then(|x| x.as_f64()).unwrap_or(0.0) * 100_000_000.0).round() as i64
        };
        Ok(serde_json::json!({
            "staking_on": mining.get("staking").and_then(|v| v.as_bool()).unwrap_or(false),
            "staking_supply": coins(mining.get("stakingsupply")),
        }))
    }

    /// Seasonal division boards as last synced from the pool (ladder order:
    /// phone, cpu, gpu, farm; ranks ascending within each class). Empty
    /// before the first pool sync or while the pool sends nothing.
    pub fn division_boards(&self) -> Result<Vec<serde_json::Value>, String> {
        const LADDER: [&str; 4] = ["phone", "cpu", "gpu", "farm"];
        let mut st = self
            .db
            .prepare(
                "SELECT rank, miner, name, eff_work, days, streak, attested, synced_at
                 FROM division_boards WHERE class = ?1 ORDER BY rank LIMIT 100",
            )
            .map_err(|e| format!("division boards: {e}"))?;
        let mut out = Vec::new();
        for class in LADDER {
            let entries = st
                .query_map([class], |r| {
                    Ok(serde_json::json!({
                        "rank": r.get::<_, i64>(0)?, "miner": r.get::<_, String>(1)?,
                        "name": r.get::<_, String>(2)?, "eff_work": r.get::<_, i64>(3)?,
                        "days": r.get::<_, i64>(4)?, "streak": r.get::<_, i64>(5)?,
                        "attested": r.get::<_, i64>(6)? != 0,
                        "synced_at": r.get::<_, i64>(7)?,
                    }))
                })
                .map_err(|e| format!("division boards: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("division boards: {e}"))?;
            out.push(serde_json::json!({
                "class": class,
                "size": entries.len(),
                "entries": entries,
                "synced_at": entries.first().and_then(|e| e.get("synced_at")).cloned().unwrap_or(serde_json::Value::Null),
            }));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_query_uses_both_union_parameters() {
        // Regression: the history UNION numbers its placeholders ?1/?2;
        // duplicating ?1 made every address lookup fail.
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-unit-test.db");
        let _ = std::fs::remove_file(&dir);
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        idx.db
            .execute_batch(
                "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought)
                 VALUES (0, 'h0', 0, 1, 1.0, 0, 0, 0), (1, 'h1', 0, 1, 1.0, 0, 0, 0);
                 INSERT INTO txs(txid, height, coinbase) VALUES ('cb0', 0, 1), ('spend1', 1, 0);
                 INSERT INTO outs(txid, n, address, value, coinbase, spent_by, spent_n)
                 VALUES ('cb0', 0, 'A1', 100, 1, 'spend1', 0);",
            )
            .expect("fixture");
        let a = idx.address("A1").expect("address must not error");
        assert_eq!(a["balance"], 0);
        assert_eq!(a["received"], 100);
        assert_eq!(a["txs"].as_array().map(|v| v.len()).unwrap_or(0), 2);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn game_tables_roundtrip() {
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-game-test.db");
        let _ = std::fs::remove_file(&dir);
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        idx.db
            .execute_batch(
                "INSERT INTO game_blocks(height, bonus, pot, prize, finder)
                 VALUES (8, 6349205, 0, 0, 'dbg5'), (9, 0, 0, 57143, 'dbg5'),
                        (10, 0, 0, 63492, 'dbg5');",
            )
            .expect("fixture");
        let b = idx.game_block(8).expect("game block").expect("row");
        assert_eq!(b["bonus"], 6349205);
        let list = idx.game_blocks_recent(10).expect("list");
        assert_eq!(list.len(), 3);
        assert_eq!(list[0]["height"], 10); // newest first
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn prize_claims_roundtrip_through_pool_upsert() {
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-claim-test.db");
        let _ = std::fs::remove_file(&dir);
        let mut idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        // Pool-mined epoch-boundary block carrying the directed claim.
        idx.upsert_game_block(12, 0, 0, 1_391_276, "mx5tDPWs", 100_413_533, "mxPJ3Mn5")
            .expect("upsert claim");
        // Plain pool block: no claim.
        idx.upsert_game_block(13, 0, 0, 1_385_727, "mx5tDPWs", 0, "")
            .expect("upsert plain");
        // External find: prize-only row gains claim/winner columns at defaults.
        idx.apply_prize_credits(&[(14, 1_380_000)]).expect("credits");

        let claims = idx.game_claims_recent(50).expect("claims");
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0]["height"], 12);
        assert_eq!(claims[0]["claim"], 100_413_533);
        assert_eq!(claims[0]["winner"], "mxPJ3Mn5");

        let g12 = idx.game_block(12).expect("g12").expect("row");
        assert_eq!(g12["claim"], 100_413_533);
        assert_eq!(g12["winner"], "mxPJ3Mn5");
        let g14 = idx.game_block(14).expect("g14").expect("row");
        assert_eq!(g14["claim"], 0);
        assert_eq!(g14["winner"], "");
        assert_eq!(g14["finder"], "external");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn pre_feature_game_blocks_gain_claim_and_winner() {
        // Migration path: a DB written before the prize-draw columns
        // existed (no `claim`/`winner` on game_blocks) must open cleanly,
        // keep its rows at column defaults, and accept new upserts.
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-migrate-test.db");
        let _ = std::fs::remove_file(&dir);
        {
            let db = Connection::open(&dir).expect("raw open");
            db.execute_batch(
                "CREATE TABLE game_blocks(
                   height INTEGER PRIMARY KEY, bonus INTEGER NOT NULL,
                   pot INTEGER NOT NULL, prize INTEGER NOT NULL, finder TEXT NOT NULL);
                 INSERT INTO game_blocks(height, bonus, pot, prize, finder)
                 VALUES (7, 100, 200, 300, 'old-finder');",
            )
            .expect("old-schema fixture");
        }
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open migrates");
        let b = idx.game_block(7).expect("game block").expect("row survives");
        assert_eq!(b["bonus"], 100);
        assert_eq!(b["claim"], 0);
        assert_eq!(b["winner"], "");
        idx.upsert_game_block(7, 100, 200, 300, "old-finder", 55, "w1")
            .expect("upsert on migrated row");
        let b2 = idx.game_block(7).expect("game block").expect("row");
        assert_eq!(b2["claim"], 55);
        assert_eq!(b2["winner"], "w1");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn subsidy_schedule_pins_locked_chainparams() {
        // Locked emission schedule (chainparams, testnet mirrors mainnet),
        // verified live via getblocksubsidy: h100 → 0.15873015 exact.
        assert_eq!(Indexer::subsidy_at(0), 0); // genesis pays nothing
        assert_eq!(Indexer::subsidy_at(100), 15_873_015); // 16*100/10080
        assert_eq!(Indexer::subsidy_at(10_080), 1_600_000_000);
        assert_eq!(Indexer::subsidy_at(10_081), 1_600_000_000); // full 16
        assert_eq!(Indexer::subsidy_at(1_062_000), 800_000_000); // →8
        assert_eq!(Indexer::subsidy_at(2_114_040), 400_000_000); // →4
        assert_eq!(Indexer::subsidy_at(3_166_080), 200_000_000); // →2
        assert_eq!(Indexer::subsidy_at(4_217_759), 200_000_000);
        assert_eq!(Indexer::subsidy_at(4_217_760), 100_000_000); // era edge: node already applies the 4th halving
        assert_eq!(Indexer::subsidy_at(4_217_761), 100_000_000); // fixed tail
        assert_eq!(Indexer::subsidy_at(99_999_999), 100_000_000);
        // Next-change countdown.
        assert_eq!(Indexer::next_subsidy_change(100), Some((10_081, 1_600_000_000)));
        assert_eq!(
            Indexer::next_subsidy_change(10_081),
            Some((1_062_000, 800_000_000))
        );
        assert_eq!(
            Indexer::next_subsidy_change(4_217_760),
            Some((4_217_761, 100_000_000))
        );
        assert_eq!(Indexer::next_subsidy_change(4_217_761), None);
        assert_eq!(Indexer::next_subsidy_change(99_999_999), None);
    }

    #[test]
    fn block_meta_richlist_recent_txs_and_address_counts() {
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-meta-test.db");
        let _ = std::fs::remove_file(&dir);
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        idx.db
            .execute_batch(
                "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot)
                 VALUES (0, 'h0', 1000, 1, 1.0, 0, 0, 0, 0, 215, 'm0'),
                        (1, 'h1', 1060, 2, 2.0, 0, 0, 0, 0, 300, 'm1');
                 INSERT INTO txs(txid, height, coinbase) VALUES ('cb0', 0, 1), ('spend1', 1, 0), ('cb1', 1, 1);
                 INSERT INTO outs(txid, n, address, value, coinbase, spent_by, spent_n)
                 VALUES ('cb0', 0, 'A1', 100, 1, 'spend1', 0),
                        ('cb1', 0, 'A2', 500, 1, NULL, NULL),
                        ('spend1', 0, 'A1', 90, 0, NULL, NULL),
                        ('spend1', 1, 'B9', 5, 0, NULL, NULL);",
            )
            .expect("fixture");
        // Block meta round-trips (backfill path needs a node; the direct
        // indexed path is what ships here).
        let b = idx.block_by_height(1).expect("block").expect("row");
        assert_eq!(b["size"], 300);
        assert_eq!(b["merkleroot"], "m1");
        assert_eq!(b["fees"], 5); // 100 in - (90 + 5) out
        let b0 = idx.block_by_height(0).expect("block").expect("row");
        assert_eq!(b0["fees"], 0); // coinbase has no inputs
        // Recent txs: newest first, with values and fees.
        let recent = idx.recent_txs(10).expect("recent");
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0]["txid"], "cb1");
        assert_eq!(recent[0]["value"], 500);
        let spend = recent.iter().find(|t| t["txid"] == "spend1").expect("spend");
        assert_eq!(spend["fee"], 5);
        assert_eq!(spend["height"], 1);
        // Rich list: unspent only, biggest first (B9's 5 + A1's 90 + A2's 500).
        let rich = idx.richlist(100).expect("rich");
        assert_eq!(rich.len(), 3);
        assert_eq!(rich[0]["address"], "A2");
        assert_eq!(rich[0]["balance"], 500);
        // Address extras: distinct tx count, first seen, unconfirmed null
        // (dead endpoint has no address-mempool index).
        let a = idx.address("A1").expect("address");
        assert_eq!(a["txcount"], 2); // cb0 received + spend1 spent
        assert_eq!(a["first_seen"], 0);
        assert!(a["unconfirmed"].is_null());
        // Tx view carries the forward spend edge (powers "spent →" links).
        let cb0 = idx.tx("cb0").expect("tx").expect("row");
        assert_eq!(cb0["vout"][0]["spent_by"], "spend1");
        let spend1 = idx.tx("spend1").expect("tx").expect("row");
        assert!(spend1["vout"][0]["spent_by"].is_null());
        assert!(spend1["vout"][1]["spent_by"].is_null());
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn daily_series_buckets_by_day_and_derives_hashrate_from_chainwork() {
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-series-test.db");
        let _ = std::fs::remove_file(&dir);
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        let day = 1_700_000_000_i64 / 86400 * 86400;
        idx.db
            .execute_batch(&format!(
                "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot, chainwork, fees)
                 VALUES (0, 'h0', {day}, 1, 10.0, 0, 0, 0, 0, 200, 'm0', '00000000000000000000000000000000000000000000000000000000000003e8', 0),
                        (1, 'h1', {}, 2, 20.0, 0, 0, 0, 0, 300, 'm1', '00000000000000000000000000000000000000000000000000000000000007d0', 500),
                        (2, 'h2', {}, 1, 30.0, 0, 0, 0, 0, 250, 'm2', '0000000000000000000000000000000000000000000000000000000000000bb8', 100);
                 INSERT INTO txs(txid, height, coinbase) VALUES ('cb0', 0, 1), ('cb1', 1, 1), ('spend1', 1, 0), ('cb2', 2, 1);
                 INSERT INTO outs(txid, n, address, value, coinbase, spent_by, spent_n)
                 VALUES ('cb0', 0, 'A1', 1000, 1, NULL, NULL),
                        ('cb1', 0, 'A2', 2000, 1, NULL, NULL),
                        ('spend1', 0, 'A1', 900, 0, NULL, NULL),
                        ('spend1', 1, 'B9', 100, 0, NULL, NULL),
                        ('cb2', 0, 'A3', 3000, 1, NULL, NULL);",
                day + 60,
                day + 120
            ))
            .expect("fixture");
        let s = idx.daily_series(30).expect("series");
        let days = s["days"].as_array().expect("days array");
        assert_eq!(days.len(), 1, "one UTC day bucket");
        let d = &days[0];
        assert_eq!(d["blocks"], 3);
        // blocks.ntx summed: 1 + 2 + 1 = 4 txs (cb0, cb1, spend1, cb2).
        assert_eq!(d["txs"], 4);
        assert_eq!(d["fees"], 600);
        // Minted = coinbase outputs that day (1000 + 2000 + 3000).
        assert_eq!(d["minted"], 6000);
        // Cumulative supply = coins in existence: minted minus the fees
        // this chain burns (600). The node's `coinsupply` uses the same
        // definition — the raw coinbase sum would be 600 too high.
        assert_eq!(d["supply"], 5400);
        // Addresses touched that day.
        assert_eq!(d["addresses"], 4);
        // Hashrate: work delta (0xbb8 - 0x3e8 = 2000) over the day's
        // time span (120 s) — the same work/time quantity the node uses.
        let hr = d["hashrate"].as_f64().expect("hashrate");
        assert!((hr - 2000.0 / 120.0).abs() < 1e-9, "hashrate {hr}");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn market_reports_supply_and_no_price_without_a_feed() {
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-market-test.db");
        let _ = std::fs::remove_file(&dir);
        let mut idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        idx.db
            .execute_batch(
                "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot, chainwork, fees)
                 VALUES (0, 'h0', 1000, 1, 1.0, 0, 0, 0, 0, 200, 'm0', '0x1', 0);
                 INSERT INTO txs(txid, height, coinbase) VALUES ('cb0', 0, 1);
                 INSERT INTO outs(txid, n, address, value, coinbase, spent_by, spent_n)
                 VALUES ('cb0', 0, 'A1', 4000, 1, NULL, NULL);",
            )
            .expect("fixture");
        let m = idx.market().expect("market");
        // Supply = coins in existence (unspent outputs). This chain burns
        // fees, so the coinbase sum is NOT the supply — the node's
        // `coinsupply` reports the unspent total.
        assert_eq!(m["supply"], 4000);
        assert_eq!(m["holders"], 1);
        assert_eq!(m["utxos"], 1);
        // No price feed configured → no price, no market cap. The page
        // must say "unavailable" rather than invent a number.
        assert!(m["price"].is_null());
        assert!(m["market_cap"].is_null());
        // The supply curve is present for the chart.
        assert_eq!(m["supply_curve"].as_array().map(|c| c.len()), Some(1));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn prize_credits_cover_external_finds_without_clobbering_rewards() {
        // A staker/peer-found block never appears in the pool's
        // `last_blocks`, so its prize inflow is only visible through the
        // `prize.credits` feed. Applying it must create the row (so the
        // block's inflow shows) and must not disturb bonus/pot/finder on
        // a block the pool DID mine.
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-prize-credit-test.db");
        let _ = std::fs::remove_file(&dir);
        let mut idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        idx.db
            .execute_batch(
                "INSERT INTO game_blocks(height, bonus, pot, prize, finder)
                 VALUES (7, 111, 22, 0, 'poolminer');",
            )
            .expect("fixture");
        let n = idx
            .apply_prize_credits(&[(7, 555), (290, 1_841_270)])
            .expect("apply");
        assert_eq!(n, 2);
        // Pool-found row: prize updated, rewards preserved.
        let g7 = idx.game_block(7).expect("g7").expect("row");
        assert_eq!((g7["prize"].as_i64(), g7["bonus"].as_i64(), g7["pot"].as_i64()), (Some(555), Some(111), Some(22)));
        assert_eq!(g7["finder"], "poolminer");
        // External-find row: created with prize only.
        let g290 = idx.game_block(290).expect("g290").expect("row");
        assert_eq!(g290["prize"], 1_841_270);
        assert_eq!((g290["bonus"].as_i64(), g290["pot"].as_i64()), (Some(0), Some(0)));
        // New rows identify themselves as non-pool finds.
        assert_eq!(g290["finder"], "external");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn tx_facts_carry_exact_size_and_fee_rate() {
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-facts-test.db");
        let _ = std::fs::remove_file(&dir);
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        idx.db
            .execute_batch(
                "INSERT INTO tx_facts(txid, size, fee, feerate, height)
                 VALUES ('t1', 200, 1000, 5.0, 7);",
            )
            .expect("fixture");
        let f = idx.tx_facts("t1").expect("facts");
        assert_eq!(f.0, 200);
        assert_eq!(f.1, 1000);
        assert_eq!(f.2, 5.0);
        assert!(idx.tx_facts("missing").is_none());
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn supply_counts_coins_in_existence_not_burned_fees() {
        // Regression: this chain BURNS transaction fees (the node's
        // GetNewCoins subtracts them, and `coinsupply` reports the
        // unspent-output total). Counting the full coinbase sum
        // overstated supply by every fee ever paid — 430k sats on the
        // testnet chain at the time this was found.
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-supply-test.db");
        let _ = std::fs::remove_file(&dir);
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        idx.db
            .execute_batch(
                "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot, chainwork, fees)
                 VALUES (0, 'h0', 1000, 1, 1.0, 0, 0, 0, 0, 200, 'm0', '0x1', 0),
                        (1, 'h1', 1060, 1, 1.0, 0, 0, 0, 0, 200, 'm1', '0x2', 100);
                 INSERT INTO txs(txid, height, coinbase) VALUES ('cb0', 0, 1), ('spend1', 1, 0);
                 INSERT INTO outs(txid, n, address, value, coinbase, spent_by, spent_n)
                 VALUES ('cb0', 0, 'A1', 1000, 1, 'spend1', 0),
                        ('spend1', 0, 'A1', 900, 0, NULL, NULL);",
            )
            .expect("fixture");
        // 1000 minted, 100 burned as fee → 900 in existence.
        let stats = idx.stats().expect("stats");
        assert_eq!(stats["supply"], 900);
        // The market view must use the same definition.
        let mut idx2 = Indexer::open(dir.to_str().expect("path"), RpcClient::new("127.0.0.1:1", "u", "p")).expect("open");
        let m = idx2.market().expect("market");
        assert_eq!(m["supply"], 900);
        // The supply curve ends at the same figure.
        let curve = m["supply_curve"].as_array().expect("curve");
        assert_eq!(curve.last().expect("last")["supply"], 900);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn rollback_clears_per_tx_facts_with_the_block() {
        // Regression: rollback_height deleted blocks/txs/outs but left
        // tx_facts rows behind. They are invisible without a txs row,
        // but dead rows must not accumulate across reorgs.
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-rollback-test.db");
        let _ = std::fs::remove_file(&dir);
        let mut idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        idx.db
            .execute_batch(
                "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot, chainwork, fees)
                 VALUES (0, 'h0', 1000, 1, 1.0, 0, 0, 0, 0, 200, 'm0', '0x1', 0);
                 INSERT INTO txs(txid, height, coinbase) VALUES ('cb0', 0, 1);
                 INSERT INTO outs(txid, n, address, value, coinbase, spent_by, spent_n)
                 VALUES ('cb0', 0, 'A1', 1000, 1, NULL, NULL);
                 INSERT INTO tx_facts(txid, size, fee, feerate, height)
                 VALUES ('orphan', 225, 100, 0.44, 0);",
            )
            .expect("fixture");
        idx.rollback_height(0).expect("rollback");
        let left: i64 = idx
            .db
            .query_row("SELECT COUNT(*) FROM tx_facts WHERE height = 0", [], |r| r.get(0))
            .expect("count");
        assert_eq!(left, 0);
        let _ = std::fs::remove_file(&dir);
    }


    #[test]
    fn feed_url_splitting_handles_both_schemes() {
        // Plain HTTP keeps the default port; SNI form carries no port.
        let (tls, host, addr, hp, path) = split_feed_url("http://127.0.0.1:8081/api/stats").expect("http");
        assert!(!tls);
        assert_eq!(host, "127.0.0.1");
        assert_eq!(addr, "127.0.0.1:8081");
        assert_eq!(hp, "127.0.0.1:8081");
        assert_eq!(path, "/api/stats");
        // Bare host gets the scheme default port.
        let (tls, host, addr, _, path) = split_feed_url("http://example.com").expect("bare");
        assert!(!tls && host == "example.com" && addr == "example.com:80" && path == "/");
        // HTTPS defaults to 443 with a bare hostname for SNI.
        let (tls, host, addr, hp, path) = split_feed_url("https://price.example.com/ticker").expect("https");
        assert!(tls);
        assert_eq!(host, "price.example.com");
        assert_eq!(addr, "price.example.com:443");
        assert_eq!(hp, "price.example.com");
        assert_eq!(path, "/ticker");
        // Explicit ports (incl. non-standard https) survive untouched.
        let (tls, host, addr, _, _) = split_feed_url("https://10.0.0.5:8443/x").expect("port");
        assert!(tls && host == "10.0.0.5" && addr == "10.0.0.5:8443");
        // Anything else is rejected rather than fetched.
        assert!(split_feed_url("ftp://x/y").is_err());
        assert!(split_feed_url("https:///x").is_err());
    }

    #[test]
    fn division_boards_roundtrip_in_ladder_order() {
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-division-test.db");
        let _ = std::fs::remove_file(&dir);
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        // Empty before the first pool sync.
        let boards = idx.division_boards().expect("boards");
        assert_eq!(boards.len(), 4);
        assert!(boards.iter().all(|b| b["entries"].as_array().map(|e| e.is_empty()).unwrap_or(false)));
        idx.db
            .execute_batch(
                "INSERT INTO division_boards(class, rank, miner, name, eff_work, days, streak, attested, synced_at)
                 VALUES ('gpu', 1, 'g1', 'g1', 400, 3, 2, 0, 7),
                        ('phone', 2, 'a2', 'addr-a2', 500, 5, 5, 1, 7),
                        ('phone', 1, 'a1', 'a1', 900, 6, 6, 0, 7);",
            )
            .expect("fixture");
        let boards = idx.division_boards().expect("boards");
        // Ladder order: phone first even though gpu was inserted first.
        assert_eq!(boards[0]["class"], "phone");
        assert_eq!(boards[0]["size"], 2);
        assert_eq!(boards[0]["entries"][0]["miner"], "a1");
        assert_eq!(boards[0]["entries"][1]["attested"], true);
        assert_eq!(boards[1]["class"], "cpu");
        assert!(boards[1]["entries"].as_array().map(|e| e.is_empty()).unwrap_or(false));
        assert_eq!(boards[2]["class"], "gpu");
        assert_eq!(boards[2]["entries"][0]["streak"], 2);
        let _ = std::fs::remove_file(&dir);
    }
}
