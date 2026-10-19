//! Tip-following chain indexer (SQLite): blocks, transactions, address
//! balances — everything the explorer UI/API serves, built from plain
//! node RPC (`getblock` verbose + `getjackpotinfo`). No `-txindex`
//! needed: the index is a full walk from genesis, then tip-following.
//!
//! Money rules: an output is *unspent* until a later indexed input
//! references it (`spent_by`/`spent_n`). Address balance = sum of
//! unspent outputs. Supply = sum of all coinbase outputs ever (unclaimed
//! bonuses/splashes are never minted, so this is exact).

use lm_stratum::rpc::RpcClient;
use rusqlite::{params, Connection};

use std::io::{Read, Write};
use std::net::TcpStream;

pub struct Indexer {
    db: Connection,
    rpc: RpcClient,
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub rolled_back: u64,
    pub indexed: u64,
    pub tip: u64,
}

/// One mirrored league-board row (pool snapshot → `league_boards`).
struct LeagueBoardRow {
    class: String,
    rank: i64,
    miner: String,
    name: String,
    eff_work: i64,
    days: i64,
    streak: i64,
    attested: i64,
}

/// Minimal HTTP GET (std only) → parsed JSON. The explorer reads the
/// pool's dashboard API for game events; everything else is node RPC.
fn http_get_json(url: &str) -> Result<serde_json::Value, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// supported: {url}"))?;
    let (host_port, path) = match rest.split_once('/') {
        Some((hp, p)) => (hp, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let addr = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    };
    let stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    (&stream)
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut raw = Vec::new();
    (&stream)
        .read_to_end(&mut raw)
        .map_err(|e| format!("read: {e}"))?;
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
           pot INTEGER NOT NULL, league INTEGER NOT NULL, finder TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS draws(
            epoch INTEGER PRIMARY KEY, height INTEGER NOT NULL, seed TEXT NOT NULL,
            winner TEXT NOT NULL, budget INTEGER NOT NULL, txid TEXT NOT NULL,
            cosigns INTEGER NOT NULL, time INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS league_boards(
            class TEXT NOT NULL, rank INTEGER NOT NULL, miner TEXT NOT NULL,
            name TEXT NOT NULL, eff_work INTEGER NOT NULL, days INTEGER NOT NULL,
            streak INTEGER NOT NULL, attested INTEGER NOT NULL DEFAULT 0,
            synced_at INTEGER NOT NULL,
            PRIMARY KEY(class, rank));
          CREATE TABLE IF NOT EXISTS mempool_seen(
            txid TEXT PRIMARY KEY, first_seen INTEGER NOT NULL);",
    )
}

impl Indexer {
    pub fn open(path: &str, rpc: RpcClient) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|e| format!("open db {path}: {e}"))?;
        schema(&db).map_err(|e| format!("schema: {e}"))?;
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
        let mut this = Self { db, rpc };
        // Backfill the exact PoS marker for indexed rows (cheap header
        // call each — the whole testnet chain is a few hundred blocks).
        // Rows already marked stay untouched.
        this.backfill_pos();
        // Backfill size/merkleroot the same way (one getblock each, skipped
        // silently when the node is unreachable — the columns just stay 0).
        this.backfill_block_meta();
        Ok(this)
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
        // IsVerusPOSBlock() the validator uses). Missing on old nodes → 0.
        let pos = b.get("pos").and_then(|v| v.as_bool()).unwrap_or(false) as i64;
        let size = b.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
        let merkleroot = b.get("merkleroot").and_then(|v| v.as_str()).unwrap_or("");

        let txn = self
            .db
            .transaction()
            .map_err(|e| format!("index txn: {e}"))?;
        txn.execute(
            "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![height as i64, hash, time, txs.len() as i64, difficulty, reserve, pot, drought, pos, size, merkleroot],
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
        Ok(())
    }

    /// One sync pass: roll back any diverged tip, then index forward.
    pub fn sync(&mut self) -> Result<SyncReport, String> {
        let tip = self
            .rpc
            .call("getblockcount", serde_json::json!([]))?
            .as_u64()
            .ok_or_else(|| "getblockcount: not a number".to_string())?;
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
        Ok(SyncReport { rolled_back, indexed, tip })
    }

    /// Pull game events (per-block jackpot/pot/league, Luckynode Draws)
    /// from the pool's dashboard API into the index. Best-effort: the
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
                let league = b.get("league_overflow").and_then(|x| x.as_i64()).unwrap_or(0);
                let finder = b.get("finder_name").and_then(|x| x.as_str()).unwrap_or("");
                self.db
                    .execute(
                        "INSERT INTO game_blocks(height, bonus, pot, league, finder)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(height) DO UPDATE SET
                           bonus=?2, pot=?3, league=?4, finder=?5",
                        params![h, bonus, pot, league, finder],
                    )
                    .map_err(|e| format!("game block {h}: {e}"))?;
                n += 1;
            }
        }
        if let Some(draws) = v
            .get("luckynode")
            .and_then(|l| l.get("recent_draws"))
            .and_then(|d| d.as_array())
        {
            for d in draws {
                let epoch = d.get("epoch").and_then(|x| x.as_i64()).unwrap_or(-1);
                if epoch < 0 {
                    continue;
                }
                let height = d.get("height").and_then(|x| x.as_i64()).unwrap_or(0);
                let seed = d.get("seed").and_then(|x| x.as_str()).unwrap_or("");
                let winner = d.get("winner").and_then(|x| x.as_str()).unwrap_or("");
                let budget = d.get("budget").and_then(|x| x.as_i64()).unwrap_or(0);
                let txid = d.get("txid").and_then(|x| x.as_str()).unwrap_or("");
                let cosigns = d.get("cosigns").and_then(|x| x.as_i64()).unwrap_or(0);
                let time = d.get("time").and_then(|x| x.as_i64()).unwrap_or(0);
                self.db
                    .execute(
                        "INSERT INTO draws(epoch, height, seed, winner, budget, txid, cosigns, time)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                         ON CONFLICT(epoch) DO UPDATE SET
                           height=?2, seed=?3, winner=?4, budget=?5, txid=?6,
                           cosigns=?7, time=?8",
                        params![epoch, height, seed, winner, budget, txid, cosigns, time],
                    )
                    .map_err(|e| format!("draw {epoch}: {e}"))?;
                n += 1;
            }
        }
        // Seasonal league boards (doc 03 §3.3: phones race phones). The
        // pool's `league.boards` is a point-in-time snapshot, so the table
        // is replaced wholesale on each well-formed non-empty fetch (a
        // missing/empty fetch leaves the last boards in place).
        if let Some(boards) = v.get("league").and_then(|l| l.get("boards")).and_then(|b| b.as_array()) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut rows: Vec<LeagueBoardRow> = Vec::new();
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
                    rows.push(LeagueBoardRow { class: class.to_string(), rank, miner, name, eff_work: eff, days, streak, attested });
                }
            }
            if !rows.is_empty() {
                let tx = self.db.transaction().map_err(|e| format!("league snapshot: {e}"))?;
                tx.execute("DELETE FROM league_boards", []).map_err(|e| format!("league snapshot: {e}"))?;
                {
                    let mut ins = tx
                        .prepare(
                            "INSERT INTO league_boards(class, rank, miner, name, eff_work, days, streak, attested, synced_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        )
                        .map_err(|e| format!("league snapshot: {e}"))?;
                    for r in &rows {
                        ins.execute(params![
                            r.class, r.rank, r.miner, r.name, r.eff_work, r.days, r.streak, r.attested, now
                        ])
                        .map_err(|e| format!("league snapshot: {e}"))?;
                    }
                }
                tx.commit().map_err(|e| format!("league snapshot: {e}"))?;
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
        type BlockRow = (String, i64, i64, f64, i64, i64, i64, i64, i64, String);
        let row: Option<BlockRow> = self
            .db
            .query_row(
                "SELECT hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot FROM blocks WHERE height = ?1",
                [height],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
            )
            .map(Some)
            .unwrap_or(None);
        let Some((hash, time, ntx, difficulty, reserve, pot, drought, pos, size, merkleroot)) = row else {
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
            .prepare("SELECT n, address, value, spent_by IS NOT NULL FROM outs WHERE txid = ?1 ORDER BY n")
            .map_err(|e| format!("tx out: {e}"))?;
        let outs = st
            .query_map([txid], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?, r.get::<_, bool>(3)?))
            })
            .map_err(|e| format!("tx out: {e}"))?;
        let mut out_list = Vec::new();
        let mut out_total = 0i64;
        for o in outs {
            let (n, addr, value, spent): (i64, Option<String>, i64, bool) =
                o.map_err(|e| format!("tx out: {e}"))?;
            out_total += value;
            out_list.push(serde_json::json!({"n": n, "address": addr, "value": value, "spent": spent}));
        }
        let tip = self.height().unwrap_or(height);
        Ok(Some(serde_json::json!({
            "txid": txid, "height": height, "coinbase": coinbase == 1,
            "confirmations": tip - height + 1,
            "vin": ins, "vout": out_list,
            "fee": if coinbase == 1 { 0 } else { in_total - out_total },
        })))
    }

    pub fn address(&self, addr: &str) -> Result<serde_json::Value, String> {
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
        let mut st = self
            .db
            .prepare(
                "SELECT o.txid, t.height FROM outs o JOIN txs t ON t.txid = o.txid
                 WHERE o.address = ?1
                 UNION SELECT o.spent_by AS txid, t.height FROM outs o JOIN txs t ON t.txid = o.spent_by
                 WHERE o.address = ?2 AND o.spent_by IS NOT NULL
                 ORDER BY height DESC LIMIT 100",
            )
            .map_err(|e| format!("addr history: {e}"))?;
        let rows = st
            .query_map([addr, addr], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
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
            "address": addr, "balance": balance, "received": received,
            "txs": history, "txs_truncated": history.len() == 100,
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
        let supply: i64 = self
            .db
            .query_row("SELECT COALESCE(SUM(value),0) FROM outs WHERE coinbase = 1", [], |r| {
                r.get(0)
            })
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

    /// Per-block game columns for one height (bonus/pot/league/finder).
    pub fn game_block(&self, height: i64) -> Result<Option<serde_json::Value>, String> {
        let row: Option<(i64, i64, i64, String)> = self
            .db
            .query_row(
                "SELECT bonus, pot, league, finder FROM game_blocks WHERE height = ?1",
                [height],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map(Some)
            .unwrap_or(None);
        Ok(row.map(|(bonus, pot, league, finder)| {
            serde_json::json!({
                "height": height, "bonus": bonus, "pot": pot,
                "league": league, "finder": finder,
            })
        }))
    }

    /// Recent game blocks (newest first): every row the pool reported.
    pub fn game_blocks_recent(&self, limit: i64) -> Result<Vec<serde_json::Value>, String> {
        let mut st = self
            .db
            .prepare(
                "SELECT height, bonus, pot, league, finder FROM game_blocks
                 ORDER BY height DESC LIMIT ?1",
            )
            .map_err(|e| format!("game list: {e}"))?;
        let rows = st
            .query_map([limit], |r| {
                Ok(serde_json::json!({
                    "height": r.get::<_, i64>(0)?, "bonus": r.get::<_, i64>(1)?,
                    "pot": r.get::<_, i64>(2)?, "league": r.get::<_, i64>(3)?,
                    "finder": r.get::<_, String>(4)?,
                }))
            })
            .map_err(|e| format!("game list: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("game list: {e}"))?;
        Ok(rows)
    }

    /// Luckynode Draws (newest first).
    pub fn draws_recent(&self, limit: i64) -> Result<Vec<serde_json::Value>, String> {
        let mut st = self
            .db
            .prepare(
                "SELECT epoch, height, seed, winner, budget, txid, cosigns, time
                 FROM draws ORDER BY epoch DESC LIMIT ?1",
            )
            .map_err(|e| format!("draw list: {e}"))?;
        let rows = st
            .query_map([limit], |r| {
                Ok(serde_json::json!({
                    "epoch": r.get::<_, i64>(0)?, "height": r.get::<_, i64>(1)?,
                    "seed": r.get::<_, String>(2)?, "winner": r.get::<_, String>(3)?,
                    "budget": r.get::<_, i64>(4)?, "txid": r.get::<_, String>(5)?,
                    "cosigns": r.get::<_, i64>(6)?, "time": r.get::<_, i64>(7)?,
                }))
            })
            .map_err(|e| format!("draw list: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("draw list: {e}"))?;
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
            }));
        }
        Ok(out)
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
    pub fn mempool(&mut self) -> Result<serde_json::Value, String> {
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
    pub fn network(&self) -> Result<serde_json::Value, String> {
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

    /// Locked emission schedule, base units (chainparams: testnet mirrors
    /// mainnet; verified live — getblocksubsidy h100 → 0.15873015 exact,
    /// h10081 → 16, h1062000 → 8, h4217760 → 1). Era 1 (0–10080) ramps
    /// 0→16 linearly; era 2 (10081–4217760) halves every 1,051,920
    /// (16/8/4/2, the node already applies the 4th halving at exactly
    /// 4217760); era 3 is the fixed 1-coin tail forever.
    #[must_use]
    pub fn subsidy_at(height: u64) -> u64 {
        const RAMP_END: u64 = 10_080;
        const ERA2_END: u64 = 4_217_760;
        const HALVING: u64 = 1_051_920;
        const PEAK: u128 = 1_600_000_000;
        if height <= RAMP_END {
            (PEAK * height as u128 / RAMP_END as u128) as u64
        } else if height <= ERA2_END {
            let halvings = (height - RAMP_END) / HALVING;
            (PEAK >> halvings.min(63)) as u64
        } else {
            100_000_000
        }
    }

    /// Next subsidy change after `height`: (change_height, new_subsidy).
    /// None once the fixed tail is reached.
    #[must_use]
    pub fn next_subsidy_change(height: u64) -> Option<(u64, u64)> {
        const RAMP_END: u64 = 10_080;
        const ERA2_END: u64 = 4_217_760;
        const HALVING: u64 = 1_051_920;
        const PEAK: u64 = 1_600_000_000;
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
    pub fn game_state(&self) -> Result<serde_json::Value, String> {
        let ji = self.rpc.call("getjackpotinfo", serde_json::json!([]))?;
        Ok(serde_json::json!({
            "height": ji.get("height").and_then(|v| v.as_i64()).unwrap_or(-1),
            "reserve": ji.get("reserve").and_then(|v| v.as_i64()).unwrap_or(0),
            "pot": ji.get("pot").and_then(|v| v.as_i64()).unwrap_or(0),
            "drought": ji.get("drought").and_then(|v| v.as_i64()).unwrap_or(0),
            "potdivisor": ji.get("potdivisor").and_then(|v| v.as_i64()).unwrap_or(1000),
            "jackpotdivisor": ji.get("jackpotdivisor").and_then(|v| v.as_i64()).unwrap_or(100),
            "potdrought": ji.get("potdrought").and_then(|v| v.as_i64()).unwrap_or(4320),
        }))
    }

    /// Live node staking state (not indexed — read through on each call).
    /// PUBLIC surface: on/off + chain supply only. Operator-wallet shape
    /// (eligible outputs/balance) is deliberately NOT published — it maps
    /// an idle hot wallet.
    pub fn staking_state(&self) -> Result<serde_json::Value, String> {
        let mining = self.rpc.call("getmininginfo", serde_json::json!([]))?;
        let coins = |v: Option<&serde_json::Value>| -> i64 {
            (v.and_then(|x| x.as_f64()).unwrap_or(0.0) * 100_000_000.0).round() as i64
        };
        Ok(serde_json::json!({
            "staking_on": mining.get("staking").and_then(|v| v.as_bool()).unwrap_or(false),
            "staking_supply": coins(mining.get("stakingsupply")),
        }))
    }

    /// Seasonal league boards as last synced from the pool (ladder order:
    /// phone, cpu, gpu, farm; ranks ascending within each class). Empty
    /// before the first pool sync or while the pool sends nothing.
    pub fn league_boards(&self) -> Result<Vec<serde_json::Value>, String> {
        const LADDER: [&str; 4] = ["phone", "cpu", "gpu", "farm"];
        let mut st = self
            .db
            .prepare(
                "SELECT rank, miner, name, eff_work, days, streak, attested, synced_at
                 FROM league_boards WHERE class = ?1 ORDER BY rank LIMIT 100",
            )
            .map_err(|e| format!("league boards: {e}"))?;
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
                .map_err(|e| format!("league boards: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("league boards: {e}"))?;
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
                "INSERT INTO game_blocks(height, bonus, pot, league, finder)
                 VALUES (8, 6349205, 0, 0, 'dbg5'), (9, 0, 0, 57143, 'dbg5'),
                        (10, 0, 0, 63492, 'dbg5');
                 INSERT INTO draws(epoch, height, seed, winner, budget, txid, cosigns, time)
                 VALUES (2, 9, '000000da', '925505b0', 2857, 'held: x', 1, 1792023245);",
            )
            .expect("fixture");
        let b = idx.game_block(8).expect("game block").expect("row");
        assert_eq!(b["bonus"], 6349205);
        let list = idx.game_blocks_recent(10).expect("list");
        assert_eq!(list.len(), 3);
        assert_eq!(list[0]["height"], 10); // newest first
        let draws = idx.draws_recent(5).expect("draws");
        assert_eq!(draws[0]["epoch"], 2);
        assert_eq!(draws[0]["cosigns"], 1);
        assert_eq!(draws[0]["budget"], 2857);
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
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn league_boards_roundtrip_in_ladder_order() {
        let rpc = RpcClient::new("127.0.0.1:1", "u", "p");
        let dir = std::env::temp_dir().join("explorer-league-test.db");
        let _ = std::fs::remove_file(&dir);
        let idx = Indexer::open(dir.to_str().expect("path"), rpc).expect("open");
        // Empty before the first pool sync.
        let boards = idx.league_boards().expect("boards");
        assert_eq!(boards.len(), 4);
        assert!(boards.iter().all(|b| b["entries"].as_array().map(|e| e.is_empty()).unwrap_or(false)));
        idx.db
            .execute_batch(
                "INSERT INTO league_boards(class, rank, miner, name, eff_work, days, streak, attested, synced_at)
                 VALUES ('gpu', 1, 'g1', 'g1', 400, 3, 2, 0, 7),
                        ('phone', 2, 'a2', 'addr-a2', 500, 5, 5, 1, 7),
                        ('phone', 1, 'a1', 'a1', 900, 6, 6, 0, 7);",
            )
            .expect("fixture");
        let boards = idx.league_boards().expect("boards");
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
