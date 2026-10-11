//! Real-node template provider: `getblocktemplate` → jobs, `submitblock`
//! → chain. This is the production [`TemplateProvider`] — the pool speaks
//! to a running forked node instead of the mock chain.
//!
//! Template mapping (Lucky Lode regtest, verified against a live node
//! 2026-10-11): the node returns a fully-built `coinbasetxn.data` plus
//! `previousblockhash`, `finalsaplingroothash`, `version`, `curtime`,
//! `bits`, `target`, and `height`. For a fresh chain with no mempool txs
//! the block is a single coinbase transaction, so the merkle root is the
//! coinbase txid (provided as `coinbasetxn.hash`).
//!
//! Header bytes are the doc 05 Addendum A layout: version ‖ prev ‖ merkle
//! ‖ sapling ‖ time ‖ bits ‖ CompactSize(solution) ‖ solution, all uint256
//! fields in internal (little-endian) order — i.e. the reverse of the
//! display hex the RPC returns. The u64 nonce is ground separately (D3:
//! low 8 bytes of CPOSNonce, upper 192 bits zero) and spliced in only when
//! assembling the full block for `submitblock`.
//!
//! Jackpot accounting fields are zero here: the node-side jackpot
//! consensus (doc 10 §10.8, two-template mining) is not wired yet; the
//! mock provider remains the money-rules testbed until it is.

use crate::provider::{BlockOutcome, JobTemplate, TemplateProvider};
use crate::rpc::RpcClient;
use lm_pow::pow_hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often the provider re-polls the node for a fresh template.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
struct TemplateState {
    template: JobTemplate,
    /// Coinbase transaction bytes (from `coinbasetxn.data`).
    coinbase: Vec<u8>,
    fetched_at: Instant,
}

/// Provider backed by a running node's JSON-RPC.
pub struct NodeProvider {
    rpc: RpcClient,
    state: Mutex<TemplateState>,
    accepted: Mutex<u64>,
    rejected: Mutex<u64>,
}

impl NodeProvider {
    /// Connect and fetch the first template; fails fast if the node is
    /// unreachable or returns something unmappable.
    pub fn connect(addr: &str, user: &str, pass: &str) -> Result<Self, String> {
        let rpc = RpcClient::new(addr, user, pass);
        let gbt = rpc.call("getblocktemplate", serde_json::json!([{"capabilities": []}]))?;
        let state = parse_gbt(&gbt)?;
        Ok(Self {
            rpc,
            state: Mutex::new(TemplateState {
                template: state.0,
                coinbase: state.1,
                fetched_at: Instant::now(),
            }),
            accepted: Mutex::new(0),
            rejected: Mutex::new(0),
        })
    }

    /// (accepted, rejected) block submissions.
    pub fn counters(&self) -> (u64, u64) {
        (
            *self.accepted.lock().expect("accepted"),
            *self.rejected.lock().expect("rejected"),
        )
    }

    fn refresh(&self) {
        let mut st = self.state.lock().expect("state");
        if st.fetched_at.elapsed() < REFRESH_INTERVAL {
            return;
        }
        match self
            .rpc
            .call("getblocktemplate", serde_json::json!([{"capabilities": []}]))
            .and_then(|gbt| parse_gbt(&gbt))
        {
            Ok((tpl, cb)) => {
                st.template = tpl;
                st.coinbase = cb;
                st.fetched_at = Instant::now();
            }
            Err(_) => {
                // Keep serving the last known template; the chain may just
                // be busy. Polling resumes on the next tick.
                st.fetched_at = Instant::now() - REFRESH_INTERVAL + Duration::from_millis(200);
            }
        }
    }
}

impl TemplateProvider for NodeProvider {
    fn current(&self) -> JobTemplate {
        self.refresh();
        self.state.lock().expect("state").template.clone()
    }

    fn submit_block(&mut self, header: &[u8], height: u64, nonce: u64) -> BlockOutcome {
        let out = pow_hash(header, nonce, height);
        let (block_hex, _) = {
            let st = self.state.lock().expect("state");
            (
                assemble_block(header, nonce, &st.coinbase),
                st.coinbase.clone(),
            )
        };
        let result = self
            .rpc
            .call("submitblock", serde_json::json!([hex_of(&block_hex)]));
        let accepted = matches!(&result, Ok(v) if v.is_null());
        if accepted {
            *self.accepted.lock().expect("accepted") += 1;
            // Force a fresh template on the next poll (new tip).
            self.state.lock().expect("state").fetched_at =
                Instant::now() - REFRESH_INTERVAL - Duration::from_secs(1);
        } else {
            *self.rejected.lock().expect("rejected") += 1;
        }
        BlockOutcome {
            accepted,
            out,
            jackpot: false,
            bonus: 0,
            league_overflow: 0,
            reserve: 0,
        }
    }
}

/// Parse a `getblocktemplate` result into (JobTemplate, coinbase bytes).
/// Pure and testable against a frozen fixture.
pub fn parse_gbt(gbt: &serde_json::Value) -> Result<(JobTemplate, Vec<u8>), String> {
    let obj = gbt.as_object().ok_or("gbt: not an object")?;
    let version = obj.get("version").and_then(|v| v.as_i64()).ok_or("gbt: version")? as u32;
    let prev = obj.get("previousblockhash").and_then(|v| v.as_str()).ok_or("gbt: prev")?;
    let merkle = obj
        .get("coinbasetxn")
        .and_then(|c| c.get("hash"))
        .and_then(|v| v.as_str())
        .ok_or("gbt: coinbasetxn.hash")?;
    let sapling = obj
        .get("finalsaplingroothash")
        .and_then(|v| v.as_str())
        .ok_or("gbt: finalsaplingroothash")?;
    let time = obj.get("curtime").and_then(|v| v.as_u64()).ok_or("gbt: curtime")? as u32;
    let bits_hex = obj.get("bits").and_then(|v| v.as_str()).ok_or("gbt: bits")?;
    let bits = u32::from_str_radix(bits_hex, 16).map_err(|_| "gbt: bits hex")?;
    let height = obj.get("height").and_then(|v| v.as_u64()).ok_or("gbt: height")?;
    let target = parse_target(
        obj.get("target").and_then(|v| v.as_str()).ok_or("gbt: target")?,
    )?;
    let coinbase = hex_decode(
        obj.get("coinbasetxn")
            .and_then(|c| c.get("data"))
            .and_then(|v| v.as_str())
            .ok_or("gbt: coinbasetxn.data")?,
    )?;
    let tx_count = obj
        .get("transactions")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if tx_count != 0 {
        return Err(format!(
            "gbt: {tx_count} mempool transactions present; multi-tx blocks not yet supported by the reference provider"
        ));
    }

    // Header bytes per doc 05 Addendum A (uint256s in internal LE order,
    // i.e. reversed from the display hex the RPC returns).
    let mut header = Vec::with_capacity(109);
    header.extend_from_slice(&version.to_le_bytes());
    header.extend_from_slice(&reverse_hex32(prev)?);
    header.extend_from_slice(&reverse_hex32(merkle)?);
    header.extend_from_slice(&reverse_hex32(sapling)?);
    header.extend_from_slice(&time.to_le_bytes());
    header.extend_from_slice(&bits.to_le_bytes());
    header.push(0x00); // CompactSize(len(nSolution)) = 0 for fresh-chain PoW blocks

    Ok((
        JobTemplate { header, height, pool_target: target, net_target: target },
        coinbase,
    ))
}

/// Assemble the full serialized block for `submitblock`.
///
/// `header` is the Addendum A hashing layout: fields (108 bytes:
/// version‖prev‖merkle‖sapling‖time‖bits) ‖ CompactSize(sol) ‖ sol.
/// CBlockHeader serialization inserts nNonce BETWEEN the fields and the
/// solution, so the assembly splices the nonce in at byte 108 — putting
/// it after the solution-length byte corrupts the node's parse (learned
/// live: merkle mismatch on the first pool drill). The nonce is the u64
/// grind domain in the low 8 bytes (D3); upper 192 bits stay zero.
pub fn assemble_block(header: &[u8], nonce: u64, coinbase: &[u8]) -> Vec<u8> {
    const FIELDS: usize = 108;
    let split = FIELDS.min(header.len());
    let mut b = Vec::with_capacity(header.len() + 32 + 1 + coinbase.len());
    b.extend_from_slice(&header[..split]); // version..bits
    let mut nonce32 = [0u8; 32];
    nonce32[..8].copy_from_slice(&nonce.to_le_bytes());
    b.extend_from_slice(&nonce32); // nNonce (D3: u64 domain, zeros above)
    b.extend_from_slice(&header[split..]); // CompactSize(sol) ‖ sol
    b.push(0x01); // CompactSize(nTx) = 1 (coinbase only)
    b.extend_from_slice(coinbase);
    b
}

/// Display hex → internal little-endian bytes (reverse of the 32 bytes).
fn reverse_hex32(hex: &str) -> Result<[u8; 32], String> {
    let v = hex_decode(hex)?;
    if v.len() != 32 {
        return Err(format!("expected 32-byte hex, got {} bytes", v.len()));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = v[31 - i];
    }
    Ok(out)
}

/// Target hex (big-endian display) stays big-endian: `meets_target` and
/// EmberHash compare digests as big-endian byte strings.
fn parse_target(hex: &str) -> Result<[u8; 32], String> {
    let v = hex_decode(hex)?;
    if v.len() != 32 {
        return Err(format!("expected 32-byte target, got {} bytes", v.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd-length hex".to_string());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| format!("bad hex: {s}")))
        .collect()
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frozen fixture captured from the live regtest node (height 5,
    /// 2026-10-11). Do not regenerate casually: the golden assertions below
    /// pin byte-level mapping.
    const GBT_FIXTURE: &str = r#"{
        "version": 4,
        "previousblockhash": "00001cdfd1bd2cba1669638b735abdc926c84aa8d0193c2d2b2d9c21fc5cedb7",
        "finalsaplingroothash": "3e49b5f954aa9d3545bc6c37744661eea48d7c34e3000d82b7f0010c30f4c2fb",
        "transactions": [],
        "coinbasetxn": {
            "data": "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff03550101ffffffff01c3a4220100000000232103f9c81821efd89ad66e9fc56e0a3d870aa8ce9a424396cb5c856fb6a609977666ac67d7cb6a",
            "hash": "782e685a3a1641a541cd1600da744a32be173485c457a23ccdc30a1533e3142c",
            "required": true
        },
        "target": "0000ffff00000000000000000000000000000000000000000000000000000000",
        "curtime": 1791743847,
        "bits": "1f00ffff",
        "height": 5
    }"#;

    #[test]
    fn parse_fixture_maps_header_bytes_exactly() {
        let gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("fixture json");
        let (tpl, cb) = parse_gbt(&gbt).expect("parse");
        assert_eq!(tpl.height, 5);
        assert_eq!(tpl.header.len(), 109);
        // version LE
        assert_eq!(&tpl.header[0..4], &[4, 0, 0, 0]);
        // prev reversed: display 00001cdf…cedb7 → LE bytes end with 00 00 1c df
        assert_eq!(&tpl.header[4..36][0..2], &[0xb7, 0xed]);
        assert_eq!(&tpl.header[4..36][28..30], &[0xdf, 0x1c]); // display[2..4] reversed
        // merkle reversed
        assert_eq!(tpl.header[36], 0x2c);
        assert_eq!(tpl.header[67], 0x78);
        // sapling reversed
        assert_eq!(tpl.header[68], 0xfb);
        assert_eq!(tpl.header[99], 0x3e);
        // time + bits LE
        assert_eq!(&tpl.header[100..104], &1791743847u32.to_le_bytes());
        assert_eq!(&tpl.header[104..108], &[0xff, 0xff, 0x00, 0x1f]);
        // empty solution
        assert_eq!(tpl.header[108], 0x00);
        // target big-endian as returned
        assert_eq!(&tpl.pool_target[..4], &[0x00, 0x00, 0xff, 0xff]);
        assert_eq!(tpl.pool_target, tpl.net_target);
        // coinbase decoded
        assert_eq!(cb.len(), 98);
        assert_eq!(cb[0], 0x01); // tx version LE low byte
    }

    #[test]
    fn assemble_block_golden_bytes() {
        let gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("fixture json");
        let (tpl, cb) = parse_gbt(&gbt).expect("parse");
        let block = assemble_block(&tpl.header, 0x1122334455667788, &cb);
        assert_eq!(block.len(), 108 + 32 + 1 + 1 + 98);
        // fields identical up to the nonce splice point
        assert_eq!(&block[..108], &tpl.header[..108]);
        // nonce: u64 LE in low 8 bytes, zeros above (bytes 108..140)
        assert_eq!(&block[108..116], &0x1122334455667788u64.to_le_bytes());
        assert!(block[116..140].iter().all(|b| *b == 0));
        // solution len 0 (from header_bytes), tx count 1
        assert_eq!(block[140], 0x00);
        assert_eq!(block[141], 0x01);
        assert_eq!(&block[142..], &cb[..]);
    }

    #[test]
    fn multi_tx_blocks_are_rejected_for_now() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["transactions"] = serde_json::json!([{"data": "00"}]);
        assert!(parse_gbt(&gbt).unwrap_err().contains("mempool transactions"));
    }

    #[test]
    fn malformed_fixture_fields_error_cleanly() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["bits"] = serde_json::json!("zz");
        assert!(parse_gbt(&gbt).is_err());
        let mut gbt2: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt2["target"] = serde_json::json!("00");
        assert!(parse_gbt(&gbt2).is_err());
    }
}
