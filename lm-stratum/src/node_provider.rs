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
//! Jackpot + Lucky Pot accounting: the node advertises `jackpotbonus`,
//! `potcredited` and `potdrought` on `getblocktemplate`; with the claim
//! flags the pool splices the claimed amounts into the coinbase and only
//! submits a claim-template find when the digest actually wins the
//! matching draw (doc 10 §10.8, internal/16). Without the flags the
//! node's template passes through untouched — normal-mode mining.

use crate::provider::{BlockOutcome, JobTemplate, TemplateProvider};
use crate::rpc::RpcClient;
use lm_pow::{is_jackpot, pow_hash};
use primitive_types::U256;
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often the provider re-polls the node for a fresh template.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// One retained template: merkle -> tx set + its claims (both 0 for
/// normal templates). Submissions are gated against the CLAIMED amounts,
/// not the current template (comparing against the current one would
/// skip the gate almost every time — same lesson as the original bonus
/// ring).
#[derive(Debug, Clone)]
pub struct RecentTemplate {
    merkle: [u8; 32],
    txs: Vec<Vec<u8>>,
    /// Jackpot bonus claimed by this template's coinbase (0 = none).
    pub bonus: u64,
    /// Pot splash claimed by this template's coinbase (0 = none).
    pub pot: u64,
    /// Normal net value of the template's coinbase (the pool's receipt).
    pub coinbase: u64,
}

#[derive(Debug, Clone)]
struct TemplateState {
    template: JobTemplate,
    /// All transactions, coinbase first, in template order.
    txs: Vec<Vec<u8>>,
    /// Recent (merkle -> tx set, claims) ring. The header's merkle field
    /// commits to the tx set, so a submission must be assembled with the
    /// tx set THAT MATCHED THE JOB'S HEADER — not the latest template
    /// (drill-caught: a 2s refresh between grind and submit swapped the
    /// coinbase and made the submitted merkle disagree with its own
    /// header).
    recent: std::collections::VecDeque<RecentTemplate>,
    /// Bonus claimed by this template's coinbase (0 = normal template).
    bonus: u64,
    /// Pot claimed by this template's coinbase (0 = normal template).
    pot_claim: u64,
    /// Normal net value of the current template's coinbase (parsed from the
    /// node's own template bytes — what an accepted block pays the pool).
    coinbase: u64,
    fetched_at: Instant,
}

/// Merkle field offset in the Addendum A header layout.
const MERKLE_RANGE: std::ops::Range<usize> = 36..68;

fn header_merkle(header: &[u8]) -> Option<[u8; 32]> {
    header.get(MERKLE_RANGE).and_then(|m| m.try_into().ok())
}

/// Provider backed by a running node's JSON-RPC.
pub struct NodeProvider {
    rpc: RpcClient,
    state: Mutex<TemplateState>,
    accepted: Mutex<u64>,
    rejected: Mutex<u64>,
    /// Serve jackpot-bonus templates (coinbase claims min(5R, reserve))
    /// and only submit their finds when the digest is a jackpot.
    claim_jackpots: bool,
    /// Serve pot templates (coinbase claims the tier-A floor of the
    /// credited pot) and only submit their finds when the digest is a
    /// pot event (`digest < T/1000`).
    claim_pot: bool,
}

impl NodeProvider {
    /// Connect and fetch the first template; fails fast if the node is
    /// unreachable or returns something unmappable.
    pub fn connect(addr: &str, user: &str, pass: &str) -> Result<Self, String> {
        Self::connect_with_claims(addr, user, pass, false, false)
    }

    /// As [`connect`], optionally serving claim templates.
    pub fn connect_with_claims(
        addr: &str,
        user: &str,
        pass: &str,
        claim_jackpots: bool,
        claim_pot: bool,
    ) -> Result<Self, String> {
        let rpc = RpcClient::new(addr, user, pass);
        let gbt = rpc.call("getblocktemplate", serde_json::json!([{"capabilities": []}]))?;
        let (tpl, txs, bonus, pot, coinbase) =
            build_template(&gbt, claim_jackpots, claim_pot)?;
        let mut recent = std::collections::VecDeque::new();
        if let Some(m) = header_merkle(&tpl.header) {
            recent.push_front(RecentTemplate { merkle: m, txs: txs.clone(), bonus, pot, coinbase });
        }
        Ok(Self {
            rpc,
            state: Mutex::new(TemplateState {
                template: tpl,
                txs,
                recent,
                bonus,
                pot_claim: pot,
                coinbase,
                fetched_at: Instant::now(),
            }),
            accepted: Mutex::new(0),
            rejected: Mutex::new(0),
            claim_jackpots,
            claim_pot,
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
        // Check due-ness under the lock, then release it for the RPC: a
        // slow node must not block every connection thread's `current()`
        // (audit fix — the lock used to be held across the HTTP call).
        // Concurrent refreshes are benign: both fetch, last write wins.
        {
            let st = self.state.lock().expect("state");
            if st.fetched_at.elapsed() < REFRESH_INTERVAL {
                return;
            }
        }
        let claim_jackpots = self.claim_jackpots;
        let claim_pot = self.claim_pot;
        match self
            .rpc
            .call("getblocktemplate", serde_json::json!([{"capabilities": []}]))
            .and_then(|gbt| build_template(&gbt, claim_jackpots, claim_pot))
        {
            Ok((tpl, txs, bonus, pot, coinbase)) => {
                let mut st = self.state.lock().expect("state");
                if let Some(m) = header_merkle(&tpl.header) {
                    st.recent.push_front(RecentTemplate {
                        merkle: m,
                        txs: txs.clone(),
                        bonus,
                        pot,
                        coinbase,
                    });
                    // 256 refreshes (~8.5 min at 2s) covers the whole job
                    // lifetime (server registry: 16 jobs x 30s), so a slow
                    // find can still be assembled with its own tx set.
                    while st.recent.len() > 256 {
                        st.recent.pop_back();
                    }
                }
                st.template = tpl;
                st.txs = txs;
                st.bonus = bonus;
                st.pot_claim = pot;
                st.coinbase = coinbase;
                st.fetched_at = Instant::now();
            }
            Err(_) => {
                // Keep serving the last known template; the chain may just
                // be busy. Polling resumes on the next tick.
                let mut st = self.state.lock().expect("state");
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
        let merkle = match header_merkle(header) {
            Some(m) => m,
            None => {
                *self.rejected.lock().expect("rejected") += 1;
                return BlockOutcome {
                    accepted: false,
                    out,
                    jackpot: false,
                    bonus: 0,
                    league_overflow: 0,
                    reserve: 0,
                    coinbase: 0,
                };
            }
        };
        // Locate the template this header belongs to (current or ring) —
        // assembly AND gating both need ITS claims, not the latest
        // template's. Its normal coinbase value rides along for payout
        // accounting.
        let (txs, bonus, pot, coinbase) = {
            let st = self.state.lock().expect("state");
            if header_merkle(&st.template.header) == Some(merkle) {
                (st.txs.clone(), st.bonus, st.pot_claim, st.coinbase)
            } else {
                match st.recent.iter().find(|r| r.merkle == merkle) {
                    Some(r) => (r.txs.clone(), r.bonus, r.pot, r.coinbase),
                    None => {
                        // Unknown template (older than the ring): refuse
                        // rather than assemble a block whose merkle cannot
                        // match its header.
                        drop(st);
                        *self.rejected.lock().expect("rejected") += 1;
                        return BlockOutcome {
                            accepted: false,
                            out,
                            jackpot: false,
                            bonus: 0,
                            league_overflow: 0,
                            reserve: 0,
                            coinbase: 0,
                        };
                    }
                }
            }
        };
        // Local draw gate: a claim-template find is only submittable when
        // the digest actually won the draw its coinbase claims (jackpot
        // `digest < T/100`, pot `digest < T/1000`). Normal templates have
        // no claims and always pass.
        let net_target = self.state.lock().expect("state").template.net_target;
        if !claim_gate(&out, &net_target, bonus, pot) {
            *self.rejected.lock().expect("rejected") += 1;
            return BlockOutcome {
                accepted: false,
                out,
                jackpot: false,
                bonus: 0,
                league_overflow: 0,
                reserve: 0,
                coinbase: 0,
            };
        }
        let block_hex = assemble_block(header, nonce, &txs);
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
            coinbase: if accepted { coinbase } else { 0 },
        }
    }
}

/// Pre-submit gate for claim templates. `bonus`/`pot` are the amounts the
/// candidate coinbase claims: a jackpot claim needs `digest < T/100`, a
/// pot claim needs `digest < T/1000` (big-endian strict compares). Both
/// draw thresholds come from the NETWORK target — vardiff can move the
/// pool target away from it. Normal templates (0/0) always pass.
#[must_use]
pub fn claim_gate(out: &[u8; 32], net_target: &[u8; 32], bonus: u64, pot: u64) -> bool {
    if bonus > 0 && !is_jackpot(out, U256::from_big_endian(net_target)) {
        return false;
    }
    if pot > 0 {
        let threshold = U256::from_big_endian(net_target) / U256::from(1000u64);
        if U256::from_big_endian(out) >= threshold {
            return false;
        }
    }
    true
}

/// Parse a `getblocktemplate` result into (JobTemplate, all tx bytes with the coinbase first).
/// Pure and testable against a frozen fixture.
pub fn parse_gbt(gbt: &serde_json::Value) -> Result<(JobTemplate, Vec<Vec<u8>>), String> {
    let obj = gbt.as_object().ok_or("gbt: not an object")?;
    let version = obj.get("version").and_then(|v| v.as_i64()).ok_or("gbt: version")? as u32;
    let prev = obj.get("previousblockhash").and_then(|v| v.as_str()).ok_or("gbt: prev")?;
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
    // Every txid we use is recomputed from its bytes and cross-checked
    // against the node's "hash" field when present — a template whose
    // stated hashes disagree with its data is refused, not assembled.
    let coinbase_txid = txid(&coinbase);
    cross_check("coinbasetxn.hash", obj.get("coinbasetxn").and_then(|c| c.get("hash")), &coinbase_txid)?;
    let mut txs = vec![coinbase];
    let mut txids = vec![coinbase_txid];
    let mempool = obj
        .get("transactions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for (i, tx) in mempool.iter().enumerate() {
        let data = hex_decode(
            tx.get("data").and_then(|v| v.as_str()).ok_or_else(|| format!("gbt: transactions[{i}].data"))?,
        )?;
        let id = txid(&data);
        cross_check(&format!("transactions[{i}].hash"), tx.get("hash"), &id)?;
        txs.push(data);
        txids.push(id);
    }
    let merkle = merkle_root(&txids)?;

    // Header bytes per doc 05 Addendum A (uint256s in internal LE order,
    // i.e. reversed from the display hex the RPC returns).
    let mut header = Vec::with_capacity(109);
    header.extend_from_slice(&version.to_le_bytes());
    header.extend_from_slice(&reverse_hex32(prev)?);
    header.extend_from_slice(&merkle); // tree root is already internal (LE) order
    header.extend_from_slice(&reverse_hex32(sapling)?);
    header.extend_from_slice(&time.to_le_bytes());
    header.extend_from_slice(&bits.to_le_bytes());
    header.push(0x00); // CompactSize(len(nSolution)) = 0 for fresh-chain PoW blocks

    Ok((
        JobTemplate { header, height, pool_target: target, net_target: target },
        txs,
    ))
}

/// Built template + its claims: (template, txs, claimed bonus, claimed
/// pot, normal coinbase value). Alias keeps the signature readable (and
/// clippy quiet).
pub type BuiltTemplate = (JobTemplate, Vec<Vec<u8>>, u64, u64, u64);

/// GBT -> [`BuiltTemplate`].
///
/// With `claim_jackpots` and a nonzero advisory `jackpotbonus`, the
/// coinbase gains an output of that amount (same script as its last
/// output). With `claim_pot` and a nonzero advisory `potcredited`, it
/// gains the tier-A floor of the credited pot (`floor(credited/10)`) —
/// the claim any pot event covers, so a floor-claim template is
/// submittable on every tier. The header merkle is recomputed after the
/// splices. Without claims (or with zero amounts) the node's template
/// passes through untouched. The normal coinbase value is parsed from
/// the node's own template bytes (before splicing) — what an accepted
/// block pays the pool.
pub fn build_template(
    gbt: &serde_json::Value,
    claim_jackpots: bool,
    claim_pot: bool,
) -> Result<BuiltTemplate, String> {
    let (mut tpl, mut txs) = parse_gbt(gbt)?;
    let bonus = gbt.get("jackpotbonus").and_then(|v| v.as_u64()).unwrap_or(0);
    let pot_credited = gbt.get("potcredited").and_then(|v| v.as_u64()).unwrap_or(0);
    // Net normal value from the node's own bytes, before splicing.
    let coinbase = crate::tx::coinbase_outputs_total(&txs[0]).map_err(|e| {
        format!("coinbase total for payout accounting: {e:?}")
    })?;
    let claimed_bonus = if claim_jackpots { bonus } else { 0 };
    let claimed_pot = if claim_pot { pot_credited / 10 } else { 0 };
    if (claimed_bonus == 0 && claimed_pot == 0) || txs.is_empty() {
        return Ok((tpl, txs, 0, 0, coinbase));
    }
    let layout = crate::tx::parse_coinbase_layout(&txs[0])
        .map_err(|e| format!("coinbase parse for claim splice: {e:?}"))?;
    if claimed_bonus > 0 {
        txs[0] = crate::tx::append_output(&txs[0], claimed_bonus as i64, &layout.last_script)
            .map_err(|e| format!("bonus splice: {e:?}"))?;
    }
    if claimed_pot > 0 {
        txs[0] = crate::tx::append_output(&txs[0], claimed_pot as i64, &layout.last_script)
            .map_err(|e| format!("pot splice: {e:?}"))?;
    }
    let new_root = merkle_root(&[txid(&txs[0])])?;
    tpl.header[MERKLE_RANGE].copy_from_slice(&new_root);
    Ok((tpl, txs, claimed_bonus, claimed_pot, coinbase))
}

/// Double-SHA256 txid in internal (little-endian) byte order — the exact
/// representation the merkle tree and header fields operate on.
#[must_use]
pub fn txid(data: &[u8]) -> [u8; 32] {
    let h1 = Sha256::digest(data);
    let h2 = Sha256::digest(h1);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h2);
    out
}

/// Bitcoin merkle tree over internal-order txids (duplicate the odd leaf).
/// The node's own check is authoritative at submit time; this computes the
/// template's stated root so assembly and header agree byte-for-byte.
pub fn merkle_root(leaves: &[[u8; 32]]) -> Result<[u8; 32], String> {
    if leaves.is_empty() {
        return Err("merkle tree needs at least one leaf".to_string());
    }
    let mut level: Vec<[u8; 32]> = leaves.to_vec();
    while level.len() > 1 {
        if level.len() % 2 == 1 {
            let last = *level.last().expect("non-empty");
            level.push(last);
        }
        level = level
            .chunks_exact(2)
            .map(|pair| {
                let mut h = Sha256::new();
                h.update(pair[0]);
                h.update(pair[1]);
                let inner = h.finalize();
                let outer = Sha256::digest(inner);
                let mut out = [0u8; 32];
                out.copy_from_slice(&outer);
                out
            })
            .collect();
    }
    Ok(level[0])
}

/// Cross-check a node-stated display-hex hash against a recomputed internal
/// txid. Missing field = skip (some templates omit it); mismatch = refuse.
fn cross_check(field: &str, stated: Option<&serde_json::Value>, computed: &[u8; 32]) -> Result<(), String> {
    let Some(s) = stated.and_then(|v| v.as_str()) else {
        return Ok(());
    };
    if &reverse_hex32(s)? != computed {
        return Err(format!("gbt: {field} disagrees with recomputed txid"));
    }
    Ok(())
}

/// Bitcoin CompactSize encoding (for the nTx count in block assembly).
fn compactsize(n: u64) -> Vec<u8> {
    if n < 0xfd {
        vec![n as u8]
    } else if n <= 0xffff {
        let mut v = vec![0xfd];
        v.extend_from_slice(&(n as u16).to_le_bytes());
        v
    } else if n <= 0xffff_ffff {
        let mut v = vec![0xfe];
        v.extend_from_slice(&(n as u32).to_le_bytes());
        v
    } else {
        let mut v = vec![0xff];
        v.extend_from_slice(&n.to_le_bytes());
        v
    }
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
pub fn assemble_block(header: &[u8], nonce: u64, txs: &[Vec<u8>]) -> Vec<u8> {
    const FIELDS: usize = 108;
    let split = FIELDS.min(header.len());
    let tx_bytes: usize = txs.iter().map(Vec::len).sum();
    let mut b = Vec::with_capacity(header.len() + 32 + 8 + tx_bytes);
    b.extend_from_slice(&header[..split]); // version..bits
    let mut nonce32 = [0u8; 32];
    nonce32[..8].copy_from_slice(&nonce.to_le_bytes());
    b.extend_from_slice(&nonce32); // nNonce (D3: u64 domain, zeros above)
    b.extend_from_slice(&header[split..]); // CompactSize(sol) ‖ sol
    b.extend_from_slice(&compactsize(txs.len() as u64));
    for tx in txs {
        b.extend_from_slice(tx);
    }
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
        let (tpl, txs) = parse_gbt(&gbt).expect("parse");
        assert_eq!(txs.len(), 1);
        let cb = &txs[0];
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
        let (tpl, txs) = parse_gbt(&gbt).expect("parse");
        assert_eq!(txs.len(), 1);
        let cb = &txs[0];
        let block = assemble_block(&tpl.header, 0x1122334455667788, std::slice::from_ref(cb));
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
    fn malformed_fixture_fields_error_cleanly() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["bits"] = serde_json::json!("zz");
        assert!(parse_gbt(&gbt).is_err());
        let mut gbt2: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt2["target"] = serde_json::json!("00");
        assert!(parse_gbt(&gbt2).is_err());
    }

    const TX1_HEX: &str = "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f40";
    const TX2_HEX: &str = "030a11181f262d343b424950575e656c737a81888f969da4abb2b9c0c7ced5dce3eaf1f8ff060d141b222930373e454c535a61686f767d848b9299a0a7aeb5bcc3cad1d8dfe6edf4fb020910171e252c333a41484f565d646b727980878e959ca3aab1b8";

    #[test]
    fn txid_and_merkle_match_golden() {
        let tx1 = hex_decode(TX1_HEX).expect("tx1");
        let tx2 = hex_decode(TX2_HEX).expect("tx2");
        assert_eq!(tx1.len(), 64);
        assert_eq!(tx2.len(), 100);
        // Golden display txids (computed independently with hashlib).
        assert_eq!(rev(&txid(&tx1)), "f70cb6b3f8fb57ab4e21c7475db89f601e9d649cdaa312d5ff6bc77505edc388");
        assert_eq!(rev(&txid(&tx2)), "e1c6db6a573fdf2008af9f0d7d0a274b67e1aae5788b2af673e1da25c40ce3c7");
        // Golden 3-leaf tree (frozen coinbase from the height-5 fixture).
        let gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        let cb = hex_decode(
            gbt["coinbasetxn"]["data"].as_str().expect("cb data"),
        ).expect("cb");
        let root = merkle_root(&[txid(&cb), txid(&tx1), txid(&tx2)]).expect("tree");
        assert_eq!(
            rev(&root),
            "a7d69683ffc4fea27083cee3aec69f0e7b0d8b2b7b66c5e229e6843df91dde5f"
        );
        // Two leaves (cb + tx1): odd-leaf duplication path already covered by
        // the 3-leaf right subtree; assert the 2-leaf golden too.
        let root2 = merkle_root(&[txid(&cb), txid(&tx1)]).expect("tree2");
        assert_eq!(
            rev(&root2),
            "31da3964ab99e08a88947390a853e098d8d012dfdf68480de3e8f1bf969bdbd5"
        );
        assert!(merkle_root(&[]).is_err());
    }

    fn rev(b: &[u8; 32]) -> String {
        b.iter().rev().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn parse_multi_tx_template_uses_tree_merkle() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["transactions"] = serde_json::json!([
            { "data": TX1_HEX, "hash": "f70cb6b3f8fb57ab4e21c7475db89f601e9d649cdaa312d5ff6bc77505edc388" },
            { "data": TX2_HEX, "hash": "e1c6db6a573fdf2008af9f0d7d0a274b67e1aae5788b2af673e1da25c40ce3c7" }
        ]);
        let (tpl, txs) = parse_gbt(&gbt).expect("parse multi");
        assert_eq!(tpl.header.len(), 109);
        assert_eq!(txs.len(), 3);
        // header merkle field == the golden 3-leaf root (internal order)
        assert_eq!(&tpl.header[36..68], &merkle_root(&[
            txid(&txs[0]), txid(&txs[1]), txid(&txs[2])
        ]).expect("tree")[..]);
        // tx bytes preserved in order
        assert_eq!(hex_of(&txs[1]), TX1_HEX.to_lowercase());
        assert_eq!(hex_of(&txs[2]), TX2_HEX.to_lowercase());
        // assemble: nTx = 3, total length pins every splice
        let cb_len = txs[0].len();
        let block = assemble_block(&tpl.header, 7, &txs);
        assert_eq!(block.len(), 108 + 32 + 1 + 1 + cb_len + 64 + 100);
        assert_eq!(block[140], 0x00); // solution len
        assert_eq!(block[141], 0x03); // nTx = 3
        let tail = &block[142..];
        assert_eq!(&tail[..cb_len], &txs[0][..]);
        assert_eq!(&tail[cb_len..cb_len + 64], &txs[1][..]);
        assert_eq!(&tail[cb_len + 64..], &txs[2][..]);
    }

    #[test]
    fn tampered_template_hash_is_refused() {
        // Node-stated hash disagreeing with the bytes: refuse, don't assemble.
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["transactions"] = serde_json::json!([
            { "data": TX1_HEX, "hash": "0000000000000000000000000000000000000000000000000000000000000000" }
        ]);
        let e = parse_gbt(&gbt).unwrap_err();
        assert!(e.contains("disagrees"), "{e}");
        // Missing hash field is tolerated (some templates omit it).
        let mut gbt2: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt2["transactions"] = serde_json::json!([{ "data": TX1_HEX }]);
        assert!(parse_gbt(&gbt2).is_ok());
    }

    #[test]
    fn bonus_template_splices_and_patches_merkle() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["jackpotbonus"] = serde_json::json!(1_142_856u64);
        // claim=false: passthrough, bonus 0, template identical to parse_gbt
        let (t0, tx0, b0, p0, c0) = build_template(&gbt, false, false).expect("passthrough");
        let (t1, tx1) = parse_gbt(&gbt).expect("parse");
        assert_eq!((b0, p0), (0, 0));
        assert_eq!(t0.header, t1.header);
        assert_eq!(tx0, tx1);
        // Normal coinbase value from the fixture's single vout.
        assert_eq!(c0, 0x01_22a4c3);
        // claim=true: coinbase gains a vout, merkle patched to the new txid
        let (t2, tx2, b2, p2, c2) = build_template(&gbt, true, false).expect("bonus template");
        assert_eq!((b2, p2), (1_142_856, 0));
        assert_eq!(c2, 0x01_22a4c3); // claims never move the normal value
        assert_eq!(tx2.len(), 1);
        let l = crate::tx::parse_coinbase_layout(&tx2[0]).expect("layout");
        assert_eq!(l.vout_count, 2);
        assert_eq!(l.last_value, 1_142_856);
        let root = merkle_root(&[txid(&tx2[0])]).expect("root");
        assert_eq!(&t2.header[MERKLE_RANGE], &root[..]);
        assert_ne!(&t2.header[MERKLE_RANGE], &t1.header[MERKLE_RANGE]);
        // claim=true with zero bonus: passthrough
        gbt["jackpotbonus"] = serde_json::json!(0);
        let (_, _, b3, p3, c3) = build_template(&gbt, true, false).expect("zero bonus");
        assert_eq!((b3, p3), (0, 0));
        assert_eq!(c3, 0x01_22a4c3);
    }

    #[test]
    fn pot_template_splices_floor_tier() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["potcredited"] = serde_json::json!(2_500_000u64);
        // claim_jackpots=false, claim_pot=true: one extra vout of the
        // tier-A floor (credited/10), merkle patched.
        let (t0, tx0, b0, p0, c0) = build_template(&gbt, false, true).expect("pot template");
        assert_eq!((b0, p0), (0, 250_000));
        assert_eq!(c0, 0x01_22a4c3);
        let l = crate::tx::parse_coinbase_layout(&tx0[0]).expect("layout");
        assert_eq!(l.vout_count, 2);
        assert_eq!(l.last_value, 250_000);
        let root = merkle_root(&[txid(&tx0[0])]).expect("root");
        assert_eq!(&t0.header[MERKLE_RANGE], &root[..]);
        // potcredited 0: passthrough.
        gbt["potcredited"] = serde_json::json!(0);
        let (_, _, b1, p1, c1) = build_template(&gbt, false, true).expect("zero pot");
        assert_eq!((b1, p1), (0, 0));
        assert_eq!(c1, 0x01_22a4c3);
        // potcredited dust (<10): floor claim is 0, passthrough.
        gbt["potcredited"] = serde_json::json!(9u64);
        let (_, _, _, p2, c2) = build_template(&gbt, false, true).expect("dust pot");
        assert_eq!(p2, 0);
        assert_eq!(c2, 0x01_22a4c3);
    }

    #[test]
    fn both_claims_splice_two_outputs() {
        let mut gbt: serde_json::Value = serde_json::from_str(GBT_FIXTURE).expect("json");
        gbt["jackpotbonus"] = serde_json::json!(1_142_856u64);
        gbt["potcredited"] = serde_json::json!(2_500_000u64);
        let (t, tx, b, p, c) = build_template(&gbt, true, true).expect("both");
        assert_eq!((b, p), (1_142_856, 250_000));
        assert_eq!(c, 0x01_22a4c3);
        let l = crate::tx::parse_coinbase_layout(&tx[0]).expect("layout");
        assert_eq!(l.vout_count, 3);
        assert_eq!(l.last_value, 250_000);
        let root = merkle_root(&[txid(&tx[0])]).expect("root");
        assert_eq!(&t.header[MERKLE_RANGE], &root[..]);
    }

    #[test]
    fn claim_gate_matches_the_node_draws() {
        fn be32(v: u64) -> [u8; 32] {
            let mut a = [0u8; 32];
            a[24..].copy_from_slice(&v.to_be_bytes());
            a
        }
        // out = 1000; targets chosen so T/100 and T/1000 sit on either
        // side of it (strict compares both ways).
        let out = be32(1000);
        let neither = be32(100_000); // T/100 = 1000 (no), T/1000 = 100 (no)
        let jackpot_only = be32(101_000); // T/100 = 1010 (yes), T/1000 = 101 (no)
        let both = be32(2_000_000); // T/100 = 20000 (yes), T/1000 = 2000 (yes)
        // Normal template: gates open regardless of draws.
        assert!(claim_gate(&out, &neither, 0, 0));
        // Jackpot claim: only jackpot-drawing targets pass.
        assert!(claim_gate(&out, &jackpot_only, 1, 0));
        assert!(claim_gate(&out, &both, 1, 0));
        assert!(!claim_gate(&out, &neither, 1, 0));
        // Pot claim: only pot-drawing targets pass.
        assert!(claim_gate(&out, &both, 0, 1));
        assert!(!claim_gate(&out, &jackpot_only, 0, 1));
        assert!(!claim_gate(&out, &neither, 0, 1));
        // Both claims: need both draws.
        assert!(claim_gate(&out, &both, 1, 1));
        assert!(!claim_gate(&out, &jackpot_only, 1, 1));
    }

    #[test]
    fn compactsize_encodes_all_ranges() {
        assert_eq!(compactsize(1), vec![0x01]);
        assert_eq!(compactsize(252), vec![0xfc]);
        assert_eq!(compactsize(253), vec![0xfd, 0xfd, 0x00]);
        assert_eq!(compactsize(0x1234), vec![0xfd, 0x34, 0x12]);
        assert_eq!(compactsize(70000), vec![0xfe, 0x70, 0x11, 0x01, 0x00]);
    }
}
