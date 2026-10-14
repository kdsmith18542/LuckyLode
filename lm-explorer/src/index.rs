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

fn schema(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS blocks(
           height INTEGER PRIMARY KEY, hash TEXT UNIQUE NOT NULL,
           time INTEGER NOT NULL, ntx INTEGER NOT NULL,
           difficulty REAL NOT NULL, reserve INTEGER NOT NULL,
           pot INTEGER NOT NULL, drought INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS txs(
           txid TEXT PRIMARY KEY, height INTEGER NOT NULL, coinbase INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS idx_txs_height ON txs(height);
         CREATE TABLE IF NOT EXISTS outs(
           txid TEXT NOT NULL, n INTEGER NOT NULL, address TEXT,
           value INTEGER NOT NULL, coinbase INTEGER NOT NULL,
           spent_by TEXT, spent_n INTEGER,
           PRIMARY KEY(txid, n));
         CREATE INDEX IF NOT EXISTS idx_outs_address ON outs(address);
         CREATE INDEX IF NOT EXISTS idx_outs_spent ON outs(spent_by);",
    )
}

impl Indexer {
    pub fn open(path: &str, rpc: RpcClient) -> Result<Self, String> {
        let db = Connection::open(path).map_err(|e| format!("open db {path}: {e}"))?;
        schema(&db).map_err(|e| format!("schema: {e}"))?;
        Ok(Self { db, rpc })
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

        let txn = self
            .db
            .transaction()
            .map_err(|e| format!("index txn: {e}"))?;
        txn.execute(
            "INSERT INTO blocks(height, hash, time, ntx, difficulty, reserve, pot, drought)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![height as i64, hash, time, txs.len() as i64, difficulty, reserve, pot, drought],
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

    // ---- query surface for the HTTP layer ----

    pub fn block_by_height(&self, height: i64) -> Result<Option<serde_json::Value>, String> {
        let row: Option<(String, i64, i64, f64, i64, i64, i64)> = self
            .db
            .query_row(
                "SELECT hash, time, ntx, difficulty, reserve, pot, drought FROM blocks WHERE height = ?1",
                [height],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .map(Some)
            .unwrap_or(None);
        let Some((hash, time, ntx, difficulty, reserve, pot, drought)) = row else {
            return Ok(None);
        };
        let tip = self.height().unwrap_or(height);
        Ok(Some(serde_json::json!({
            "height": height, "hash": hash, "time": time, "ntx": ntx,
            "confirmations": tip - height + 1,
            "difficulty": difficulty, "reserve": reserve, "pot": pot, "drought": drought,
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
            history.push(serde_json::json!({"txid": txid, "height": height}));
        }
        Ok(serde_json::json!({
            "address": addr, "balance": balance, "received": received,
            "txs": history, "txs_truncated": history.len() == 100,
        }))
    }

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
}
